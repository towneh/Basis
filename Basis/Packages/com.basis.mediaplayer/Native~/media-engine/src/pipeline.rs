//! Pipeline assembly (§6.3): demux, release and decode threads around the
//! M1 foundations — the Bank between demux and decode, media-clock as the
//! one position source, condvars instead of sleep-polls, generations
//! across every seek.
//!
//! Thread map per session (M2, Windows):
//!   demux    pulls the Demuxer, pushes into the Bank, executes seeks
//!   release  drains the Bank on the 1x schedule, routes AUs to decoders
//!   video    MF H.264 decode → FramePool → paced upload to the shared
//!            texture (the §6.8 conversion pass rides here until the GPU
//!            pass lands)
//!   audio    MF AAC decode → priming drop → PcmRing
//! The Unity render thread only ever runs the keyed-mutex copy; the Unity
//! audio thread only ever runs the lock-free ring pull.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use media_bank::{Bank, PushOutcome};
use media_clock::{Correction, Generation, Master, MediaClock, MediaTime};
use media_decode::{AudioDecoder, SubmitOutcome, VideoDecoder, VideoFrame};
use media_demux::{Au, Demuxer, Format, StreamEvent};
use media_diag::{EventCode, SessionDiag, Stage};

use crate::audio::{
    AudioConsumer, AudioFormatInfo, AudioProducer, audio_pair, frames_before_origin,
};
use crate::pool::FramePool;
use crate::present::PresentShared;
use crate::route::{open_audio_decoder, open_video_decoder};
use crate::sink::VideoSink;
use crate::{EngineError, SessionShared, State};

/// How long a fully banked pipeline waits before rechecking for commands.
const IDLE_WAIT: Duration = Duration::from_millis(50);
/// Decode-thread receive granularity: bounds presentation-timing error.
const DECODE_TICK: Duration = Duration::from_millis(4);
/// A consumer pull within this window keeps audio as the clock master.
const AUDIO_LIVENESS: MediaTime = MediaTime::from_millis(500);

/// The engine wall clock: monotonic µs that freeze while paused, so the
/// Bank's release schedule and the media clock pause together without
/// either component knowing about pause.
pub struct EngineWall {
    origin: Instant,
    state: Mutex<WallState>,
}

struct WallState {
    paused_accum: Duration,
    paused_since: Option<Instant>,
}

impl EngineWall {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            state: Mutex::new(WallState {
                paused_accum: Duration::ZERO,
                paused_since: None,
            }),
        }
    }

    pub fn now(&self) -> MediaTime {
        let state = self.state.lock().expect("wall lock");
        let paused = state.paused_accum
            + state
                .paused_since
                .map(|since| since.elapsed())
                .unwrap_or(Duration::ZERO);
        MediaTime::from_micros((self.origin.elapsed() - paused).as_micros() as i64)
    }

    pub fn pause(&self) {
        let mut state = self.state.lock().expect("wall lock");
        if state.paused_since.is_none() {
            state.paused_since = Some(Instant::now());
        }
    }

    pub fn resume(&self) {
        let mut state = self.state.lock().expect("wall lock");
        if let Some(since) = state.paused_since.take() {
            state.paused_accum += since.elapsed();
        }
    }
}

/// Bank + its condvar: pushed by demux, drained by release, both signal.
pub struct BankShared {
    pub bank: Mutex<Bank>,
    pub changed: Condvar,
}

pub enum Command {
    Seek(MediaTime),
}

/// Everything the pipeline threads share.
pub struct PipelineShared {
    pub shared: Arc<SessionShared>,
    pub diag: Arc<SessionDiag>,
    pub wall: Arc<EngineWall>,
    pub clock: Arc<Mutex<MediaClock>>,
    pub bank: Arc<BankShared>,
    pub pool: Arc<FramePool>,
    pub commands: Mutex<Vec<Command>>,
    pub audio_consumer: Mutex<Option<AudioConsumer>>,
    pub audio_shared: Arc<crate::audio::AudioShared>,
    /// Cancels in-flight connects and reads on teardown.
    pub io_cancel: media_io::CancelToken,
    /// Track presence, set by the release thread as Formats route: which
    /// decode thread owns declaring Ended.
    pub video_active: std::sync::atomic::AtomicBool,
    pub audio_active: std::sync::atomic::AtomicBool,
    /// Lock-free mirror of the clock's playing-ness for the audio pull
    /// path: the ring serves silence while the clock is parked (startup,
    /// seeks), so a seek settle can never play out the post-seek tail
    /// against a parked clock. Written under the clock lock at every
    /// `set_playing` site; the `State` gate alone is not enough — a
    /// present in flight can race a seek back to Playing.
    pub clock_playing: std::sync::atomic::AtomicBool,
    /// Audio-leading start requested by the descriptor (honoured on live
    /// sessions only; the audio thread combines it with the Bank's
    /// liveness).
    pub audio_leading: bool,
    /// Decode-route preference from the descriptor (§6.7): consumed by
    /// the video thread's route resolution.
    pub decode_preference: crate::DecodePreference,
    /// The pts the parked clock started presentation at this generation
    /// (µs; `i64::MIN` = not started). Written by the video thread at
    /// clock start — the authoritative join point for the audio thread's
    /// pre-join shed, which cannot reliably sample the pool once frames
    /// are being consumed.
    pub presentation_origin_us: std::sync::atomic::AtomicI64,
    /// Caption cues scanned from the video AUs' SEI on the demux thread,
    /// surfaced on arrival with their due PTS (§6.2/§6.12 — captions
    /// bypass the Bank's release schedule so the consumer gets the full
    /// pre-roll). Drop-oldest at [`CAPTION_RING`].
    pub captions: Mutex<std::collections::VecDeque<media_bitstream::CaptionCue>>,
    /// Render-event selection state (§6.8): the render thread's
    /// clock mirror, its vsync estimate, and the consumer-liveness stamp
    /// that hands frame selection between it and the video thread.
    pub present: PresentShared,
    /// Shared-playback soft sync target (§8.4): the last reported owner
    /// position, extrapolated at 1x between reports.
    pub(crate) sync: crate::sync::SyncShared,
    /// The sync ladder's wanted rate offset from 1x, ppm. On audio-master
    /// lanes the managed audio pull applies it through its resampler (the
    /// snapshot surfaces it); on wall-master lanes the engine has already
    /// applied it to the clock and this mirrors what it did.
    pub sync_rate_ppm: std::sync::atomic::AtomicI64,
    /// Bank liveness, mirrored lock-free once the opener installs the
    /// session's real Bank (a playlist can override the request's stated
    /// liveness). Live lanes ignore sync targets (§8.5).
    pub live: std::sync::atomic::AtomicBool,
    /// Split-source coordination (`OpenRequest::audio_url`). Absent on the
    /// ordinary one-source session, and every split-only branch is behind
    /// this being `Some`, so single-source behaviour is untouched.
    pub split: std::sync::OnceLock<SplitLegs>,
    /// The Windows shared-texture presenter, shared between the render
    /// event (selection + conversion at display cadence) and the video
    /// thread (configure on Format; tick-paced fallback presents while no
    /// render consumer is live). Neither holder does GPU-external work
    /// under other media-path locks, and the render event only ever
    /// try-locks it.
    #[cfg(windows)]
    pub presenter: Mutex<Option<media_present::SharedTexturePresenter>>,
}

/// Cue ring depth (the C player's CUE_RING).
const CAPTION_RING: usize = 64;

/// Which source a demux thread is reading, in a session that has more than
/// one. Adaptive ladders serve high rungs as a video-only and an audio-only
/// stream that have to be played together; both are cuts of the same
/// content, so a single Bank meters them against one timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// The only source. Carries whatever tracks it carries.
    Single,
    /// The video leg of a split pair. Owns seek: it advances the
    /// generation, snaps the clock and flushes the decode threads, exactly
    /// as the single-source path does.
    Video,
    /// The audio leg of a split pair. Follows the video leg's seeks rather
    /// than taking commands of its own.
    Audio,
}

impl Leg {
    /// Whether this leg's tracks of the other kind are dropped before the
    /// Bank sees them. A video leg selected from an adaptive ladder is
    /// video-only in practice, but a caller can hand us anything.
    fn wants(self, format: &Format) -> bool {
        match self {
            Self::Single => true,
            Self::Video => matches!(format, Format::Video { .. }),
            Self::Audio => matches!(format, Format::Audio { .. }),
        }
    }
}

/// Keeps the audio leg's track ids from colliding with the video leg's:
/// each demuxer numbers its own tracks from zero, and the Bank and the
/// release thread route on nothing but the id.
const AUDIO_LEG_TRACK_BIT: u32 = 0x8000_0000;

/// What the two demux threads of a split session share.
pub struct SplitLegs {
    /// The seek the video leg last performed: its generation and where it
    /// landed. The audio leg watches this and follows.
    pub seek: Mutex<Option<(Generation, MediaTime)>>,
    /// Whether each leg has reached the end of its own source, video
    /// first. The session's Eos reaches the Bank only once both have, so a
    /// leg that runs a little short cannot end the other one early.
    pub eos: [std::sync::atomic::AtomicBool; 2],
    /// Set by whichever leg carried the Eos through, so two legs finishing
    /// together still bank exactly one.
    eos_carried: std::sync::atomic::AtomicBool,
    /// The dts each leg has most recently banked, video first. Two
    /// producers share one bounded Bank, so without this the leg that
    /// reads faster — a small audio file against a large video one — fills
    /// it on its own and blocks the other leg out. That is a deadlock, not
    /// just waste: the clock will not start until the video leg lands a
    /// frame, release will not drain until the clock starts, and the Bank
    /// will not take the video leg's frame until release drains.
    banked_dts_us: [std::sync::atomic::AtomicI64; 2],
}

/// How far ahead of the other leg a leg may bank before it waits.
///
/// Deliberately tight. The Bank releases its queue in arrival order on a
/// dts-derived schedule, so an event that arrives early but is due late
/// sits at the head and holds up everything behind it — including the
/// other leg's frames, which are due now. Keeping the legs within a
/// fraction of a second of each other keeps arrival order close to
/// timeline order, and bounds that head-of-line wait to well inside the
/// decoder's cushion. It also has to stay comfortably under the Bank's own
/// read-ahead depth, or one leg fills the Bank before the cap ever bites
/// and the other leg cannot get in at all.
const SPLIT_LEAD_CAP_US: i64 = 100_000;

impl SplitLegs {
    pub fn new() -> Self {
        Self {
            seek: Mutex::new(None),
            eos: [
                std::sync::atomic::AtomicBool::new(false),
                std::sync::atomic::AtomicBool::new(false),
            ],
            eos_carried: std::sync::atomic::AtomicBool::new(false),
            banked_dts_us: [
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
            ],
        }
    }

    /// Whether this leg has to wait for the other one to catch up before
    /// banking `dts`. A leg that has reached the end of its source never
    /// holds the other back, since it has nothing left to catch up with.
    fn must_wait_for_other(&self, leg: Leg, dts_us: i64) -> bool {
        let other = 1 - Self::index(leg);
        if self.eos[other].load(Ordering::Relaxed) {
            return false;
        }
        dts_us - self.banked_dts_us[other].load(Ordering::Relaxed) > SPLIT_LEAD_CAP_US
    }

    fn note_banked(&self, leg: Leg, dts_us: i64) {
        self.banked_dts_us[Self::index(leg)].store(dts_us, Ordering::Relaxed);
    }

    /// Both legs restart from the same place after a seek.
    fn rebase(&self, position_us: i64) {
        for slot in &self.banked_dts_us {
            slot.store(position_us, Ordering::Relaxed);
        }
    }

    fn index(leg: Leg) -> usize {
        match leg {
            Leg::Audio => 1,
            _ => 0,
        }
    }

    /// Records this leg as having reached the end of its own source.
    fn mark_eos(&self, leg: Leg) {
        self.eos[Self::index(leg)].store(true, Ordering::SeqCst);
    }

    fn both_reached_eos(&self) -> bool {
        self.eos.iter().all(|f| f.load(Ordering::SeqCst))
    }

    /// Claims the job of banking the session's Eos, for exactly one leg.
    fn claim_carrier(&self) -> bool {
        !self.eos_carried.swap(true, Ordering::SeqCst)
    }

    fn clear_eos(&self) {
        self.eos_carried.store(false, Ordering::SeqCst);
        for flag in &self.eos {
            flag.store(false, Ordering::SeqCst);
        }
    }
}

/// Namespaces the audio leg's track ids and drops the tracks a leg does not
/// carry, so two demuxers can feed one Bank. Both number their own tracks
/// from zero and the Bank and release thread route on nothing else, so
/// without this the legs' tracks would land on top of each other. Returns
/// `None` for an event this leg should not contribute.
fn adapt_leg_event(
    leg: Leg,
    foreign: &mut std::collections::HashSet<media_demux::TrackId>,
    event: StreamEvent,
) -> Option<StreamEvent> {
    if leg == Leg::Single {
        return Some(event);
    }
    let remap = |track: media_demux::TrackId| {
        if leg == Leg::Audio {
            media_demux::TrackId(track.0 | AUDIO_LEG_TRACK_BIT)
        } else {
            track
        }
    };
    match event {
        StreamEvent::Format(track, format) => {
            if !leg.wants(&format) {
                foreign.insert(track);
                return None;
            }
            Some(StreamEvent::Format(remap(track), format))
        }
        StreamEvent::Au(mut au) => {
            if foreign.contains(&au.track) {
                return None;
            }
            au.track = remap(au.track);
            Some(StreamEvent::Au(au))
        }
        StreamEvent::Discontinuity(track, reason) => {
            if foreign.contains(&track) {
                return None;
            }
            Some(StreamEvent::Discontinuity(remap(track), reason))
        }
        // Captions ride the video bitstream, so an audio leg has none to
        // contribute. Metadata and Eos carry no track.
        StreamEvent::Caption(_) if leg == Leg::Audio => None,
        other => Some(other),
    }
}

impl Default for SplitLegs {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineShared {
    pub fn set_state(&self, state: State) {
        let previous = self.shared.state.swap(state as u32, Ordering::Relaxed);
        if previous != state as u32 {
            self.diag.event(
                self.wall.now(),
                EventCode::StateChange,
                Stage::Clock,
                format!("{state:?}"),
            );
        }
    }

    pub fn state(&self) -> u32 {
        self.shared.state.load(Ordering::Relaxed)
    }

    pub fn fail(&self, error: EngineError) {
        self.shared.last_error.store(error.code, Ordering::Relaxed);
        self.shared
            .last_error_category
            .store(error.category as u32, Ordering::Relaxed);
        self.diag.event(
            self.wall.now(),
            EventCode::Error,
            error.stage,
            error.detail.clone(),
        );
        eprintln!("[basis-media] session error: {}", error.detail);
        self.set_state(State::Error);
        self.shared.stop.store(true, Ordering::Relaxed);
        self.bank.changed.notify_all();
    }

    pub fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
    }

    fn push_caption(&self, cue: media_bitstream::CaptionCue) {
        let mut ring = self.captions.lock().expect("captions lock");
        if ring.len() >= CAPTION_RING {
            ring.pop_front();
        }
        ring.push_back(cue);
    }
}

pub enum MediaMsg {
    Format(Format),
    Au(Au),
    /// Seek/flush marker: drop decoder state, adopt the generation.
    Flush {
        generation: Generation,
    },
    Eos,
}

/// Rebuilds the source + demuxer for the resilience path. `None` when the
/// lane cannot reconnect (VOD, injected sources): failures fail the
/// session as before.
pub type DemuxFactory = Box<dyn FnMut() -> Result<Box<dyn Demuxer>, EngineError> + Send>;

/// Reconnect posture (§6.10): engine-owned, instrumented, logs by default.
const RECONNECT_ATTEMPTS: u32 = 6;
const RECONNECT_BASE: Duration = Duration::from_millis(500);
const RECONNECT_CAP: Duration = Duration::from_secs(8);

/// Whether a demux-thread failure is the transport dying underneath us
/// (worth a reconnect) rather than a parse refusal (a property of the
/// stream, retried forever it would loop).
fn is_transport_loss(error: &media_demux::DemuxError) -> bool {
    match error {
        media_demux::DemuxError::Source(source) => {
            source.downcast_ref::<media_io::IoError>().is_none_or(|io| {
                matches!(
                    io.kind,
                    media_io::IoErrorKind::Read
                        | media_io::IoErrorKind::Connect
                        | media_io::IoErrorKind::Http
                        | media_io::IoErrorKind::Resolve
                )
            })
        }
        media_demux::DemuxError::Io(_) => true,
        _ => false,
    }
}

/// Demux thread: pull events, push into the Bank with backpressure, run
/// seeks, and — on live lanes — rebuild the transport when it dies. The
/// generation does not advance across a reconnect (it is not a seek: the
/// banked depth keeps playing through the outage) and the clock's snap
/// absorbs the timeline jump when post-reconnect frames arrive.
///
/// One of these runs per source. `Leg::Single` is the ordinary session and
/// takes every branch it always did; a split pair runs two, and everything
/// specific to that is behind [`PipelineShared::split`] being set.
pub fn run_demux_leg(
    px: &Arc<PipelineShared>,
    mut demuxer: Box<dyn Demuxer>,
    video_tx: &SyncSender<MediaMsg>,
    audio_tx: &SyncSender<MediaMsg>,
    mut factory: Option<DemuxFactory>,
    leg: Leg,
) {
    let mut eos_reached = false;
    let mut pending: Option<StreamEvent> = None;
    // Tracks of the kind this leg does not carry, learned from its own
    // Format events, so their AUs can be dropped before the Bank sees
    // them. Empty on a single-source session, which keeps everything.
    let mut foreign_tracks: std::collections::HashSet<media_demux::TrackId> =
        std::collections::HashSet::new();
    // The seek the audio leg has already followed.
    let mut followed_seek: Option<Generation> = None;
    // Whether this leg has been picked to carry the pair's Eos. Held
    // across a Bank-full retry of that Eos: the pick is made once, and
    // asking again would hand the Eos to nobody and hang the session.
    let mut carries_eos = false;
    // The Eos this leg pulled but has not banked yet.
    let mut held_eos: Option<StreamEvent> = None;
    // In-band CEA-608: every H.264 AU is scanned for caption SEI here, in
    // decode order on arrival (the 608 pair stream is stateful and rides
    // decode order; display selection against the due PTS happens at the
    // consumer). One scanner per session; seeks reset it.
    let mut caption_scanner = media_bitstream::CaptionScanner::new();
    let mut caption_track: Option<media_demux::TrackId> = None;
    loop {
        if px.stopping() {
            return;
        }

        // The audio leg of a split pair takes no commands of its own: it
        // goes where the video leg's seek landed, so both sides of the
        // pair resume from the same point on the same generation.
        if leg == Leg::Audio
            && let Some(split) = px.split.get()
        {
            let wanted = *split.seek.lock().expect("split seek lock");
            if let Some((generation, landed)) = wanted
                && followed_seek != Some(generation)
            {
                followed_seek = Some(generation);

                match demuxer.seek(landed, generation) {
                    Ok(_) => {}
                    // An unseekable audio leg simply plays on; the video
                    // leg has already reported the refusal.
                    Err(media_demux::DemuxError::Unsupported(_)) => {}
                    Err(e) => {
                        px.fail(EngineError::demux(e));
                        return;
                    }
                }
                pending = None;
                eos_reached = false;
                carries_eos = false;
                held_eos = None;
                continue;
            }
        }

        // Seeks execute here: the demuxer and the Bank are both ours.
        let command = if leg == Leg::Audio {
            None
        } else {
            px.commands.lock().expect("commands lock").pop()
        };
        if let Some(Command::Seek(target)) = command {
            let generation = px.bank.bank.lock().expect("bank lock").generation().next();
            match demuxer.seek(target, generation) {
                Ok(landed) => {
                    px.bank
                        .bank
                        .lock()
                        .expect("bank lock")
                        .advance_generation(generation);
                    px.shared.generation.store(generation.0, Ordering::Relaxed);
                    {
                        // Snap to the landed position and park the clock;
                        // the video thread restarts it when the first
                        // post-seek frame is ready, so decode latency never
                        // reads as lateness.
                        let wall = px.wall.now();
                        let mut clock = px.clock.lock().expect("clock lock");
                        clock.advance_generation(wall, generation, landed);
                        clock.set_playing(wall, false);
                        px.clock_playing.store(false, Ordering::Relaxed);
                        px.present.mirror_clock(wall, MediaTime::ZERO, false);
                    }
                    // Re-assert Buffering: a present that raced the seek
                    // command may have flipped the state back to Playing
                    // between the session call and the clock parking here.
                    px.set_state(State::Buffering);
                    let _ = video_tx.send(MediaMsg::Flush { generation });
                    let _ = audio_tx.send(MediaMsg::Flush { generation });
                    px.diag.event(
                        px.wall.now(),
                        EventCode::Seek,
                        Stage::Demux,
                        format!("to {target}, landed {landed}"),
                    );
                    px.shared
                        .position_us
                        .store(landed.as_micros(), Ordering::Relaxed);
                    pending = None;
                    eos_reached = false;
                    carries_eos = false;
                    held_eos = None;
                    // Captions from the old position must not survive the
                    // jump: reset the decoder, drop queued cues, and clear
                    // the display at the landed position.
                    caption_scanner.reset();
                    px.captions.lock().expect("captions lock").clear();
                    px.push_caption(media_bitstream::CaptionCue {
                        pts_us: landed.as_micros(),
                        text: String::new(),
                    });
                    // Hand the landing to the audio leg, and let both legs
                    // reach the end again on the new timeline.
                    if let Some(split) = px.split.get() {
                        split.clear_eos();
                        split.rebase(landed.as_micros());
                        *split.seek.lock().expect("split seek lock") = Some((generation, landed));
                    }
                    px.bank.changed.notify_all();
                }
                // An unseekable (live) source refuses the seek and plays
                // on; that is a property of the lane, not a failure.
                Err(media_demux::DemuxError::Unsupported(what)) => {
                    px.diag.event(
                        px.wall.now(),
                        EventCode::Seek,
                        Stage::Demux,
                        format!("seek refused: {what}"),
                    );
                }
                Err(e) => px.fail(EngineError::demux(e)),
            }
            continue;
        }

        if eos_reached && pending.is_none() {
            // A held Eos is re-offered every tick rather than decided once
            // when it arrived. The two legs finish independently and a
            // seek can reset the pair mid-handshake, so a single edge is
            // exactly the thing that goes missing; re-checking here means
            // the session always ends, at worst one tick late.
            if let Some(split) = px.split.get()
                && !carries_eos
                && held_eos.is_some()
                && split.both_reached_eos()
                && split.claim_carrier()
            {
                carries_eos = true;
                eos_reached = false;
                pending = held_eos.take();
                continue;
            }
            // Nothing to pull; idle until a seek or stop.
            std::thread::park_timeout(IDLE_WAIT);
            continue;
        }

        let event = match pending.take() {
            Some(event) => event,
            None => match demuxer.next_event() {
                Ok(event) => {
                    px.diag
                        .stage(Stage::Demux)
                        .out_count
                        .fetch_add(1, Ordering::Relaxed);
                    px.diag
                        .stage(Stage::Demux)
                        .out_bytes
                        .fetch_add(event.payload_bytes() as u64, Ordering::Relaxed);
                    // Caption scan on first pull only — a Bank-full retry
                    // must not re-feed the stateful 608 decoder.
                    match &event {
                        StreamEvent::Format(track, Format::Video { codec, .. }) => {
                            caption_track =
                                (*codec == media_demux::VideoCodec::H264).then_some(*track);
                        }
                        StreamEvent::Au(au) if Some(au.track) == caption_track => {
                            if let Some(cue) =
                                caption_scanner.scan_au(&au.data, false, au.pts.as_micros())
                            {
                                px.push_caption(cue);
                            }
                        }
                        _ => {}
                    }
                    match adapt_leg_event(leg, &mut foreign_tracks, event) {
                        Some(event) => event,
                        // A track this leg does not contribute.
                        None => continue,
                    }
                }
                Err(e) => {
                    if let Some(rebuild) = factory.as_mut()
                        && is_transport_loss(&e)
                    {
                        match reconnect(px, rebuild, &e) {
                            Some(rebuilt) => {
                                demuxer = rebuilt;
                                continue;
                            }
                            None => {
                                if !px.stopping() {
                                    px.fail(EngineError::demux(e));
                                }
                                return;
                            }
                        }
                    }
                    px.fail(EngineError::demux(e));
                    return;
                }
            },
        };
        // A live transport delivering EOF is loss until proven otherwise —
        // a dropped TCP connection with a connection-close body is
        // indistinguishable from a finished stream, so try to rejoin; only
        // exhausted attempts let the Eos through (Ended, not Error: the
        // broadcaster may genuinely have stopped).
        if matches!(event, StreamEvent::Eos(_))
            && let Some(rebuild) = factory.as_mut()
        {
            let cause = media_demux::DemuxError::Unsupported("live source delivered EOF");
            match reconnect(px, rebuild, &cause) {
                Some(rebuilt) => {
                    demuxer = rebuilt;
                    continue;
                }
                None => {
                    if px.stopping() {
                        return;
                    }
                    // Fall through: the Eos flows to the Bank as usual.
                }
            }
        }
        let is_eos = matches!(event, StreamEvent::Eos(_));

        // On a split pair the session has ended only once both sources
        // have. The legs are cuts of the same content but rarely the exact
        // same length, so the shorter one must not end the other. The pick
        // is remembered because a full Bank sends this same Eos round
        // again, and asking twice would give it to neither leg.
        if is_eos
            && let Some(split) = px.split.get()
            && !carries_eos
        {
            split.mark_eos(leg);
            if split.both_reached_eos() && split.claim_carrier() {
                carries_eos = true;
            } else {
                // Hold it: the other leg is still going, or it got there
                // first. The idle branch re-checks, so whichever leg ends
                // up holding an unbanked Eos will carry it.
                eos_reached = true;
                held_eos = Some(event);
                continue;
            }
        }

        // Keep the pair in step. One Bank serves both legs, so a leg that
        // reads far ahead of the other takes the room the other one needs.
        let banked_dts_us = match (&event, px.split.get()) {
            (StreamEvent::Au(au), Some(split)) => {
                let dts_us = au.dts.as_micros();
                if split.must_wait_for_other(leg, dts_us) {
                    pending = Some(event);
                    let bank = px.bank.bank.lock().expect("bank lock");
                    let _ = px.bank.changed.wait_timeout(bank, IDLE_WAIT);
                    continue;
                }
                Some(dts_us)
            }
            _ => None,
        };

        let wall = px.wall.now();
        let mut bank = px.bank.bank.lock().expect("bank lock");
        match bank.push(wall, event) {
            PushOutcome::Accepted => {
                px.diag
                    .stage(Stage::Bank)
                    .in_count
                    .fetch_add(1, Ordering::Relaxed);
                if let (Some(dts_us), Some(split)) = (banked_dts_us, px.split.get()) {
                    split.note_banked(leg, dts_us);
                }
                px.bank.changed.notify_all();
                if is_eos {
                    eos_reached = true;
                }
            }
            PushOutcome::StaleGeneration => {
                px.diag
                    .stage(Stage::Bank)
                    .drops
                    .fetch_add(1, Ordering::Relaxed);
            }
            PushOutcome::Full(event) => {
                pending = Some(event);
                // Backpressure: wait for release to drain (or a command).
                let _ = px.bank.changed.wait_timeout(bank, IDLE_WAIT);
            }
        }
    }
}

/// The reconnect loop: bounded attempts with jittered exponential backoff,
/// every attempt a diagnostics event at default verbosity (L7). Returns
/// `None` when attempts are exhausted or the session is stopping.
fn reconnect(
    px: &Arc<PipelineShared>,
    factory: &mut DemuxFactory,
    cause: &media_demux::DemuxError,
) -> Option<Box<dyn Demuxer>> {
    for attempt in 1..=RECONNECT_ATTEMPTS {
        let backoff = RECONNECT_BASE
            .saturating_mul(1 << (attempt - 1).min(4))
            .min(RECONNECT_CAP);
        // ±25% jitter so a synchronised room does not thundering-herd the
        // origin; entropy from the wall clock is plenty here.
        let jitter_ppm = (px.wall.now().as_micros() % 500_000) - 250_000;
        let backoff = Duration::from_micros(
            (backoff.as_micros() as i64 * (1_000_000 + jitter_ppm) / 1_000_000) as u64,
        );
        px.diag.event(
            px.wall.now(),
            EventCode::Reconnect,
            Stage::Source,
            format!("attempt {attempt}/{RECONNECT_ATTEMPTS} in {backoff:?} after: {cause}"),
        );
        eprintln!(
            "[basis-media] transport lost ({cause}); reconnect attempt {attempt}/{RECONNECT_ATTEMPTS} in {backoff:?}"
        );

        let deadline = Instant::now() + backoff;
        while Instant::now() < deadline {
            if px.stopping() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        match factory() {
            Ok(demuxer) => {
                px.diag.event(
                    px.wall.now(),
                    EventCode::Reconnect,
                    Stage::Source,
                    format!("reconnected on attempt {attempt}"),
                );
                eprintln!("[basis-media] reconnected on attempt {attempt}");
                return Some(demuxer);
            }
            Err(e) => {
                px.diag.event(
                    px.wall.now(),
                    EventCode::Reconnect,
                    Stage::Source,
                    format!("attempt {attempt} failed: {}", e.detail),
                );
            }
        }
    }
    None
}

/// How often a parked message retries its channel: no notification exists
/// for a `SyncSender` slot freeing, so a blocked target is polled.
const PARKED_POLL: Duration = Duration::from_millis(4);

/// One decode channel's parked tail: messages the channel had no room for,
/// delivered in order before anything newer is popped for this target.
/// While non-empty the target's whole track is gated in the Bank, so
/// per-track order is exact; the other track keeps routing (§6.3 — the
/// per-track-aware release).
#[derive(Default)]
struct ParkedTarget {
    msgs: std::collections::VecDeque<MediaMsg>,
}

impl ParkedTarget {
    /// Deliver as much of the tail as the channel accepts.
    fn flush(&mut self, tx: &SyncSender<MediaMsg>) {
        while let Some(msg) = self.msgs.pop_front() {
            match tx.try_send(msg) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(msg)) => {
                    self.msgs.push_front(msg);
                    return;
                }
                // Teardown: the receiver is gone, nothing left to order.
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
    }

    fn send(&mut self, tx: &SyncSender<MediaMsg>, msg: MediaMsg) {
        if !self.msgs.is_empty() {
            self.msgs.push_back(msg);
            return;
        }
        match tx.try_send(msg) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            Err(std::sync::mpsc::TrySendError::Full(msg)) => self.msgs.push_back(msg),
        }
    }

    /// A seek happened while messages were parked: stale AUs and the
    /// stale Eos must not cross into the new generation (Eos carries no
    /// generation, so a late delivery would end the fresh timeline).
    /// Formats are timeline-free decoder config and are NOT re-announced
    /// after a seek — they survive the flush.
    fn drop_stale(&mut self) {
        self.msgs.retain(|msg| matches!(msg, MediaMsg::Format(_)));
    }
}

/// Release thread: drain the Bank on the 1x schedule and route events.
/// Track identity is learned from the Format events flowing through the
/// Bank (a TS demuxer only names its PIDs once the PMT arrives), so
/// routing needs no demuxer-specific knowledge at spawn time. Sends
/// never block: a full decode channel parks that target's messages and
/// gates its track in the Bank while the other track keeps releasing —
/// one track's chain capacity cannot wedge the other's release.
pub fn run_release(
    px: &Arc<PipelineShared>,
    video_tx: &SyncSender<MediaMsg>,
    audio_tx: &SyncSender<MediaMsg>,
) {
    let mut video_track: Option<media_demux::TrackId> = None;
    let mut audio_track: Option<media_demux::TrackId> = None;
    let mut parked_video = ParkedTarget::default();
    let mut parked_audio = ParkedTarget::default();
    let mut seen_generation: Option<Generation> = None;
    loop {
        if px.stopping() {
            return;
        }
        parked_video.flush(video_tx);
        parked_audio.flush(audio_tx);

        let wall = px.wall.now();
        let mut bank = px.bank.bank.lock().expect("bank lock");
        let generation = bank.generation();
        if seen_generation != Some(generation) {
            seen_generation = Some(generation);
            parked_video.drop_stale();
            parked_audio.drop_stale();
        }
        let blocked = |event: &StreamEvent| -> bool {
            match event {
                StreamEvent::Au(au) => {
                    (!parked_video.msgs.is_empty() && Some(au.track) == video_track)
                        || (!parked_audio.msgs.is_empty() && Some(au.track) == audio_track)
                }
                StreamEvent::Format(_, Format::Video { .. }) => !parked_video.msgs.is_empty(),
                StreamEvent::Format(_, Format::Audio { .. }) => !parked_audio.msgs.is_empty(),
                _ => false,
            }
        };
        let popped = bank.pop_due_gated(wall, &blocked);
        let metrics = bank.metrics();
        let awaiting_presentation = bank.awaiting_presentation();
        let next_due = if popped.is_none() {
            bank.next_due_gated(wall, &blocked)
        } else {
            None
        };
        drop(bank);

        // A priming join anchors its schedule presentation-relative: once
        // the clock has started (the first frame is reaching the viewer,
        // on the video thread or the audio-only path), tell the Bank so
        // the 1x schedule phase absorbs the decoder's input-to-output
        // depth instead of starving the primed decoder or spending the
        // banked lag.
        if awaiting_presentation {
            let playing = px.clock.lock().expect("clock lock").is_playing();
            if playing {
                px.bank
                    .bank
                    .lock()
                    .expect("bank lock")
                    .presentation_started(px.wall.now());
            }
        }

        px.shared
            .banked_us
            .store(metrics.banked.as_micros(), Ordering::Relaxed);
        px.shared
            .bank_holding
            .store(metrics.holding, Ordering::Relaxed);
        let diag_bank = px.diag.stage(Stage::Bank);
        diag_bank
            .occupancy
            .store(metrics.banked.as_millis() as u64, Ordering::Relaxed);
        diag_bank
            .occupancy_bytes
            .store(metrics.banked_bytes as u64, Ordering::Relaxed);

        match popped {
            Some(event) => {
                diag_bank.out_count.fetch_add(1, Ordering::Relaxed);
                px.bank.changed.notify_all();
                match event {
                    StreamEvent::Format(track, format) => {
                        let (target, tx) = match &format {
                            Format::Video { .. } => {
                                video_track = Some(track);
                                px.video_active.store(true, Ordering::Relaxed);
                                (&mut parked_video, video_tx)
                            }
                            Format::Audio { .. } => {
                                audio_track = Some(track);
                                px.audio_active.store(true, Ordering::Relaxed);
                                (&mut parked_audio, audio_tx)
                            }
                        };
                        target.send(tx, MediaMsg::Format(format));
                    }
                    StreamEvent::Au(au) => {
                        let (target, tx) = if Some(au.track) == video_track {
                            (&mut parked_video, video_tx)
                        } else if Some(au.track) == audio_track {
                            (&mut parked_audio, audio_tx)
                        } else {
                            // No format announced for this track yet:
                            // nothing downstream could decode it.
                            diag_bank.drops.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        // A full channel parks the AU; the channel depth
                        // stays the decoder's appetite bound (L4), it
                        // just no longer holds the other track hostage.
                        target.send(tx, MediaMsg::Au(au));
                    }
                    StreamEvent::Eos(_) => {
                        parked_video.send(video_tx, MediaMsg::Eos);
                        parked_audio.send(audio_tx, MediaMsg::Eos);
                    }
                    StreamEvent::Metadata(_)
                    | StreamEvent::Caption(_)
                    | StreamEvent::Discontinuity(..) => {}
                }
            }
            None => {
                let parked = !parked_video.msgs.is_empty() || !parked_audio.msgs.is_empty();
                let bank = px.bank.bank.lock().expect("bank lock");
                let wait = match next_due {
                    Some(due) if due > wall => {
                        Duration::from_micros((due - wall).as_micros().min(50_000) as u64)
                    }
                    Some(_) => Duration::from_millis(1),
                    None => IDLE_WAIT,
                };
                // A parked message has no wake-up when its channel frees:
                // poll it instead of sleeping the full schedule wait.
                let wait = if parked { wait.min(PARKED_POLL) } else { wait };
                let _ = px.bank.changed.wait_timeout(bank, wait);
            }
        }
    }
}

/// A hardware decoder detected the platform silently falling back to CPU
/// output mid-stream (a probe false-positive — the DXGI-backing signal).
/// Reroute to the software rung, reporting `DecodeFallbackHwToSw`; a
/// software refusal (including the performance cap) lands in the
/// CodecRefused posture — video mutes, audio plays on. Returns the
/// replacement decoder, or `None` when video is now muted. Frames between
/// the fallback point and the next keyframe are lost; the software
/// decoder picks up there.
fn reroute_hw_fallback(
    px: &Arc<PipelineShared>,
    current_coded: Option<(media_demux::VideoCodec, u32, u32)>,
    codec_private: &[u8],
    live: bool,
    error: &media_decode::DecodeError,
) -> Option<Box<dyn VideoDecoder>> {
    let Some((codec, width, height)) = current_coded else {
        px.video_active.store(false, Ordering::Relaxed);
        return None;
    };
    match crate::route::open_video_decoder(
        codec,
        width,
        height,
        live,
        crate::DecodePreference::SoftwareOnly,
        codec_private,
    ) {
        Ok(route) => {
            px.diag.event(
                px.wall.now(),
                EventCode::DecodeFallbackHwToSw,
                Stage::Decode,
                format!("{error}; decoding {codec:?} on {}", route.label),
            );
            eprintln!(
                "[basis-media] {error}; decoding {codec:?} on {}",
                route.label
            );
            Some(route.decoder)
        }
        Err(refused) => {
            px.diag.event(
                px.wall.now(),
                EventCode::CodecRefused,
                Stage::Decode,
                format!("{error}; software route refused {codec:?}: {refused}"),
            );
            px.video_active.store(false, Ordering::Relaxed);
            None
        }
    }
}

/// Video thread: decode into the FramePool, present due frames on the
/// clock's schedule. Nothing here ever blocks: a full pool parks the
/// decoded frame in `pending_frame`, a refusing MFT parks the AU in
/// `pending_au`, and both retry each tick while presentation keeps running.
pub fn run_video(px: &Arc<PipelineShared>, rx: &Receiver<MediaMsg>) {
    let mut decoder: Option<Box<dyn VideoDecoder>> = None;
    let mut sink = VideoSink::new();
    let mut generation = {
        let bank = px.bank.bank.lock().expect("bank lock");
        bank.generation()
    };
    let live = {
        let bank = px.bank.bank.lock().expect("bank lock");
        bank.config().liveness == media_bank::Liveness::Live
    };
    let mut draining = false;
    let mut pending_frame: Option<VideoFrame> = None;
    let mut pending_au: Option<Au> = None;
    let mut eos_after_drain = false;
    let mut current_coded: Option<(media_demux::VideoCodec, u32, u32)> = None;
    let mut current_private: Vec<u8> = Vec::new();

    loop {
        if px.stopping() {
            return;
        }

        // A seek has advanced the session generation and this thread's
        // Flush is still in the channel: everything held here is stale
        // timeline. Drop it rather than park on it — a parked AU against
        // a full decoder would starve the intake that delivers the Flush
        // (presentation is parked across the seek, so nothing frees the
        // decoder), and decode pulls would only churn frames the Flush
        // is about to clear.
        let flush_pending = Generation(px.shared.generation.load(Ordering::Relaxed)) != generation;
        if flush_pending {
            pending_au = None;
            pending_frame = None;
        }

        // 1. Move a parked frame into the pool; only then pull more output.
        if let Some(frame) = pending_frame.take() {
            match px.pool.try_publish(frame) {
                Ok(()) => {
                    px.diag
                        .stage(Stage::Decode)
                        .out_count
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(frame) => pending_frame = Some(frame),
            }
        }
        if !flush_pending && pending_frame.is_none() && decoder.is_some() {
            match decoder.as_mut().expect("decoder checked").try_output() {
                Ok(Some(frame)) => {
                    px.shared.frames_decoded.fetch_add(1, Ordering::Relaxed);
                    match px.pool.try_publish(frame) {
                        Ok(()) => {
                            px.diag
                                .stage(Stage::Decode)
                                .out_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        Err(frame) => pending_frame = Some(frame),
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    if decoder.as_ref().is_some_and(|d| d.hardware_fell_back()) {
                        decoder =
                            reroute_hw_fallback(px, current_coded, &current_private, live, &e);
                        if draining && let Some(rerouted) = decoder.as_mut() {
                            let _ = rerouted.begin_drain();
                        }
                    } else {
                        px.fail(EngineError::decode(e));
                        return;
                    }
                }
            }
        }

        // 2. Present the newest due frame. A parked clock (startup, or the
        //    frames after a seek) starts at the first ready frame's pts so
        //    buffering time never converts into lateness. The gate is
        //    clock-parked-ness, not the Buffering state: a stale pre-flush
        //    present can race the seek back to Playing, and only an
        //    explicit pause may keep the clock parked then. While the Bank
        //    holds, frames may already exist (a priming join decodes during
        //    the hold) — presentation stays gated so the join delivers its
        //    configured depth. The gate asks the Bank directly: during a
        //    priming join the release thread can sit blocked on a decode
        //    channel that only presentation drains, so a stored flag would
        //    deadlock the join.
        let wall = px.wall.now();
        let state = px.state();
        if state != State::Paused as u32
            && state != State::Error as u32
            && let Some(first_pts) = px.pool.first_ready_pts()
        {
            let parked = !px.clock.lock().expect("clock lock").is_playing();
            let gated = parked && px.bank.bank.lock().expect("bank lock").holding(wall);
            if !gated {
                let mut clock = px.clock.lock().expect("clock lock");
                // The generations must agree: after a seek parks the clock,
                // pre-flush frames still sit in the pool until the Flush is
                // processed, and restarting from one would resume the old
                // timeline (presenting its stale tail and racing the state
                // back to Playing). The clock adopts the new generation
                // when the demux thread parks it; this thread adopts it at
                // the Flush — between the two, stay parked.
                if !clock.is_playing() && clock.generation() == generation {
                    // Anchor at the audible position: the master playhead
                    // reads sink-latency behind the pull, so starting
                    // the clock latency-back lands the standing error at
                    // zero instead of leaving it to converge by slew and
                    // then sit at the dead-band edge. 0 on desktop.
                    let latency = MediaTime::from_micros(
                        px.audio_shared.output_latency_us.load(Ordering::Relaxed),
                    );
                    clock.discontinuity(wall, first_pts - latency);
                    clock.set_playing(wall, true);
                    px.clock_playing.store(true, Ordering::Relaxed);
                    px.present.mirror_clock(wall, clock.now(wall), true);
                    px.presentation_origin_us
                        .store(first_pts.as_micros(), Ordering::Relaxed);
                }
            }
        }
        let (now, playing) = {
            let clock = px.clock.lock().expect("clock lock");
            (clock.now(wall), clock.is_playing())
        };
        // Refresh the render thread's clock mirror every tick: slew moves
        // the offset slowly, so a ≤4 ms-stale mirror costs ≤0.2 ms of
        // selection error.
        px.present.mirror_clock(wall, now, playing);
        // While a render consumer is live, the render event owns frame
        // selection (§6.8 — due-ness and display share the vsync
        // quantiser); this thread presents only for consumers that issue
        // no render events (headless sessions, a non-rendering app).
        if playing
            && !px.present.consumer_live(wall)
            && let Some(mut lease) = px.pool.take_due(now)
        {
            if sink.ready() {
                match sink.present(px, &mut lease) {
                    Ok(_fresh) => {
                        px.diag
                            .stage(Stage::Present)
                            .out_count
                            .fetch_add(1, Ordering::Relaxed);
                        px.shared
                            .position_us
                            .store(lease.pts.as_micros(), Ordering::Relaxed);
                        if px.state() == State::Buffering as u32 {
                            px.set_state(State::Playing);
                        }
                    }
                    Err(e) => {
                        px.fail(EngineError::present(e));
                        return;
                    }
                }
            }
            px.pool.release(lease);
        }
        px.diag
            .stage(Stage::Pool)
            .occupancy
            .store(px.pool.ready_count() as u64, Ordering::Relaxed);
        px.diag
            .stage(Stage::Pool)
            .drops
            .store(px.pool.dropped(), Ordering::Relaxed);

        // 3. Retry a parked AU before taking anything new.
        if pending_au.is_some() && decoder.is_some() {
            let au = pending_au.take().expect("pending_au checked");
            match decoder
                .as_mut()
                .expect("decoder checked")
                .submit(&au.data, au.pts.as_micros())
            {
                Ok(SubmitOutcome::Accepted) => {}
                Ok(SubmitOutcome::NotAccepting) => pending_au = Some(au),
                Err(e) => {
                    if decoder.as_ref().is_some_and(|d| d.hardware_fell_back()) {
                        decoder =
                            reroute_hw_fallback(px, current_coded, &current_private, live, &e);
                        // The replacement decoder picks up from this AU
                        // (it joins cleanly at the next keyframe).
                        if decoder.is_some() {
                            pending_au = Some(au);
                        }
                    } else {
                        px.fail(EngineError::decode(e));
                        return;
                    }
                }
            }
        }

        // 4. Take the next message only when nothing is parked upstream.
        if pending_au.is_some() {
            std::thread::sleep(DECODE_TICK);
            continue;
        }
        match rx.recv_timeout(DECODE_TICK) {
            Ok(MediaMsg::Format(Format::Video {
                codec,
                coded_width,
                coded_height,
                display_width,
                display_height,
                codec_private,
            })) => {
                // A re-announce with unchanged geometry (a reconnect on the
                // same rendition) keeps the decoder and the shared texture:
                // the managed side holds the texture handle and must not
                // see it change mid-session.
                if decoder.is_some()
                    && sink.ready()
                    && current_coded == Some((codec, coded_width, coded_height))
                {
                    continue;
                }
                current_coded = Some((codec, coded_width, coded_height));
                current_private = codec_private;
                let decode_device;
                match open_video_decoder(
                    codec,
                    coded_width,
                    coded_height,
                    live,
                    px.decode_preference,
                    &current_private,
                ) {
                    Ok(route) => {
                        if let Some(reason) = &route.fallback {
                            px.diag.event(
                                px.wall.now(),
                                EventCode::DecodeFallbackHwToSw,
                                Stage::Decode,
                                format!("{reason}; decoding {codec:?} on {}", route.label),
                            );
                            eprintln!(
                                "[basis-media] {reason}; decoding {codec:?} on {}",
                                route.label
                            );
                        }
                        decode_device = route.decode_device;
                        decoder = Some(route.decoder);
                    }
                    Err(e) => {
                        // Refused video mutes the picture, audio plays on
                        // (§6.7: absence is a diagnostic, not a mystery) —
                        // and Ended becomes the audio thread's call.
                        px.diag.event(
                            px.wall.now(),
                            EventCode::CodecRefused,
                            Stage::Decode,
                            format!("no video decoder for {codec:?}: {e}"),
                        );
                        px.video_active.store(false, Ordering::Relaxed);
                        decoder = None;
                        continue;
                    }
                }
                px.shared.width.store(display_width, Ordering::Relaxed);
                px.shared.height.store(display_height, Ordering::Relaxed);
                // The output target carries the coded size; the consumer
                // crops to display size when it samples (M0 contract).
                if let Err(e) = sink.configure(px, coded_width, coded_height, decode_device) {
                    px.fail(EngineError::present(e));
                    return;
                }
            }
            Ok(MediaMsg::Format(_)) => {}
            Ok(MediaMsg::Au(au)) => {
                if au.generation != generation || flush_pending {
                    continue;
                }
                px.diag
                    .stage(Stage::Decode)
                    .in_count
                    .fetch_add(1, Ordering::Relaxed);
                if decoder.is_some() {
                    pending_au = Some(au);
                }
            }
            Ok(MediaMsg::Flush { generation: new }) => {
                generation = new;
                draining = false;
                eos_after_drain = false;
                pending_au = None;
                pending_frame = None;
                px.presentation_origin_us.store(i64::MIN, Ordering::Relaxed);
                if let Some(active) = decoder.as_mut()
                    && let Err(e) = active.reset()
                {
                    px.fail(EngineError::decode(e));
                    return;
                }
                px.pool.clear();
            }
            Ok(MediaMsg::Eos) => {
                if let Some(active) = decoder.as_mut() {
                    if let Err(e) = active.begin_drain() {
                        px.fail(EngineError::decode(e));
                        return;
                    }
                    draining = true;
                } else if !px.audio_active.load(Ordering::Relaxed) {
                    // No track reached either decode thread: nothing will
                    // ever present, end here. An audio-only session ends on
                    // the audio thread once its ring drains.
                    px.set_state(State::Ended);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // 5. Drained dry after EOS with nothing left due: the session ends
        //    once the last frame has been presented. An async adapter's
        //    `None` is only dry when it says so — until then keep polling
        //    (the adapter bounds its own wait), so a flush arriving during
        //    the drain is still picked up within a tick.
        if !flush_pending && draining && pending_frame.is_none() && decoder.is_some() {
            match decoder.as_mut().expect("decoder checked").try_output() {
                Ok(Some(frame)) => {
                    px.shared.frames_decoded.fetch_add(1, Ordering::Relaxed);
                    pending_frame = Some(frame);
                }
                Ok(None) => {
                    if decoder.as_ref().expect("decoder checked").drain_dry() {
                        draining = false;
                        eos_after_drain = true;
                    }
                }
                Err(e) => {
                    if decoder.as_ref().is_some_and(|d| d.hardware_fell_back()) {
                        decoder =
                            reroute_hw_fallback(px, current_coded, &current_private, live, &e);
                        match decoder.as_mut() {
                            // The replacement has no queued input: its
                            // drain goes dry immediately and the session
                            // ends on whatever was already presented.
                            Some(rerouted) => {
                                let _ = rerouted.begin_drain();
                            }
                            None => {
                                draining = false;
                                eos_after_drain = true;
                            }
                        }
                    } else {
                        px.fail(EngineError::decode(e));
                        return;
                    }
                }
            }
        }
        if eos_after_drain
            && pending_frame.is_none()
            && px.pool.ready_count() == 0
            && px.state() == State::Playing as u32
        {
            eos_after_drain = false;
            px.set_state(State::Ended);
        }
    }
}

/// Audio thread: decode, drop priming, feed the ring, drive the clock.
pub fn run_audio(px: &Arc<PipelineShared>, rx: &Receiver<MediaMsg>) {
    let mut decoder: Option<Box<dyn AudioDecoder>> = None;
    let mut producer: Option<AudioProducer> = None;
    /// PCM waiting for ring space: (pts of the *next* unwritten frame, data).
    struct Pending {
        pts_us: i64,
        data: Vec<f32>,
        offset: usize,
    }
    let mut pending: Option<Pending> = None;
    let mut generation = {
        let bank = px.bank.bank.lock().expect("bank lock");
        bank.generation()
    };
    let live = {
        let bank = px.bank.bank.lock().expect("bank lock");
        bank.config().liveness == media_bank::Liveness::Live
    };
    let mut last_master = Master::Wall;
    let mut last_correction = Correction::None;
    let mut current_audio: Option<(media_demux::AudioCodec, u32, u32, Vec<u8>)> = None;
    let mut pending_au: Option<Au> = None;
    // Wall time a chunk first stuck against a full ring with no consumer
    // progress; the discard grace window measures from here.
    let mut park_since: Option<MediaTime> = None;
    // Serve-trim narration state: trims counted at the last AudioTrim
    // event, and when it fired (rate-limited — steady trimming would
    // otherwise flood the bounded event queue).
    let mut trimmed_reported = 0u64;
    let mut last_trim_event = MediaTime::from_secs(-3600);
    // Live video-led joins: the presentation origin — the pts the parked
    // clock will start at, latched from the earliest decoded video frame
    // (frames are not consumed before the clock starts, so the pool's
    // oldest ready pts IS the start point). Audio preceding it is shed,
    // audio at or after it is kept; until it is known, audio parks and
    // banks upstream (the release gate keeps video flowing past it).
    let mut join_origin: Option<i64> = None;
    // Audio has reached the ring this generation: the join is over and
    // the shed stands down.
    let mut audio_joined = false;
    // Pre-join span shed this generation, reported once at the join.
    let mut shed_us: i64 = 0;
    let mut shed_reported = false;
    let mut draining = false;
    let mut decoder_dry = false;

    loop {
        if px.stopping() {
            return;
        }

        // Stale-timeline work drops when a seek's Flush is still queued
        // (see run_video): a parked AU here would starve the intake that
        // delivers the Flush, and once the clock restarts on the new
        // timeline a not-yet-swapped ring would briefly play the old
        // tail.
        let flush_pending = Generation(px.shared.generation.load(Ordering::Relaxed)) != generation;
        if flush_pending {
            pending = None;
            pending_au = None;
        }

        // Audio-leading start (descriptor-stated): the audio ring
        // starts the clock exactly as an audio-only session would, so
        // the join is audible immediately and video appears at its
        // keyframe against the running clock.
        let audio_led = live && px.audio_leading;
        // The pre-join shed only applies to live video-led sessions: on
        // VOD the read-ahead never runs meaningfully ahead of the origin,
        // on audio-only sessions the ring itself starts the clock, and on
        // an audio-leading start the first banked audio IS the join.
        let video_led = live && !audio_led && px.video_active.load(Ordering::Relaxed);
        // The shed is a join mechanism only: once audio has entered the
        // ring this generation, later timeline breaks (HLS wrap splices
        // restart timestamps) must not re-shed against a stale origin.
        let gate = video_led && !audio_joined;
        if gate && join_origin.is_none() {
            // The clock-start store is authoritative; the pool sample
            // covers the hold window, where decoded frames sit unconsumed
            // (post-start, a frame can be published and presented inside
            // one video-thread iteration — sampling would race).
            let started = px.presentation_origin_us.load(Ordering::Relaxed);
            join_origin = if started != i64::MIN {
                Some(started)
            } else {
                px.pool.first_ready_pts().map(|p| p.as_micros())
            };
        }

        // 1. Move pending PCM into the ring. A full ring is backpressure
        //    while the consumer pulls — and briefly at startup, before its
        //    first pull. Video-led live joins gate admission on the
        //    presentation origin instead: pre-join audio is shed (never
        //    heard — presentation starts at the video join point), primed
        //    audio at or after it is kept even against a full ring, and
        //    while the origin is unknown the chunk simply parks — the
        //    Bank banks the track upstream in compressed form. Once
        //    Playing, a consumer that stops pulling for the liveness
        //    window is inert and the stuck chunk is discarded so it
        //    cannot stall the pipeline behind it — the same rule the
        //    Ended logic applies. Non-gated lanes keep the inert-consumer
        //    discard in every state: with no video join point there is
        //    nothing to grade staleness against, and a headless session
        //    must not deadlock behind a consumer that never pulls.
        let origin_pending = gate && join_origin.is_none();
        if !origin_pending && let (Some(chunk), Some(out)) = (pending.as_mut(), producer.as_mut()) {
            let rate = out.sample_rate().max(1);
            let channels = out.channels().max(1) as usize;
            let remaining = &chunk.data[chunk.offset..];
            if !remaining.is_empty() {
                let frames_left = remaining.len() / channels;
                // Drop what still precedes the origin: encoder priming
                // (negative pts), plus the pre-join span on gated lanes.
                let origin = if gate { join_origin.unwrap_or(0) } else { 0 };
                let drop_frames =
                    frames_before_origin(chunk.pts_us.saturating_sub(origin), frames_left, rate);
                if drop_frames > 0 {
                    chunk.offset += drop_frames * channels;
                    if gate && chunk.pts_us >= 0 {
                        shed_us += drop_frames as i64 * 1_000_000 / i64::from(rate);
                    }
                    chunk.pts_us += drop_frames as i64 * 1_000_000 / i64::from(rate);
                } else {
                    let written = out.push(chunk.pts_us, remaining);
                    chunk.offset += written;
                    chunk.pts_us += (written / channels) as i64 * 1_000_000 / i64::from(rate);
                    if written > 0 {
                        audio_joined = true;
                        if shed_us > 0 && !shed_reported {
                            shed_reported = true;
                            px.diag.event(
                                px.wall.now(),
                                EventCode::AudioShed,
                                Stage::AudioRing,
                                format!("pre-join audio shed: {} ms", shed_us / 1000),
                            );
                        }
                    }
                    if written == 0 {
                        let wall = px.wall.now();
                        let last_pull_us =
                            px.audio_shared.last_pull_wall_us.load(Ordering::Relaxed);
                        // i64::MIN = never pulled (and would overflow Sub).
                        let live_consumer = last_pull_us != i64::MIN
                            && wall - MediaTime::from_micros(last_pull_us) <= AUDIO_LIVENESS;
                        // On video-led live lanes a stuck chunk is at/after
                        // the join point: it only discards once Playing (a
                        // dead consumer), never during the hold — that
                        // primed audio is the depth the viewer joins with.
                        let armed = !video_led || px.state() == State::Playing as u32;
                        if live_consumer {
                            park_since = None;
                        } else if armed && wall - *park_since.get_or_insert(wall) > AUDIO_LIVENESS {
                            px.diag
                                .stage(Stage::AudioRing)
                                .drops
                                .fetch_add(1, Ordering::Relaxed);
                            pending = None;
                        }
                    } else {
                        park_since = None;
                    }
                }
            }
            if pending.as_ref().is_some_and(|c| c.offset >= c.data.len()) {
                pending = None;
            }
        }

        // 2. Pull decoder output when the previous chunk is fully placed.
        if !flush_pending
            && pending.is_none()
            && let Some(active) = decoder.as_mut()
        {
            match active.try_output() {
                Ok(Some(chunk)) => {
                    px.diag
                        .stage(Stage::Decode)
                        .out_bytes
                        .fetch_add((chunk.data.len() * 4) as u64, Ordering::Relaxed);
                    px.shared
                        .audio_rate
                        .store(chunk.sample_rate, Ordering::Relaxed);
                    px.shared
                        .audio_channels
                        .store(chunk.channels, Ordering::Relaxed);
                    pending = Some(Pending {
                        pts_us: chunk.pts_us,
                        data: chunk.data,
                        offset: 0,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    px.fail(EngineError::decode(e));
                    return;
                }
            }
        }

        // 2b. Audio-only sessions — and audio-leading live joins — start
        //     the parked clock at the first banked PCM's pts and own the
        //     clock-derived position (until video presents, nothing else
        //     does either).
        if !px.video_active.load(Ordering::Relaxed) || audio_led {
            let ringing = producer.as_ref().is_some_and(|p| !p.is_drained());
            let state = px.state();
            if ringing
                && state == State::Buffering as u32
                && !px
                    .bank
                    .bank
                    .lock()
                    .expect("bank lock")
                    .holding(px.wall.now())
            {
                let wall = px.wall.now();
                let mut clock = px.clock.lock().expect("clock lock");
                // Generation gate as in run_video's restart: across a seek
                // the old ring (and its base pts) is stale until this
                // thread processes the Flush — a parked clock of another
                // generation stays parked, and the state stays Buffering.
                if !clock.is_playing() && clock.generation() == generation {
                    let base = px.audio_shared.base_pts_us.load(Ordering::Relaxed);
                    // Latency-back anchor as in run_video's restart.
                    let latency = MediaTime::from_micros(
                        px.audio_shared.output_latency_us.load(Ordering::Relaxed),
                    );
                    clock.discontinuity(wall, MediaTime::from_micros(base) - latency);
                    clock.set_playing(wall, true);
                    px.clock_playing.store(true, Ordering::Relaxed);
                    px.present.mirror_clock(wall, clock.now(wall), true);
                }
                let playing = clock.is_playing();
                drop(clock);
                if playing {
                    px.set_state(State::Playing);
                }
            } else if state == State::Playing as u32
                && (!px.video_active.load(Ordering::Relaxed)
                    || px
                        .diag
                        .stage(Stage::Present)
                        .out_count
                        .load(Ordering::Relaxed)
                        == 0)
            {
                // Once video presents, the presented pts owns position.
                let wall = px.wall.now();
                let now = px.clock.lock().expect("clock lock").now(wall);
                px.shared
                    .position_us
                    .store(now.as_micros(), Ordering::Relaxed);
            }
        }

        // 3. Clock: audio is master while the consumer demonstrably pulls.
        {
            let wall = px.wall.now();
            let playhead = px.audio_shared.playhead(wall);
            let last_pull =
                MediaTime::from_micros(px.audio_shared.last_pull_wall_us.load(Ordering::Relaxed));
            let consumer_live = playhead.is_some() && wall - last_pull <= AUDIO_LIVENESS;
            let mut clock = px.clock.lock().expect("clock lock");
            // Mirror the clock for the pull path's serve trim: the
            // pull must never take this lock. MIN while parked disables
            // the trim across startup, seeks and join holds.
            if clock.is_playing() {
                px.audio_shared
                    .clock_now_us
                    .store(clock.now(wall).as_micros(), Ordering::Relaxed);
                px.audio_shared
                    .clock_wall_us
                    .store(wall.as_micros(), Ordering::Relaxed);
            } else {
                px.audio_shared
                    .clock_now_us
                    .store(i64::MIN, Ordering::Relaxed);
            }
            let master = if consumer_live {
                Master::Audio
            } else {
                Master::Wall
            };
            if master != last_master {
                clock.set_master(wall, master);
                last_master = master;
            }
            if master == Master::Audio
                && let Some(playhead) = playhead
            {
                let correction = clock.observe_master(wall, playhead);
                drop(clock);
                match correction {
                    Correction::Slew { rate_ppm }
                        if !matches!(last_correction, Correction::Slew { .. }) =>
                    {
                        px.diag.event(
                            wall,
                            EventCode::SlewCorrection,
                            Stage::Clock,
                            format!("{rate_ppm} ppm towards audio master"),
                        );
                    }
                    Correction::Snap { error } => {
                        px.diag.event(
                            wall,
                            EventCode::SnapCorrection,
                            Stage::Clock,
                            format!("snap {error} to audio master"),
                        );
                    }
                    _ => {}
                }
                last_correction = correction;
            }
        }

        // 4. Occupancy + flow counters for the diagnostics block, and the
        // serve-trim advisory: trims run on the pull path, which
        // must not touch the event lock, so this thread narrates them.
        {
            let ring_stage = px.diag.stage(Stage::AudioRing);
            ring_stage.occupancy.store(
                producer
                    .as_ref()
                    .map(|p| p.free_frames() as u64)
                    .unwrap_or(0),
                Ordering::Relaxed,
            );
            ring_stage.in_count.store(
                px.audio_shared.pushed_frames.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            ring_stage.out_count.store(
                px.audio_shared.consumed_frames.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            let trimmed = px.audio_shared.trimmed_frames.load(Ordering::Relaxed);
            let wall = px.wall.now();
            if trimmed > trimmed_reported && wall - last_trim_event >= MediaTime::from_secs(5) {
                px.diag.event(
                    wall,
                    EventCode::AudioTrim,
                    Stage::AudioRing,
                    format!("serve trimmed {trimmed} frames (source outruns its pts timeline)"),
                );
                trimmed_reported = trimmed;
                last_trim_event = wall;
            }
        }

        // 4b. Retry a parked AU before taking anything new; while one is
        //     parked the decoder is full and phases 1–2 own making space,
        //     so nothing new comes off the channel (mirrors the video
        //     thread — the old inline submit loop could drop an AU or
        //     overwrite a pending chunk when the decoder pushed back).
        if let Some(au) = pending_au.take()
            && let Some(active) = decoder.as_mut()
        {
            match active.submit(&au.data, au.pts.as_micros()) {
                Ok(SubmitOutcome::Accepted) => {}
                Ok(SubmitOutcome::NotAccepting) => pending_au = Some(au),
                Err(e) => {
                    px.fail(EngineError::decode(e));
                    return;
                }
            }
        }
        if pending_au.is_some() {
            std::thread::sleep(DECODE_TICK);
            continue;
        }

        // If the ring is full and a chunk is stuck, wait a beat off the
        // channel so the consumer can drain.
        let wait = if pending.is_some() {
            Duration::from_millis(10)
        } else {
            DECODE_TICK
        };

        // 5. Take the next message.
        match rx.recv_timeout(wait) {
            Ok(MediaMsg::Format(Format::Audio {
                codec,
                sample_rate,
                channels,
                codec_private,
            })) => {
                let announced = (codec, sample_rate, channels, codec_private);
                if decoder.is_some() && current_audio.as_ref() == Some(&announced) {
                    // Unchanged format on a live reconnect: keep the ring.
                    continue;
                }
                let (codec, sample_rate, channels, codec_private) = announced;
                current_audio = Some((codec, sample_rate, channels, codec_private.clone()));
                match open_audio_decoder(codec, sample_rate, channels, &codec_private) {
                    Ok(d) => {
                        let (out_rate, out_channels) = d.output_format();
                        decoder = Some(d);
                        let (new_producer, consumer) = audio_pair(
                            AudioFormatInfo {
                                sample_rate: out_rate,
                                channels: out_channels,
                            },
                            Arc::clone(&px.audio_shared),
                        );
                        producer = Some(new_producer);
                        px.shared.audio_rate.store(out_rate, Ordering::Relaxed);
                        px.shared
                            .audio_channels
                            .store(out_channels, Ordering::Relaxed);
                        *px.audio_consumer.lock().expect("consumer slot") = Some(consumer);
                    }
                    Err(e) => {
                        // The C player's posture: refused audio mutes, video
                        // is unaffected.
                        px.diag.event(
                            px.wall.now(),
                            EventCode::CodecRefused,
                            Stage::Decode,
                            format!("{codec:?} decoder: {e}"),
                        );
                        decoder = None;
                    }
                }
            }
            Ok(MediaMsg::Format(_)) => {}
            Ok(MediaMsg::Au(au)) => {
                if au.generation != generation || flush_pending {
                    continue;
                }
                px.diag
                    .stage(Stage::Decode)
                    .in_count
                    .fetch_add(1, Ordering::Relaxed);
                if decoder.is_some() {
                    pending_au = Some(au);
                }
            }
            Ok(MediaMsg::Flush { generation: new }) => {
                generation = new;
                pending = None;
                pending_au = None;
                park_since = None;
                // The pool was cleared with the old timeline: the join
                // origin re-latches from the first post-seek frame.
                join_origin = None;
                audio_joined = false;
                shed_us = 0;
                shed_reported = false;
                draining = false;
                decoder_dry = false;
                if let Some(active) = decoder.as_mut()
                    && let Err(e) = active.reset()
                {
                    px.fail(EngineError::decode(e));
                    return;
                }
                // Fresh ring for the new timeline; the consumer slot swaps
                // under its lock, never on the pull path.
                if let Some(active) = decoder.as_ref() {
                    let (out_rate, out_channels) = active.output_format();
                    let (new_producer, consumer) = audio_pair(
                        AudioFormatInfo {
                            sample_rate: out_rate,
                            channels: out_channels,
                        },
                        Arc::clone(&px.audio_shared),
                    );
                    producer = Some(new_producer);
                    *px.audio_consumer.lock().expect("consumer slot") = Some(consumer);
                }
            }
            Ok(MediaMsg::Eos) => {
                match decoder.as_mut() {
                    Some(active) => {
                        if let Err(e) = active.begin_drain() {
                            px.fail(EngineError::decode(e));
                            return;
                        }
                        draining = true;
                    }
                    // Audio was refused and no video exists: nothing left
                    // to play out.
                    None if !px.video_active.load(Ordering::Relaxed) => {
                        px.set_state(State::Ended);
                    }
                    None => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // Drained dry after EOS with the ring consumed: an audio-only
        // session ends here, where the last sample's consumption is
        // visible — ending on the video thread's EOS would cut the tail
        // of the ring (up to its full depth). A consumer that stopped
        // pulling doesn't hold the session open.
        if !flush_pending
            && draining
            && pending.is_none()
            && let Some(active) = decoder.as_mut()
        {
            match active.try_output() {
                Ok(Some(chunk)) => {
                    pending = Some(Pending {
                        pts_us: chunk.pts_us,
                        data: chunk.data,
                        offset: 0,
                    });
                }
                Ok(None) => {
                    // Async adapters bound their own drain wait; keep
                    // polling until they report dry (see run_video).
                    if active.drain_dry() {
                        draining = false;
                        decoder_dry = true;
                    }
                }
                Err(e) => {
                    px.fail(EngineError::decode(e));
                    return;
                }
            }
        }
        if decoder_dry
            && pending.is_none()
            && !px.video_active.load(Ordering::Relaxed)
            && px.state() == State::Playing as u32
        {
            let wall = px.wall.now();
            let last_pull =
                MediaTime::from_micros(px.audio_shared.last_pull_wall_us.load(Ordering::Relaxed));
            let consumer_live = wall - last_pull <= AUDIO_LIVENESS;
            let ring_drained = producer.as_ref().is_none_or(|p| p.is_drained());
            if ring_drained || !consumer_live {
                decoder_dry = false;
                px.set_state(State::Ended);
            }
        }
    }
}
