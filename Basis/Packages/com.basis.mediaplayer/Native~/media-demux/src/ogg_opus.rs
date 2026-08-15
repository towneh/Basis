//! Ogg Opus demuxer (§6.6): the `ogg` crate walks pages; this wrapper
//! follows the OpusHead stream, derives per-packet durations from the TOC
//! byte (exact for valid streams — granule positions only re-state it) and
//! shifts the timeline by pre-skip so priming samples carry negative pts,
//! which the engine's origin drop consumes (§6.9).

use std::io::{Read, Seek, SeekFrom};

use media_clock::{Generation, MediaTime};
use ogg::PacketReader;

use crate::{
    Au, AudioCodec, ByteSource, DemuxError, DemuxLimits, Demuxer, EosReason, Format, StreamEvent,
    TrackId,
};

const TRACK: TrackId = TrackId(1);
const OPUS_RATE: i64 = 48_000;

/// `Read + Seek` over a [`ByteSource`] for the page walker, with the same
/// served-bytes budget the Matroska path carries: a hostile layout must
/// become a typed refusal, not a spin.
struct SourceIo {
    src: Box<dyn ByteSource>,
    pos: u64,
    len: Option<u64>,
    served: u64,
    served_budget: u64,
    eof_reads: u32,
}

const EOF_READ_CAP: u32 = 1024;

impl Read for SourceIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self
            .src
            .read_at(self.pos, buf)
            .map_err(std::io::Error::other)?;
        if n == 0 {
            self.eof_reads += 1;
            if self.eof_reads > EOF_READ_CAP {
                return Err(std::io::Error::other("ogg read budget: end-of-source loop"));
            }
        } else {
            self.served += n as u64;
            if self.served > self.served_budget {
                return Err(std::io::Error::other(
                    "ogg read budget: served bytes exceeded",
                ));
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SourceIo {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let new = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => {
                let len = self
                    .len
                    .ok_or_else(|| std::io::Error::other("seek from end on an unsized source"))?;
                len as i64 + delta
            }
        };
        if new < 0 {
            return Err(std::io::Error::other("seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

/// Packet duration in 48 kHz samples from the TOC byte (RFC 6716 §3.1).
/// `None` = malformed (code-3 packet without a count byte, or over the
/// 120 ms cap).
fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame_samples: u32 = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
        12..=15 => [480, 960][usize::from(config % 2)],
        _ => [120, 240, 480, 960][usize::from(config % 4)],
    };
    let frames: u32 = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    if frames == 0 {
        return None;
    }
    let samples = frame_samples.checked_mul(frames)?;
    // RFC 6716: a packet must not exceed 120 ms.
    (samples <= 5760).then_some(samples)
}

pub struct OggOpusDemuxer {
    reader: PacketReader<SourceIo>,
    generation: Generation,
    serial: u32,
    channels: u32,
    /// OpusHead, announced as codec private data.
    head: Option<Vec<u8>>,
    /// 48 kHz sample position of the next packet, pre-skip applied (starts
    /// negative).
    samples_out: i64,
    notes: Vec<String>,
    foreign_noted: bool,
    ended: bool,
}

impl OggOpusDemuxer {
    pub fn open(
        mut src: Box<dyn ByteSource>,
        limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        let len = src.size().map_err(DemuxError::Source)?;
        let served_budget = limits
            .max_metadata_bytes
            .saturating_add(len.unwrap_or(0).saturating_mul(8));
        let io = SourceIo {
            src,
            pos: 0,
            len,
            served: 0,
            served_budget,
            eof_reads: 0,
        };
        let mut reader = PacketReader::new(io);

        // First packet of the first stream must be OpusHead (RFC 7845).
        let first = reader
            .read_packet()
            .map_err(|e| DemuxError::Parse(format!("ogg: {e}")))?
            .ok_or(DemuxError::Unsupported("empty Ogg stream"))?;
        if first.data.len() < 19 || !first.data.starts_with(b"OpusHead") {
            return Err(DemuxError::Unsupported(
                "Ogg stream is not Opus (no OpusHead)",
            ));
        }
        let serial = first.stream_serial();
        let channels = u32::from(first.data[9]);
        let pre_skip = i64::from(u16::from_le_bytes([first.data[10], first.data[11]]));

        // Second packet: OpusTags, skipped.
        let tags = reader
            .read_packet()
            .map_err(|e| DemuxError::Parse(format!("ogg: {e}")))?;
        if !tags.is_some_and(|t| t.data.starts_with(b"OpusTags")) {
            return Err(DemuxError::Parse("Ogg Opus stream without OpusTags".into()));
        }

        Ok(Self {
            reader,
            generation,
            serial,
            channels,
            head: Some(first.data),
            samples_out: -pre_skip,
            notes: Vec::new(),
            foreign_noted: false,
            ended: false,
        })
    }
}

impl Demuxer for OggOpusDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        if let Some(head) = self.head.take() {
            return Ok(StreamEvent::Format(
                TRACK,
                Format::Audio {
                    codec: AudioCodec::Opus,
                    sample_rate: OPUS_RATE as u32,
                    channels: self.channels,
                    codec_private: head,
                },
            ));
        }
        if self.ended {
            return Ok(StreamEvent::Eos(EosReason::Natural));
        }
        loop {
            let packet = match self
                .reader
                .read_packet()
                .map_err(|e| DemuxError::Parse(format!("ogg: {e}")))?
            {
                Some(packet) => packet,
                None => {
                    self.ended = true;
                    return Ok(StreamEvent::Eos(EosReason::Natural));
                }
            };
            if packet.stream_serial() != self.serial {
                // Multiplexed or chained secondary stream: one stream is
                // the contract here.
                if !self.foreign_noted {
                    self.foreign_noted = true;
                    self.notes.push(format!(
                        "ignoring packets from Ogg stream serial {:#x}",
                        packet.stream_serial()
                    ));
                }
                continue;
            }
            if packet.data.is_empty() {
                continue;
            }
            let Some(samples) = packet_samples(&packet.data) else {
                return Err(DemuxError::Parse("malformed Opus packet TOC".into()));
            };
            let pts = MediaTime::from_micros(self.samples_out * 1_000_000 / OPUS_RATE);
            self.samples_out += i64::from(samples);
            return Ok(StreamEvent::Au(Au {
                track: TRACK,
                data: packet.data,
                pts,
                dts: pts,
                key: true,
                generation: self.generation,
            }));
        }
    }

    fn seek(
        &mut self,
        _target: MediaTime,
        _generation: Generation,
    ) -> Result<MediaTime, DemuxError> {
        // Granule-position bisection is the mechanism; not built yet.
        Err(DemuxError::Unsupported("seek on an Ogg Opus file"))
    }

    fn duration(&self) -> Option<MediaTime> {
        None
    }

    fn audio_track(&self) -> Option<TrackId> {
        Some(TRACK)
    }

    fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
}
