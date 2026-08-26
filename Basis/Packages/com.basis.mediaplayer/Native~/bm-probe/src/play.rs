//! `play`: a timed headless run through the full engine pipeline — the
//! capture recorder as a first-class artefact (§12.4), plus the headless
//! audio lane the C harness never had.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use media_diag::CaptureRecorder;
use media_engine::{OpenRequest, PipelineShared, Session, State};

pub struct Options {
    pub url: String,
    pub duration: u64,
    pub csv: Option<std::path::PathBuf>,
    pub interval_ms: u64,
    pub audio_out: Option<std::path::PathBuf>,
    pub allow_local: bool,
    pub live: bool,
    pub audio_lead: bool,
    pub audio_track: usize,
    pub audio_url: Option<String>,
}

pub fn run(options: &Options) -> ExitCode {
    let mut request = OpenRequest::new(options.url.clone());
    request.audio_url = options.audio_url.clone();
    request.allow_local_addresses = options.allow_local;
    if options.live {
        request.liveness = media_engine::SourceLiveness::Live;
    }
    request.audio_leading = options.audio_lead;
    request.audio_track = options.audio_track;
    let mut session = Session::open(request);
    let shared = session.shared().clone();
    let px = session.pipeline().clone();
    let diag = session.diag().clone();

    let mut recorder = CaptureRecorder::default();
    let mut audio_file = match options
        .audio_out
        .as_ref()
        .map(std::fs::File::create)
        .transpose()
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("play: audio out: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut audio_frames = 0u64;
    let mut caption_count = 0u64;
    // SEI user data per UUID: messages and payload bytes.
    let mut user_data: std::collections::BTreeMap<[u8; 16], (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut audio_epoch: Option<Instant> = None;
    // Frames the host asked for, served or not. Unity's OnAudioFilterRead
    // hands over a fixed buffer and zero-fills whatever the ring could not
    // serve, so an underrun costs wall-clock time that never comes back.
    // Pacing off frames *served* instead lets the consumer catch its own
    // shortfall up on the next pass, which drains the ring at exactly the
    // rate the producer fills it and hides every underrun this harness
    // exists to find.
    let mut budget_frames = 0u64;
    // Unity's DSP quantum, sized by the session's real channel count. A
    // flat sample count reads as a different frame count per geometry —
    // 341 frames on 5.1 against 1024 on mono — so the cadence stops
    // matching the host's the moment the lane is not the one it was
    // written for.
    const DSP_FRAMES: usize = 1024;
    let mut audio_buf = vec![0.0f32; DSP_FRAMES];
    let mut sized_for = 0u32;
    let mut next_sample = Instant::now();
    let interval = Duration::from_millis(options.interval_ms.max(10));
    let end = Instant::now() + Duration::from_secs(options.duration);

    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        let state = shared.state.load(Ordering::Relaxed);
        if state == State::Error as u32 || state == State::Ended as u32 {
            break;
        }

        if now >= next_sample {
            recorder.sample(px.wall.now(), &diag);
            next_sample = now + interval;
        }

        for cue in Session::drain_captions(&px, 16) {
            caption_count += 1;
            let text = if cue.text.is_empty() {
                "(clear)"
            } else {
                &cue.text
            };
            println!(
                "caption:   [{:>10}us] {}",
                cue.pts_us,
                text.replace('\n', " / ")
            );
        }

        drain_user_data(&px, &mut user_data);

        // Pull audio at the hardware cadence, as the Unity audio thread
        // will.
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
        if rate > 0 && state == State::Playing as u32 {
            if sized_for != channels {
                audio_buf = vec![0.0f32; DSP_FRAMES * channels as usize];
                sized_for = channels;
            }
            let epoch = *audio_epoch.get_or_insert_with(Instant::now);
            let due =
                epoch.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - budget_frames;
            if due as usize >= DSP_FRAMES {
                let frames = Session::read_audio(&px, &mut audio_buf);
                audio_frames += frames as u64;
                budget_frames += DSP_FRAMES as u64;
                if let Some(file) = audio_file.as_mut() {
                    // The whole buffer, zero-fill included: the capture is a
                    // timeline, and dropping the silence would close the gap
                    // an underrun left and shift every sample after it early.
                    let bytes: Vec<u8> = audio_buf.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if let Err(e) = file.write_all(&bytes) {
                        eprintln!("play: audio out: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    recorder.sample(px.wall.now(), &diag);
    if let Some(path) = &options.csv {
        match std::fs::File::create(path) {
            Ok(file) => {
                if let Err(e) = recorder.write_csv(std::io::BufWriter::new(file)) {
                    eprintln!("play: csv: {e}");
                    return ExitCode::FAILURE;
                }
                println!("csv:       {}", path.display());
            }
            Err(e) => {
                eprintln!("play: csv: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let state = shared.state.load(Ordering::Relaxed);
    let snapshot = diag.snapshot();
    println!(
        "state:     {state} ({})",
        match state {
            s if s == State::Playing as u32 => "playing",
            s if s == State::Ended as u32 => "ended",
            s if s == State::Error as u32 => "error",
            s if s == State::Buffering as u32 => "buffering",
            _ => "other",
        }
    );
    println!(
        "video:     {} decoded, {} presented, {} pool drops",
        shared.frames_decoded.load(Ordering::Relaxed),
        snapshot[media_diag::Stage::Present as usize].out_count,
        snapshot[media_diag::Stage::Pool as usize].drops,
    );
    println!(
        "audio:     {} frames pulled, {} silence, {} trimmed @ {} Hz x {}",
        audio_frames,
        budget_frames.saturating_sub(audio_frames),
        diag.audio_trimmed(),
        shared.audio_rate.load(Ordering::Relaxed),
        shared.audio_channels.load(Ordering::Relaxed),
    );
    println!(
        "position:  {} ms of {} ms",
        shared.position_us.load(Ordering::Relaxed) / 1000,
        shared.duration_us.load(Ordering::Relaxed) / 1000,
    );
    // The loop leaves on a deadline or a settled state, either of which
    // can land between drains; what arrived since is still owed to the
    // totals. Bounded at the ring's depth in passes: the session is still
    // open here, so a live source dense enough to refill the ring faster
    // than it drains would otherwise hold the summary hostage.
    let mut passes = 0;
    while drain_user_data(&px, &mut user_data) > 0 {
        passes += 1;
        if passes == 16 {
            println!("user data: still arriving at the deadline; later messages not counted");
            break;
        }
    }
    if caption_count > 0 {
        println!("captions:  {caption_count} cues");
    }
    for (uuid, (count, bytes)) in &user_data {
        let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
        println!("user data: {hex} {count} messages, {bytes} bytes");
    }
    for event in diag.take_events() {
        println!(
            "event:     [{:>10}us] {:?} {} {}",
            event.wall.as_micros(),
            event.code,
            event.stage.name(),
            event.detail
        );
    }
    session.close();

    if state == State::Error as u32 {
        eprintln!(
            "play: session error code {}",
            shared.last_error.load(Ordering::Relaxed)
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// One pass over the pending SEI user data, tallied per UUID as
/// (messages, payload bytes); returns how many it took.
fn drain_user_data(
    px: &PipelineShared,
    totals: &mut std::collections::BTreeMap<[u8; 16], (u64, u64)>,
) -> usize {
    let messages = Session::drain_user_data(px, 64, 1 << 20);
    for m in &messages {
        let e = totals.entry(m.uuid).or_default();
        e.0 += 1;
        e.1 += m.payload.len() as u64;
    }
    messages.len()
}
