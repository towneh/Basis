//! MPEG-TS demuxer behaviour over the committed fixtures, plus replay of
//! the C player's pinned fuzz crashes (the four fixes the port carries).

use media_clock::{Generation, MediaTime};
use media_demux::{
    AudioCodec, ContainerKind, DemuxLimits, Demuxer, Format, MemSource, StreamEvent, TsDemuxer,
    VideoCodec, sniff_container,
};

fn open(bytes: Vec<u8>) -> TsDemuxer {
    TsDemuxer::open(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation(0),
    )
    .expect("ts open")
}

fn drain(demux: &mut TsDemuxer, cap: usize) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for _ in 0..cap {
        match demux.next_event().expect("no source errors on fixtures") {
            StreamEvent::Eos(_) => break,
            event => events.push(event),
        }
    }
    events
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name);
    std::fs::read(path).expect("fixture readable")
}

#[test]
fn demuxes_the_av_fixture() {
    let mut demux = open(fixture("h264-aac-640x360-30fps.ts"));
    let events = drain(&mut demux, 100_000);

    let mut video_format = None;
    let mut audio_format = None;
    let mut video_aus = 0usize;
    let mut audio_aus = 0usize;
    let mut keyframes = 0usize;
    for event in &events {
        match event {
            StreamEvent::Format(_, f @ Format::Video { .. }) => video_format = Some(f.clone()),
            StreamEvent::Format(_, f @ Format::Audio { .. }) => audio_format = Some(f.clone()),
            StreamEvent::Au(au) => {
                if Some(au.track) == demux.video_track() {
                    video_aus += 1;
                    if au.key {
                        keyframes += 1;
                    }
                } else {
                    audio_aus += 1;
                }
            }
            _ => {}
        }
    }

    // Counts pinned against ffprobe (the conformance gate checks the full
    // per-packet detail; this keeps a cheap in-tree signal).
    assert_eq!(video_aus, 180);
    assert_eq!(audio_aus, 283);
    assert_eq!(keyframes, 3); // GOP 60 over 180 frames
    let Some(Format::Video {
        codec,
        display_width,
        display_height,
        ..
    }) = video_format
    else {
        panic!("no video format announced");
    };
    assert_eq!(codec, VideoCodec::H264);
    assert_eq!((display_width, display_height), (640, 360));
    let Some(Format::Audio {
        codec,
        sample_rate,
        channels,
        codec_private,
    }) = audio_format
    else {
        panic!("no audio format announced");
    };
    assert_eq!(codec, AudioCodec::Aac);
    assert_eq!(sample_rate, 48000);
    assert_eq!(channels, 2);
    assert_eq!(codec_private.len(), 2);
}

#[test]
fn m2ts_lpcm_announces_and_flows() {
    let mut demux = open(fixture("h264-lpcm-320x180.m2ts"));
    let events = drain(&mut demux, 100_000);
    let audio_format = events.iter().find_map(|e| match e {
        StreamEvent::Format(_, f @ Format::Audio { .. }) => Some(f.clone()),
        _ => None,
    });
    let Some(Format::Audio {
        codec,
        sample_rate,
        channels,
        codec_private,
    }) = audio_format
    else {
        panic!("no LPCM format announced");
    };
    assert_eq!(codec, AudioCodec::Pcm);
    assert_eq!(sample_rate, 48000);
    assert_eq!(channels, 2);
    // [channel_assignment, bits_code, flags]: stereo is assignment 3,
    // 16-bit is 1, and flags bit 0 clear says the samples are big-endian.
    assert_eq!(codec_private, vec![3, 1, 0]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Au(au) if Some(au.track) == demux.audio_track()))
    );
}

#[test]
fn mid_gop_join_waits_for_an_sps_keyframe() {
    // Drop the first 40% of the A/V fixture so the demuxer joins mid-GOP:
    // no AU may be emitted before an SPS-bearing keyframe, and the format
    // must still announce real dimensions.
    let bytes = fixture("h264-aac-640x360-30fps.ts");
    let cut = (bytes.len() * 2 / 5 / 188) * 188 + 100; // deliberately misaligned
    let mut demux = open(bytes[cut..].to_vec());
    let events = drain(&mut demux, 100_000);

    let first_video_key = events.iter().find_map(|e| match e {
        StreamEvent::Au(au) if Some(au.track) == demux.video_track() => Some(au.key),
        _ => None,
    });
    assert_eq!(
        first_video_key,
        Some(true),
        "first video AU must be a keyframe"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Format(
            _,
            Format::Video {
                display_width: 640,
                ..
            }
        )
    )));
}

#[test]
fn seek_is_unsupported() {
    let mut demux = open(fixture("h264-aac-640x360-30fps.ts"));
    assert!(demux.seek(MediaTime::from_secs(1), Generation(1)).is_err());
    assert_eq!(demux.duration(), None);
}

#[test]
fn sniffs_ts_and_m2ts() {
    assert_eq!(
        sniff_container(&fixture("h264-aac-640x360-30fps.ts")[..1024]),
        Some(ContainerKind::MpegTs)
    );
    assert_eq!(
        sniff_container(&fixture("h264-lpcm-320x180.m2ts")[..1024]),
        Some(ContainerKind::MpegTs)
    );
    assert_eq!(
        sniff_container(&fixture("h264-aac-640x360-30fps.mp4")[..1024]),
        Some(ContainerKind::Mp4)
    );
    assert_eq!(sniff_container(&[0u8; 1024]), None);
}

/// The C player's pinned fuzz crashes, carried over as seeds: each must
/// walk to EOS (or a typed error) without panicking.
#[test]
fn replays_the_pinned_c_fuzz_crashes() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fuzz/corpus/ts_stream");
    let mut replayed = 0usize;
    for entry in std::fs::read_dir(dir).expect("seed corpus present") {
        let path = entry.expect("dir entry").path();
        let bytes = std::fs::read(&path).expect("seed readable");
        let mut demux = open(bytes);
        for _ in 0..100_000 {
            match demux.next_event() {
                Ok(StreamEvent::Eos(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        replayed += 1;
    }
    assert!(replayed >= 4, "expected the four pinned crash inputs");
}
