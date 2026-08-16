//! `bm-probe`: the harness player (spec §12.4). A CLI over the engine
//! crates directly — no Unity, no C ABI in the loop — with null sinks in
//! place of the GPU presenter: frames land as hashes.
//!
//! Subcommands accrete per milestone; `probe` from M1, `play` and
//! `conformance` from M2, `impair` at M3, `bench`/`resolve` at M4.

mod bench;
mod conformance;
mod impair;
mod play;
mod probe;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bm-probe", about = "Basis media engine harness player")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open a source and report container, codec, capabilities and
    /// first-frame timing.
    Probe {
        /// Local MP4 path or http(s) URL.
        url: String,
        /// Also decode to the first frame and report timing (Windows,
        /// Media Foundation).
        #[arg(long)]
        decode: bool,
        /// Permit sources that resolve to private/loopback addresses (the
        /// local test rig).
        #[arg(long)]
        allow_local: bool,
    },
    /// Timed headless run through the full engine pipeline, emitting the
    /// diagnostics timeline as CSV — the capture recorder as a
    /// first-class artefact.
    Play {
        /// Local MP4 path or http(s) URL.
        url: String,
        /// Seconds to run.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Write the capture timeline (one row per sample interval).
        #[arg(long)]
        csv: Option<std::path::PathBuf>,
        /// Capture sample interval, milliseconds.
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
        /// Write decoded audio as raw interleaved f32 (the headless audio
        /// lane).
        #[arg(long)]
        audio_out: Option<std::path::PathBuf>,
        /// Permit sources that resolve to private/loopback addresses.
        #[arg(long)]
        allow_local: bool,
        /// Force the live path. Liveness is inferred from the source by
        /// default; this overrules it (§6.11).
        #[arg(long)]
        live: bool,
        /// Audio-leading start: live joins start audible at the
        /// first banked audio; video appears at its keyframe.
        #[arg(long)]
        audio_lead: bool,
        /// Which of the container's audio tracks to bind, by index into
        /// the offered list. Out of range falls back to the first.
        #[arg(long, default_value_t = 0)]
        audio_track: usize,
        /// A separate audio-only source to play against `url`, which is
        /// then treated as video-only — the shape adaptive ladders serve
        /// above their muxed rung. On-demand HTTP(S) and files only.
        #[arg(long)]
        audio_url: Option<String>,
    },
    /// Measure the §11 budgets for one lane: startup-to-first-frame and
    /// seek-to-settled, repeated and aggregated.
    Bench {
        /// Local path or http(s) URL.
        url: String,
        /// Runs to aggregate over.
        #[arg(long, default_value_t = 3)]
        runs: u32,
        /// Seek target in ms (default: 3/4 of the reported duration; the
        /// seek phase is skipped when neither is available).
        #[arg(long)]
        seek_to_ms: Option<u64>,
        /// Permit sources that resolve to private/loopback addresses.
        #[arg(long)]
        allow_local: bool,
        /// Open as a live source (no seek phase).
        #[arg(long)]
        live: bool,
        /// Audio-leading start: the startup phase then measures
        /// time-to-first-audio (Playing without a presented frame).
        #[arg(long)]
        audio_lead: bool,
        /// Skip the seek phase (lanes whose demuxer refuses seeks).
        #[arg(long)]
        no_seek: bool,
        /// Per-phase timeout, seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Print the engine-declared capability set (§6.11) as JSON — the
    /// blob `bm_capabilities` serves, so format-selection rules are
    /// testable without Unity.
    Caps {
        /// Compact single-line output (the exact ABI blob) instead of
        /// pretty-printed.
        #[arg(long)]
        compact: bool,
    },
    /// Demux a fixture and diff the AU stream against the ffprobe oracle
    /// (codec, count, timestamps, payload MD5). The per-PR CI gate.
    Conformance {
        /// Fixture path (or a directory of fixtures).
        fixture: std::path::PathBuf,
    },
    /// Replay a phase-0 impairment profile over a live lane (a TS file
    /// paced to 1x, or a live URL) and grade the Bank against the sizing
    /// model (§12.2).
    Impair {
        /// Local .ts path or live http(s) URL.
        url: String,
        /// Profile name (a phase-0 capture, e.g. ts-rtt300-loss005).
        #[arg(long)]
        profile: String,
        /// Seconds to run (default: the profile's analysed window).
        #[arg(long)]
        duration: Option<u64>,
        /// Explicit buffer depth in ms (default: Auto).
        #[arg(long)]
        depth_ms: Option<u32>,
        /// Permit sources that resolve to private/loopback addresses.
        #[arg(long)]
        allow_local: bool,
        /// Write the capture timeline.
        #[arg(long)]
        csv: Option<std::path::PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Probe {
            url,
            decode,
            allow_local,
        } => probe::run(&url, decode, allow_local),
        Command::Play {
            url,
            duration,
            csv,
            interval_ms,
            audio_out,
            allow_local,
            live,
            audio_lead,
            audio_track,
            audio_url,
        } => play::run(&play::Options {
            url,
            duration,
            csv,
            interval_ms,
            audio_out,
            allow_local,
            live,
            audio_lead,
            audio_track,
            audio_url,
        }),
        Command::Bench {
            url,
            runs,
            seek_to_ms,
            allow_local,
            live,
            audio_lead,
            no_seek,
            timeout,
        } => bench::run(&bench::Options {
            url,
            runs,
            seek_to_ms,
            allow_local,
            live,
            audio_lead,
            no_seek,
            timeout_s: timeout,
        }),
        Command::Caps { compact } => {
            let caps = media_engine::capabilities();
            if compact {
                println!("{}", caps.to_json());
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&caps).expect("capability set serialises")
                );
            }
            ExitCode::SUCCESS
        }
        Command::Conformance { fixture } => conformance::run(&fixture),
        Command::Impair {
            url,
            profile,
            duration,
            depth_ms,
            allow_local,
            csv,
        } => impair::run(&impair::Options {
            url,
            profile,
            duration,
            depth_ms,
            allow_local,
            csv,
        }),
    }
}

/// Open a sequential live byte source for a URL (the impair harness wraps
/// it; the engine builds its own for play).
pub(crate) fn open_live_source(
    url: &str,
    allow_local: bool,
) -> Result<Box<dyn media_demux::ByteSource>, String> {
    let gate: std::sync::Arc<dyn media_io::AddressGate> = if allow_local {
        std::sync::Arc::new(media_io::AllowAllGate)
    } else {
        std::sync::Arc::new(media_io::PublicAddressGate)
    };
    media_io::HttpLiveSource::open(
        url,
        media_io::IoLimits::default(),
        gate,
        media_io::CancelToken::new(),
    )
    .map(|s| Box::new(s) as Box<dyn media_demux::ByteSource>)
    .map_err(|e| e.to_string())
}

/// Open a byte source for a path-or-URL argument.
pub(crate) fn open_source(
    url: &str,
    allow_local: bool,
) -> Result<Box<dyn media_demux::ByteSource>, String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let gate: std::sync::Arc<dyn media_io::AddressGate> = if allow_local {
            std::sync::Arc::new(media_io::AllowAllGate)
        } else {
            std::sync::Arc::new(media_io::PublicAddressGate)
        };
        media_io::HttpSource::open(url, media_io::IoLimits::default(), gate)
            .map(|s| Box::new(s) as Box<dyn media_demux::ByteSource>)
            .map_err(|e| e.to_string())
    } else {
        media_io::FileSource::open(std::path::Path::new(url))
            .map(|s| Box::new(s) as Box<dyn media_demux::ByteSource>)
            .map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
pub(crate) fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
