#![forbid(unsafe_code)]

//! HLS (§6.6): playlist-driven segment chaining over the TS and fMP4
//! demuxers. `m3u8-rs` parses playlist bytes; everything schedulable is
//! ours — variant choice, the live window cursor, refresh cadence,
//! join point, discontinuity splices, seek-to-segment.
//!
//! TS segments feed one continuous `TsDemuxer` through a chaining source
//! (segments may legally continue PES/GOP state across boundaries), which
//! rebuilds only across stated discontinuities. fMP4 segments parse
//! per-segment as `init + segment` — `tfdt` keeps their timestamps
//! absolute, so no cross-segment correction exists to get wrong.

use std::collections::{HashMap, VecDeque};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use media_clock::{Generation, MediaTime};

use media_demux::{
    ByteSource, DemuxError, DemuxLimits, Demuxer, DiscontinuityReason, EosReason, Format,
    Mp4Demuxer, SourceError, StreamEvent, TrackId, TsDemuxer,
};

/// Whole-resource fetch plus a pacing seam. `media-io` implements the
/// network version; tests drive a virtual one. `wait` exists so the
/// refresh cadence stays schedulable without the demuxer owning a clock.
pub trait SegmentFetcher: Send {
    /// Fetch an entire resource, refusing past `cap` bytes.
    fn fetch(&mut self, url: &str, cap: u64) -> Result<Vec<u8>, SourceError>;

    /// Sleep (or advance virtual time) between live playlist refreshes.
    fn wait(&mut self, duration: Duration);
}

/// Parse-time caps (§6.6): enforced here, not around the demuxer.
const PLAYLIST_CAP: u64 = 4 * 1024 * 1024;
const SEGMENT_CAP: u64 = 64 * 1024 * 1024;
const MAX_SEGMENTS: usize = 65_536;
/// Stated durations beyond this are hostile numbers, not media: with the
/// segment-count cap this keeps every cumulative-duration fold far from
/// i64 microseconds (fuzz-found overflow).
const MAX_SEGMENT_SECONDS: f64 = 3600.0;
/// Attempts per resource before the failure propagates (live segments are
/// skipped instead — the window moves on without them).
const RESOURCE_ATTEMPTS: u32 = 3;
/// Live refreshes with no window progress before the lane reads as dead.
const STALE_REFRESHES: u32 = 40;
/// RFC 8216 §6.3.3: join no closer than three target durations from the
/// live edge, expressed in whole segments.
const LIVE_EDGE_SEGMENTS: usize = 3;

/// `#EXTM3U` leads the playlist (BOM/whitespace tolerated) — the router's
/// sniff for HLS lanes.
pub fn looks_like_playlist(head: &[u8]) -> bool {
    let head = head.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(head);
    let mut idx = 0;
    while idx < head.len() && (head[idx] == b' ' || head[idx] == b'\r' || head[idx] == b'\n') {
        idx += 1;
    }
    head[idx..].starts_with(b"#EXTM3U")
}

/// Resolve a possibly relative playlist URI against its playlist's URL
/// (or filesystem path — local fixtures play without a server).
fn resolve(base: &str, rel: &str) -> Result<String, DemuxError> {
    if rel.contains("://") {
        return Ok(rel.to_string());
    }
    match url::Url::parse(base) {
        // A single-letter "scheme" is a Windows drive letter, not a URL.
        Ok(base_url) if base_url.scheme().len() > 1 && !base_url.cannot_be_a_base() => base_url
            .join(rel)
            .map(|joined| joined.to_string())
            .map_err(|e| DemuxError::Parse(format!("bad segment URI {rel:?}: {e}"))),
        _ => {
            let path = std::path::Path::new(base);
            Ok(path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(""))
                .join(rel)
                .to_string_lossy()
                .into_owned())
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistSegment {
    pub url: String,
    pub duration: MediaTime,
    pub discontinuity: bool,
    pub map_url: Option<String>,
}

/// One parsed media-playlist window.
#[derive(Debug, Clone)]
pub struct PlaylistWindow {
    pub target_duration: MediaTime,
    pub first_sequence: u64,
    pub segments: Vec<PlaylistSegment>,
    pub ended: bool,
}

/// A parsed playlist of either kind — also the fuzz target's surface.
pub enum ParsedPlaylist {
    /// (bandwidth, resolved URI) per variant, best candidate first.
    Master(Vec<(u64, String)>),
    Media(Box<PlaylistWindow>),
}

/// Parse playlist bytes, refusing the features we do not carry (encrypted
/// media, byte-range segments, I-frame playlists) as typed errors.
pub fn parse_playlist(bytes: &[u8], base_url: &str) -> Result<ParsedPlaylist, DemuxError> {
    let playlist = m3u8_rs::parse_playlist_res(bytes)
        .map_err(|_| DemuxError::Parse("not a valid m3u8 playlist".into()))?;
    match playlist {
        m3u8_rs::Playlist::MasterPlaylist(master) => {
            let mut variants: Vec<(u64, String)> = Vec::new();
            for variant in &master.variants {
                if variant.is_i_frame {
                    continue;
                }
                variants.push((variant.bandwidth, resolve(base_url, &variant.uri)?));
            }
            if variants.is_empty() {
                return Err(DemuxError::Unsupported(
                    "master playlist with no usable variant",
                ));
            }
            variants.sort_by_key(|v| std::cmp::Reverse(v.0));
            Ok(ParsedPlaylist::Master(variants))
        }
        m3u8_rs::Playlist::MediaPlaylist(media) => {
            if media.i_frames_only {
                return Err(DemuxError::Unsupported("I-frame-only playlist"));
            }
            if media.segments.len() > MAX_SEGMENTS {
                return Err(DemuxError::Cap("playlist segment count"));
            }
            let mut segments = Vec::with_capacity(media.segments.len());
            // EXT-X-MAP applies to every segment after it (RFC 8216
            // §4.3.2.5); carry it forward.
            let mut current_map: Option<String> = None;
            for segment in &media.segments {
                if segment.key.is_some() {
                    return Err(DemuxError::Unsupported("encrypted HLS (EXT-X-KEY)"));
                }
                if segment.byte_range.is_some() {
                    return Err(DemuxError::Unsupported(
                        "byte-range segments (EXT-X-BYTERANGE)",
                    ));
                }
                match &segment.map {
                    Some(map) if map.byte_range.is_some() => {
                        return Err(DemuxError::Unsupported(
                            "byte-range init segments (EXT-X-MAP BYTERANGE)",
                        ));
                    }
                    Some(map) => current_map = Some(resolve(base_url, &map.uri)?),
                    None => {}
                }
                let duration = f64::from(segment.duration);
                if !duration.is_finite() || !(0.0..=MAX_SEGMENT_SECONDS).contains(&duration) {
                    return Err(DemuxError::Cap("segment duration"));
                }
                segments.push(PlaylistSegment {
                    url: resolve(base_url, &segment.uri)?,
                    duration: MediaTime::from_micros((duration * 1e6) as i64),
                    discontinuity: segment.discontinuity,
                    map_url: current_map.clone(),
                });
            }
            Ok(ParsedPlaylist::Media(Box::new(PlaylistWindow {
                target_duration: MediaTime::from_secs(
                    media.target_duration.clamp(1, MAX_SEGMENT_SECONDS as u64) as i64,
                ),
                first_sequence: media.media_sequence,
                segments,
                ended: media.end_list,
            })))
        }
    }
}

/// A fetched segment ready to demux.
struct FetchedSegment {
    data: Vec<u8>,
    /// A stated splice precedes this segment (EXT-X-DISCONTINUITY, a
    /// window fall-out jump, or a skipped-segment gap).
    discontinuity: DiscontinuityKind,
    map_url: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscontinuityKind {
    None,
    /// The playlist stated it.
    Stated,
    /// The scheduler jumped (fell out of the window, or skipped a dead
    /// segment).
    Jump,
}

/// The scheduler: playlist window cursor + refresh cadence + fetch policy.
struct Scheduler {
    fetcher: Box<dyn SegmentFetcher>,
    playlist_url: String,
    window: PlaylistWindow,
    live: bool,
    next_sequence: u64,
    pending_jump: bool,
    notes: Vec<String>,
}

impl Scheduler {
    fn fetch_with_retries(&mut self, url: &str, cap: u64) -> Result<Vec<u8>, DemuxError> {
        let mut last_error = None;
        for attempt in 0..RESOURCE_ATTEMPTS {
            if attempt > 0 {
                self.fetcher.wait(Duration::from_millis(250 << attempt));
            }
            match self.fetcher.fetch(url, cap) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last_error = Some(e),
            }
        }
        Err(DemuxError::Source(
            last_error.expect("at least one attempt ran"),
        ))
    }

    /// The segment for `sequence`, if the current window still carries it.
    fn segment_at(&self, sequence: u64) -> Option<&PlaylistSegment> {
        let index = usize::try_from(sequence.checked_sub(self.window.first_sequence)?).ok()?;
        self.window.segments.get(index)
    }

    /// Blocking: the next segment to demux, `None` at a VOD end. Live
    /// windows refresh (with waits) until the cursor's segment appears,
    /// the playlist ends, or the lane reads as dead.
    fn next_segment(&mut self) -> Result<Option<FetchedSegment>, DemuxError> {
        loop {
            // Fell out of the window: jump to the live join point and say so.
            if self.live && self.next_sequence < self.window.first_sequence {
                let jump_to = self.live_join_sequence();
                self.notes.push(format!(
                    "window advanced past sequence {}; jumping to {jump_to}",
                    self.next_sequence
                ));
                self.next_sequence = jump_to;
                self.pending_jump = true;
            }

            if let Some(segment) = self.segment_at(self.next_sequence) {
                let stated = segment.discontinuity;
                let url = segment.url.clone();
                let map_url = segment.map_url.clone();
                match self.fetch_with_retries(&url, SEGMENT_CAP) {
                    Ok(data) => {
                        self.next_sequence += 1;
                        let discontinuity = if stated {
                            DiscontinuityKind::Stated
                        } else if self.pending_jump {
                            DiscontinuityKind::Jump
                        } else {
                            DiscontinuityKind::None
                        };
                        self.pending_jump = false;
                        return Ok(Some(FetchedSegment {
                            data,
                            discontinuity,
                            map_url,
                        }));
                    }
                    // A dead live segment is a gap to skip, not a session
                    // failure; VOD has no window racing away, so it fails.
                    Err(e) if self.live => {
                        self.notes.push(format!(
                            "segment {} unfetchable ({e}); skipping",
                            self.next_sequence
                        ));
                        self.next_sequence += 1;
                        self.pending_jump = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else if self.window.ended {
                return Ok(None);
            } else if self.live {
                self.refresh_until_progress()?;
            } else {
                // A VOD playlist that neither carries the cursor nor has
                // ended is malformed.
                return Err(DemuxError::Parse(
                    "playlist window ended without EXT-X-ENDLIST".into(),
                ));
            }
        }
    }

    fn live_join_sequence(&self) -> u64 {
        let backoff = self.window.segments.len().min(LIVE_EDGE_SEGMENTS);
        self.window.first_sequence + (self.window.segments.len() - backoff) as u64
    }

    /// Refresh the playlist until the cursor's segment is visible or the
    /// playlist ends. RFC 8216 cadence: half a target duration between
    /// attempts; a window that stops advancing for `STALE_REFRESHES`
    /// attempts is a dead lane.
    fn refresh_until_progress(&mut self) -> Result<(), DemuxError> {
        let mut stale = 0u32;
        loop {
            let wait = Duration::from_micros((self.window.target_duration.as_micros() / 2) as u64)
                .max(Duration::from_millis(500));
            self.fetcher.wait(wait);

            let url = self.playlist_url.clone();
            let bytes = self.fetch_with_retries(&url, PLAYLIST_CAP)?;
            let window = match parse_playlist(&bytes, &url)? {
                ParsedPlaylist::Media(window) => *window,
                ParsedPlaylist::Master(_) => {
                    return Err(DemuxError::Parse(
                        "media playlist URL started returning a master playlist".into(),
                    ));
                }
            };

            let progressed = window.first_sequence + window.segments.len() as u64
                > self.window.first_sequence + self.window.segments.len() as u64
                || window.ended;
            self.window = window;
            if self.window.ended
                || self.segment_at(self.next_sequence).is_some()
                || self.next_sequence < self.window.first_sequence
            {
                return Ok(());
            }
            stale = if progressed { 0 } else { stale + 1 };
            if stale >= STALE_REFRESHES {
                return Err(DemuxError::Source("live playlist stopped advancing".into()));
            }
        }
    }
}

/// Byte window the chaining TS source serves from.
struct ChainState {
    buf: Vec<u8>,
    /// Absolute offset of `buf[0]` on the demuxer's timeline.
    base: u64,
    ended: bool,
    /// Segment stashed when its discontinuity flag fired. The source
    /// serves end-of-stream so the old demuxer flushes its trailing PES
    /// cleanly; the wrapper then rebuilds on this segment.
    pending: Option<FetchedSegment>,
}

/// Sequential TS bytes across segments; fetches inside `read_at` exactly
/// like a live transport source blocks on its socket.
struct TsChainSource {
    state: Arc<Mutex<ChainState>>,
    scheduler: Arc<Mutex<Scheduler>>,
}

impl ByteSource for TsChainSource {
    fn size(&mut self) -> Result<Option<u64>, SourceError> {
        Ok(None)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        loop {
            {
                let mut state = self.state.lock().expect("chain lock");
                if offset < state.base {
                    return Err("TS chain read below the retained window".into());
                }
                let rel = (offset - state.base) as usize;
                if rel < state.buf.len() {
                    let n = out.len().min(state.buf.len() - rel);
                    out[..n].copy_from_slice(&state.buf[rel..rel + n]);
                    // The demuxer reads strictly forward; keep a little
                    // slack and drop the rest.
                    let keep_from = rel.saturating_sub(64 * 1024);
                    if keep_from > 0 {
                        state.buf.drain(..keep_from);
                        state.base += keep_from as u64;
                    }
                    return Ok(n);
                }
                if state.ended || state.pending.is_some() {
                    return Ok(0);
                }
            }
            let next = self
                .scheduler
                .lock()
                .expect("scheduler lock")
                .next_segment()
                .map_err(|e| Box::new(e) as SourceError)?;
            let mut state = self.state.lock().expect("chain lock");
            match next {
                Some(segment) if segment.discontinuity != DiscontinuityKind::None => {
                    state.pending = Some(segment);
                }
                Some(segment) => state.buf.extend_from_slice(&segment.data),
                None => state.ended = true,
            }
        }
    }
}

/// Downstream-facing state shared by both modes: format dedup, track
/// identity, the timeline origin.
#[derive(Default)]
struct Adapter {
    announced: Vec<(TrackId, Format)>,
    video_track: Option<TrackId>,
    audio_track: Option<TrackId>,
    timeline_origin: Option<MediaTime>,
}

impl Adapter {
    /// Suppress duplicate Format re-announcements from later segments;
    /// learn track identity and the timeline origin.
    fn adapt(&mut self, event: StreamEvent) -> Option<StreamEvent> {
        match event {
            StreamEvent::Format(track, format) => {
                match &format {
                    Format::Video { .. } => self.video_track = Some(track),
                    Format::Audio { .. } => self.audio_track = Some(track),
                }
                if self
                    .announced
                    .iter()
                    .any(|(t, f)| *t == track && *f == format)
                {
                    return None;
                }
                self.announced.retain(|(t, _)| *t != track);
                self.announced.push((track, format.clone()));
                Some(StreamEvent::Format(track, format))
            }
            StreamEvent::Au(au) => {
                if self.timeline_origin.is_none() {
                    self.timeline_origin = Some(au.pts);
                }
                Some(StreamEvent::Au(au))
            }
            other => Some(other),
        }
    }

    fn splice_event(&self, kind: DiscontinuityKind) -> StreamEvent {
        let track = self.video_track.unwrap_or(TrackId(0));
        let reason = match kind {
            DiscontinuityKind::Stated => DiscontinuityReason::AdSplice,
            _ => DiscontinuityReason::Reconnect,
        };
        StreamEvent::Discontinuity(track, reason)
    }
}

/// Fetch (or reuse) the init segment and parse `init + segment` — `tfdt`
/// keeps the timestamps absolute across segments.
fn build_fmp4_inner(
    scheduler: &Arc<Mutex<Scheduler>>,
    limits: &DemuxLimits,
    generation: Generation,
    segment: &FetchedSegment,
    init_cache: &mut HashMap<String, Vec<u8>>,
) -> Result<Mp4Demuxer, DemuxError> {
    let mut bytes = Vec::new();
    if let Some(map_url) = &segment.map_url {
        if !init_cache.contains_key(map_url) {
            let init = scheduler
                .lock()
                .expect("scheduler lock")
                .fetch_with_retries(map_url, SEGMENT_CAP)?;
            init_cache.insert(map_url.clone(), init);
        }
        bytes.extend_from_slice(&init_cache[map_url]);
    }
    bytes.extend_from_slice(&segment.data);
    Mp4Demuxer::open(
        Box::new(media_demux::MemSource(bytes)),
        limits.clone(),
        generation,
    )
}

enum Mode {
    /// First pull decides TS vs fMP4 from the first segment.
    Undecided,
    Ts {
        demuxer: Box<TsDemuxer>,
        chain: Arc<Mutex<ChainState>>,
    },
    Fmp4 {
        inner: Option<Box<Mp4Demuxer>>,
        init_cache: HashMap<String, Vec<u8>>,
    },
}

pub struct HlsDemuxer {
    scheduler: Arc<Mutex<Scheduler>>,
    limits: DemuxLimits,
    generation: Generation,
    mode: Mode,
    adapter: Adapter,
    duration: Option<MediaTime>,
    pending: VecDeque<StreamEvent>,
    ended: bool,
    notes: Vec<String>,
}

impl HlsDemuxer {
    /// Open from already-fetched playlist bytes (the router sniffed them).
    /// A master playlist resolves to its highest-bandwidth variant.
    pub fn open(
        playlist_url: &str,
        playlist_bytes: Vec<u8>,
        mut fetcher: Box<dyn SegmentFetcher>,
        limits: DemuxLimits,
        generation: Generation,
    ) -> Result<Self, DemuxError> {
        let mut url = playlist_url.to_string();
        let mut notes = Vec::new();
        let window = match parse_playlist(&playlist_bytes, &url)? {
            ParsedPlaylist::Media(window) => *window,
            ParsedPlaylist::Master(variants) => {
                let (bandwidth, variant_url) = variants[0].clone();
                notes.push(format!(
                    "master playlist: picked {bandwidth} bps of {} variants",
                    variants.len()
                ));
                let bytes = fetcher
                    .fetch(&variant_url, PLAYLIST_CAP)
                    .map_err(DemuxError::Source)?;
                url = variant_url;
                match parse_playlist(&bytes, &url)? {
                    ParsedPlaylist::Media(window) => *window,
                    ParsedPlaylist::Master(_) => {
                        return Err(DemuxError::Unsupported(
                            "master playlist pointing at master playlists",
                        ));
                    }
                }
            }
        };

        let live = !window.ended;
        let duration = if window.ended {
            Some(
                window
                    .segments
                    .iter()
                    .fold(MediaTime::ZERO, |acc, s| acc + s.duration),
            )
        } else {
            None
        };
        let mut scheduler = Scheduler {
            fetcher,
            playlist_url: url,
            window,
            live,
            next_sequence: 0,
            pending_jump: false,
            notes,
        };
        scheduler.next_sequence = if live {
            scheduler.live_join_sequence()
        } else {
            scheduler.window.first_sequence
        };

        Ok(Self {
            scheduler: Arc::new(Mutex::new(scheduler)),
            limits,
            generation,
            mode: Mode::Undecided,
            adapter: Adapter::default(),
            duration,
            pending: VecDeque::new(),
            ended: false,
            notes: Vec::new(),
        })
    }

    /// Liveness as the playlist itself states it (`EXT-X-ENDLIST` ⇒ VOD).
    /// In-protocol statement, not inference — the engine aligns the Bank
    /// mode with it.
    pub fn is_live(&self) -> bool {
        self.scheduler.lock().expect("scheduler lock").live
    }

    /// Decide TS vs fMP4 from the first fetched segment and build the
    /// inner demuxer state around it.
    fn decide_mode(&mut self) -> Result<(), DemuxError> {
        let first = self
            .scheduler
            .lock()
            .expect("scheduler lock")
            .next_segment()?;
        let Some(segment) = first else {
            self.ended = true;
            return Ok(());
        };
        let is_ts = segment.map_url.is_none()
            && (segment.data.first() == Some(&0x47)
                || (segment.data.get(4) == Some(&0x47) && segment.data.len() >= 192));
        if is_ts {
            let chain = Arc::new(Mutex::new(ChainState {
                buf: segment.data,
                base: 0,
                ended: false,
                pending: None,
            }));
            let source = TsChainSource {
                state: Arc::clone(&chain),
                scheduler: Arc::clone(&self.scheduler),
            };
            let demuxer = TsDemuxer::open(Box::new(source), self.limits.clone(), self.generation)?;
            self.mode = Mode::Ts {
                demuxer: Box::new(demuxer),
                chain,
            };
        } else {
            let mut init_cache = HashMap::new();
            let inner = build_fmp4_inner(
                &self.scheduler,
                &self.limits,
                self.generation,
                &segment,
                &mut init_cache,
            )?;
            self.mode = Mode::Fmp4 {
                inner: Some(Box::new(inner)),
                init_cache,
            };
        }
        Ok(())
    }
}

impl Demuxer for HlsDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            if self.ended {
                return Ok(StreamEvent::Eos(EosReason::Natural));
            }
            if matches!(self.mode, Mode::Undecided) {
                self.decide_mode()?;
                continue;
            }
            match &mut self.mode {
                Mode::Undecided => unreachable!("decided above"),
                Mode::Ts { demuxer, chain } => match demuxer.next_event() {
                    Ok(StreamEvent::Eos(reason)) => {
                        // The chain serves EOS at a splice so the old
                        // demuxer flushes its trailing PES; a stashed
                        // segment means rebuild, not end.
                        let kind = {
                            let mut state = chain.lock().expect("chain lock");
                            state.pending.take().map(|segment| {
                                let kind = segment.discontinuity;
                                state.buf = segment.data;
                                state.base = 0;
                                state.ended = false;
                                kind
                            })
                        };
                        match kind {
                            Some(kind) => {
                                let source = TsChainSource {
                                    state: Arc::clone(chain),
                                    scheduler: Arc::clone(&self.scheduler),
                                };
                                **demuxer = TsDemuxer::open(
                                    Box::new(source),
                                    self.limits.clone(),
                                    self.generation,
                                )?;
                                self.pending.push_back(self.adapter.splice_event(kind));
                            }
                            None => {
                                self.ended = true;
                                return Ok(StreamEvent::Eos(reason));
                            }
                        }
                    }
                    Ok(event) => {
                        if let Some(out) = self.adapter.adapt(event) {
                            return Ok(out);
                        }
                    }
                    Err(e) => return Err(e),
                },
                Mode::Fmp4 { inner, init_cache } => {
                    if inner.is_none() {
                        let next = self
                            .scheduler
                            .lock()
                            .expect("scheduler lock")
                            .next_segment()?;
                        match next {
                            None => {
                                self.ended = true;
                                return Ok(StreamEvent::Eos(EosReason::Natural));
                            }
                            Some(segment) => {
                                if segment.discontinuity != DiscontinuityKind::None {
                                    self.pending.push_back(
                                        self.adapter.splice_event(segment.discontinuity),
                                    );
                                }
                                let built = build_fmp4_inner(
                                    &self.scheduler,
                                    &self.limits,
                                    self.generation,
                                    &segment,
                                    init_cache,
                                )?;
                                *inner = Some(Box::new(built));
                            }
                        }
                        continue;
                    }
                    match inner.as_mut().expect("ensured above").next_event() {
                        // Per-segment EOS just advances the chain.
                        Ok(StreamEvent::Eos(_)) => {
                            *inner = None;
                        }
                        Ok(event) => {
                            if let Some(out) = self.adapter.adapt(event) {
                                return Ok(out);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }

    fn seek(&mut self, target: MediaTime, generation: Generation) -> Result<MediaTime, DemuxError> {
        if self.is_live() {
            return Err(DemuxError::Unsupported("seek on a live HLS lane"));
        }
        let origin = self.adapter.timeline_origin.unwrap_or(MediaTime::ZERO);
        let rel = (target - origin).max(MediaTime::ZERO);
        let mut scheduler = self.scheduler.lock().expect("scheduler lock");
        let mut cumulative = MediaTime::ZERO;
        let mut index = 0usize;
        let mut landed = MediaTime::ZERO;
        for (i, segment) in scheduler.window.segments.iter().enumerate() {
            index = i;
            landed = cumulative;
            if cumulative + segment.duration > rel {
                break;
            }
            cumulative += segment.duration;
        }
        scheduler.next_sequence = scheduler.window.first_sequence + index as u64;
        scheduler.pending_jump = false;
        drop(scheduler);

        self.generation = generation;
        self.pending.clear();
        self.ended = false;
        // The inner demuxer rebuilds on the target segment (well-formed
        // VOD segments open on keyframes).
        self.mode = Mode::Undecided;
        Ok(origin + landed)
    }

    fn duration(&self) -> Option<MediaTime> {
        self.duration
    }

    fn video_track(&self) -> Option<TrackId> {
        self.adapter.video_track
    }

    fn audio_track(&self) -> Option<TrackId> {
        self.adapter.audio_track
    }

    fn take_notes(&mut self) -> Vec<String> {
        let mut notes = std::mem::take(&mut self.notes);
        notes.extend(std::mem::take(
            &mut self.scheduler.lock().expect("scheduler lock").notes,
        ));
        notes
    }
}
