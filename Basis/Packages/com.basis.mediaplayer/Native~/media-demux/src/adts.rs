//! Raw ADTS (.aac) file demuxer (§6.6): walks ADTS frames, strips the
//! 7/9-byte headers and reconstructs the 2-byte AudioSpecificConfig so the
//! platform AAC decoder gets exactly what the MP4 lane feeds it (raw
//! frames, payload type 0). The same channel screen applies: explicitly
//! signalled 1..=6 channel configurations only.

use media_clock::{Generation, MediaTime};

use crate::source::SeqReader;
use crate::{
    Au, AudioCodec, ByteSource, DemuxError, DemuxLimits, Demuxer, EosReason, Format, StreamEvent,
    TrackId,
};

const TRACK: TrackId = TrackId(1);
const RESYNC_CAP: u64 = 256 * 1024;

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

#[derive(Clone, Copy)]
struct FrameInfo {
    /// AOT − 1 as ADTS codes it (0 = Main, 1 = LC, 2 = SSR).
    profile: u8,
    sf_index: u8,
    sample_rate: u32,
    channel_config: u8,
    header_len: usize,
    /// Whole frame length in bytes, header included.
    frame_len: usize,
}

/// Parse an ADTS frame header. `None` = not an ADTS header here.
fn parse_frame_header(h: &[u8]) -> Option<FrameInfo> {
    if h.len() < 7 || h[0] != 0xFF || h[1] & 0xF6 != 0xF0 {
        // Sync 0xFFF + layer bits 00; the protection bit may be either.
        return None;
    }
    let protection_absent = h[1] & 0x01 != 0;
    let profile = h[2] >> 6;
    let sf_index = (h[2] >> 2) & 0x0F;
    if usize::from(sf_index) >= SAMPLE_RATES.len() {
        return None;
    }
    let channel_config = (h[2] & 0x01) << 2 | h[3] >> 6;
    let frame_len =
        usize::from(h[3] & 0x03) << 11 | usize::from(h[4]) << 3 | usize::from(h[5]) >> 5;
    let header_len = if protection_absent { 7 } else { 9 };
    if frame_len <= header_len {
        return None;
    }
    Some(FrameInfo {
        profile,
        sf_index,
        sample_rate: SAMPLE_RATES[usize::from(sf_index)],
        channel_config,
        header_len,
        frame_len,
    })
}

pub struct AdtsDemuxer {
    reader: SeqReader,
    generation: Generation,
    first: FrameInfo,
    announced: bool,
    /// AAC frames emitted so far; each is 1024 samples at the core rate.
    frames_out: u64,
    notes: Vec<String>,
    ended: bool,
}

impl AdtsDemuxer {
    pub fn open(
        src: Box<dyn ByteSource>,
        _limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        let mut reader = SeqReader::new(src);
        let mut skipped = 0u64;
        let first = loop {
            let data = reader.peek(9).map_err(DemuxError::Source)?;
            if data.len() < 7 {
                return Err(DemuxError::Unsupported("no ADTS frames found"));
            }
            if let Some(info) = parse_frame_header(data) {
                // Confirm on the next header to defeat false syncs.
                let data = reader
                    .peek(info.frame_len + 9)
                    .map_err(DemuxError::Source)?;
                let confirmed = match data.get(info.frame_len..) {
                    Some(next) if next.len() >= 7 => parse_frame_header(next).is_some(),
                    _ => data.len() >= info.frame_len, // sole frame before EOF
                };
                if confirmed {
                    break info;
                }
            }
            reader.consume(1);
            skipped += 1;
            if skipped > RESYNC_CAP {
                return Err(DemuxError::Unsupported("no ADTS frames found"));
            }
        };
        if first.channel_config == 0 || first.channel_config > 6 {
            // 0 = layout in an in-band PCE; >6 is wider than the managed
            // splitter maps. Same screen as the MP4 lane.
            return Err(DemuxError::Unsupported(
                "ADTS channel configuration outside 1..=6",
            ));
        }
        // number_of_raw_data_blocks_in_frame: nonzero packs several AAC
        // frames into one ADTS frame and breaks per-AU timing; encoders
        // do not emit it in practice.
        let head = reader.peek(7).map_err(DemuxError::Source)?;
        if head.len() >= 7 && head[6] & 0x03 != 0 {
            return Err(DemuxError::Unsupported(
                "ADTS with multiple raw data blocks per frame",
            ));
        }
        Ok(Self {
            reader,
            generation,
            first,
            announced: false,
            frames_out: 0,
            notes: Vec::new(),
            ended: false,
        })
    }

    /// The 2-byte explicit AudioSpecificConfig the MF adapter contract
    /// wants: AOT, frequency index, channel configuration.
    fn asc(&self) -> Vec<u8> {
        let aot = self.first.profile + 1;
        vec![
            aot << 3 | self.first.sf_index >> 1,
            (self.first.sf_index & 1) << 7 | self.first.channel_config << 3,
        ]
    }
}

impl Demuxer for AdtsDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        if !self.announced {
            self.announced = true;
            return Ok(StreamEvent::Format(
                TRACK,
                Format::Audio {
                    codec: AudioCodec::Aac,
                    sample_rate: self.first.sample_rate,
                    channels: u32::from(self.first.channel_config),
                    codec_private: self.asc(),
                },
            ));
        }
        if self.ended {
            return Ok(StreamEvent::Eos(EosReason::Natural));
        }

        let mut skipped = 0u64;
        loop {
            let data = self.reader.peek(9).map_err(DemuxError::Source)?;
            if data.len() < 7 {
                self.ended = true;
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            let info = match parse_frame_header(data) {
                Some(info)
                    if info.sf_index == self.first.sf_index
                        && info.channel_config == self.first.channel_config =>
                {
                    info
                }
                _ => {
                    self.reader.consume(1);
                    skipped += 1;
                    if skipped > RESYNC_CAP {
                        return Err(DemuxError::Cap("ADTS resync distance"));
                    }
                    continue;
                }
            };
            let data = self
                .reader
                .peek(info.frame_len)
                .map_err(DemuxError::Source)?;
            if data.len() < info.frame_len {
                self.notes
                    .push("dropped a truncated final ADTS frame".into());
                self.ended = true;
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            let payload = data[info.header_len..info.frame_len].to_vec();
            self.reader.consume(info.frame_len);
            let pts = MediaTime::from_micros(
                (self.frames_out as i64).saturating_mul(1_024 * 1_000_000)
                    / i64::from(info.sample_rate),
            );
            self.frames_out += 1;
            return Ok(StreamEvent::Au(Au {
                track: TRACK,
                data: payload,
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
        Err(DemuxError::Unsupported("seek on a raw ADTS stream"))
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
