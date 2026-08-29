//! Session lifecycle over the real pipeline (Windows: MF decode): state
//! machine, pause freezing the position, keyframe-clean seek, natural end.

#![cfg(windows)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use media_clock::MediaTime;
use media_engine::{OpenRequest, Session, State};

fn fixture_path() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4")
        .to_string_lossy()
        .into_owned()
}

fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Audio-only: the session plays without a video track, and Ended waits
/// for the ring's tail to be consumed instead of firing at demux EOS —
/// the pulled total must cover (nearly) the whole fixture.
#[test]
fn audio_only_plays_out_the_tail() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/aac-48k-stereo.m4a")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    // Pull like the Unity audio thread: at the hardware cadence.
    let mut pulled = 0u64;
    let mut buf = vec![0f32; 2048];
    let mut epoch: Option<Instant> = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let state = shared.state.load(Ordering::Relaxed);
        assert_ne!(
            state,
            State::Error as u32,
            "error {}",
            shared.last_error.load(Ordering::Relaxed)
        );
        if state == State::Ended as u32 {
            break;
        }
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
        if rate > 0 && state == State::Playing as u32 {
            let at = *epoch.get_or_insert_with(Instant::now);
            let budget = at.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - pulled;
            if budget as usize >= buf.len() / channels as usize {
                pulled += Session::read_audio(&px, &mut buf) as u64;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(
        shared.state.load(Ordering::Relaxed),
        State::Ended as u32,
        "audio-only session must end naturally"
    );
    // The fixture is ~6 s at 48 kHz; the tail must not be cut at EOS
    // (the old behaviour lost up to the ring depth, 2 s).
    assert!(
        pulled >= 5 * 48_000,
        "pulled only {pulled} frames — the ring tail was cut"
    );
    assert!(
        shared.position_us.load(Ordering::Relaxed) > 4_000_000,
        "position must be clock-derived for audio-only sessions"
    );
    session.close();
}

/// The A/V twin of `audio_only_plays_out_the_tail`. A session carrying both
/// kinds of track used to declare Ended the moment the last *picture* was
/// presented, with no regard for what the audio ring still held, and
/// `read_audio` serves nothing outside Playing — so whatever had not been
/// pulled by then was simply unreachable.
///
/// Asserted as an invariant rather than a frame total, because a total
/// cannot separate this from the serve-side lateness trim, which discards
/// late audio deliberately and would be blamed for the same shortfall. At
/// Ended every frame pushed into the ring must have been accounted for:
/// handed to the consumer, or trimmed. Anything else was cut.
#[test]
fn an_av_session_plays_out_the_audio_tail() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut pulled = 0u64;
    let mut buf = vec![0f32; 2048];
    let mut epoch: Option<Instant> = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let state = shared.state.load(Ordering::Relaxed);
        assert_ne!(
            state,
            State::Error as u32,
            "error {}",
            shared.last_error.load(Ordering::Relaxed)
        );
        if state == State::Ended as u32 {
            break;
        }
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
        if rate > 0 && state == State::Playing as u32 {
            let at = *epoch.get_or_insert_with(Instant::now);
            let budget = at.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - pulled;
            if budget as usize >= buf.len() / channels as usize {
                pulled += Session::read_audio(&px, &mut buf) as u64;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(
        shared.state.load(Ordering::Relaxed),
        State::Ended as u32,
        "an A/V session must still end naturally"
    );
    let pushed = px.audio_shared.pushed_frames.load(Ordering::Relaxed);
    let consumed = px.audio_shared.consumed_frames.load(Ordering::Relaxed);
    let trimmed = px.audio_shared.trimmed_frames.load(Ordering::Relaxed);
    assert!(pushed > 0, "the fixture has audio");
    assert_eq!(
        pushed,
        consumed + trimmed,
        "Ended left {} frames in the ring — the tail was cut when the picture ran out          (pushed {pushed}, consumed {consumed}, trimmed {trimmed})",
        pushed - consumed - trimmed
    );
    session.close();
}

/// A seek issued near EOS, where the audio side has already announced that it
/// has nothing left to play out for the generation being left behind.
///
/// The two decode threads observe a seek independently, so the video thread can
/// process its Flush, run the short remainder of the new generation and reach
/// its end check while the audio thread is still on the old one. Reading a bare
/// "audio is done" flag there ends the session on the previous generation's
/// answer, cutting the new one off before it plays. The published value carries
/// the generation it belongs to for that reason.
///
/// This row guards the outcome rather than the race: the window is two threads
/// wide and cannot be scheduled on demand.
#[test]
fn a_seek_near_eos_does_not_end_the_new_generation_early() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut buf = vec![0f32; 2048];
    let pull = |pulled: &mut u64, buf: &mut [f32]| {
        if shared.audio_rate.load(Ordering::Relaxed) > 0
            && shared.state.load(Ordering::Relaxed) == State::Playing as u32
        {
            *pulled += Session::read_audio(&px, buf) as u64;
        }
    };

    // Up to the tail of the fixture, so the audio side has drained and said so.
    let mut before = 0u64;
    let reached = wait_for(Duration::from_secs(20), || {
        pull(&mut before, &mut buf);
        std::thread::sleep(Duration::from_millis(2));
        shared.position_us.load(Ordering::Relaxed) > 5_400_000
            || shared.state.load(Ordering::Relaxed) == State::Ended as u32
    });
    assert!(reached, "never reached the tail of the fixture");

    session.seek(MediaTime::from_millis(5_400));

    // The new generation has only ~0.6 s to play, which is the point: the
    // video thread finishes it almost at once, so if the end check took the
    // old generation's answer the session is Ended before any of it is heard.
    let mut after = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if shared.state.load(Ordering::Relaxed) == State::Ended as u32 {
            break;
        }
        pull(&mut after, &mut buf);
        std::thread::sleep(Duration::from_millis(2));
    }

    assert!(
        after >= 20_000,
        "pulled only {after} frames after the seek — the new generation was cut short"
    );
    session.close();
}

/// The PCM interleave for multichannel audio is WAV/channel-mask order —
/// FL FR C LFE BL BR — the order every decoder behind the engine emits
/// (Media Foundation's PCM convention here; the Android AAC decoder's FDK
/// default and FLAC's stored order elsewhere). The managed stereo downmix
/// keys its matrix on it. Pinned with a channel-marker fixture: one
/// distinct sine per speaker, identified per interleave slot.
#[test]
fn multichannel_interleave_is_wav_order() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/sine-48k-51.m4a")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut pulled: Vec<f32> = Vec::new();
    let mut buf = vec![0f32; 6 * 512];
    let mut epoch: Option<Instant> = None;
    let mut frames = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let state = shared.state.load(Ordering::Relaxed);
        assert_ne!(state, State::Error as u32);
        if state == State::Ended as u32 {
            break;
        }
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        if rate > 0 && state == State::Playing as u32 {
            let at = *epoch.get_or_insert_with(Instant::now);
            let budget = at.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - frames;
            if budget as usize >= 512 {
                let got = Session::read_audio(&px, &mut buf);
                frames += got as u64;
                pulled.extend_from_slice(&buf[..got * 6]);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    session.close();
    assert_eq!(
        shared.audio_channels.load(Ordering::Relaxed),
        6,
        "fixture must announce 5.1"
    );
    let total = pulled.len() / 6;
    assert!(total > 96_000, "pulled only {total} frames");

    // Goertzel power at each marker tone, one window mid-stream.
    let window = &pulled[6 * 48_000..6 * 96_000];
    let tones = [400.0f64, 800.0, 1200.0, 60.0, 1600.0, 2000.0];
    let goertzel = |slot: usize, freq: f64| -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / 48_000.0;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for frame in window.chunks_exact(6) {
            let s0 = f64::from(frame[slot]) + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    for (slot, _) in tones.iter().enumerate() {
        let powers: Vec<f64> = tones.iter().map(|&f| goertzel(slot, f)).collect();
        let best = powers
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(
            best, slot,
            "slot {slot} carries the tone for WAV-order channel {best}"
        );
    }
}

/// The ABI-facing latency setter clamps to the engine's sane range
/// (0..=500 ms) before the playhead subtracts it.
#[test]
fn audio_latency_setter_clamps_to_a_sane_range() {
    let mut session = Session::open(OpenRequest::new(fixture_path()));
    let px = session.pipeline().clone();

    Session::set_audio_latency(&px, 60_000);
    assert_eq!(
        px.audio_shared.output_latency_us.load(Ordering::Relaxed),
        60_000
    );
    Session::set_audio_latency(&px, -5);
    assert_eq!(px.audio_shared.output_latency_us.load(Ordering::Relaxed), 0);
    Session::set_audio_latency(&px, 10_000_000);
    assert_eq!(
        px.audio_shared.output_latency_us.load(Ordering::Relaxed),
        500_000
    );
    session.close();
}

#[test]
fn pause_seek_and_natural_end() {
    let mut session = Session::open(OpenRequest::new(fixture_path()));
    let shared = session.shared().clone();

    assert!(
        wait_for(Duration::from_secs(10), || {
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
        }),
        "never reached Playing (state {}, error {})",
        shared.state.load(Ordering::Relaxed),
        shared.last_error.load(Ordering::Relaxed),
    );

    // Pause freezes the position (the pacer clock is credited).
    assert!(wait_for(Duration::from_secs(5), || {
        shared.position_us.load(Ordering::Relaxed) > 200_000
    }));
    session.pause();
    assert_eq!(shared.state.load(Ordering::Relaxed), State::Paused as u32);
    let frozen = shared.position_us.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    let still = shared.position_us.load(Ordering::Relaxed);
    assert!(
        (still - frozen).abs() < 40_000,
        "position moved while paused: {frozen} -> {still}"
    );

    // Resume advances again.
    session.play();
    assert!(
        wait_for(Duration::from_secs(2), || {
            shared.position_us.load(Ordering::Relaxed) > still + 100_000
        }),
        "position did not advance after resume"
    );

    // Seek near the end lands keyframe-clean at/before the target and plays
    // through to the natural end.
    session.seek(MediaTime::from_millis(5500));
    assert!(
        wait_for(Duration::from_secs(5), || {
            let position = shared.position_us.load(Ordering::Relaxed);
            (3_900_000..=6_100_000).contains(&position)
                && shared.state.load(Ordering::Relaxed) == State::Playing as u32
        }),
        "seek did not settle (position {}, state {})",
        shared.position_us.load(Ordering::Relaxed),
        shared.state.load(Ordering::Relaxed),
    );
    assert!(
        wait_for(Duration::from_secs(8), || {
            shared.state.load(Ordering::Relaxed) == State::Ended as u32
        }),
        "never reached Ended (position {}, state {})",
        shared.position_us.load(Ordering::Relaxed),
        shared.state.load(Ordering::Relaxed),
    );

    session.close();
}

/// A seek after Ended revives the pipeline — the generation advance
/// rebuilds decode state and presentation resumes on the new timeline.
/// Runs on the progressive MP4 lane and the HLS-TS VOD lane (whose
/// demuxer latches an internal end state the seek must clear).
#[test]
fn seek_after_ended_revives_the_session() {
    for lane in [
        fixture_path(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/hls/ts/index.m3u8")
            .to_string_lossy()
            .into_owned(),
    ] {
        let mut session = Session::open(OpenRequest::new(lane.clone()));
        let shared = session.shared().clone();
        let diag = session.diag().clone();

        assert!(
            wait_for(Duration::from_secs(10), || {
                shared.state.load(Ordering::Relaxed) == State::Playing as u32
            }),
            "{lane}: never reached Playing"
        );
        // Jump near the end and let it finish.
        let origin = shared.position_us.load(Ordering::Relaxed);
        session.seek(MediaTime::from_micros(origin + 5_300_000));
        assert!(
            wait_for(Duration::from_secs(10), || {
                shared.state.load(Ordering::Relaxed) == State::Ended as u32
            }),
            "{lane}: never reached Ended (state {})",
            shared.state.load(Ordering::Relaxed)
        );

        let presented_at_end = diag.snapshot()[media_diag::Stage::Present as usize].out_count;
        session.seek(MediaTime::from_micros(origin + 1_000_000));
        assert!(
            wait_for(Duration::from_secs(10), || {
                shared.state.load(Ordering::Relaxed) == State::Playing as u32
                    && diag.snapshot()[media_diag::Stage::Present as usize].out_count
                        > presented_at_end
            }),
            "{lane}: seek after Ended did not revive (state {})",
            shared.state.load(Ordering::Relaxed)
        );
        // And it ends cleanly a second time.
        assert!(
            wait_for(Duration::from_secs(15), || {
                shared.state.load(Ordering::Relaxed) == State::Ended as u32
            }),
            "{lane}: revived session never ended again"
        );
        session.close();
    }
}

/// The ordering row: a seek issued while the video thread is inside
/// its EOS drain tail (the whole fixture released, banked at zero, the
/// pool still presenting the tail). The demux thread parks the clock and
/// advances the generation; until the video thread processes the Flush,
/// stale pre-seek frames still sit in the pool — restarting the parked
/// clock from one would resume the old timeline, race the state back to
/// Playing, let the audio ring free-run through the settle, and end in a
/// backwards snap to the audio master once the landed frames arrive
/// (observed as a ~5 s settle with the tail audio consumed on Quest,
/// where the OMX drain stretches the window; the ordering half
/// reproduces here). Clean shape: the clock stays parked until the new
/// generation's first frame, so the settle carries no master snap and
/// the tail plays out at 1x.
#[test]
fn seek_during_eos_drain_settles_without_a_snap() {
    let mut session = Session::open(OpenRequest::new(fixture_path()));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();
    let diag = session.diag().clone();

    // Pull audio like the Unity thread (budget at the stream rate) so the
    // audio master is live — the defect surfaced through it.
    let mut pulled = 0u64;
    let mut buf = vec![0f32; 2048];
    let mut epoch: Option<Instant> = None;
    let mut pump_until = |deadline: Duration, pred: &mut dyn FnMut() -> bool| -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            let state = shared.state.load(Ordering::Relaxed);
            assert_ne!(
                state,
                State::Error as u32,
                "error {}",
                shared.last_error.load(Ordering::Relaxed)
            );
            if state != State::Playing as u32 {
                // Budget restarts across settles so the catch-up after a
                // parked-clock window cannot race the playhead.
                epoch = None;
                pulled = 0;
            } else {
                let rate = shared.audio_rate.load(Ordering::Relaxed);
                let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
                if rate > 0 {
                    let at = *epoch.get_or_insert_with(Instant::now);
                    let budget =
                        at.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - pulled;
                    if budget as usize >= buf.len() / channels as usize {
                        pulled += Session::read_audio(&px, &mut buf) as u64;
                    }
                }
            }
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    };

    assert!(
        pump_until(Duration::from_secs(10), &mut || {
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
        }),
        "never reached Playing"
    );
    // The whole 6 s fixture is released well before it finishes playing
    // (VOD read-ahead + startup burst): banked zero with the position
    // mid-file means the Eos is through and the drain tail is presenting.
    assert!(
        pump_until(Duration::from_secs(10), &mut || {
            shared.banked_us.load(Ordering::Relaxed) == 0
                && shared.position_us.load(Ordering::Relaxed) > 3_500_000
        }),
        "drain-tail window never reached (banked {}, position {})",
        shared.banked_us.load(Ordering::Relaxed),
        shared.position_us.load(Ordering::Relaxed),
    );

    let _ = diag.take_events();
    session.seek(MediaTime::from_millis(2_500));
    assert!(
        pump_until(Duration::from_secs(3), &mut || {
            let position = shared.position_us.load(Ordering::Relaxed);
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
                && (1_800_000..=3_200_000).contains(&position)
        }),
        "seek did not settle (position {}, state {})",
        shared.position_us.load(Ordering::Relaxed),
        shared.state.load(Ordering::Relaxed),
    );
    let settled = Instant::now();
    assert!(
        pump_until(Duration::from_secs(10), &mut || {
            shared.state.load(Ordering::Relaxed) == State::Ended as u32
        }),
        "never reached Ended after the seek"
    );
    // ~4 s of tail from the 2 s keyframe: played at 1x, not rushed out
    // against a stale clock.
    assert!(
        settled.elapsed() >= Duration::from_secs(3),
        "tail rushed: Ended {}ms after settle",
        settled.elapsed().as_millis()
    );
    let snaps: Vec<_> = diag
        .take_events()
        .into_iter()
        .filter(|e| e.code == media_diag::EventCode::SnapCorrection)
        .collect();
    assert!(
        snaps.is_empty(),
        "master snap during the post-seek settle: {:?}",
        snaps.iter().map(|e| e.detail.as_str()).collect::<Vec<_>>()
    );
    session.close();
}

/// In-band CEA-608 (§6.12): the authored caption fixture's scripted cue
/// sequence surfaces through the caption lane — text (including special +
/// extended characters and the two-row roll-up), clears, and 2 s spacing
/// keyed to the video PTS. The script is tools/gen-caption-fixture.py's.
#[test]
fn caption_lane_delivers_the_scripted_cues() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-608-640x360-30fps.ts")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut cues = Vec::new();
    assert!(
        wait_for(Duration::from_secs(20), || {
            cues.extend(Session::drain_captions(&px, 16));
            let state = shared.state.load(Ordering::Relaxed);
            assert_ne!(
                state,
                State::Error as u32,
                "error {}",
                shared.last_error.load(Ordering::Relaxed)
            );
            state == State::Ended as u32
        }),
        "caption session never ended"
    );
    cues.extend(Session::drain_captions(&px, 16));

    let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "HELLO WORLD",
            "CAFÉ MAÑANA",
            "",
            "ROLL UP",
            "ROLL UP\nSECOND",
            "",
        ],
        "cue sequence mismatch"
    );
    // The script spaces cues on the video timeline: 0/2/4/6/7/8 s from the
    // first frame, whatever base the mux added.
    let origin = cues[0].pts_us;
    let offsets: Vec<i64> = cues.iter().map(|c| (c.pts_us - origin) / 1000).collect();
    assert_eq!(offsets, vec![0, 2000, 4000, 6000, 7000, 8000]);
    session.close();
}

/// `diag_csv` writes the capture-recorder CSV on close: header row per the
/// pinned column contract plus at least one 100 ms sample per second of
/// playback (the engine-owned sampler behind the managed ABI).
#[test]
fn diag_csv_written_on_close() {
    let dir = std::env::temp_dir().join(format!("bm-diag-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("capture.csv");

    let mut request = OpenRequest::new(fixture_path());
    request.diag_csv = Some(path.clone());
    let mut session = Session::open(request);
    let shared = session.shared().clone();
    assert!(
        wait_for(Duration::from_secs(10), || {
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
        }),
        "never reached Playing"
    );
    std::thread::sleep(Duration::from_secs(1));
    session.close();

    let csv = std::fs::read_to_string(&path).expect("csv written on close");
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().map(|h| h.to_owned()),
        Some(media_diag::CaptureRecorder::header()),
        "header is the pinned column contract"
    );
    assert!(lines.count() >= 10, "expected >=10 samples over >=1s");
    let _ = std::fs::remove_dir_all(&dir);
}

/// §8.4's ladder over a playing A/V session (audio master): a target
/// inside the dead band asks for nothing, a target ahead engages the
/// +2% slew (surfaced for the managed audio pull), and a target past the
/// seek threshold seeks — the last rung, never the first.
#[test]
fn sync_target_ladder_slew_then_seek() {
    let mut session = Session::open(OpenRequest::new(fixture_path()));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut buf = vec![0f32; 2048];
    let mut pulled = 0u64;
    let mut epoch: Option<Instant> = None;
    let mut pull = |px: &std::sync::Arc<media_engine::PipelineShared>| {
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
        if rate > 0 && shared.state.load(Ordering::Relaxed) == State::Playing as u32 {
            let at = *epoch.get_or_insert_with(Instant::now);
            let budget = at.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - pulled;
            if budget as usize >= buf.len() / channels as usize {
                pulled += Session::read_audio(px, &mut buf) as u64;
            }
        }
    };

    assert!(
        wait_for(Duration::from_secs(10), || {
            pull(&px);
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
                && shared.position_us.load(Ordering::Relaxed) > 200_000
        }),
        "never started playing"
    );

    // Dead band: the current position is (near) the target — no action.
    let position = shared.position_us.load(Ordering::Relaxed);
    Session::set_sync_target(&px, position);
    assert_eq!(px.sync_rate_ppm.load(Ordering::Relaxed), 0);

    // Slew band: 1 s ahead wants +2%, and the wanted rate is visible to
    // the audio consumer contract.
    let position = shared.position_us.load(Ordering::Relaxed);
    Session::set_sync_target(&px, position + 1_000_000);
    assert_eq!(px.sync_rate_ppm.load(Ordering::Relaxed), 20_000);

    // Behind by 1 s wants -2%. Needs a second of track behind the
    // playhead first — a negative target is the clear sentinel.
    assert!(
        wait_for(Duration::from_secs(5), || {
            pull(&px);
            shared.position_us.load(Ordering::Relaxed) > 1_300_000
        }),
        "position never reached 1.3 s"
    );
    let position = shared.position_us.load(Ordering::Relaxed);
    Session::set_sync_target(&px, position - 1_000_000);
    assert_eq!(px.sync_rate_ppm.load(Ordering::Relaxed), -20_000);

    // Clearing releases the correction.
    Session::set_sync_target(&px, -1);
    assert_eq!(px.sync_rate_ppm.load(Ordering::Relaxed), 0);

    // Seek rung: 3 s ahead is past the threshold; the session re-buffers
    // and settles near the target on the fixture's keyframe grid.
    let position = shared.position_us.load(Ordering::Relaxed);
    let target = position + 3_000_000;
    Session::set_sync_target(&px, target);
    assert_eq!(
        px.sync_rate_ppm.load(Ordering::Relaxed),
        0,
        "seek clears the slew"
    );
    assert!(
        wait_for(Duration::from_secs(10), || {
            pull(&px);
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
                && (shared.position_us.load(Ordering::Relaxed) - target).abs() < 2_500_000
        }),
        "never settled near the sync-seek target (position {} vs target {})",
        shared.position_us.load(Ordering::Relaxed),
        target
    );
    session.close();
}

/// Wall-master lanes (no audio track) apply the sync slew to the clock
/// directly: the engine has no audio consumer to ride, so the correction
/// is engine-side and `sync_rate_ppm` mirrors it.
#[test]
fn sync_target_slews_the_wall_clock_on_video_only() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-640x360-30fps.mp4")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    assert!(
        wait_for(Duration::from_secs(10), || {
            shared.state.load(Ordering::Relaxed) == State::Playing as u32
                && shared.position_us.load(Ordering::Relaxed) > 200_000
        }),
        "never started playing"
    );

    let position = shared.position_us.load(Ordering::Relaxed);
    Session::set_sync_target(&px, position + 1_000_000);
    assert_eq!(px.sync_rate_ppm.load(Ordering::Relaxed), 20_000);
    assert_eq!(
        px.clock.lock().unwrap().rate_ppm(),
        20_000,
        "wall-master slew is applied to the clock itself"
    );

    Session::set_sync_target(&px, -1);
    assert_eq!(px.clock.lock().unwrap().rate_ppm(), 0);
    session.close();
}

/// `diag_csv_append` keeps every session's capture in one file instead of the
/// last one only — the shape a player that goes dormant and wakes needs. The
/// header belongs to the file, not to each capture, so a second run adds rows
/// and nothing else: a header row in the middle would read as data to anything
/// consuming this format.
#[test]
fn diag_csv_appends_without_a_second_header() {
    let dir = std::env::temp_dir().join(format!("bm-diag-append-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("capture.csv");
    let _ = std::fs::remove_file(&path);

    let run = || {
        let mut request = OpenRequest::new(fixture_path());
        request.diag_csv = Some(path.clone());
        request.diag_csv_append = true;
        let mut session = Session::open(request);
        let shared = session.shared().clone();
        assert!(
            wait_for(Duration::from_secs(10), || {
                shared.state.load(Ordering::Relaxed) == State::Playing as u32
            }),
            "never reached Playing"
        );
        std::thread::sleep(Duration::from_millis(600));
        session.close();
        std::fs::read_to_string(&path).expect("csv written on close")
    };

    let first = run();
    let first_lines = first.lines().count();
    assert!(first_lines > 1, "first run wrote no rows");

    let second = run();
    let header = media_diag::CaptureRecorder::header();
    assert_eq!(
        second.lines().filter(|l| *l == header).count(),
        1,
        "the header belongs to the file, once"
    );
    assert!(
        second.lines().count() > first_lines,
        "the second run replaced the first instead of appending"
    );
    // The first run's rows survive verbatim.
    assert!(second.starts_with(&first), "earlier rows were rewritten");

    let _ = std::fs::remove_file(&path);
}

/// SEI user data: the authored fixture stamps one type-5 message into
/// every access unit (tools/gen-sei-userdata-fixture.py's layout), and the
/// lane hands each over with its UUID split off and its PTS, in order and
/// without loss. x264's own build-string message rides through on the
/// first AU under its own UUID, which is what the consumer-side UUID
/// filter exists for.
#[test]
fn user_data_lane_delivers_every_frames_message() {
    const FIXTURE_UUID: [u8; 16] = [
        0x7a, 0x1c, 0x3e, 0x5f, 0x9b, 0x2d, 0x4c, 0x6e, 0x8f, 0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f,
        0x60,
    ];
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-sei-userdata-640x360-30fps.ts")
        .to_string_lossy()
        .into_owned();
    let mut session = Session::open(OpenRequest::new(path));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();

    let mut messages = Vec::new();
    assert!(
        wait_for(Duration::from_secs(20), || {
            messages.extend(Session::drain_user_data(&px, 64, 1 << 20));
            let state = shared.state.load(Ordering::Relaxed);
            assert_ne!(
                state,
                State::Error as u32,
                "error {}",
                shared.last_error.load(Ordering::Relaxed)
            );
            state == State::Ended as u32
        }),
        "user data session never ended"
    );
    messages.extend(Session::drain_user_data(&px, 64, 1 << 20));
    session.close();

    let ours: Vec<_> = messages.iter().filter(|m| m.uuid == FIXTURE_UUID).collect();
    assert_eq!(ours.len(), 180, "one message per AU, none dropped");
    for (i, m) in ours.iter().enumerate() {
        assert_eq!(&m.payload[..4], b"BMUD");
        let frame = u32::from_be_bytes(m.payload[4..8].try_into().unwrap());
        assert_eq!(frame as usize, i, "decode order preserved");
        assert_eq!(m.payload.len(), 8 + 512);
        assert!(
            m.payload[8..]
                .iter()
                .enumerate()
                .all(|(k, &b)| b == ((i + k) & 0xFF) as u8),
            "filler intact on frame {i}"
        );
        if i > 0 {
            let step = m.pts_us - ours[i - 1].pts_us;
            assert!(
                (33_000..=34_000).contains(&step),
                "pts step {step} on frame {i}"
            );
        }
    }
    let foreign: Vec<_> = messages.iter().filter(|m| m.uuid != FIXTURE_UUID).collect();
    assert!(
        foreign.iter().any(|m| m.payload.starts_with(b"x264")),
        "x264's own user data passes through under its UUID"
    );
    assert_eq!(foreign[0].pts_us, ours[0].pts_us);
}

/// A seek clears the A/V offset and only re-arms it from the new timeline.
///
/// **This does not cover the interleaving it looks like it covers.** The
/// defect it was written beside is a race: the offset is computed on the
/// audio thread while the origin used to be cleared on the *video* thread's
/// own Flush, so audio could install the new generation and publish an
/// offset pairing the old video position with the new playhead. Reverting
/// the fix leaves this row green — the video thread reaches its Flush fast
/// enough that the sentinel appears anyway, and nothing here can hold it
/// back. Forcing that order needs a seam in `run_video` that does not
/// exist; the predicates on both sides are pinned by unit rows instead
/// (`a_frame_from_a_retired_timeline_cannot_arm_the_origin` and
/// `presentation_arms_the_origin_and_the_first_writer_wins`), and the gap
/// is recorded in TESTING.md.
///
/// What it does pin, end to end: a seek clears the origin at all, and the
/// offset comes back only once the new timeline has presented. Both halves
/// assert they were reached, so neither can pass by never arriving.
#[test]
fn a_seek_clears_the_offset_and_re_arms_on_the_new_timeline() {
    let session = Session::open(OpenRequest::new(fixture_path()));
    let shared = session.shared().clone();
    let px = session.pipeline().clone();
    let mut buf = vec![0f32; 2048];
    let pull = |buf: &mut [f32]| {
        if shared.audio_rate.load(Ordering::Relaxed) > 0
            && shared.state.load(Ordering::Relaxed) == State::Playing as u32
        {
            Session::read_audio(&px, buf);
        }
    };

    // Play until the offset is genuinely being exported, so the pre-seek
    // state is "armed" and a stale carry-over would be visible.
    let armed = wait_for(Duration::from_secs(20), || {
        pull(&mut buf);
        std::thread::sleep(Duration::from_millis(2));
        shared.av_offset_us.load(Ordering::Relaxed) != i32::MIN
    });
    assert!(armed, "never exported an offset before the seek");
    // `shared.frames_presented` counts host render events, so it stays 0
    // in a headless session; the pipeline's own present path books into the
    // diag stage instead.
    let presented = || {
        px.diag
            .stage(media_diag::Stage::Present)
            .out_count
            .load(Ordering::Relaxed)
    };
    let presented_before = presented();

    session.seek(MediaTime::from_millis(1_500));

    // `seek` only queues the command, so the old timeline's offset is
    // legitimately exported until the demux thread reaches it. The clear
    // landing is the start of the window this row is about; from there the
    // offset must stay unknown until the new timeline presents.
    let cleared = wait_for(Duration::from_secs(10), || {
        pull(&mut buf);
        std::thread::sleep(Duration::from_millis(1));
        shared.av_offset_us.load(Ordering::Relaxed) == i32::MIN
    });
    assert!(
        cleared,
        "the seek never cleared the offset, so the origin survived the flush"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_new_presentation = false;
    while Instant::now() < deadline {
        let offset = shared.av_offset_us.load(Ordering::Relaxed);
        let now_presented = presented();
        if offset != i32::MIN {
            assert!(
                now_presented > presented_before,
                "offset {offset} exported after the flush before the new                  timeline presented anything ({now_presented} presented,                  {presented_before} before the seek)"
            );
            saw_new_presentation = true;
            break;
        }
        pull(&mut buf);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        saw_new_presentation,
        "the new timeline never re-armed the offset, so the row proved only          that it had been cleared"
    );
}
