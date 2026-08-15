//! Audio through MediaCodec (§6.7: AAC and MP3 decode on the platform,
//! never bundled). No surface — PCM comes back through the codec's output
//! buffers, converted to interleaved f32 for the ring. Float output is
//! requested at configure; the output format's stated encoding decides
//! how each buffer is read, so a codec that ignores the request still
//! decodes correctly through the s16 path.

use std::ffi::CStr;
use std::time::Duration;

use media_decode::{AudioDecoder, DecodeError, PcmChunk, SubmitOutcome};

use crate::driver::AsyncCodec;
use crate::ffi::*;

/// Drain-tail wait budget, sliced (see the video adapter's rationale).
const DRAIN_BUDGET: Duration = Duration::from_secs(1);
const DRAIN_SLICE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy)]
pub enum AudioMime {
    Aac,
    Mp3,
}

impl AudioMime {
    fn as_cstr(self) -> &'static CStr {
        match self {
            AudioMime::Aac => c"audio/mp4a-latm",
            AudioMime::Mp3 => c"audio/mpeg",
        }
    }
}

pub struct McAudioDecoder {
    codec: AsyncCodec,
    /// The format the ring is built for (`output_format()` contract):
    /// as configured; chunks re-state the codec's live values.
    configured: (u32, u32),
    eos_out: bool,
    drain_waited: Duration,
}

impl McAudioDecoder {
    /// `codec_private` is the AAC AudioSpecificConfig (csd-0); MP3 takes
    /// none. The demux layer reconstructs the ASC on raw/TS lanes, so
    /// every AAC lane arrives with one.
    pub fn new(
        mime: AudioMime,
        sample_rate: u32,
        channels: u32,
        codec_private: &[u8],
    ) -> Result<Self, DecodeError> {
        // SAFETY: format construction with checked pointers; consumed by
        // AsyncCodec::start.
        unsafe {
            let format = AMediaFormat_new();
            if format.is_null() {
                return Err(DecodeError("AMediaFormat_new failed".into()));
            }
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_cstr().as_ptr());
            AMediaFormat_setInt32(format, c"sample-rate".as_ptr(), sample_rate.max(1) as i32);
            AMediaFormat_setInt32(format, c"channel-count".as_ptr(), channels.max(1) as i32);
            AMediaFormat_setInt32(format, c"pcm-encoding".as_ptr(), ENCODING_PCM_FLOAT);
            // Some devices' AAC decoders fold multichannel down to stereo
            // unless configured with an output-channel ceiling. Both the
            // generic (API 32+) and the legacy AAC key — unknown keys are
            // ignored, values above the stream's count clamp to it.
            AMediaFormat_setInt32(format, c"max-output-channel-count".as_ptr(), 99);
            AMediaFormat_setInt32(format, c"aac-max-output-channel_count".as_ptr(), 99);
            if matches!(mime, AudioMime::Aac) {
                if codec_private.is_empty() {
                    AMediaFormat_delete(format);
                    return Err(DecodeError("AAC without AudioSpecificConfig".into()));
                }
                AMediaFormat_setBuffer(
                    format,
                    c"csd-0".as_ptr(),
                    codec_private.as_ptr().cast(),
                    codec_private.len(),
                );
            }
            let codec = AsyncCodec::start(mime.as_cstr(), format, core::ptr::null_mut())?;
            crate::ffi::alog(&format!("mediacodec audio: {}", codec.name));
            Ok(Self {
                codec,
                configured: (sample_rate.max(1), channels.max(1)),
                eos_out: false,
                drain_waited: Duration::ZERO,
            })
        }
    }

    fn take_ready(&mut self) -> Result<Option<PcmChunk>, DecodeError> {
        let Some((index, info)) = self.codec.pop_output(Duration::ZERO) else {
            return Ok(None);
        };
        if info.flags & BUFFER_FLAG_END_OF_STREAM != 0 {
            self.eos_out = true;
        }
        let chunk = if info.size > 0 {
            // The codec's live output format wins over the configured one
            // (HE-AAC doubles the rate mid-stream via format-changed).
            let format = self.codec.output_format();
            let sample_rate = if format.sample_rate > 0 {
                format.sample_rate as u32
            } else {
                self.configured.0
            };
            let channels = if format.channels > 0 {
                format.channels as u32
            } else {
                self.configured.1
            };
            let float_out = format.seen && format.pcm_encoding == ENCODING_PCM_FLOAT;

            // SAFETY: `index` is a granted output index; the buffer spans
            // `size` readable bytes and the reads below stay inside
            // `info.offset..info.offset + info.size`.
            let data = unsafe {
                let mut size = 0usize;
                let buf = AMediaCodec_getOutputBuffer(self.codec.raw(), index as usize, &mut size);
                if buf.is_null() || (info.offset as usize) + (info.size as usize) > size {
                    return Err(DecodeError(format!(
                        "output buffer {index} out of bounds on {}",
                        self.codec.name
                    )));
                }
                let payload =
                    core::slice::from_raw_parts(buf.add(info.offset as usize), info.size as usize);
                if float_out {
                    payload
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect::<Vec<f32>>()
                } else {
                    payload
                        .chunks_exact(2)
                        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                        .collect::<Vec<f32>>()
                }
            };
            Some(PcmChunk {
                sample_rate,
                channels,
                pts_us: info.presentation_time_us,
                data,
            })
        } else {
            None
        };
        // SAFETY: release the granted index exactly once, never rendered.
        let status =
            unsafe { AMediaCodec_releaseOutputBuffer(self.codec.raw(), index as usize, false) };
        if status != AMEDIA_OK {
            return Err(DecodeError(format!(
                "releaseOutputBuffer failed ({status}) on {}",
                self.codec.name
            )));
        }
        Ok(chunk)
    }
}

impl AudioDecoder for McAudioDecoder {
    fn output_format(&self) -> (u32, u32) {
        let format = self.codec.output_format();
        if format.seen && format.sample_rate > 0 && format.channels > 0 {
            (format.sample_rate as u32, format.channels as u32)
        } else {
            self.configured
        }
    }

    fn submit(&mut self, au: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        self.codec.submit(au, pts_us)
    }

    fn try_output(&mut self) -> Result<Option<PcmChunk>, DecodeError> {
        if let Some(e) = self.codec.take_error() {
            return Err(e);
        }
        loop {
            if let Some(chunk) = self.take_ready()? {
                return Ok(Some(chunk));
            }
            // An empty non-EOS buffer (codec config echo) loops; anything
            // else falls through.
            let state = self.codec.cb.state.lock().expect("cb lock");
            if state.output_ready.is_empty() {
                break;
            }
        }
        // One slice per call under the cumulative budget (the video
        // adapter's rationale): the caller stays responsive to flushes
        // while `drain_dry` reports false.
        if self.codec.draining() && !self.eos_out {
            if self.drain_waited < DRAIN_BUDGET {
                self.drain_waited += DRAIN_SLICE;
                if let Some(entry) = self.codec.pop_output(DRAIN_SLICE) {
                    let mut state = self.codec.cb.state.lock().expect("cb lock");
                    state.output_ready.push_front(entry);
                    drop(state);
                    if let Some(chunk) = self.take_ready()? {
                        return Ok(Some(chunk));
                    }
                }
                if let Some(e) = self.codec.take_error() {
                    return Err(e);
                }
            }
            if !self.eos_out && self.drain_waited >= DRAIN_BUDGET {
                crate::ffi::alog(&format!(
                    "mediacodec audio drain timed out on {}; declaring dry",
                    self.codec.name
                ));
                self.eos_out = true;
            }
        }
        Ok(None)
    }

    fn begin_drain(&mut self) -> Result<(), DecodeError> {
        self.codec.begin_drain()
    }

    fn drain_dry(&self) -> bool {
        self.eos_out
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.codec.reset()?;
        self.eos_out = false;
        self.drain_waited = Duration::ZERO;
        Ok(())
    }
}
