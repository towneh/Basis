//! The live priming join: during the startup hold, release runs ahead of
//! 1x into the decoder — bounded by `startup_burst` beyond the 1x line
//! from the first arrival — so the decoder's first-output input depth
//! accumulates while the hold fills. Presentation stays gated behind the
//! hold (the engine's job, driven by the `holding` metric), and the 1x
//! schedule anchors presentation-relative when the engine signals the
//! first presentation. A zero burst restores the strict hold-then-1x
//! startup, pinned by tests/properties.rs.

use media_bank::{Bank, BankConfig, BufferDepth, Liveness, PushOutcome};
use media_clock::{Generation, MediaTime};
use media_demux::{Au, StreamEvent, TrackId};

const INTERVAL_US: i64 = 33_000;

fn au(dts_us: i64) -> StreamEvent {
    StreamEvent::Au(Au {
        track: TrackId(0),
        data: vec![0u8; 100],
        pts: MediaTime::from_micros(dts_us),
        dts: MediaTime::from_micros(dts_us),
        key: false,
        generation: Generation(0),
    })
}

fn cfg(depth_ms: u32, burst_ms: i64) -> BankConfig {
    BankConfig {
        depth: BufferDepth::Millis(depth_ms),
        liveness: Liveness::Live,
        startup_burst: MediaTime::from_millis(burst_ms),
        ..BankConfig::default()
    }
}

fn push_ok(bank: &mut Bank, wall: MediaTime, dts_us: i64) {
    assert!(matches!(bank.push(wall, au(dts_us)), PushOutcome::Accepted));
}

fn pop_all(bank: &mut Bank, wall: MediaTime) -> usize {
    let mut n = 0;
    while let Some(ev) = bank.pop_due(wall) {
        if matches!(ev, StreamEvent::Au(_)) {
            n += 1;
        }
    }
    n
}

/// 1x arrivals release on arrival during the hold (the priming overlap),
/// and the hold lifts at the full configured depth — presentation starts
/// at hold-lift on a priming join, so the arrived span is the viewer's
/// protection and must be the whole depth, cushion included.
#[test]
fn priming_releases_on_arrival_and_holds_to_full_depth() {
    let depth_ms = 3000u32;
    let mut bank = Bank::new(cfg(depth_ms, 2000), Generation(0)).unwrap();
    let mut released = 0usize;
    let mut pushed = 0usize;
    for i in 0..120i64 {
        let wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pushed += 1;
        released += pop_all(&mut bank, wall);
        let m = bank.metrics();
        let arrived_ms = i * INTERVAL_US / 1000;
        if arrived_ms < depth_ms as i64 {
            assert!(m.holding, "hold lifted early at {arrived_ms}ms arrived");
            assert!(!bank.awaiting_presentation());
        } else {
            assert!(!m.holding, "hold still set at {arrived_ms}ms arrived");
            assert!(bank.awaiting_presentation());
        }
    }
    // Priming released everything as it arrived (1x sits far inside the
    // burst allowance).
    assert_eq!(released, pushed);
}

/// A backlog burst (the HLS live join shape) releases only `startup_burst`
/// ahead of the 1x line from the first arrival — the cap is a moving line,
/// so release never wedges on a stream whose priming needs outlast the
/// burst, and a huge backlog cannot flood the decode channels.
#[test]
fn priming_cap_is_a_moving_one_x_line() {
    let mut bank = Bank::new(cfg(3000, 1000), Generation(0)).unwrap();
    // 5 s of backlog arrives instantly.
    for i in 0..150i64 {
        push_ok(&mut bank, MediaTime::ZERO, i * INTERVAL_US);
    }
    // Backlog beyond the hold target: the hold lifts immediately.
    assert!(!bank.metrics().holding);
    assert!(bank.awaiting_presentation());

    let at_zero = pop_all(&mut bank, MediaTime::ZERO);
    let released_ms = at_zero as i64 * INTERVAL_US / 1000;
    assert!(
        (900..=1100).contains(&released_ms),
        "released {released_ms}ms at wall 0, wanted ~the 1000ms burst"
    );

    // Half a second later the line has moved half a second.
    let later = MediaTime::from_millis(500);
    let more_ms = pop_all(&mut bank, later) as i64 * INTERVAL_US / 1000;
    assert!(
        (400..=600).contains(&more_ms),
        "released {more_ms}ms more by wall 500ms, wanted ~500ms"
    );
}

/// The presentation signal fixes the schedule presentation-relative at
/// the whole released span, so release carries straight on at 1x. The
/// released-ahead media is in-flight depth downstream, so the bank's own
/// lag is what is left un-released — near zero on a join that released
/// everything it took. Crediting only the cushion would defer the
/// schedule by the difference, and since one anchor governs both tracks
/// that pause takes the audio ring down with the pool.
#[test]
fn presentation_anchor_continues_the_schedule_without_a_pause() {
    let config = cfg(3000, 2000);
    let lead = config.pace_lead_with_lag;
    let mut bank = Bank::new(config, Generation(0)).unwrap();

    // 1x arrivals to the full depth; priming releases them all.
    let mut wall = MediaTime::ZERO;
    let mut i = 0i64;
    while !bank.awaiting_presentation() {
        wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pop_all(&mut bank, wall);
        i += 1;
        assert!(i < 200, "hold never lifted");
    }
    // Everything that arrived was released: well past the 500ms cushion,
    // which is the case that used to stall.
    let primed = wall;
    assert!(
        primed > MediaTime::from_millis(1500),
        "primed only {primed}"
    );

    let present_at = wall + MediaTime::from_millis(50);
    bank.presentation_started(present_at);
    let quantum = MediaTime::from_micros(INTERVAL_US);
    assert!(
        bank.metrics().lag <= quantum,
        "lag {} after the anchor, wanted ~0: released media is downstream depth",
        bank.metrics().lag
    );

    // The arrival that follows the join is due at once: the schedule is
    // where release left it, not a cushion behind it.
    let next_dts = i * INTERVAL_US;
    push_ok(&mut bank, present_at, next_dts);
    assert_eq!(
        pop_all(&mut bank, present_at),
        1,
        "the schedule paused at the join"
    );

    // And it is still 1x, not a licence to run away: media half a second
    // ahead of the line waits for the line.
    let ahead = next_dts + 500_000;
    push_ok(&mut bank, present_at, ahead);
    assert_eq!(pop_all(&mut bank, present_at), 0);
    let due = present_at + MediaTime::from_millis(500) - lead;
    assert_eq!(pop_all(&mut bank, due - MediaTime::from_millis(40)), 0);
    assert_eq!(pop_all(&mut bank, due + quantum), 1);
}

/// A target-zero lane (the §6.14 shallow posture: depth = cushion) never
/// holds — the gate opens on the first arrival and release tracks the
/// edge, so the sub-second join is untouched.
#[test]
fn target_zero_lifts_immediately_and_releases_at_the_edge() {
    let mut bank = Bank::new(cfg(500, 2000), Generation(0)).unwrap();
    push_ok(&mut bank, MediaTime::ZERO, 0);
    assert!(!bank.metrics().holding);
    assert!(bank.awaiting_presentation());
    assert_eq!(pop_all(&mut bank, MediaTime::ZERO), 1);
    // Still releasing on arrival while presentation warms up.
    let wall = MediaTime::from_micros(INTERVAL_US);
    push_ok(&mut bank, wall, INTERVAL_US);
    assert_eq!(pop_all(&mut bank, wall), 1);
}

/// The startup timeout lifts the hold on a starved join, opening the
/// presentation gate for whatever did arrive; the flip runs from
/// `next_due` too, so a stalled source cannot strand the gate.
#[test]
fn timeout_lifts_the_hold_on_a_starved_join() {
    let config = cfg(3000, 2000);
    let timeout = config.startup_timeout;
    let mut bank = Bank::new(config, Generation(0)).unwrap();
    // One second of arrivals, then the source stalls.
    for i in 0..30i64 {
        let wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pop_all(&mut bank, wall);
    }
    assert!(bank.metrics().holding);
    let _ = bank.next_due(timeout + MediaTime::from_millis(100));
    assert!(!bank.metrics().holding);
    assert!(bank.awaiting_presentation());
}

/// The debt bound still maintains the schedule after a priming join: a
/// delivery gap deeper than the cushion re-anchors and is accounted.
#[test]
fn priming_join_keeps_the_debt_bound() {
    let mut bank = Bank::new(cfg(1500, 2000), Generation(0)).unwrap();
    let mut i = 0i64;
    let mut wall = MediaTime::ZERO;
    while !bank.awaiting_presentation() {
        wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pop_all(&mut bank, wall);
        i += 1;
        assert!(i < 100, "hold never lifted");
    }
    bank.presentation_started(wall);
    // Drain the schedule for a while, then a 3 s delivery gap.
    let gap_start = wall + MediaTime::from_secs(2);
    let mut t = wall;
    while t < gap_start {
        t += MediaTime::from_millis(10);
        if t >= MediaTime::from_micros(i * INTERVAL_US) {
            push_ok(&mut bank, t, i * INTERVAL_US);
            i += 1;
        }
        pop_all(&mut bank, t);
    }
    let resume = gap_start + MediaTime::from_secs(3);
    let mut drained = t;
    while drained < resume {
        drained += MediaTime::from_millis(10);
        pop_all(&mut bank, drained);
    }
    // The burst behind the gap arrives at once.
    while MediaTime::from_micros(i * INTERVAL_US) <= resume {
        push_ok(&mut bank, resume, i * INTERVAL_US);
        i += 1;
    }
    let m = bank.metrics();
    assert!(m.reanchors > 0, "no re-anchor across a 3s gap");
    assert!(m.stall_total > MediaTime::ZERO);
}

/// Auto lanes hold to the estimator's lag only — the cold seed makes
/// target_lag a hair above zero, and a full-depth hold would tax every
/// Auto live join by the cushion. The hold must lift within the first
/// few arrivals.
#[test]
fn auto_cold_join_lifts_within_the_seed_lag() {
    let config = BankConfig {
        depth: BufferDepth::Auto,
        liveness: Liveness::Live,
        startup_burst: MediaTime::from_millis(2000),
        ..BankConfig::default()
    };
    let mut bank = Bank::new(config, Generation(0)).unwrap();
    for i in 0..3i64 {
        let wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pop_all(&mut bank, wall);
    }
    assert!(
        bank.awaiting_presentation(),
        "Auto cold join still holding after 66ms arrived"
    );
}

/// The presentation signal is a no-op outside a priming join.
#[test]
fn presentation_signal_is_inert_on_the_strict_path() {
    let mut bank = Bank::new(cfg(1500, 0), Generation(0)).unwrap();
    for i in 0..40i64 {
        let wall = MediaTime::from_micros(i * INTERVAL_US);
        push_ok(&mut bank, wall, i * INTERVAL_US);
        pop_all(&mut bank, wall);
        assert!(!bank.awaiting_presentation());
    }
    let before = bank.metrics();
    bank.presentation_started(MediaTime::from_secs(2));
    let after = bank.metrics();
    assert_eq!(before.lag, after.lag);
    assert_eq!(before.reanchors, after.reanchors);
}
