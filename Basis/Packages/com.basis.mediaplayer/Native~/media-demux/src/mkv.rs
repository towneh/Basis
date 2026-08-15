//! Matroska/WebM demuxer (§6.6): `matroska-demuxer` walks the EBML;
//! this wrapper maps tracks onto the §6.7 codec table, converts stored
//! H.264 to Annex B, and serves the pull model. Codecs without a decode
//! adapter still announce — refusal is the decode layer's typed call,
//! not a demux failure.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use media_clock::{Generation, MediaTime};

use crate::avc::{self, AvcConfig};
use crate::source::ByteSource;
use crate::{
    Au, AudioCodec, DemuxError, DemuxLimits, Demuxer, EosReason, Format, StreamEvent, TrackId,
    VideoCodec,
};

/// `Read + Seek` over a [`ByteSource`] for the EBML walker, which reads
/// lazily — media data is only pulled as frames are requested.
///
/// The walker retries reads while chasing seek-head positions, so hostile
/// layouts can loop it forever (fuzz-found: ~6M reads/s on a 141-byte
/// input). Two budgets turn that into a typed refusal: a hard cap on
/// end-of-source reads (a legit parse hits EOF a handful of times) and a
/// served-bytes budget sized to the file (a legit walk serves each byte a
/// few times).
struct SourceIo {
    src: Box<dyn ByteSource>,
    pos: u64,
    len: Option<u64>,
    served: u64,
    served_budget: u64,
    eof_reads: u32,
    /// Two-block read cache: the EBML walk issues thousands of tiny reads
    /// and revisits regions across SeekHead/Cues jumps; over a ranged HTTP
    /// source every position jump otherwise reopens the connection (a TLS
    /// round trip each — measured ~5 s of open time on a remote WebM).
    /// Two blocks cover the walk-here-jump-there pattern.
    cache: [(u64, Vec<u8>); 2],
}

const EOF_READ_CAP: u32 = 1024;
const CACHE_BLOCK: u64 = 256 * 1024;

impl SourceIo {
    fn read_uncached(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self
            .src
            .read_at(self.pos, buf)
            .map_err(std::io::Error::other)?;
        if n == 0 {
            self.eof_reads += 1;
            if self.eof_reads > EOF_READ_CAP {
                return Err(std::io::Error::other(
                    "matroska read budget: end-of-source loop",
                ));
            }
        }
        Ok(n)
    }
}

impl Read for SourceIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let block_start = self.pos - self.pos % CACHE_BLOCK;
        let cached = self
            .cache
            .iter()
            .position(|(start, data)| *start == block_start && !data.is_empty());
        let index = match cached {
            Some(index) => index,
            None => {
                // Fetch the whole aligned block (short at end of source),
                // evicting the older cache entry.
                let mut block = vec![0u8; CACHE_BLOCK as usize];
                let mut filled = 0usize;
                while filled < block.len() {
                    let n = self
                        .src
                        .read_at(block_start + filled as u64, &mut block[filled..])
                        .map_err(std::io::Error::other)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                block.truncate(filled);
                if block.is_empty() {
                    return self.read_uncached(buf).inspect(|&n| {
                        self.pos += n as u64;
                    });
                }
                self.cache.swap(0, 1);
                self.cache[0] = (block_start, block);
                0
            }
        };
        let (start, data) = &self.cache[index];
        let offset = (self.pos - start) as usize;
        if offset >= data.len() {
            // Past the cached (short, end-of-source) block: end of source.
            self.eof_reads += 1;
            if self.eof_reads > EOF_READ_CAP {
                return Err(std::io::Error::other(
                    "matroska read budget: end-of-source loop",
                ));
            }
            return Ok(0);
        }
        let n = buf.len().min(data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        self.served += n as u64;
        if self.served > self.served_budget {
            return Err(std::io::Error::other(
                "matroska read budget: served bytes exceeded",
            ));
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

struct SelectedVideo {
    number: u64,
    track: TrackId,
    avc: Option<AvcConfig>,
}

struct SelectedAudio {
    number: u64,
    track: TrackId,
    /// CodecDelay in µs: block timestamps include it, playback time
    /// subtracts it (Matroska stores Opus pre-skip this way), so priming
    /// samples arrive with negative pts and the engine's origin drop
    /// consumes them (§6.9).
    delay_us: i64,
}

pub struct MkvDemuxer {
    file: MatroskaFile<SourceIo>,
    limits: DemuxLimits,
    generation: Generation,
    /// Nanoseconds per Matroska tick.
    scale_ns: u64,
    duration: Option<MediaTime>,
    video: Option<SelectedVideo>,
    audio: Option<SelectedAudio>,
    pending: VecDeque<StreamEvent>,
    notes: Vec<String>,
    ended: bool,
    frame: Frame,
}

fn map_video_codec(codec_id: &str) -> Option<VideoCodec> {
    match codec_id {
        "V_MPEG4/ISO/AVC" => Some(VideoCodec::H264),
        "V_MPEGH/ISO/HEVC" => Some(VideoCodec::H265),
        "V_VP8" => Some(VideoCodec::Vp8),
        "V_VP9" => Some(VideoCodec::Vp9),
        "V_AV1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

fn map_audio_codec(codec_id: &str) -> Option<AudioCodec> {
    match codec_id {
        "A_AAC" => Some(AudioCodec::Aac),
        "A_OPUS" => Some(AudioCodec::Opus),
        "A_FLAC" => Some(AudioCodec::Flac),
        "A_MPEG/L3" => Some(AudioCodec::Mp3),
        _ => None,
    }
}

impl MkvDemuxer {
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
            cache: [(u64::MAX, Vec::new()), (u64::MAX, Vec::new())],
        };
        let file =
            MatroskaFile::open(io).map_err(|e| DemuxError::Parse(format!("matroska: {e:?}")))?;

        let scale_ns = file.info().timestamp_scale().get();
        let duration = file
            .info()
            .duration()
            .map(|ticks| MediaTime::from_micros((ticks * scale_ns as f64 / 1000.0) as i64));

        let mut this = Self {
            file,
            limits,
            generation,
            scale_ns,
            duration,
            video: None,
            audio: None,
            pending: VecDeque::new(),
            notes: Vec::new(),
            ended: false,
            frame: Frame::default(),
        };
        this.select_tracks()?;
        if this.video.is_none() && this.audio.is_none() {
            return Err(DemuxError::Unsupported(
                "no recognised video or audio track in the Matroska file",
            ));
        }
        Ok(this)
    }

    fn select_tracks(&mut self) -> Result<(), DemuxError> {
        for entry in self.file.tracks() {
            let number = entry.track_number().get();
            let track = TrackId(number as u32);
            match entry.track_type() {
                TrackType::Video if self.video.is_none() => {
                    let codec_id = entry.codec_id().to_string();
                    let Some(codec) = map_video_codec(&codec_id) else {
                        self.notes
                            .push(format!("track {number}: skipped video ({codec_id})"));
                        continue;
                    };
                    let (width, height) = entry
                        .video()
                        .map(|video| {
                            (
                                video.pixel_width().get() as u32,
                                video.pixel_height().get() as u32,
                            )
                        })
                        .unwrap_or((0, 0));
                    let avc = match codec {
                        VideoCodec::H264 => {
                            let Some(private) = entry.codec_private() else {
                                self.notes.push(format!(
                                    "track {number}: H.264 without codec private data; skipped"
                                ));
                                continue;
                            };
                            Some(AvcConfig::parse(private)?)
                        }
                        VideoCodec::H265 => {
                            let Some(private) = entry.codec_private() else {
                                self.notes.push(format!(
                                    "track {number}: H.265 without codec private data; skipped"
                                ));
                                continue;
                            };
                            Some(crate::hevc::parse_hvcc(private)?)
                        }
                        _ => None,
                    };
                    // MKV's V_AV1 CodecPrivate is the av1C payload: a
                    // 4-byte header then the config OBUs hardware decoders
                    // want prepended to the first AU.
                    let codec_private = match codec {
                        VideoCodec::Av1 => entry
                            .codec_private()
                            .filter(|p| p.len() > 4)
                            .map(|p| p[4..].to_vec())
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    self.pending.push_back(StreamEvent::Format(
                        track,
                        Format::Video {
                            codec,
                            coded_width: width,
                            coded_height: height,
                            display_width: width,
                            display_height: height,
                            codec_private,
                        },
                    ));
                    self.video = Some(SelectedVideo { number, track, avc });
                }
                TrackType::Audio if self.audio.is_none() => {
                    let codec_id = entry.codec_id().to_string();
                    let Some(codec) = map_audio_codec(&codec_id) else {
                        self.notes
                            .push(format!("track {number}: skipped audio ({codec_id})"));
                        continue;
                    };
                    let (sample_rate, channels) = entry
                        .audio()
                        .map(|audio| {
                            (
                                audio.sampling_frequency() as u32,
                                audio.channels().get() as u32,
                            )
                        })
                        .unwrap_or((0, 0));
                    self.pending.push_back(StreamEvent::Format(
                        track,
                        Format::Audio {
                            codec,
                            sample_rate,
                            channels,
                            codec_private: entry.codec_private().unwrap_or(&[]).to_vec(),
                        },
                    ));
                    self.audio = Some(SelectedAudio {
                        number,
                        track,
                        delay_us: entry.codec_delay().map_or(0, |ns| (ns / 1000) as i64),
                    });
                }
                _ => {
                    self.notes.push(format!(
                        "track {number}: skipped ({:?} {})",
                        entry.track_type(),
                        entry.codec_id()
                    ));
                }
            }
        }
        Ok(())
    }

    fn frame_to_au(&mut self) -> Result<Option<StreamEvent>, DemuxError> {
        let pts_us = (self.frame.timestamp.saturating_mul(self.scale_ns) / 1000) as i64;
        let pts = MediaTime::from_micros(pts_us);
        if self.frame.data.len() as u64 > self.limits.max_au_bytes {
            return Err(DemuxError::Cap("matroska frame size"));
        }
        if let Some(video) = &self.video
            && self.frame.track == video.number
        {
            // Simple blocks carry the keyframe flag; block groups do not —
            // treat unflagged video frames as non-sync.
            let key = self.frame.is_keyframe.unwrap_or(false);
            let data = match &video.avc {
                Some(avc) => avc::to_annex_b(
                    &avc.sps,
                    &avc.pps,
                    avc.nal_length_size,
                    &self.frame.data,
                    key,
                )?,
                None => std::mem::take(&mut self.frame.data),
            };
            return Ok(Some(StreamEvent::Au(Au {
                track: video.track,
                data,
                pts,
                dts: pts,
                key,
                generation: self.generation,
            })));
        }
        if let Some(audio) = &self.audio
            && self.frame.track == audio.number
        {
            let pts = MediaTime::from_micros(pts_us - audio.delay_us);
            return Ok(Some(StreamEvent::Au(Au {
                track: audio.track,
                data: std::mem::take(&mut self.frame.data),
                pts,
                dts: pts,
                key: true,
                generation: self.generation,
            })));
        }
        Ok(None)
    }
}

impl Demuxer for MkvDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            if self.ended {
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            // Same containment as the open boundary: a parser panic on a
            // hostile block is a typed refusal, not a session abort.
            let more = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.file.next_frame(&mut self.frame)
            })) {
                Ok(Ok(more)) => more,
                Ok(Err(e)) => return Err(DemuxError::Parse(format!("matroska: {e:?}"))),
                Err(_) => {
                    return Err(DemuxError::Parse(
                        "matroska parser panicked on a hostile block".into(),
                    ));
                }
            };
            if !more {
                self.ended = true;
                continue;
            }
            if let Some(event) = self.frame_to_au()? {
                return Ok(event);
            }
        }
    }

    fn seek(&mut self, target: MediaTime, generation: Generation) -> Result<MediaTime, DemuxError> {
        // The vendored matroska-demuxer carries the cue-seek fixes (see
        // third_party/matroska-demuxer/PATCHES.md): cue-relative offsets
        // resolved against the right cluster, landing on the cue point
        // (a keyframe) at or before the target.
        let ticks = (target.as_micros().max(0) as u64).saturating_mul(1000) / self.scale_ns.max(1);
        self.file
            .seek_to_cue_point(ticks)
            .map_err(|e| DemuxError::Parse(format!("matroska seek: {e:?}")))?;
        self.pending.clear();
        self.ended = false;
        self.generation = generation;

        // Pull the first landed frame to learn the actual position; it is
        // served from `pending` on the next pull.
        loop {
            let more = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.file.next_frame(&mut self.frame)
            })) {
                Ok(Ok(more)) => more,
                Ok(Err(e)) => return Err(DemuxError::Parse(format!("matroska: {e:?}"))),
                Err(_) => {
                    return Err(DemuxError::Parse(
                        "matroska parser panicked on a hostile block".into(),
                    ));
                }
            };
            if !more {
                // Seek past the end: serve Eos from here.
                self.ended = true;
                return Ok(target);
            }
            if let Some(event) = self.frame_to_au()? {
                let landed = match &event {
                    StreamEvent::Au(au) => au.pts,
                    _ => target,
                };
                self.pending.push_back(event);
                return Ok(landed);
            }
        }
    }

    fn duration(&self) -> Option<MediaTime> {
        self.duration
    }

    fn video_track(&self) -> Option<TrackId> {
        self.video.as_ref().map(|v| v.track)
    }

    fn audio_track(&self) -> Option<TrackId> {
        self.audio.as_ref().map(|a| a.track)
    }

    fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
}
