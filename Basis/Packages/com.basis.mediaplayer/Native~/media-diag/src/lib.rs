//! Diagnostics and observability (spec §10).
//!
//! The rule: a pipeline stage is not done until its input rate, its
//! occupancy and its output rate are exported. Every stage publishes
//! counters into a per-session block; a structured event log runs at
//! default verbosity with stable codes (no failure path hides behind a
//! verbose flag); and the capture recorder dumps the timeline as CSV with a
//! stable column contract for the analysis tooling.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use media_clock::MediaTime;

/// Where a textual diagnostic goes. `None` means stderr, which is the
/// console a desktop run or `bm-probe` has. A host whose platform gives
/// the process no usable stderr installs its own as it loads: Android
/// discards a native process's stderr outright, so without one nothing
/// the engine says about a session survives it.
static SINK: Mutex<Option<fn(&str)>> = Mutex::new(None);

/// Installs the sink [`log`] routes to. A host calls this once as it
/// loads, before opening anything; the last writer wins.
pub fn set_log_sink(sink: fn(&str)) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
}

/// Emits one diagnostic line. The default stderr sink names the source,
/// since it shares a console with whatever else the host writes; an
/// installed sink is not given the prefix, because a platform log that
/// needs one carries it as a tag instead.
pub fn log(line: &str) {
    // Copied out so the sink runs with the lock released: a sink is
    // host-supplied and may log, and re-entering here would deadlock.
    let sink = *SINK.lock().unwrap_or_else(|e| e.into_inner());
    match sink {
        Some(sink) => sink(line),
        None => eprintln!("[basis-media] {line}"),
    }
}

/// [`log`] with `format!` arguments.
#[macro_export]
macro_rules! diag_log {
    ($($arg:tt)*) => { $crate::log(&format!($($arg)*)) };
}

/// Every stage of the one pipeline shape (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    Source = 0,
    Demux = 1,
    Bank = 2,
    Decode = 3,
    Pool = 4,
    Present = 5,
    AudioRing = 6,
    Clock = 7,
}

pub const STAGE_COUNT: usize = 8;

pub const STAGES: [Stage; STAGE_COUNT] = [
    Stage::Source,
    Stage::Demux,
    Stage::Bank,
    Stage::Decode,
    Stage::Pool,
    Stage::Present,
    Stage::AudioRing,
    Stage::Clock,
];

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Source => "source",
            Stage::Demux => "demux",
            Stage::Bank => "bank",
            Stage::Decode => "decode",
            Stage::Pool => "pool",
            Stage::Present => "present",
            Stage::AudioRing => "audio_ring",
            Stage::Clock => "clock",
        }
    }
}

/// In rate, occupancy, out rate — plus the failure counters no stage ships
/// without. All relaxed atomics: single-writer per counter, readers take
/// snapshots.
#[derive(Debug, Default)]
pub struct StageCounters {
    pub in_count: AtomicU64,
    pub in_bytes: AtomicU64,
    pub out_count: AtomicU64,
    pub out_bytes: AtomicU64,
    pub occupancy: AtomicU64,
    pub occupancy_bytes: AtomicU64,
    pub drops: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageSnapshot {
    pub in_count: u64,
    pub in_bytes: u64,
    pub out_count: u64,
    pub out_bytes: u64,
    pub occupancy: u64,
    pub occupancy_bytes: u64,
    pub drops: u64,
    pub errors: u64,
}

impl StageCounters {
    pub fn snapshot(&self) -> StageSnapshot {
        StageSnapshot {
            in_count: self.in_count.load(Ordering::Relaxed),
            in_bytes: self.in_bytes.load(Ordering::Relaxed),
            out_count: self.out_count.load(Ordering::Relaxed),
            out_bytes: self.out_bytes.load(Ordering::Relaxed),
            occupancy: self.occupancy.load(Ordering::Relaxed),
            occupancy_bytes: self.occupancy_bytes.load(Ordering::Relaxed),
            drops: self.drops.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// Structured event codes, stable across platforms and releases. Silent
/// fallbacks are a contradiction in terms: every fallback, refusal, cap hit
/// and correction is one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum EventCode {
    StateChange = 1,
    Reconnect = 2,
    DecodeFallbackHwToSw = 3,
    TransportFallback = 4,
    CodecRefused = 5,
    CapHit = 6,
    UrlBlocked = 7,
    SlewCorrection = 8,
    SnapCorrection = 9,
    Seek = 10,
    Discontinuity = 11,
    CapabilityProbe = 12,
    Error = 13,
    /// Live join: audio preceding the presentation origin was shed
    /// (never presentable — the clock starts at the video join point).
    AudioShed = 14,
    /// The ring's serve-side trim engaged: banked audio crossed the
    /// high-water mark and excess frames were discarded to keep the
    /// serve on the media timeline (a source delivering more samples
    /// than its pts timeline claims). Detail carries the
    /// cumulative trimmed count.
    AudioTrim = 15,
    /// Shared-playback soft target (§8.4): the slew rung engaged or
    /// released. Detail carries the target error and the applied rate.
    SyncSlew = 16,
    /// Shared-playback soft target: the error crossed the seek
    /// threshold — the last rung, never the first. Detail carries the
    /// error and the landed target.
    SyncSeek = 17,
}

#[derive(Debug, Clone)]
pub struct DiagEvent {
    pub wall: MediaTime,
    pub code: EventCode,
    pub stage: Stage,
    pub detail: String,
}

/// Per-session diagnostics block: one set of stage counters, the audio
/// serve trim's running total and the bounded event log. Shared by `Arc`
/// across the session's threads.
#[derive(Debug)]
pub struct SessionDiag {
    stages: [StageCounters; STAGE_COUNT],
    events: Mutex<Vec<DiagEvent>>,
    event_cap: usize,
    /// Events lost to the cap — visible, never silent.
    events_dropped: AtomicU64,
    /// Frames discarded by the audio ring's serve-side trim. Session
    /// level rather than a field on `Stage::AudioRing`, whose `drops`
    /// already counts whole chunks dropped for an inert consumer: one
    /// column cannot carry both units.
    audio_trimmed_frames: AtomicU64,
    /// Presented video pts minus the audio playhead, microseconds, as the
    /// engine measures it. `i32::MIN` while either term is missing; real
    /// values are clamped clear of the sentinel so it stays unambiguous.
    /// Session-level rather than a stage field: it is a relationship between
    /// two stages, not a property of either.
    av_offset_us: AtomicI32,
}

impl SessionDiag {
    pub fn new(event_cap: usize) -> Self {
        Self {
            stages: Default::default(),
            events: Mutex::new(Vec::new()),
            event_cap,
            events_dropped: AtomicU64::new(0),
            audio_trimmed_frames: AtomicU64::new(0),
            av_offset_us: AtomicI32::new(i32::MIN),
        }
    }

    pub fn stage(&self, stage: Stage) -> &StageCounters {
        &self.stages[stage as usize]
    }

    pub fn event(&self, wall: MediaTime, code: EventCode, stage: Stage, detail: impl Into<String>) {
        let mut events = self.events.lock().expect("diag event lock");
        if events.len() >= self.event_cap {
            self.events_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        events.push(DiagEvent {
            wall,
            code,
            stage,
            detail: detail.into(),
        });
    }

    /// Drain the pending events (the ABI/event-surface consumer).
    pub fn take_events(&self) -> Vec<DiagEvent> {
        std::mem::take(&mut *self.events.lock().expect("diag event lock"))
    }

    pub fn events_dropped(&self) -> u64 {
        self.events_dropped.load(Ordering::Relaxed)
    }

    /// Publish the audio ring's cumulative serve-trim total, in frames.
    /// The trim itself runs on the pull path, which must not touch the
    /// diag lock, so the audio thread carries the figure across.
    pub fn set_audio_trimmed(&self, frames: u64) {
        self.audio_trimmed_frames.store(frames, Ordering::Relaxed);
    }

    /// Publish the A/V offset for the capture. The engine already computes it
    /// for the ABI snapshot; this is the same value, so the capture and the
    /// snapshot can never disagree.
    pub fn set_av_offset(&self, offset_us: i32) {
        self.av_offset_us.store(offset_us, Ordering::Relaxed);
    }

    pub fn audio_trimmed(&self) -> u64 {
        self.audio_trimmed_frames.load(Ordering::Relaxed)
    }

    pub fn av_offset(&self) -> i32 {
        self.av_offset_us.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> [StageSnapshot; STAGE_COUNT] {
        STAGES.map(|s| self.stage(s).snapshot())
    }
}

impl Default for SessionDiag {
    fn default() -> Self {
        Self::new(1024)
    }
}

/// The capture recorder: the per-session timeline as CSV rows with a stable
/// column contract, so the analysis tooling keeps working unchanged across
/// platforms and releases. Columns only ever get appended; existing names
/// and order are the contract.
#[derive(Debug, Default)]
pub struct CaptureRecorder {
    rows: Vec<String>,
}

impl CaptureRecorder {
    /// The column contract. Serialisation is size-aware end to end (growable
    /// buffers, no fixed-size truncation).
    pub fn header() -> String {
        let mut h = String::from("wall_us");
        for stage in STAGES {
            let name = stage.name();
            for col in [
                "in_count",
                "in_bytes",
                "out_count",
                "out_bytes",
                "occupancy",
                "occupancy_bytes",
                "drops",
                "errors",
            ] {
                let _ = write!(h, ",{name}_{col}");
            }
        }
        // Appended after the stage block: the serve trim is the audio
        // path's only loss channel with no stage field of its own, and
        // reconstructing it from in/out/occupancy is error-prone.
        let _ = write!(h, ",audio_trimmed_frames");
        // Also appended after the stage block, and for the same reason: the
        // A/V offset is a relationship between the present and audio-ring
        // stages rather than a counter belonging to either. Without it here
        // nothing headless can see the offset's shape over time — it reached
        // only the ABI snapshot and the managed frame capture, which is how a
        // presentation lag came to be read as a clock error.
        let _ = write!(h, ",av_offset_us");
        h
    }

    pub fn sample(&mut self, wall: MediaTime, diag: &SessionDiag) {
        let mut row = String::with_capacity(256);
        let _ = write!(row, "{}", wall.as_micros());
        for snap in diag.snapshot() {
            let _ = write!(
                row,
                ",{},{},{},{},{},{},{},{}",
                snap.in_count,
                snap.in_bytes,
                snap.out_count,
                snap.out_bytes,
                snap.occupancy,
                snap.occupancy_bytes,
                snap.drops,
                snap.errors
            );
        }
        let _ = write!(row, ",{}", diag.audio_trimmed());
        let _ = write!(row, ",{}", diag.av_offset());
        self.rows.push(row);
    }

    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// Writes the capture and flushes it. The sink is taken by value, so
    /// returning without flushing would leave a buffered writer's tail to
    /// its `Drop`, which discards the error by design: an `Ok` here has to
    /// mean the rows reached the sink, not that they reached its buffer.
    pub fn write_csv<W: std::io::Write>(&self, w: W) -> std::io::Result<()> {
        self.write_csv_rows(w, true)
    }

    /// As [`Self::write_csv`], with the header suppressed. Appending a
    /// second capture to a file that already has one would otherwise put a
    /// header row in the middle of the data, which every reader of this
    /// format would take as a row.
    pub fn write_csv_rows<W: std::io::Write>(&self, mut w: W, header: bool) -> std::io::Result<()> {
        if header {
            writeln!(w, "{}", Self::header())?;
        }
        for row in &self.rows {
            writeln!(w, "{row}")?;
        }
        w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column contract is stable: this failing means downstream
    /// analysis tooling breaks — append columns, never rename or reorder.
    #[test]
    fn column_contract_is_stable() {
        let header = CaptureRecorder::header();
        assert!(header.starts_with("wall_us,source_in_count,"));
        // wall_us + the stage block + the two appended session columns.
        assert_eq!(header.split(',').count(), 1 + STAGE_COUNT * 8 + 2);
        assert!(header.contains(",bank_occupancy_bytes,"));
        assert!(header.contains(",clock_errors,"));
        assert!(header.ends_with(",audio_trimmed_frames,av_offset_us"));
    }

    /// Both appended session columns reach the capture, in the order the
    /// header states. Asserted by position rather than by suffix: a suffix
    /// check silently becomes a check of whichever column was appended last.
    #[test]
    fn the_appended_session_columns_reach_the_capture() {
        let diag = SessionDiag::default();
        diag.set_audio_trimmed(21_535);
        diag.set_av_offset(-27_475);
        let mut rec = CaptureRecorder::default();
        rec.sample(MediaTime::from_millis(16), &diag);
        let fields: Vec<&str> = rec.rows()[0].split(',').collect();
        let header_line = CaptureRecorder::header();
        let header: Vec<&str> = header_line.split(',').collect();
        assert_eq!(fields.len(), header.len());
        assert_eq!(header[header.len() - 2], "audio_trimmed_frames");
        assert_eq!(fields[fields.len() - 2], "21535");
        assert_eq!(header[header.len() - 1], "av_offset_us");
        assert_eq!(fields[fields.len() - 1], "-27475");
    }

    /// The unknown sentinel survives the round trip: a capture taken before
    /// audio and a presented frame both exist must read as "no value", not as
    /// an offset of zero.
    #[test]
    fn an_unmeasurable_av_offset_reaches_the_capture_as_the_sentinel() {
        let diag = SessionDiag::default();
        let mut rec = CaptureRecorder::default();
        rec.sample(MediaTime::from_millis(16), &diag);
        let last = rec.rows()[0].rsplit(',').next().expect("a last field");
        assert_eq!(last, i32::MIN.to_string(), "default must be the sentinel");
    }

    #[test]
    fn rows_match_header_width() {
        let diag = SessionDiag::default();
        diag.stage(Stage::Bank)
            .in_count
            .fetch_add(3, Ordering::Relaxed);
        let mut rec = CaptureRecorder::default();
        rec.sample(MediaTime::from_millis(16), &diag);
        let header_cols = CaptureRecorder::header().split(',').count();
        assert_eq!(rec.rows()[0].split(',').count(), header_cols);
    }

    #[test]
    fn event_log_caps_visibly() {
        let diag = SessionDiag::new(2);
        for i in 0..3 {
            diag.event(
                MediaTime::from_millis(i),
                EventCode::CapHit,
                Stage::Bank,
                "byte cap",
            );
        }
        assert_eq!(diag.take_events().len(), 2);
        assert_eq!(diag.events_dropped(), 1);
    }

    /// Accepts every byte and fails only when asked to flush: the shape a
    /// file presents when the volume fills while its buffer is still held.
    struct FlushFails;

    impl std::io::Write for FlushFails {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("no space left on device"))
        }
    }

    static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The sink and the capture behind it are process-wide and the harness
    /// runs rows in parallel, so a row touching either takes this first —
    /// otherwise one row's lines land in another row's expectations.
    static GLOBALS: Mutex<()> = Mutex::new(());

    fn capture(line: &str) {
        CAPTURED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_string());
    }

    fn capture_marked(line: &str) {
        CAPTURED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("replaced: {line}"));
    }

    /// One row rather than three because the sink is process-wide state,
    /// so separate rows would race each other under the test harness.
    /// Covers delivery, the macro's formatting and replacement; the
    /// default stderr path is the one branch no in-process row can read.
    #[test]
    fn an_installed_sink_takes_every_line() {
        let _globals = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        set_log_sink(capture);
        log("plain");
        crate::diag_log!("formatted {} {}", 1, "two");
        set_log_sink(capture_marked);
        crate::diag_log!("after replacing");
        let lines = CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Put the default back before asserting: the sink is process-wide,
        // the harness runs rows in parallel, and a row that panicked here
        // would leave the capture installed for whatever ran next.
        *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
        assert_eq!(
            lines,
            ["plain", "formatted 1 two", "replaced: after replacing"]
        );
    }

    /// Fails every write. Behind a buffer larger than the capture, the
    /// first write it ever sees is the one the flush makes.
    struct WriteFails;

    impl std::io::Write for WriteFails {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no space left on device"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn one_row() -> CaptureRecorder {
        let diag = SessionDiag::default();
        let mut rec = CaptureRecorder::default();
        rec.sample(MediaTime::from_millis(16), &diag);
        rec
    }

    /// The capture is the post-mortem channel for a failed session, so a
    /// write that never reached the file must not return Ok. The sink is
    /// taken by value and dropped inside the call, and a buffered writer's
    /// Drop flushes with the error discarded, so the flush has to be made
    /// and reported here or nowhere.
    #[test]
    fn a_failing_flush_is_reported() {
        let rec = one_row();
        assert!(rec.write_csv(FlushFails).is_err());
    }

    /// Nothing was written, so nothing can have failed on the way out —
    /// bar the flush, which still has to be made and reported.
    #[test]
    fn an_empty_capture_still_flushes() {
        let rec = CaptureRecorder::default();
        assert!(rec.write_csv_rows(FlushFails, false).is_err());
    }

    /// The call sites all pass a `BufWriter`. Sized past the whole capture
    /// so no row can spill early, which is what makes the tail flush the
    /// only write and this row the shipped failure mode rather than an
    /// overflow the `?` on `writeln!` would have caught anyway.
    #[test]
    fn a_buffered_tail_that_never_lands_is_reported() {
        let rec = one_row();
        let bytes = CaptureRecorder::header().len() + rec.rows()[0].len() + 2;
        let w = std::io::BufWriter::with_capacity(bytes * 4, WriteFails);
        assert!(rec.write_csv(w).is_err());
    }
}
