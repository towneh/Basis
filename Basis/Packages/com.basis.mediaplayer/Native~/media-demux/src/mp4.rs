//! Streaming progressive-MP4 demuxer: re_mp4 parses the box structure and
//! sample tables from positioned reads (mdat is skipped, so a trailing moov
//! costs a couple of range requests, never a full download); sample payloads
//! are then range-read on demand as the engine pulls.
//!
//! M2 scope: one H.264 video track and one AAC audio track, interleaved in
//! decode order. Remaining tracks are reported via [`Mp4Demuxer::take_notes`]
//! so the engine can surface them as diagnostics rather than dropping them
//! silently.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};

use media_clock::{Generation, MediaTime};

use crate::demuxer::{AudioTrackInfo, DemuxLimits, DemuxOptions, Demuxer};
use crate::source::{ByteSource, SourceReader};
use crate::{Au, AudioCodec, DemuxError, EosReason, Format, StreamEvent, TrackId, VideoCodec};

/// ISO/IEC 14496-3 sampling-frequency-index table.
const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

#[derive(Debug, Clone, Copy)]
struct SampleRef {
    offset: u64,
    size: u32,
    pts: MediaTime,
    dts: MediaTime,
    sync: bool,
}

struct VideoTrack {
    id: TrackId,
    samples: Vec<SampleRef>,
    /// H.264/HEVC conversion parameters (HEVC packs VPS/SPS/PPS into
    /// `sps`); `None` for codecs whose samples pass through as stored
    /// (VP9 raw frames, AV1 temporal units).
    avc: Option<AvcParams>,
}

struct AvcParams {
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    nal_length_size: usize,
}

struct AudioTrack {
    id: TrackId,
    samples: Vec<SampleRef>,
}

pub struct Mp4Demuxer {
    src: Box<dyn ByteSource>,
    limits: DemuxLimits,
    generation: Generation,
    duration: Option<MediaTime>,
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    pending: VecDeque<StreamEvent>,
    vidx: usize,
    aidx: usize,
    notes: Vec<String>,
    emit_raw_video: bool,
    audio_tracks: Vec<AudioTrackInfo>,
    /// Cover art from `moov/udta/meta/ilst/covr`.
    artwork: Option<crate::Artwork>,
}

impl Mp4Demuxer {
    pub fn open(
        src: Box<dyn ByteSource>,
        limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        Self::open_with(src, limits, generation, &DemuxOptions::default())
    }

    pub fn open_with(
        src: Box<dyn ByteSource>,
        limits: DemuxLimits,
        generation: Generation,
        options: &DemuxOptions,
    ) -> Result<Self, DemuxError> {
        // Cached reads: the box walk revisits headers and fragmented
        // files interleave per-track sample runs, both of which thrash a
        // ranged HTTP source without a cache.
        let mut src: Box<dyn ByteSource> = Box::new(crate::source::CachedSource::new(src));
        let len = src
            .size()
            .map_err(DemuxError::Source)?
            .ok_or(DemuxError::Unsupported(
                "progressive MP4 needs a source with a known length",
            ))?;

        let mp4 = {
            let reader = SourceReader::new(src.as_mut(), len, limits.max_metadata_bytes);
            // The parser is safe Rust but has panic paths on inconsistent
            // sample tables; contain them to a typed error at this boundary
            // so hostile metadata is a refusal, not a session abort.
            match catch_unwind(AssertUnwindSafe(|| re_mp4::Mp4::read(reader, len))) {
                Ok(Ok(mp4)) => mp4,
                Ok(Err(re_mp4::Error::Io(io))) => return Err(DemuxError::Io(io)),
                Ok(Err(other)) => return Err(DemuxError::Parse(other.to_string())),
                Err(_) => {
                    return Err(DemuxError::Parse(
                        "mp4 parser panicked on inconsistent metadata".into(),
                    ));
                }
            }
        };

        let mut this = Self {
            src,
            limits,
            generation,
            duration: None,
            video: None,
            audio: None,
            pending: VecDeque::new(),
            vidx: 0,
            aidx: 0,
            notes: Vec::new(),
            emit_raw_video: false,
            audio_tracks: Vec::new(),
            artwork: artwork_from_moov(&mp4),
        };
        this.extract_tracks(&mp4, options)?;

        if this.video.is_none() && this.audio.is_none() {
            return Err(DemuxError::Unsupported(
                "no decodable track (need H.264 video or AAC audio)",
            ));
        }
        Ok(this)
    }

    /// Per-track findings the engine should surface as diagnostics
    /// (skipped tracks, refused layouts). Drained once after open.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    pub fn video_track(&self) -> Option<TrackId> {
        self.video.as_ref().map(|v| v.id)
    }

    /// Conformance/oracle mode: emit video payloads exactly as stored
    /// (length-prefixed), skipping Annex-B conversion, so payload hashes
    /// compare directly with ffprobe's per-packet data hashes — keyframes
    /// included. The conversion itself is covered by the decode tests.
    pub fn set_emit_raw_video(&mut self, raw: bool) {
        self.emit_raw_video = raw;
    }

    pub fn audio_track(&self) -> Option<TrackId> {
        self.audio.as_ref().map(|a| a.id)
    }

    fn extract_tracks(
        &mut self,
        mp4: &re_mp4::Mp4,
        options: &DemuxOptions,
    ) -> Result<(), DemuxError> {
        let mut duration = MediaTime::ZERO;

        // The audio tracks a caller could pick between, in container
        // order, before any of them is bound. Only offered when there is
        // more than one: a picker with a single entry is not a choice.
        let audio_ids: Vec<u32> = mp4
            .tracks()
            .iter()
            .filter(|(_, track)| track.kind == Some(re_mp4::TrackKind::Audio))
            .map(|(id, _)| *id)
            .collect();
        // Bind the requested track, or the first when the index is out of
        // range; an undecodable choice falls through to the next below.
        if audio_ids.len() > 1 {
            self.audio_tracks = audio_ids
                .iter()
                .filter_map(|id| {
                    let track = mp4.tracks().get(id)?;
                    Some(describe_audio(mp4, track, TrackId(*id)))
                })
                .collect();
        }
        let wanted = audio_ids.get(options.audio_track).copied();
        if wanted.is_none() && options.audio_track != 0 {
            self.notes.push(format!(
                "audio track {} requested, container has {}; using the first",
                options.audio_track,
                audio_ids.len()
            ));
        }

        for (id, track) in mp4.tracks() {
            let track_id = TrackId(*id);
            match track.kind {
                Some(re_mp4::TrackKind::Video) if self.video.is_none() => {
                    match self.extract_video(mp4, track, track_id)? {
                        Some(()) => {}
                        None => continue,
                    }
                }
                // Skip past the unwanted tracks until the chosen one is
                // reached; if that one turns out undecodable the next
                // decodable track takes over, which is why this is not an
                // equality test.
                Some(re_mp4::TrackKind::Audio)
                    if self.audio.is_none() && wanted.is_none_or(|w| *id >= w) =>
                {
                    if self.extract_audio(mp4, track, track_id).is_none() {
                        continue;
                    }
                }
                _ => {
                    self.notes
                        .push(format!("track {id}: skipped ({:?})", track.kind));
                    continue;
                }
            }
            if track.timescale > 0 {
                let track_duration =
                    MediaTime::from_micros(scale_to_us(track.duration as i64, track.timescale));
                duration = duration.max(track_duration);
            }
        }

        if duration > MediaTime::ZERO {
            self.duration = Some(duration);
        }
        Ok(())
    }

    fn extract_video(
        &mut self,
        mp4: &re_mp4::Mp4,
        track: &re_mp4::Track,
        track_id: TrackId,
    ) -> Result<Option<()>, DemuxError> {
        let stsd = &track.trak(mp4).mdia.minf.stbl.stsd;
        let mut codec_private = Vec::new();
        let (codec, box_width, box_height, avc) = match &stsd.contents {
            re_mp4::StsdBoxContent::Avc1(avc1) => {
                let avcc = &avc1.avcc.contents;
                (
                    VideoCodec::H264,
                    u32::from(avc1.width),
                    u32::from(avc1.height),
                    Some(AvcParams {
                        sps: avcc
                            .sequence_parameter_sets
                            .iter()
                            .map(|n| n.bytes.clone())
                            .collect(),
                        pps: avcc
                            .picture_parameter_sets
                            .iter()
                            .map(|n| n.bytes.clone())
                            .collect(),
                        nal_length_size: (avcc.length_size_minus_one & 0x3) as usize + 1,
                    }),
                )
            }
            re_mp4::StsdBoxContent::Hev1(hev) | re_mp4::StsdBoxContent::Hvc1(hev) => {
                let config = crate::hevc::parse_hvcc(&hev.hvcc.raw)?;
                (
                    VideoCodec::H265,
                    u32::from(hev.width),
                    u32::from(hev.height),
                    Some(AvcParams {
                        sps: config.sps,
                        pps: config.pps,
                        nal_length_size: config.nal_length_size,
                    }),
                )
            }
            // VP9/AV1 samples are stored as the decoders take them (raw
            // frames / temporal units): announce and pass through.
            re_mp4::StsdBoxContent::Vp09(vp09) => (
                VideoCodec::Vp9,
                u32::from(vp09.width),
                u32::from(vp09.height),
                None,
            ),
            re_mp4::StsdBoxContent::Av01(av01) => {
                // The av1C payload is a 4-byte header then the config OBUs
                // (the sequence header); hardware decoders want the OBUs
                // prepended to the first AU.
                if av01.av1c.raw.len() > 4 {
                    codec_private = av01.av1c.raw[4..].to_vec();
                }
                (
                    VideoCodec::Av1,
                    u32::from(av01.width),
                    u32::from(av01.height),
                    None,
                )
            }
            _ => {
                self.notes.push(format!(
                    "track {}: skipped video (unsupported sample entry: {})",
                    track_id.0,
                    track.codec_string(mp4).unwrap_or_default()
                ));
                return Ok(None);
            }
        };

        let samples = self.collect_samples(&track.samples)?;
        let width = if box_width != 0 {
            box_width
        } else {
            track.width as u32
        };
        let height = if box_height != 0 {
            box_height
        } else {
            track.height as u32
        };

        self.pending.push_back(StreamEvent::Format(
            track_id,
            Format::Video {
                codec,
                coded_width: width,
                coded_height: height,
                display_width: width,
                display_height: height,
                codec_private,
            },
        ));
        self.video = Some(VideoTrack {
            id: track_id,
            samples,
            avc,
        });
        Ok(Some(()))
    }

    fn extract_audio(
        &mut self,
        mp4: &re_mp4::Mp4,
        track: &re_mp4::Track,
        track_id: TrackId,
    ) -> Option<()> {
        let trak = track.trak(mp4);
        let stsd = &trak.mdia.minf.stbl.stsd;
        let re_mp4::StsdBoxContent::Mp4a(mp4a) = &stsd.contents else {
            self.notes.push(format!(
                "track {}: skipped audio (not AAC/mp4a)",
                track_id.0
            ));
            return None;
        };
        let Some(esds) = &mp4a.esds else {
            self.notes
                .push(format!("track {}: mp4a without esds", track_id.0));
            return None;
        };
        let dec = &esds.es_desc.dec_config;
        // 0x40 = MPEG-4 Audio, 0x67 = MPEG-2 AAC-LC.
        if dec.object_type_indication != 0x40 && dec.object_type_indication != 0x67 {
            self.notes.push(format!(
                "track {}: skipped audio (object type {:#x}, not AAC)",
                track_id.0, dec.object_type_indication
            ));
            return None;
        }
        let spec = &dec.dec_specific;

        // The parser keeps the decoded ASC fields, not the raw bytes;
        // rebuild the two-byte AudioSpecificConfig for the explicit
        // common case and refuse the escapes (AOT 31, freq index 15) whose
        // reconstruction would be a guess.
        if spec.profile == 0 || spec.profile > 31 || spec.freq_index >= 15 {
            self.notes.push(format!(
                "track {}: refused AAC config (AOT {}, freq index {})",
                track_id.0, spec.profile, spec.freq_index
            ));
            return None;
        }
        // The in-box platform decoders handle at most 6 explicitly
        // signalled channels (the C player's discovered contract: wider
        // layouts AV inside the MFT rather than erroring). PCE-defined
        // layouts (chan_conf 0) leave the real width unknown: refused too.
        if spec.chan_conf < 1 || spec.chan_conf > 6 {
            self.notes.push(format!(
                "track {}: refused AAC channel configuration {}",
                track_id.0, spec.chan_conf
            ));
            return None;
        }
        let asc = vec![
            (spec.profile << 3) | (spec.freq_index >> 1),
            ((spec.freq_index & 1) << 7) | (spec.chan_conf << 3),
        ];
        let sample_rate = AAC_RATES
            .get(spec.freq_index as usize)
            .copied()
            .unwrap_or_else(|| u32::from(mp4a.samplerate.value()));

        // re_mp4 parses the edit list but does not apply it. For audio the
        // initial media_time offset is the encoder priming (video's reorder
        // shift is already normalised away in the sample table): shift the
        // track so priming samples carry negative timestamps and the PCM
        // stage can drop everything before the origin.
        let priming = trak
            .edts
            .as_ref()
            .and_then(|e| e.elst.as_ref())
            .and_then(|elst| {
                elst.entries
                    .iter()
                    .find(|e| e.media_time != u64::MAX && e.media_time != u64::from(u32::MAX))
            })
            .map(|e| e.media_time as i64)
            .unwrap_or(0);

        let samples = match self.collect_shifted_samples(&track.samples, priming) {
            Ok(samples) => samples,
            Err(e) => {
                self.notes
                    .push(format!("track {}: audio refused: {e}", track_id.0));
                return None;
            }
        };

        self.pending.push_back(StreamEvent::Format(
            track_id,
            Format::Audio {
                codec: AudioCodec::Aac,
                sample_rate,
                channels: u32::from(spec.chan_conf),
                codec_private: asc,
            },
        ));
        self.audio = Some(AudioTrack {
            id: track_id,
            samples,
        });
        Some(())
    }

    fn collect_samples(&self, samples: &[re_mp4::Sample]) -> Result<Vec<SampleRef>, DemuxError> {
        self.collect_shifted_samples(samples, 0)
    }

    fn collect_shifted_samples(
        &self,
        samples: &[re_mp4::Sample],
        shift: i64,
    ) -> Result<Vec<SampleRef>, DemuxError> {
        samples
            .iter()
            .map(|s| {
                if s.size > self.limits.max_au_bytes {
                    return Err(DemuxError::Cap("sample larger than the AU ceiling"));
                }
                let timescale = s.timescale.max(1);
                Ok(SampleRef {
                    offset: s.offset,
                    size: s.size as u32,
                    pts: MediaTime::from_micros(scale_to_us(
                        s.composition_timestamp - shift,
                        timescale,
                    )),
                    dts: MediaTime::from_micros(scale_to_us(s.decode_timestamp - shift, timescale)),
                    sync: s.is_sync,
                })
            })
            .collect()
    }

    fn read_sample(&mut self, sample: SampleRef) -> Result<Vec<u8>, DemuxError> {
        let mut data = vec![0u8; sample.size as usize];
        self.src
            .read_exact_at(sample.offset, &mut data)
            .map_err(DemuxError::Source)?;
        Ok(data)
    }

    /// Convert one length-prefixed H.264 sample to Annex B (SPS/PPS
    /// prepended on keyframes so the stream stays decodable from any sync
    /// point); other codecs' samples pass through as stored.
    fn convert_sample(
        video: &VideoTrack,
        src: Vec<u8>,
        keyframe: bool,
    ) -> Result<Vec<u8>, DemuxError> {
        match &video.avc {
            Some(avc) => {
                crate::avc::to_annex_b(&avc.sps, &avc.pps, avc.nal_length_size, &src, keyframe)
            }
            None => Ok(src),
        }
    }

    fn next_video(&self) -> Option<SampleRef> {
        let v = self.video.as_ref()?;
        v.samples.get(self.vidx).copied()
    }

    fn next_audio(&self) -> Option<SampleRef> {
        let a = self.audio.as_ref()?;
        a.samples.get(self.aidx).copied()
    }
}

/// What a picker needs to show for one audio track, read straight from
/// the container rather than from a bound decoder — a track that is never
/// selected still has to be describable.
fn describe_audio(mp4: &re_mp4::Mp4, track: &re_mp4::Track, id: TrackId) -> AudioTrackInfo {
    let trak = track.trak(mp4);
    // ISO 639-2/T, with the unset marker spelled out rather than shown.
    let language = match trak.mdia.mdhd.language.as_str() {
        "und" | "" => None,
        other => Some(other.to_string()),
    };
    let (sample_rate, channels) = match &trak.mdia.minf.stbl.stsd.contents {
        re_mp4::StsdBoxContent::Mp4a(mp4a) => {
            let rate = mp4a
                .esds
                .as_ref()
                .and_then(|esds| {
                    AAC_RATES
                        .get(esds.es_desc.dec_config.dec_specific.freq_index as usize)
                        .copied()
                })
                .unwrap_or_else(|| u32::from(mp4a.samplerate.value()));
            (rate, u32::from(mp4a.channelcount))
        }
        _ => (0, 0),
    };
    AudioTrackInfo {
        id,
        language,
        label: None,
        codec: AudioCodec::Aac,
        sample_rate,
        channels,
    }
}

/// Cover art from the iTunes-style metadata atom. The box parser already
/// walks `udta`, so this reads what it found rather than opening a second
/// path through the same untrusted bytes.
fn artwork_from_moov(mp4: &re_mp4::Mp4) -> Option<crate::Artwork> {
    let udta = mp4.moov.udta.as_ref()?;
    let re_mp4::MetaBox::Mdir { ilst } = udta.meta.as_ref()? else {
        return None;
    };
    let item = ilst.as_ref()?.items.get(&re_mp4::MetadataKey::Poster)?;
    let data = &item.data.data;
    if data.is_empty() || data.len() > crate::artwork::MAX_ARTWORK_BYTES {
        return None;
    }
    // `covr` states only "image"; the format is in the bytes, so it is
    // sniffed rather than trusted.
    let mime = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else {
        return None;
    };
    Some(crate::Artwork {
        mime: mime.to_string(),
        data: data.clone(),
    })
}

impl Demuxer for Mp4Demuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }

        // Interleave in decode order; audio wins ties so it never trails a
        // burst of larger video AUs.
        let take_video = match (self.next_video(), self.next_audio()) {
            (Some(v), Some(a)) => v.dts < a.dts,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return Ok(StreamEvent::Eos(EosReason::Natural)),
        };

        if take_video {
            let sample = self.next_video().expect("checked above");
            let raw = self.read_sample(sample)?;
            let video = self.video.as_ref().expect("checked above");
            let data = if self.emit_raw_video {
                raw
            } else {
                Self::convert_sample(video, raw, sample.sync)?
            };
            let track = video.id;
            self.vidx += 1;
            Ok(StreamEvent::Au(Au {
                track,
                data,
                pts: sample.pts,
                dts: sample.dts,
                key: sample.sync,
                generation: self.generation,
            }))
        } else {
            let sample = self.next_audio().expect("checked above");
            let data = self.read_sample(sample)?;
            let track = self.audio.as_ref().expect("checked above").id;
            self.aidx += 1;
            Ok(StreamEvent::Au(Au {
                track,
                data,
                pts: sample.pts,
                dts: sample.dts,
                key: true,
                generation: self.generation,
            }))
        }
    }

    fn seek(&mut self, target: MediaTime, generation: Generation) -> Result<MediaTime, DemuxError> {
        self.generation = generation;

        let landed = if let Some(video) = &self.video {
            // Last sync sample at or before the target (by decode order —
            // sync samples present at their decode time).
            let mut key = 0usize;
            for (i, s) in video.samples.iter().enumerate() {
                if s.sync && s.dts <= target {
                    key = i;
                } else if s.dts > target {
                    break;
                }
            }
            self.vidx = key;
            video.samples.get(key).map(|s| s.pts).unwrap_or(target)
        } else {
            target
        };

        if let Some(audio) = &self.audio {
            self.aidx = audio.samples.partition_point(|s| s.pts < landed);
        }
        Ok(landed)
    }

    fn duration(&self) -> Option<MediaTime> {
        self.duration
    }

    fn video_track(&self) -> Option<TrackId> {
        Mp4Demuxer::video_track(self)
    }

    fn audio_track(&self) -> Option<TrackId> {
        Mp4Demuxer::audio_track(self)
    }

    fn audio_tracks(&self) -> Vec<AudioTrackInfo> {
        self.audio_tracks.clone()
    }

    fn artwork(&self) -> Option<&crate::Artwork> {
        self.artwork.as_ref()
    }

    fn take_notes(&mut self) -> Vec<String> {
        Mp4Demuxer::take_notes(self)
    }
}

fn scale_to_us(value: i64, timescale: u64) -> i64 {
    (value as i128 * 1_000_000 / timescale as i128) as i64
}
