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
use std::sync::atomic::{AtomicU64, Ordering};

use media_clock::MediaTime;

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

/// Per-session diagnostics block: one set of stage counters plus the
/// bounded event log. Shared by `Arc` across the session's threads.
#[derive(Debug)]
pub struct SessionDiag {
    stages: [StageCounters; STAGE_COUNT],
    events: Mutex<Vec<DiagEvent>>,
    event_cap: usize,
    /// Events lost to the cap — visible, never silent.
    events_dropped: AtomicU64,
}

impl SessionDiag {
    pub fn new(event_cap: usize) -> Self {
        Self {
            stages: Default::default(),
            events: Mutex::new(Vec::new()),
            event_cap,
            events_dropped: AtomicU64::new(0),
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
        self.rows.push(row);
    }

    pub fn rows(&self) -> &[String] {
        &self.rows
    }

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
        Ok(())
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
        assert_eq!(header.split(',').count(), 1 + STAGE_COUNT * 8);
        assert!(header.contains(",bank_occupancy_bytes,"));
        assert!(header.ends_with(",clock_errors"));
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
}
