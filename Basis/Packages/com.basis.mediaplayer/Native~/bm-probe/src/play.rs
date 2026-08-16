//! `play`: a timed headless run through the full engine pipeline — the
//! capture recorder as a first-class artefact (§12.4), plus the headless
//! audio lane the C harness never had.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use media_diag::CaptureRecorder;
use media_engine::{OpenRequest, Session, State};

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
    let mut audio_epoch: Option<Instant> = None;
    let mut audio_buf = vec![0.0f32; 2048];
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

        // Pull audio at the hardware cadence, as the Unity audio thread
        // will.
        let rate = shared.audio_rate.load(Ordering::Relaxed);
        let channels = shared.audio_channels.load(Ordering::Relaxed).max(1);
        if rate > 0 && state == State::Playing as u32 {
            let epoch = *audio_epoch.get_or_insert_with(Instant::now);
            let budget =
                epoch.elapsed().as_micros() as u64 * u64::from(rate) / 1_000_000 - audio_frames;
            if budget as usize >= audio_buf.len() / channels as usize {
                let frames = Session::read_audio(&px, &mut audio_buf);
                audio_frames += frames as u64;
                if let Some(file) = audio_file.as_mut() {
                    let samples = &audio_buf[..frames * channels as usize];
                    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
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
        "audio:     {} frames pulled @ {} Hz x {}",
        audio_frames,
        shared.audio_rate.load(Ordering::Relaxed),
        shared.audio_channels.load(Ordering::Relaxed),
    );
    println!(
        "position:  {} ms of {} ms",
        shared.position_us.load(Ordering::Relaxed) / 1000,
        shared.duration_us.load(Ordering::Relaxed) / 1000,
    );
    if caption_count > 0 {
        println!("captions:  {caption_count} cues");
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
