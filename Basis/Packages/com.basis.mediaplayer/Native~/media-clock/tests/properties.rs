//! Property tests for the drift ladder: schedule in → position out.

use media_clock::{ClockConfig, Correction, Generation, Master, MediaClock, MediaTime};
use proptest::prelude::*;

fn clock_at_zero() -> MediaClock {
    let mut c = MediaClock::new(
        ClockConfig::default(),
        MediaTime::ZERO,
        MediaTime::ZERO,
        Generation(0),
    );
    c.set_playing(MediaTime::ZERO, true);
    c.set_master(MediaTime::ZERO, Master::Audio);
    c
}

/// An observation schedule: (wall step µs, master error µs) pairs. Errors are
/// kept inside the snap threshold so the run stays in the slew regime.
fn slew_regime_schedule() -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((1_000i64..2_000_000, -699_000i64..699_000), 1..64)
}

proptest! {
    /// In the slew regime (no snaps), position never runs backwards.
    #[test]
    fn monotone_under_slew(schedule in slew_regime_schedule()) {
        let mut c = clock_at_zero();
        let mut wall = MediaTime::ZERO;
        let mut last = c.now(wall);
        for (step, err) in schedule {
            // Observe a master sitting `err` away from the current position.
            let target = c.now(wall) + MediaTime::from_micros(err);
            let corr = c.observe_master(wall, target);
            let snapped = matches!(corr, Correction::Snap { .. });
            prop_assert!(!snapped);
            wall += MediaTime::from_micros(step);
            let now = c.now(wall);
            prop_assert!(now >= last, "position went backwards: {last} -> {now}");
            last = now;
        }
    }

    /// The instantaneous rate stays within the 2% cap between observations.
    #[test]
    fn rate_bounded(schedule in slew_regime_schedule()) {
        let cap_ppm = ClockConfig::default().slew_cap_ppm;
        let mut c = clock_at_zero();
        let mut wall = MediaTime::ZERO;
        for (step, err) in schedule {
            let target = c.now(wall) + MediaTime::from_micros(err);
            c.observe_master(wall, target);
            let before = c.now(wall);
            wall += MediaTime::from_micros(step);
            let after = c.now(wall);
            let advance = (after - before).as_micros();
            let lo = step + step * -cap_ppm / 1_000_000 - 1;
            let hi = step + step * cap_ppm / 1_000_000 + 1;
            prop_assert!(advance >= lo && advance <= hi,
                "advance {advance} outside [{lo}, {hi}] for step {step}");
        }
    }

    /// A steady 1x master with a fixed offset inside the slew band is
    /// converged on: error falls to the dead band and stays there.
    #[test]
    fn converges_on_steady_master(offset_us in 21_000i64..699_000, sign in prop::bool::ANY) {
        let offset = if sign { offset_us } else { -offset_us };
        let cfg = ClockConfig::default();
        let mut c = clock_at_zero();
        // Master runs at exactly 1x, `offset` ahead of (or behind) the clock.
        let master = |wall: MediaTime| wall + MediaTime::from_micros(offset);
        let mut wall = MediaTime::ZERO;
        // At a 2% slew, 699 ms of error needs < 35 s; observe every 100 ms.
        let budget_steps = 400;
        let mut converged_at = None;
        for i in 0..budget_steps {
            let corr = c.observe_master(wall, master(wall));
            if matches!(corr, Correction::None) && i > 0 {
                converged_at = Some(wall);
                break;
            }
            let snapped = matches!(corr, Correction::Snap { .. });
            prop_assert!(!snapped);
            wall += MediaTime::from_millis(100);
        }
        let converged_at = converged_at.expect("never converged");
        // Once converged, it stays converged (no oscillation out of the band).
        for _ in 0..50 {
            wall += MediaTime::from_millis(100);
            let corr = c.observe_master(wall, master(wall));
            prop_assert!(matches!(corr, Correction::None),
                "left the dead band after converging at {converged_at}: {corr:?}");
            let err = (master(wall) - c.now(wall)).abs();
            prop_assert!(err <= cfg.dead_band);
        }
    }

    /// Beyond the snap threshold the clock jumps exactly to the master.
    #[test]
    fn snaps_beyond_threshold(err_us in 700_000i64..30_000_000, sign in prop::bool::ANY) {
        let err = if sign { err_us } else { -err_us };
        let mut c = clock_at_zero();
        let wall = MediaTime::from_secs(5);
        let target = c.now(wall) + MediaTime::from_micros(err);
        let corr = c.observe_master(wall, target);
        let snapped = matches!(corr, Correction::Snap { .. });
        prop_assert!(snapped);
        prop_assert_eq!(c.now(wall), target);
        prop_assert_eq!(c.rate_ppm(), 0);
    }

    /// Pause freezes position; resume continues from the same place.
    #[test]
    fn pause_freezes_position(pause_at in 0i64..10_000_000, pause_for in 0i64..60_000_000) {
        let mut c = clock_at_zero();
        let pause_wall = MediaTime::from_micros(pause_at);
        let frozen = c.now(pause_wall);
        c.set_playing(pause_wall, false);
        let mid = pause_wall + MediaTime::from_micros(pause_for / 2);
        let resume_wall = pause_wall + MediaTime::from_micros(pause_for);
        prop_assert_eq!(c.now(mid), frozen);
        c.set_playing(resume_wall, true);
        prop_assert_eq!(c.now(resume_wall), frozen);
    }

    /// A generation change snaps regardless of error size.
    #[test]
    fn generation_change_snaps(err_us in -100_000i64..100_000) {
        let mut c = clock_at_zero();
        let wall = MediaTime::from_secs(1);
        let target = c.now(wall) + MediaTime::from_micros(err_us);
        let corr = c.advance_generation(wall, Generation(1), target);
        let snapped = matches!(corr, Correction::Snap { .. });
        prop_assert!(snapped);
        prop_assert_eq!(c.now(wall), target);
        prop_assert_eq!(c.generation(), Generation(1));
    }
}

#[test]
fn wall_master_ignores_observations() {
    let mut c = MediaClock::new(
        ClockConfig::default(),
        MediaTime::ZERO,
        MediaTime::ZERO,
        Generation(0),
    );
    c.set_playing(MediaTime::ZERO, true);
    assert_eq!(c.master(), Master::Wall);
    let wall = MediaTime::from_secs(1);
    let corr = c.observe_master(wall, MediaTime::from_secs(30));
    assert_eq!(corr, Correction::None);
    assert_eq!(c.now(wall), MediaTime::from_secs(1));
}

#[test]
fn master_switch_does_not_move_position() {
    let mut c = clock_at_zero();
    let wall = MediaTime::from_secs(2);
    // Get a slew running, then switch to wall pacing mid-slew.
    c.observe_master(wall, c.now(wall) + MediaTime::from_millis(100));
    let wall2 = wall + MediaTime::from_millis(500);
    let before = c.now(wall2);
    c.set_master(wall2, Master::Wall);
    assert_eq!(c.now(wall2), before);
    assert_eq!(c.rate_ppm(), 0);
}

/// §8.4's wall-master rung: `slew_wall` never moves `now` at the call
/// instant, runs at 1x + ppm afterwards, clamps to the cap, and is inert
/// under the audio master (there the correction rides the playhead).
#[test]
fn slew_wall_shifts_rate_without_moving_now() {
    let mut c = clock_at_zero();
    c.set_master(MediaTime::ZERO, Master::Wall);
    let t1 = MediaTime::from_millis(1_000);
    let before = c.now(t1);
    c.slew_wall(t1, 20_000);
    assert_eq!(c.now(t1), before, "slew_wall must not move now");
    let t2 = t1 + MediaTime::from_secs(1);
    assert_eq!(
        c.now(t2) - before,
        MediaTime::from_secs(1) + MediaTime::from_millis(20),
        "1 s at +2% is 1.020 s"
    );
    c.slew_wall(t2, 0);
    let settled = c.now(t2);
    assert_eq!(
        c.now(t2 + MediaTime::from_secs(1)) - settled,
        MediaTime::from_secs(1)
    );
}

#[test]
fn slew_wall_clamps_to_the_cap_and_ignores_audio_master() {
    let mut c = clock_at_zero();
    c.set_master(MediaTime::ZERO, Master::Wall);
    c.slew_wall(MediaTime::ZERO, 1_000_000);
    assert_eq!(c.rate_ppm(), 20_000, "clamped to the slew cap");
    c.slew_wall(MediaTime::ZERO, -1_000_000);
    assert_eq!(c.rate_ppm(), -20_000);

    let mut audio = clock_at_zero();
    audio.slew_wall(MediaTime::ZERO, 20_000);
    assert_eq!(audio.rate_ppm(), 0, "audio master ignores slew_wall");
}
