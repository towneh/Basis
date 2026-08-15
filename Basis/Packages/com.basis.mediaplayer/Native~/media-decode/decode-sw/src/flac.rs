//! FLAC decode on claxon (§6.7: in-process, no platform decoder involved —
//! the C-era blocker was the hanging MF FLAC MFT, not FLAC itself).
//!
//! Each submitted AU is one complete FLAC frame (Matroska stores frames;
//! the raw-file demuxer emits whole frames), decoded synchronously into a
//! small output queue — FLAC has no codec latency, so drain is a no-op.

use std::collections::VecDeque;
use std::io::Cursor;

use claxon::frame::FrameReader;
use media_decode::{AudioDecoder, DecodeError, PcmChunk, SubmitOutcome};

/// Decoded chunks the queue holds before `submit` pushes back; the release
/// schedule bounds arrivals well below this in practice.
const READY_CAP: usize = 64;

pub struct FlacDecoder {
    sample_rate: u32,
    channels: u32,
    /// Normalisation for the stated bit depth: samples arrive as integers
    /// scaled to `bits_per_sample`.
    scale: f32,
    ready: VecDeque<PcmChunk>,
    /// claxon's decode buffer, recycled across frames.
    buffer: Vec<i32>,
}

impl FlacDecoder {
    /// `codec_private` is the FLAC stream header (the `fLaC` marker plus
    /// metadata blocks) as Matroska stores it and the raw demuxer forwards
    /// it; STREAMINFO inside it is authoritative for format and bit depth.
    pub fn new(codec_private: &[u8]) -> Result<Self, DecodeError> {
        let reader = claxon::FlacReader::new(Cursor::new(codec_private))
            .map_err(|e| DecodeError(format!("FLAC stream header: {e}")))?;
        let info = reader.streaminfo();
        if info.channels == 0 || info.channels > 8 {
            return Err(DecodeError(format!(
                "FLAC channel count {} unsupported",
                info.channels
            )));
        }
        if info.sample_rate == 0 {
            return Err(DecodeError("FLAC sample rate 0".into()));
        }
        if info.bits_per_sample == 0 || info.bits_per_sample > 32 {
            return Err(DecodeError(format!(
                "FLAC bit depth {} unsupported",
                info.bits_per_sample
            )));
        }
        Ok(Self {
            sample_rate: info.sample_rate,
            channels: info.channels,
            scale: 1.0 / (1i64 << (info.bits_per_sample - 1)) as f32,
            ready: VecDeque::new(),
            buffer: Vec::new(),
        })
    }
}

impl AudioDecoder for FlacDecoder {
    fn output_format(&self) -> (u32, u32) {
        (self.sample_rate, self.channels)
    }

    fn submit(&mut self, au: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        if self.ready.len() >= READY_CAP {
            return Ok(SubmitOutcome::NotAccepting);
        }
        let mut frames = FrameReader::new(Cursor::new(au));
        let mut pts_us = pts_us;
        loop {
            match frames.read_next_or_eof(std::mem::take(&mut self.buffer)) {
                Ok(Some(block)) => {
                    let frames_n = block.duration();
                    let channels = block.channels();
                    if channels != self.channels {
                        return Err(DecodeError(format!(
                            "FLAC frame channel count {channels} != stream {}",
                            self.channels
                        )));
                    }
                    let mut data = Vec::with_capacity((frames_n * channels) as usize);
                    for i in 0..frames_n {
                        for ch in 0..channels {
                            data.push(block.channel(ch)[i as usize] as f32 * self.scale);
                        }
                    }
                    self.ready.push_back(PcmChunk {
                        sample_rate: self.sample_rate,
                        channels,
                        pts_us,
                        data,
                    });
                    pts_us += i64::from(frames_n) * 1_000_000 / i64::from(self.sample_rate);
                    self.buffer = block.into_buffer();
                }
                Ok(None) => return Ok(SubmitOutcome::Accepted),
                Err(e) => return Err(DecodeError(format!("FLAC frame: {e}"))),
            }
        }
    }

    fn try_output(&mut self) -> Result<Option<PcmChunk>, DecodeError> {
        Ok(self.ready.pop_front())
    }

    fn begin_drain(&mut self) -> Result<(), DecodeError> {
        Ok(())
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.ready.clear();
        Ok(())
    }
}
