//! The media clock: one `MediaTime` type, master selection, and the drift
//! correction ladder (dead band / bounded slew / snap).
//!
//! One clock per session. Position as reported anywhere is always
//! clock-derived: `MediaClock::now` is the position, there is no other
//! position source.

#![forbid(unsafe_code)]

use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// The one time type: signed microseconds. Used for media positions,
/// durations, and monotonic wall-clock readings alike. Timestamp wrap
/// handling (33-bit PCR, 32-bit RTP) happens in the demux/RTP crates;
/// nothing holding a `MediaTime` ever sees a wrapped value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaTime(i64);

impl MediaTime {
    pub const ZERO: MediaTime = MediaTime(0);
    pub const MAX: MediaTime = MediaTime(i64::MAX);

    pub const fn from_micros(us: i64) -> Self {
        Self(us)
    }

    pub const fn from_millis(ms: i64) -> Self {
        Self(ms * 1_000)
    }

    pub const fn from_secs(s: i64) -> Self {
        Self(s * 1_000_000)
    }

    pub const fn as_micros(self) -> i64 {
        self.0
    }

    pub const fn as_millis(self) -> i64 {
        self.0 / 1_000
    }

    pub const fn abs(self) -> Self {
        Self(self.0.abs())
    }

    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }

    pub fn clamp(self, min: Self, max: Self) -> Self {
        Self(self.0.clamp(min.0, max.0))
    }

    pub const fn min(self, rhs: Self) -> Self {
        if self.0 <= rhs.0 { self } else { rhs }
    }

    pub const fn max(self, rhs: Self) -> Self {
        if self.0 >= rhs.0 { self } else { rhs }
    }

    /// Scale by a parts-per-million factor, rounding towards zero.
    pub fn scale_ppm(self, ppm: i64) -> Self {
        Self((self.0 as i128 * ppm as i128 / 1_000_000) as i64)
    }
}

impl Add for MediaTime {
    type Output = MediaTime;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for MediaTime {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for MediaTime {
    type Output = MediaTime;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for MediaTime {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Neg for MediaTime {
    type Output = MediaTime;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl fmt::Display for MediaTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}us", self.0)
    }
}

/// Seek/reconnect generation. Every stage drops stale-generation events on
/// sight; the clock snaps across a generation change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

impl Generation {
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Which source the clock chases. Explicit state, switchable at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Master {
    /// Audio playhead is master while an audio track is playing.
    Audio,
    /// Wall-clock paced: the clock free-runs at 1x and ignores observations.
    Wall,
}

/// Ladder parameters. Defaults follow the calibration in the spec: 20 ms dead
/// band (Media3's figure), 2% slew cap (validated on the C VOD branch, well
/// under the ~6% audibility bound), 700 ms snap threshold (the C live
/// branch's resync figure; configurable pending re-examination once slew
/// exists).
#[derive(Debug, Clone)]
pub struct ClockConfig {
    pub dead_band: MediaTime,
    pub snap_threshold: MediaTime,
    pub slew_cap_ppm: i64,
    /// Time constant of a first-order filter applied to the master error
    /// before the dead-band/slew rungs act on it. `None` acts on the raw
    /// error. Set this where the master's *measurement* carries platform
    /// jitter wider than the dead band (Android DSP callbacks alternate
    /// and miss slots, wobbling a consumed-frames playhead by ±40 ms
    /// around the true position); the filter keeps that wobble from
    /// reaching frame due-times while genuine offsets still converge.
    /// The snap rung always acts on the raw error — a real discontinuity
    /// must never wait out a filter.
    pub master_filter: Option<MediaTime>,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            dead_band: MediaTime::from_millis(20),
            snap_threshold: MediaTime::from_millis(700),
            slew_cap_ppm: 20_000,
            master_filter: None,
        }
    }
}

/// What an observation did, for the diagnostics event log (slew/seek
/// corrections are default-verbosity events, §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    /// Error inside the dead band, or observation ignored (wall master,
    /// paused, stale generation).
    None,
    /// Presentation rate slewed towards the master; the offset applied, ppm.
    Slew { rate_ppm: i64 },
    /// Error beyond the snap threshold, a discontinuity, or a generation
    /// change: the clock jumped to the master position.
    Snap { error: MediaTime },
}

#[derive(Debug)]
pub struct MediaClock {
    cfg: ClockConfig,
    /// Wall time of the current linear segment's origin.
    anchor_wall: MediaTime,
    /// Media position at the segment origin.
    anchor_media: MediaTime,
    /// Rate offset from 1x in ppm, clamped to ±slew_cap_ppm.
    rate_ppm: i64,
    master: Master,
    playing: bool,
    generation: Generation,
    /// Filtered master error (`master_filter` engaged): the running
    /// estimate and the wall time it was last advanced. Cleared on snap
    /// and master switch so a fresh timeline seeds from its first
    /// observation.
    filtered: Option<(MediaTime, MediaTime)>,
}

impl MediaClock {
    pub fn new(
        cfg: ClockConfig,
        wall: MediaTime,
        origin: MediaTime,
        generation: Generation,
    ) -> Self {
        Self {
            cfg,
            anchor_wall: wall,
            anchor_media: origin,
            rate_ppm: 0,
            master: Master::Wall,
            playing: false,
            generation,
            filtered: None,
        }
    }

    /// The session position. Clock-derived, always.
    pub fn now(&self, wall: MediaTime) -> MediaTime {
        if !self.playing {
            return self.anchor_media;
        }
        let elapsed = wall - self.anchor_wall;
        self.anchor_media + elapsed + elapsed.scale_ppm(self.rate_ppm)
    }

    pub fn rate_ppm(&self) -> i64 {
        self.rate_ppm
    }

    pub fn master(&self) -> Master {
        self.master
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Re-anchor the linear segment at the current position so a rate or
    /// state change never moves `now`.
    fn rebase(&mut self, wall: MediaTime) {
        self.anchor_media = self.now(wall);
        self.anchor_wall = wall;
    }

    pub fn set_playing(&mut self, wall: MediaTime, playing: bool) {
        if self.playing != playing {
            self.rebase(wall);
            self.playing = playing;
        }
    }

    /// Switch master selection (audio-only seeks and mute transitions were a
    /// C bug family; this is explicit state). Switching never moves `now`;
    /// switching to `Wall` also clears any running slew.
    pub fn set_master(&mut self, wall: MediaTime, master: Master) {
        self.rebase(wall);
        self.master = master;
        self.filtered = None;
        if master == Master::Wall {
            self.rate_ppm = 0;
        }
    }

    /// Feed a master position report (the audio playhead). Applies the
    /// ladder: dead band → nothing (and any running slew ends), slew band →
    /// rate offset capped at `slew_cap_ppm`, beyond `snap_threshold` → snap.
    /// With `master_filter` set, the dead-band/slew rungs act on a
    /// first-order-filtered error; the snap rung always acts on the raw
    /// error and clears the filter.
    pub fn observe_master(&mut self, wall: MediaTime, master_pos: MediaTime) -> Correction {
        if self.master != Master::Audio || !self.playing {
            return Correction::None;
        }
        let raw = master_pos - self.now(wall);
        if raw.abs() >= self.cfg.snap_threshold {
            self.snap(wall, master_pos);
            return Correction::Snap { error: raw };
        }
        let error = match self.cfg.master_filter {
            None => raw,
            Some(tau) => self.filter_error(wall, raw, tau),
        };
        self.rebase(wall);
        if error.abs() <= self.cfg.dead_band {
            self.rate_ppm = 0;
            return Correction::None;
        }
        // Fixed-rate catch-up at the cap, the dash.js/C-VOD-branch shape.
        self.rate_ppm = if error > MediaTime::ZERO {
            self.cfg.slew_cap_ppm
        } else {
            -self.cfg.slew_cap_ppm
        };
        Correction::Slew {
            rate_ppm: self.rate_ppm,
        }
    }

    /// Advance the filtered error towards `raw` with gain `dt / (tau + dt)`
    /// — a first-order low-pass that is exact under irregular observation
    /// cadence. The first observation after a reset seeds the estimate
    /// directly, so a fresh timeline's initial correction is not delayed.
    fn filter_error(&mut self, wall: MediaTime, raw: MediaTime, tau: MediaTime) -> MediaTime {
        match &mut self.filtered {
            None => {
                self.filtered = Some((raw, wall));
                raw
            }
            Some((estimate, last)) => {
                let dt = (wall - *last).max(MediaTime::ZERO).as_micros();
                *last = wall;
                let step = (raw - *estimate).as_micros() * dt / (tau.as_micros() + dt).max(1);
                *estimate += MediaTime::from_micros(step);
                *estimate
            }
        }
    }

    /// External soft-target slew under the wall master (§8.4's ladder on
    /// masterless lanes): rebase so `now` never jumps, then run at
    /// 1x + `ppm`, clamped to the slew cap. Ignored under the audio
    /// master — there the correction rides the audio playhead and
    /// `observe_master` carries the clock along.
    pub fn slew_wall(&mut self, wall: MediaTime, ppm: i64) {
        if self.master != Master::Wall {
            return;
        }
        self.rebase(wall);
        self.rate_ppm = ppm.clamp(-self.cfg.slew_cap_ppm, self.cfg.slew_cap_ppm);
    }

    /// A timeline break (PCR wrap, ad splice, decoder reset): snap, never
    /// slew.
    pub fn discontinuity(&mut self, wall: MediaTime, new_pos: MediaTime) -> Correction {
        let error = new_pos - self.now(wall);
        self.snap(wall, new_pos);
        Correction::Snap { error }
    }

    /// Seek/reconnect: adopt the new generation and snap to its position.
    pub fn advance_generation(
        &mut self,
        wall: MediaTime,
        generation: Generation,
        new_pos: MediaTime,
    ) -> Correction {
        self.generation = generation;
        let error = new_pos - self.now(wall);
        self.snap(wall, new_pos);
        Correction::Snap { error }
    }

    fn snap(&mut self, wall: MediaTime, pos: MediaTime) {
        self.anchor_wall = wall;
        self.anchor_media = pos;
        self.rate_ppm = 0;
        self.filtered = None;
    }
}
