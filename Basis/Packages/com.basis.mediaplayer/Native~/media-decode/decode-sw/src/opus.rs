//! Opus decode on libopus via the `opus` crate (§6.7: in-process on every
//! platform, one behaviour — retires the reverse-engineered MF Opus path).
//!
//! Pre-skip is a timestamp concern resolved before the ring (§6.9): the
//! demuxer shifts pts by the codec delay so priming samples arrive with
//! negative pts and the engine's origin drop removes them. The decoder
//! itself is stateless about it.

use std::collections::VecDeque;

use media_decode::{AudioDecoder, DecodeError, PcmChunk, SubmitOutcome};

/// Opus always decodes at 48 kHz; the OpusHead input rate is informational.
const OPUS_RATE: u32 = 48_000;
/// Longest packet libopus will hand back: 120 ms at 48 kHz.
const MAX_FRAMES: usize = 5760;
const READY_CAP: usize = 64;

/// The identification header carried as codec private data (RFC 7845 §5.1).
pub struct OpusHead {
    pub channels: u32,
    pub pre_skip: u16,
    pub mapping_family: u8,
}

impl OpusHead {
    pub fn parse(private: &[u8]) -> Result<Self, DecodeError> {
        if private.len() < 19 || &private[..8] != b"OpusHead" {
            return Err(DecodeError("missing OpusHead".into()));
        }
        Ok(Self {
            channels: u32::from(private[9]),
            pre_skip: u16::from_le_bytes([private[10], private[11]]),
            mapping_family: private[18],
        })
    }

    /// Pre-skip as a duration at the 48 kHz decode rate.
    pub fn pre_skip_us(&self) -> i64 {
        i64::from(self.pre_skip) * 1_000_000 / i64::from(OPUS_RATE)
    }
}

pub struct OpusDecoder {
    inner: opus::Decoder,
    channels: u32,
    ready: VecDeque<PcmChunk>,
}

impl OpusDecoder {
    /// `codec_private` is the OpusHead as Matroska/Ogg store it. Mapping
    /// family 0 only: the `opus` crate exposes no multistream decoder, so
    /// surround Opus is a typed refusal.
    pub fn new(codec_private: &[u8]) -> Result<Self, DecodeError> {
        let head = OpusHead::parse(codec_private)?;
        if head.mapping_family != 0 || head.channels > 2 {
            return Err(DecodeError(format!(
                "Opus mapping family {} with {} channels unsupported (mono/stereo only)",
                head.mapping_family, head.channels
            )));
        }
        let channels = match head.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            n => return Err(DecodeError(format!("Opus channel count {n} unsupported"))),
        };
        let inner = opus::Decoder::new(OPUS_RATE, channels)
            .map_err(|e| DecodeError(format!("libopus decoder: {e}")))?;
        Ok(Self {
            inner,
            channels: head.channels,
            ready: VecDeque::new(),
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn output_format(&self) -> (u32, u32) {
        (OPUS_RATE, self.channels)
    }

    fn submit(&mut self, au: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        if self.ready.len() >= READY_CAP {
            return Ok(SubmitOutcome::NotAccepting);
        }
        let mut data = vec![0.0f32; MAX_FRAMES * self.channels as usize];
        let frames = self
            .inner
            .decode_float(au, &mut data, false)
            .map_err(|e| DecodeError(format!("Opus decode: {e}")))?;
        data.truncate(frames * self.channels as usize);
        self.ready.push_back(PcmChunk {
            sample_rate: OPUS_RATE,
            channels: self.channels,
            pts_us,
            data,
        });
        Ok(SubmitOutcome::Accepted)
    }

    fn try_output(&mut self) -> Result<Option<PcmChunk>, DecodeError> {
        Ok(self.ready.pop_front())
    }

    fn begin_drain(&mut self) -> Result<(), DecodeError> {
        Ok(())
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.ready.clear();
        self.inner
            .reset_state()
            .map_err(|e| DecodeError(format!("Opus reset: {e}")))
    }
}
