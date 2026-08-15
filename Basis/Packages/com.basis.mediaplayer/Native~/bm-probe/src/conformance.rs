//! `conformance`: diff the demuxer's AU stream against the ffprobe oracle
//! (§12.4) — announce, count, timestamps to 1 µs, and payload MD5 for
//! every packet (raw stored payloads, so keyframes need no exemption).
//!
//! Needs `ffprobe` on PATH; the fixture set is the committed `fixtures/`
//! directory or any file/directory argument.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use md5::Digest as _;
use media_clock::Generation;
use media_demux::{AudioCodec, DemuxLimits, Demuxer, Format, Mp4Demuxer, StreamEvent, TsDemuxer};

const PTS_TOLERANCE_US: i64 = 1;

pub fn run(fixture: &Path) -> ExitCode {
    let fixtures: Vec<PathBuf> = if fixture.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(fixture)
            .expect("fixture dir readable")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext == "mp4" || ext == "ts" || ext == "m2ts")
            })
            .collect();
        entries.sort();
        entries
    } else {
        vec![fixture.to_path_buf()]
    };
    if fixtures.is_empty() {
        eprintln!("conformance: no fixtures under {}", fixture.display());
        return ExitCode::FAILURE;
    }

    let mut failed = false;
    for path in &fixtures {
        match check_fixture(path) {
            Ok(summary) => println!("PASS {} ({summary})", path.display()),
            Err(reason) => {
                failed = true;
                println!("FAIL {}: {reason}", path.display());
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

struct AuRecord {
    pts_us: i64,
    key: bool,
    size: usize,
    md5: String,
}

fn check_fixture(path: &Path) -> Result<String, String> {
    // Our side: raw stored payloads, decode order per track. TS leaves the
    // demuxer as stored already (Annex-B video, and ADTS audio in raw
    // mode); MP4 needs raw mode to skip the Annex-B conversion.
    let is_ts = path
        .extension()
        .is_some_and(|ext| ext == "ts" || ext == "m2ts");
    let source = crate::open_source(&path.to_string_lossy(), true)?;
    let mut demux: Box<dyn Demuxer> = if is_ts {
        let mut demux = TsDemuxer::open(source, DemuxLimits::default(), Generation(0))
            .map_err(|e| format!("demux open: {e}"))?;
        demux.set_emit_raw_audio(true);
        Box::new(demux)
    } else {
        let mut demux = Mp4Demuxer::open(source, DemuxLimits::default(), Generation(0))
            .map_err(|e| format!("demux open: {e}"))?;
        demux.set_emit_raw_video(true);
        Box::new(demux)
    };

    let mut records: Vec<(media_demux::TrackId, AuRecord)> = Vec::new();
    let mut our_dims = (0u32, 0u32);
    let mut our_audio_fmt = (0u32, 0u32);
    let mut our_audio_codec = None;
    loop {
        match demux.next_event().map_err(|e| format!("demux: {e}"))? {
            StreamEvent::Format(
                _,
                Format::Video {
                    display_width,
                    display_height,
                    ..
                },
            ) => our_dims = (display_width, display_height),
            StreamEvent::Format(
                _,
                Format::Audio {
                    codec,
                    sample_rate,
                    channels,
                    ..
                },
            ) => {
                our_audio_fmt = (sample_rate, channels);
                our_audio_codec = Some(codec);
            }
            StreamEvent::Au(au) => {
                let record = AuRecord {
                    pts_us: au.pts.as_micros(),
                    key: au.key,
                    size: au.data.len(),
                    md5: format!("{:x}", md5::Md5::digest(&au.data)),
                };
                records.push((au.track, record));
            }
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    // Track identity is only final at EOS for TS (the PMT names the PIDs
    // mid-stream), so partition after the pull loop.
    let video_track = demux.video_track();
    let audio_track = demux.audio_track();
    let mut ours_video: Vec<AuRecord> = Vec::new();
    let mut ours_audio: Vec<AuRecord> = Vec::new();
    for (track, record) in records {
        if Some(track) == video_track {
            ours_video.push(record);
        } else if Some(track) == audio_track {
            ours_audio.push(record);
        }
    }

    // Oracle side.
    let oracle = ffprobe(path)?;
    let mut checks = Vec::new();

    if let Some(stream) = oracle.streams.iter().find(|s| s.codec_type == "video") {
        if stream.codec_name != "h264" {
            return Err(format!("oracle video codec {}", stream.codec_name));
        }
        if (stream.width.unwrap_or(0), stream.height.unwrap_or(0)) != our_dims {
            return Err(format!(
                "announce: video {}x{} vs oracle {}x{}",
                our_dims.0,
                our_dims.1,
                stream.width.unwrap_or(0),
                stream.height.unwrap_or(0)
            ));
        }
        let packets = oracle.packets_for(stream.index);
        diff_stream("video", &ours_video, &packets, true)?;
        checks.push(format!("video {} AUs", ours_video.len()));
    } else if !ours_video.is_empty() {
        return Err("we demuxed video the oracle does not see".into());
    }

    if let Some(stream) = oracle.streams.iter().find(|s| s.codec_type == "audio") {
        let lpcm = our_audio_codec == Some(AudioCodec::Pcm);
        let codec_ok = match stream.codec_name.as_str() {
            "aac" => our_audio_codec == Some(AudioCodec::Aac),
            "pcm_bluray" => lpcm,
            other => return Err(format!("oracle audio codec {other}")),
        };
        if !codec_ok {
            return Err(format!(
                "announce: audio codec {our_audio_codec:?} vs oracle {}",
                stream.codec_name
            ));
        }
        let oracle_fmt = (
            stream
                .sample_rate
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u32),
            stream.channels.unwrap_or(0),
        );
        if oracle_fmt != our_audio_fmt {
            return Err(format!(
                "announce: audio {}Hz/{}ch vs oracle {}Hz/{}ch",
                our_audio_fmt.0, our_audio_fmt.1, oracle_fmt.0, oracle_fmt.1
            ));
        }
        if lpcm {
            // LPCM has no canonical packetisation (we emit per PES, ffmpeg
            // re-chunks), so per-packet comparisons are meaningless — but
            // emitting nothing at all is still a regression.
            if ours_audio.is_empty() {
                return Err("no LPCM frames emitted".into());
            }
            checks.push(format!(
                "audio {} LPCM frames (announce only)",
                ours_audio.len()
            ));
        } else {
            let packets = oracle.packets_for(stream.index);
            diff_stream("audio", &ours_audio, &packets, false)?;
            checks.push(format!("audio {} AUs", ours_audio.len()));
        }
    } else if !ours_audio.is_empty() {
        return Err("we demuxed audio the oracle does not see".into());
    }

    Ok(checks.join(", "))
}

fn diff_stream(
    kind: &str,
    ours: &[AuRecord],
    oracle: &[OraclePacket],
    check_keys: bool,
) -> Result<(), String> {
    if ours.len() != oracle.len() {
        return Err(format!(
            "{kind} count: {} vs oracle {}",
            ours.len(),
            oracle.len()
        ));
    }
    for (i, (au, packet)) in ours.iter().zip(oracle).enumerate() {
        let oracle_pts = packet.pts_us();
        if (au.pts_us - oracle_pts).abs() > PTS_TOLERANCE_US {
            return Err(format!(
                "{kind} pts[{i}]: {} vs oracle {oracle_pts}",
                au.pts_us
            ));
        }
        if au.size != packet.size_bytes() {
            return Err(format!(
                "{kind} size[{i}]: {} vs oracle {}",
                au.size,
                packet.size_bytes()
            ));
        }
        if let Some(hash) = packet.md5()
            && au.md5 != hash
        {
            return Err(format!("{kind} md5[{i}]: {} vs oracle {hash}", au.md5));
        }
        if check_keys {
            let oracle_key = packet.flags.as_deref().is_some_and(|f| f.contains('K'));
            if au.key != oracle_key {
                return Err(format!(
                    "{kind} keyframe[{i}]: {} vs oracle {oracle_key}",
                    au.key
                ));
            }
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct OracleOutput {
    #[serde(default)]
    packets: Vec<OraclePacket>,
    #[serde(default)]
    streams: Vec<OracleStream>,
}

#[derive(serde::Deserialize)]
struct OraclePacket {
    stream_index: u32,
    pts_time: Option<String>,
    size: String,
    data_hash: Option<String>,
    flags: Option<String>,
}

#[derive(serde::Deserialize)]
struct OracleStream {
    index: u32,
    codec_type: String,
    codec_name: String,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<String>,
    channels: Option<u32>,
}

impl OracleOutput {
    fn packets_for(&self, stream_index: u32) -> Vec<OraclePacket> {
        self.packets
            .iter()
            .filter(|p| p.stream_index == stream_index)
            .map(|p| OraclePacket {
                stream_index: p.stream_index,
                pts_time: p.pts_time.clone(),
                size: p.size.clone(),
                data_hash: p.data_hash.clone(),
                flags: p.flags.clone(),
            })
            .collect()
    }
}

impl OraclePacket {
    fn pts_us(&self) -> i64 {
        self.pts_time
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1e6).round() as i64)
            .unwrap_or(i64::MIN)
    }

    fn size_bytes(&self) -> usize {
        self.size.parse().unwrap_or(0)
    }

    fn md5(&self) -> Option<String> {
        self.data_hash
            .as_deref()
            .and_then(|h| h.split(':').nth(1))
            .map(str::to_ascii_lowercase)
    }
}

fn ffprobe(path: &Path) -> Result<OracleOutput, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_packets",
            "-show_data_hash",
            "md5",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe not runnable: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("ffprobe json: {e}"))
}
