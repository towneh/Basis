//! Raw MP3 file demuxer (§6.6): ID3v2 skip, then an MPEG audio frame walk
//! — each AU is one Layer III frame, length computed from its header, pts
//! from accumulated samples. A leading Xing/Info/VBRI frame is metadata,
//! not audio, and is skipped.

use media_clock::{Generation, MediaTime};

use crate::source::SeqReader;
use crate::{
    Au, AudioCodec, ByteSource, DemuxError, DemuxLimits, Demuxer, EosReason, Format, StreamEvent,
    TrackId,
};

const TRACK: TrackId = TrackId(1);

/// Bytes of garbage tolerated while hunting a frame sync (tags between
/// frames, truncated tails).
const RESYNC_CAP: u64 = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Version {
    Mpeg1,
    Mpeg2,
    Mpeg25,
}

#[derive(Clone, Copy)]
struct FrameInfo {
    version: Version,
    sample_rate: u32,
    channels: u32,
    /// Whole frame length in bytes, header included.
    frame_len: usize,
    /// Inter-channel samples per frame at this version.
    samples: u32,
}

const BITRATES_V1_L3: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const BITRATES_V2_L3: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// Parse a 4-byte MPEG audio frame header; Layer III only (the decode
/// route is the MP3 decoder). `None` = not a Layer III header here.
fn parse_frame_header(h: &[u8]) -> Option<FrameInfo> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = match (h[1] >> 3) & 0x03 {
        0b00 => Version::Mpeg25,
        0b10 => Version::Mpeg2,
        0b11 => Version::Mpeg1,
        _ => return None,
    };
    // Layer bits: 01 = Layer III.
    if (h[1] >> 1) & 0x03 != 0b01 {
        return None;
    }
    let bitrate_idx = (h[2] >> 4) as usize;
    let rate_idx = ((h[2] >> 2) & 0x03) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 || rate_idx == 3 {
        // Free-format (index 0) is refused: length is not derivable.
        return None;
    }
    let base_rate = [44_100u32, 48_000, 32_000][rate_idx];
    let sample_rate = match version {
        Version::Mpeg1 => base_rate,
        Version::Mpeg2 => base_rate / 2,
        Version::Mpeg25 => base_rate / 4,
    };
    let bitrate = match version {
        Version::Mpeg1 => BITRATES_V1_L3[bitrate_idx],
        _ => BITRATES_V2_L3[bitrate_idx],
    };
    let padding = usize::from((h[2] >> 1) & 1);
    let (samples, factor) = match version {
        Version::Mpeg1 => (1152u32, 144_000),
        _ => (576, 72_000),
    };
    let frame_len = (factor * bitrate / sample_rate) as usize + padding;
    if frame_len <= 4 {
        return None;
    }
    let channels = if (h[3] >> 6) & 0x03 == 0b11 { 1 } else { 2 };
    Some(FrameInfo {
        version,
        sample_rate,
        channels,
        frame_len,
        samples,
    })
}

/// Whether this frame is a Xing/Info/VBRI metadata frame (the encoder's
/// seek/duration table, decodes as silence): checked at the side-info
/// offset for Xing/Info and at the fixed offset 36 for VBRI.
fn is_metadata_frame(frame: &[u8], info: &FrameInfo) -> bool {
    let side_info = match (info.version, info.channels) {
        (Version::Mpeg1, 1) => 17,
        (Version::Mpeg1, _) => 32,
        (_, 1) => 9,
        (_, _) => 17,
    };
    let xing_at = 4 + side_info;
    let has = |offset: usize, magic: &[u8]| {
        frame
            .get(offset..offset + magic.len())
            .is_some_and(|w| w == magic)
    };
    has(xing_at, b"Xing") || has(xing_at, b"Info") || has(36, b"VBRI")
}

pub struct Mp3Demuxer {
    reader: SeqReader,
    generation: Generation,
    version: Version,
    sample_rate: u32,
    channels: u32,
    announced: bool,
    /// Inter-channel samples emitted so far: the exact pts base.
    samples_out: u64,
    notes: Vec<String>,
    ended: bool,
}

impl Mp3Demuxer {
    pub fn open(
        src: Box<dyn ByteSource>,
        _limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        let mut reader = SeqReader::new(src);

        // ID3v2: "ID3", version (2), flags (1), syncsafe size (4).
        let head = reader.peek(10).map_err(DemuxError::Source)?;
        if head.starts_with(b"ID3") && head.len() >= 10 {
            let size = (u64::from(head[6] & 0x7F) << 21)
                | (u64::from(head[7] & 0x7F) << 14)
                | (u64::from(head[8] & 0x7F) << 7)
                | u64::from(head[9] & 0x7F);
            let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
            reader
                .seek_to(10 + size + footer)
                .map_err(DemuxError::Source)?;
        }

        // Find the first frame: a valid header whose length lands on
        // another valid header (or end of source) — the standard defence
        // against payload false-syncs.
        let mut skipped = 0u64;
        let info = loop {
            let data = reader.peek(4).map_err(DemuxError::Source)?;
            if data.len() < 4 {
                return Err(DemuxError::Unsupported("no MPEG audio frames found"));
            }
            if let Some(info) = parse_frame_header(data) {
                let data = reader
                    .peek(info.frame_len + 4)
                    .map_err(DemuxError::Source)?;
                let confirmed = match data.get(info.frame_len..info.frame_len + 4) {
                    Some(next) => parse_frame_header(next).is_some(),
                    None => data.len() >= 4, // sole frame before EOF
                };
                if confirmed {
                    break info;
                }
            }
            reader.consume(1);
            skipped += 1;
            if skipped > RESYNC_CAP {
                return Err(DemuxError::Unsupported("no MPEG audio frames found"));
            }
        };

        Ok(Self {
            reader,
            generation,
            version: info.version,
            sample_rate: info.sample_rate,
            channels: info.channels,
            announced: false,
            samples_out: 0,
            notes: Vec::new(),
            ended: false,
        })
    }
}

impl Demuxer for Mp3Demuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        if !self.announced {
            self.announced = true;
            return Ok(StreamEvent::Format(
                TRACK,
                Format::Audio {
                    codec: AudioCodec::Mp3,
                    sample_rate: self.sample_rate,
                    channels: self.channels,
                    codec_private: Vec::new(),
                },
            ));
        }
        if self.ended {
            return Ok(StreamEvent::Eos(EosReason::Natural));
        }

        let mut skipped = 0u64;
        loop {
            let data = self.reader.peek(4).map_err(DemuxError::Source)?;
            if data.len() < 4 {
                // Tail bytes that cannot hold a frame (ID3v1 remnants).
                self.ended = true;
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            // A header must match the stream's fixed identity; anything
            // else is a false sync inside garbage — resync.
            let info = match parse_frame_header(data) {
                Some(info)
                    if info.version == self.version
                        && info.sample_rate == self.sample_rate
                        && info.channels == self.channels =>
                {
                    info
                }
                _ => {
                    self.reader.consume(1);
                    skipped += 1;
                    if skipped > RESYNC_CAP {
                        return Err(DemuxError::Cap("MP3 resync distance"));
                    }
                    continue;
                }
            };
            let data = self
                .reader
                .peek(info.frame_len)
                .map_err(DemuxError::Source)?;
            if data.len() < info.frame_len {
                // Truncated final frame: not decodable, drop it.
                self.notes
                    .push("dropped a truncated final MP3 frame".into());
                self.ended = true;
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            let frame = data[..info.frame_len].to_vec();
            self.reader.consume(info.frame_len);
            if self.samples_out == 0 && is_metadata_frame(&frame, &info) {
                continue;
            }
            let pts = MediaTime::from_micros(
                (self.samples_out as i64).saturating_mul(1_000_000) / i64::from(self.sample_rate),
            );
            self.samples_out += u64::from(info.samples);
            return Ok(StreamEvent::Au(Au {
                track: TRACK,
                data: frame,
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
        // CBR arithmetic or the Xing TOC would both serve; neither is
        // built yet.
        Err(DemuxError::Unsupported("seek on a raw MP3 file"))
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
