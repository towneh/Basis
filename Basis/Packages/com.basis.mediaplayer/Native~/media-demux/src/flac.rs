//! Raw FLAC file demuxer (§6.6): parses the stream header, then walks
//! frames by header validation (sync pattern, structural fields, CRC-8,
//! and the expected frame/sample number) so each AU is one complete FLAC
//! frame with an exact sample-derived pts. Decoding stays in decode-sw;
//! the whole header region travels as codec private data.

use media_clock::{Generation, MediaTime};

use crate::source::SeqReader;
use crate::{
    Au, AudioCodec, ByteSource, DemuxError, DemuxLimits, Demuxer, EosReason, Format, StreamEvent,
    TrackId,
};

const TRACK: TrackId = TrackId(1);
const SCAN_CHUNK: usize = 64 * 1024;
/// A header candidate needs this much visibility before a failed parse is
/// trusted (headers are at most ~16 bytes; 64 leaves margin).
const HEADER_VISIBILITY: usize = 64;

/// CRC-8 over the frame header, polynomial 0x07 (the FLAC one).
fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A parsed and CRC-validated frame header.
struct FrameHeader {
    /// Total header bytes including the CRC-8.
    len: usize,
    /// Frame number (fixed block size) or first sample number (variable).
    coded_number: u64,
    variable_block_size: bool,
    /// Block size in inter-channel samples.
    block_size: u32,
}

/// Parse a frame header at the start of `data`, validating everything the
/// format pins down plus the CRC-8. `None` = not a frame header here.
fn parse_frame_header(data: &[u8]) -> Option<FrameHeader> {
    if data.len() < 6 || data[0] != 0xFF || data[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable_block_size = data[1] & 0x01 != 0;
    let bs_bits = data[2] >> 4;
    let sr_bits = data[2] & 0x0F;
    let chan_bits = data[3] >> 4;
    let size_bits = (data[3] >> 1) & 0x07;
    if bs_bits == 0 || sr_bits == 15 || chan_bits > 10 || size_bits == 3 || data[3] & 1 != 0 {
        return None;
    }

    // UTF-8-coded frame/sample number: up to 7 bytes.
    let mut idx = 4;
    let first = *data.get(idx)?;
    let extra = match first {
        0x00..=0x7F => 0,
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF7 => 3,
        0xF8..=0xFB => 4,
        0xFC..=0xFD => 5,
        0xFE => 6,
        _ => return None,
    };
    let mut coded_number = if extra == 0 {
        u64::from(first)
    } else {
        u64::from(first & (0x7F >> (extra + 1)))
    };
    idx += 1;
    for _ in 0..extra {
        let b = *data.get(idx)?;
        if b & 0xC0 != 0x80 {
            return None;
        }
        coded_number = (coded_number << 6) | u64::from(b & 0x3F);
        idx += 1;
    }

    let block_size = match bs_bits {
        1 => 192,
        2..=5 => 576 << (bs_bits - 2),
        6 => {
            let b = *data.get(idx)?;
            idx += 1;
            u32::from(b) + 1
        }
        7 => {
            let hi = *data.get(idx)?;
            let lo = *data.get(idx + 1)?;
            idx += 2;
            (u32::from(hi) << 8 | u32::from(lo)) + 1
        }
        _ => 256 << (bs_bits - 8),
    };
    match sr_bits {
        12 => idx += 1,
        13 | 14 => idx += 2,
        _ => {}
    }
    if crc8(data.get(..idx)?) != *data.get(idx)? {
        return None;
    }
    Some(FrameHeader {
        len: idx + 1,
        coded_number,
        variable_block_size,
        block_size,
    })
}

pub struct FlacDemuxer {
    reader: SeqReader,
    limits: DemuxLimits,
    generation: Generation,
    sample_rate: u32,
    /// STREAMINFO max block size: the sample multiplier for fixed-blocksize
    /// frame numbers.
    max_block_size: u32,
    duration: Option<MediaTime>,
    /// The stream header region, announced as codec private data.
    header: Option<Vec<u8>>,
    channels: u32,
    ended: bool,
}

impl FlacDemuxer {
    pub fn open(
        src: Box<dyn ByteSource>,
        limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        let mut reader = SeqReader::new(src);
        let head = reader.peek(4).map_err(DemuxError::Source)?;
        if !head.starts_with(b"fLaC") {
            return Err(DemuxError::Unsupported("not a FLAC stream"));
        }

        // Walk the metadata blocks to find where frames start; keep the
        // whole region as codec private data.
        let mut header_len = 4usize;
        let mut streaminfo: Option<(u32, u32, u32, u64)> = None;
        loop {
            if header_len as u64 + 4 > limits.max_metadata_bytes {
                return Err(DemuxError::Cap("FLAC metadata size"));
            }
            let data = reader.peek(header_len + 4).map_err(DemuxError::Source)?;
            let block: [u8; 4] = data
                .get(header_len..header_len + 4)
                .ok_or_else(|| DemuxError::Parse("FLAC header truncated".into()))?
                .try_into()
                .expect("sliced 4");
            let last = block[0] & 0x80 != 0;
            let kind = block[0] & 0x7F;
            let len =
                usize::from(block[1]) << 16 | usize::from(block[2]) << 8 | usize::from(block[3]);
            let body_start = header_len + 4;
            if body_start as u64 + len as u64 > limits.max_metadata_bytes {
                return Err(DemuxError::Cap("FLAC metadata size"));
            }
            if kind == 0 {
                let data = reader.peek(body_start + len).map_err(DemuxError::Source)?;
                let body = data
                    .get(body_start..body_start + len)
                    .ok_or_else(|| DemuxError::Parse("FLAC STREAMINFO truncated".into()))?;
                if body.len() < 34 {
                    return Err(DemuxError::Parse("FLAC STREAMINFO short".into()));
                }
                let max_block = u32::from(body[2]) << 8 | u32::from(body[3]);
                let rate =
                    u32::from(body[10]) << 12 | u32::from(body[11]) << 4 | u32::from(body[12]) >> 4;
                let channels = ((u32::from(body[12]) >> 1) & 0x07) + 1;
                let total = (u64::from(body[13]) & 0x0F) << 32
                    | u64::from(body[14]) << 24
                    | u64::from(body[15]) << 16
                    | u64::from(body[16]) << 8
                    | u64::from(body[17]);
                streaminfo = Some((rate, channels, max_block, total));
            }
            header_len = body_start + len;
            if last {
                break;
            }
        }
        let Some((sample_rate, channels, max_block_size, total_samples)) = streaminfo else {
            return Err(DemuxError::Parse("FLAC header without STREAMINFO".into()));
        };
        if sample_rate == 0 {
            return Err(DemuxError::Parse("FLAC sample rate 0".into()));
        }
        let data = reader.peek(header_len).map_err(DemuxError::Source)?;
        let header = data
            .get(..header_len)
            .ok_or_else(|| DemuxError::Parse("FLAC header truncated".into()))?
            .to_vec();
        reader.consume(header_len);

        let duration = (total_samples > 0).then(|| {
            MediaTime::from_micros(
                (total_samples as i64).saturating_mul(1_000_000) / i64::from(sample_rate),
            )
        });
        Ok(Self {
            reader,
            limits,
            generation,
            sample_rate,
            max_block_size: max_block_size.max(1),
            duration,
            header: Some(header),
            channels,
            ended: false,
        })
    }

    fn pts_of(&self, header: &FrameHeader) -> MediaTime {
        let samples = if header.variable_block_size {
            header.coded_number
        } else {
            header
                .coded_number
                .saturating_mul(u64::from(self.max_block_size))
        };
        MediaTime::from_micros(
            (samples as i64).saturating_mul(1_000_000) / i64::from(self.sample_rate),
        )
    }
}

impl Demuxer for FlacDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        if let Some(private) = self.header.take() {
            return Ok(StreamEvent::Format(
                TRACK,
                Format::Audio {
                    codec: AudioCodec::Flac,
                    sample_rate: self.sample_rate,
                    channels: self.channels,
                    codec_private: private,
                },
            ));
        }
        if self.ended {
            return Ok(StreamEvent::Eos(EosReason::Natural));
        }

        // The next frame should start here; resync over garbage rather
        // than fail (padding, tags), bounded by the AU cap.
        let mut skipped = 0u64;
        let header = loop {
            let data = self
                .reader
                .peek(HEADER_VISIBILITY)
                .map_err(DemuxError::Source)?;
            if data.is_empty() {
                self.ended = true;
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            if let Some(header) = parse_frame_header(data) {
                break header;
            }
            self.reader.consume(1);
            skipped += 1;
            if skipped > self.limits.max_au_bytes {
                return Err(DemuxError::Cap("FLAC resync distance"));
            }
        };
        let pts = self.pts_of(&header);

        // Frame end = the next header that carries the expected coded
        // number (frame + 1, or sample + block size), which together with
        // the CRC-8 makes payload false-syncs a non-issue.
        let expected = if header.variable_block_size {
            header.coded_number + u64::from(header.block_size)
        } else {
            header.coded_number + 1
        };
        let mut end = header.len;
        loop {
            let want = end + SCAN_CHUNK;
            let data = self.reader.peek(want).map_err(DemuxError::Source)?;
            let at_eof = data.len() < want;
            let scan_limit = if at_eof {
                data.len()
            } else {
                data.len() - HEADER_VISIBILITY
            };
            let mut found = None;
            let mut i = end;
            while i < scan_limit {
                if data[i] == 0xFF
                    && let Some(candidate) = parse_frame_header(&data[i..])
                    && candidate.variable_block_size == header.variable_block_size
                    && candidate.coded_number == expected
                {
                    found = Some(i);
                    break;
                }
                i += 1;
            }
            if let Some(i) = found {
                end = i;
                break;
            }
            if at_eof {
                end = data.len();
                break;
            }
            end = scan_limit;
            if end as u64 > self.limits.max_au_bytes {
                return Err(DemuxError::Cap("FLAC frame size"));
            }
        }
        let data = self.reader.peek(end).map_err(DemuxError::Source)?[..end].to_vec();
        self.reader.consume(end);
        Ok(StreamEvent::Au(Au {
            track: TRACK,
            data,
            pts,
            dts: pts,
            key: true,
            generation: self.generation,
        }))
    }

    fn seek(
        &mut self,
        _target: MediaTime,
        _generation: Generation,
    ) -> Result<MediaTime, DemuxError> {
        // Byte positions for a target sample need the SEEKTABLE (often
        // absent) or a bisection over validated headers; neither is built
        // yet.
        Err(DemuxError::Unsupported("seek on a raw FLAC file"))
    }

    fn duration(&self) -> Option<MediaTime> {
        self.duration
    }

    fn audio_track(&self) -> Option<TrackId> {
        Some(TRACK)
    }
}
