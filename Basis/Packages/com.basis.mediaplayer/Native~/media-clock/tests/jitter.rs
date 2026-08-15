//! The master-filter rung: DSP-callback jitter measured on Quest Pro
//! (2026-08-15 frame captures) must not reach frame due-times, while
//! genuine offsets, discontinuities and generation changes correct
//! exactly as on the unfiltered ladder.

use media_clock::{ClockConfig, Correction, Generation, Master, MediaClock, MediaTime};

const FILTER_TAU: MediaTime = MediaTime::from_millis(400);

fn clock(master_filter: Option<MediaTime>) -> MediaClock {
    let mut c = MediaClock::new(
        ClockConfig {
            master_filter,
            ..ClockConfig::default()
        },
        MediaTime::ZERO,
        MediaTime::ZERO,
        Generation(0),
    );
    c.set_playing(MediaTime::ZERO, true);
    c.set_master(MediaTime::ZERO, Master::Audio);
    c
}

/// One delivery of PCM to the consumer: wall time and frames pulled.
struct Delivery {
    wall_us: i64,
    frames: i64,
}

/// The Quest Pro capture pattern (rtspt stereo lane, tenth session):
/// 512-sample buffers at 24 kHz (21.3 ms nominal). Jitter comes in
/// multi-second episodes — callbacks alternating ~14/28 ms
/// (double-buffer bursts) and every 15th slot missed outright (a ~42 ms
/// gap, ~320 ms period) with the next pull catching up — separated by
/// quiet phases of uniform cadence. The episode boundaries are where the
/// raw ladder moves the clock.
fn quest_delivery_schedule(duration_us: i64) -> Vec<Delivery> {
    const SLOT_US: i64 = 21_333;
    const JITTER_US: i64 = 7_000;
    const PHASE_US: i64 = 3_000_000;
    let mut deliveries = Vec::new();
    let mut carried = 0i64;
    let mut slot = 0i64;
    loop {
        let nominal = slot * SLOT_US;
        if nominal > duration_us {
            return deliveries;
        }
        // Quiet phase / episode phase, alternating every 3 s.
        let episode = (nominal / PHASE_US) % 2 == 1;
        let jitter = match (episode, slot % 2 == 0) {
            (false, _) => 0,
            (true, true) => -JITTER_US,
            (true, false) => JITTER_US,
        };
        if episode && slot % 15 == 14 {
            // Missed slot: its samples arrive with the next pull.
            carried += 512;
        } else {
            deliveries.push(Delivery {
                wall_us: (nominal + jitter).max(0),
                frames: 512 + carried,
            });
            carried = 0;
        }
        slot += 1;
    }
}

/// The engine's measured playhead over that schedule: consumed frames
/// plus wall-since-last-pull extrapolation capped at 40 ms (the
/// `AudioShared::playhead` model). Piecewise constant error against true
/// time, alternating ~±7 ms around one buffer of standing offset.
fn measured_playhead(deliveries: &[Delivery], wall_us: i64) -> Option<i64> {
    const RATE: i64 = 24_000;
    let mut consumed = 0i64;
    let mut last_pull = None;
    for d in deliveries {
        if d.wall_us > wall_us {
            break;
        }
        consumed += d.frames;
        last_pull = Some(d.wall_us);
    }
    let last_pull = last_pull?;
    let since = (wall_us - last_pull).clamp(0, 40_000);
    Some(consumed * 1_000_000 / RATE + since)
}

/// Run the capture pattern for 15 s, observing every 4 ms (the audio
/// thread's cadence). Returns the clock's post-settle wander — how far
/// `now(wall) - wall` moved over the measured window, i.e. how much the
/// jitter dragged frame due-times — plus the snap count.
fn run_quest_pattern(c: &mut MediaClock, settle_us: i64) -> (i64, usize) {
    let deliveries = quest_delivery_schedule(15_000_000);
    let mut snaps = 0;
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    let mut wall_us = 0i64;
    while wall_us <= 15_000_000 {
        if let Some(playhead) = measured_playhead(&deliveries, wall_us) {
            let correction = c.observe_master(
                MediaTime::from_micros(wall_us),
                MediaTime::from_micros(playhead),
            );
            if wall_us >= settle_us {
                if matches!(correction, Correction::Snap { .. }) {
                    snaps += 1;
                }
                let err = (c.now(MediaTime::from_micros(wall_us))
                    - MediaTime::from_micros(wall_us))
                .as_micros();
                lo = lo.min(err);
                hi = hi.max(err);
            }
        }
        wall_us += 4_000;
    }
    (hi - lo, snaps)
}

/// Filtered ladder: once converged onto the playhead's standing offset,
/// episode onsets and callback jitter move the clock — and with it every
/// frame's due time — by only a couple of ms across the whole run (the
/// band-edge walk at the first episode onset), safely under half a 72 Hz
/// vsync (6.9 ms): presentation holds the ideal cadence through the
/// episodes. The raw ladder walks the full alternation
/// amplitude (~7 ms) on the same trace.
#[test]
fn filter_holds_due_times_through_callback_jitter() {
    let mut c = clock(Some(FILTER_TAU));
    let (wander, snaps) = run_quest_pattern(&mut c, 2_500_000);
    assert_eq!(snaps, 0, "jitter must never snap");
    assert!(
        wander < 3_000,
        "filtered clock wandered {wander} µs against 1x"
    );
}

/// The same trace on the raw ladder: each episode onset swings one side
/// of the alternation past the dead band, and the resulting slew walks
/// the clock by most of the jitter amplitude — frame due-times slide
/// across vsync boundaries, which is the judder the capture measured.
/// Pinned so the A/B stays visible.
#[test]
fn raw_ladder_wanders_on_the_same_trace() {
    let mut c = clock(None);
    let (wander, snaps) = run_quest_pattern(&mut c, 2_500_000);
    assert_eq!(snaps, 0);
    assert!(
        wander > 4_000,
        "expected the raw ladder to wander with the episodes, saw {wander} µs"
    );
}

/// A genuine standing offset still converges through the filter: slew
/// engages, closes to the dead band, and stays quiet — no flapping at
/// the band edge on a clean-cadence master.
#[test]
fn filter_converges_on_genuine_offset() {
    let mut c = clock(Some(FILTER_TAU));
    let offset = MediaTime::from_millis(300);
    let mut wall = MediaTime::ZERO;
    let mut converged_at = None;
    for _ in 0..400 {
        let correction = c.observe_master(wall, wall + offset);
        assert!(!matches!(correction, Correction::Snap { .. }));
        if converged_at.is_none()
            && matches!(correction, Correction::None)
            && wall > MediaTime::ZERO
        {
            converged_at = Some(wall);
        }
        wall += MediaTime::from_millis(100);
    }
    let converged_at = converged_at.expect("never converged");
    // At a 2% slew, 300 ms of error closes in 15 s; the filter's lag adds
    // well under a second.
    assert!(converged_at < MediaTime::from_secs(18));
    // Once inside the band it stays there.
    for _ in 0..50 {
        wall += MediaTime::from_millis(100);
        let correction = c.observe_master(wall, wall + offset);
        assert!(
            matches!(correction, Correction::None),
            "left the dead band after converging: {correction:?}"
        );
    }
}

/// The snap rung acts on the raw error: a real discontinuity must never
/// be averaged away by a settled filter.
#[test]
fn snap_acts_on_the_raw_error() {
    let mut c = clock(Some(FILTER_TAU));
    let mut wall = MediaTime::ZERO;
    // Settle the filter at zero error.
    for _ in 0..100 {
        c.observe_master(wall, wall);
        wall += MediaTime::from_millis(10);
    }
    let target = wall + MediaTime::from_secs(2);
    let correction = c.observe_master(wall, target);
    assert!(matches!(correction, Correction::Snap { .. }));
    assert_eq!(c.now(wall), target);
}

/// A generation change (seek) clears the filter: the first observation on
/// the new timeline seeds fresh instead of blending with the old one.
#[test]
fn generation_change_resets_the_filter() {
    let mut c = clock(Some(FILTER_TAU));
    let mut wall = MediaTime::ZERO;
    // Settle the filter onto a large (sub-snap) standing error.
    for _ in 0..200 {
        c.observe_master(wall, wall + MediaTime::from_millis(600));
        wall += MediaTime::from_millis(10);
    }
    c.advance_generation(wall, Generation(1), MediaTime::from_secs(60));
    // On the fresh timeline the master sits exactly on the clock: a stale
    // filtered estimate would report a phantom error and slew.
    for _ in 0..10 {
        wall += MediaTime::from_millis(10);
        let now = c.now(wall);
        let correction = c.observe_master(wall, now);
        assert!(
            matches!(correction, Correction::None),
            "stale filter state survived the generation change: {correction:?}"
        );
    }
    assert_eq!(c.rate_ppm(), 0);
}
