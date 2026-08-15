//! Streaming MP4 demuxer tests over the committed fixtures: the three
//! layouts (faststart, trailing moov, fragmented) must demux identically,
//! events must interleave in decode order, and hostile input must produce
//! typed errors.

use media_clock::{Generation, MediaTime};
use media_demux::{
    AudioCodec, DemuxLimits, Demuxer, EosReason, Format, MemSource, Mp4Demuxer, StreamEvent,
    VideoCodec,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name);
    std::fs::read(path).expect("fixture readable")
}

fn open(name: &str) -> Mp4Demuxer {
    Mp4Demuxer::open(
        Box::new(MemSource(fixture(name))),
        DemuxLimits::default(),
        Generation(1),
    )
    .expect("fixture opens")
}

struct Summary {
    video_formats: u32,
    audio_formats: u32,
    video_aus: u32,
    audio_aus: u32,
    video_keys: u32,
    first_audio_pts: Option<MediaTime>,
    first_video_au: Option<Vec<u8>>,
}

fn drain(demux: &mut Mp4Demuxer) -> Summary {
    let mut s = Summary {
        video_formats: 0,
        audio_formats: 0,
        video_aus: 0,
        audio_aus: 0,
        video_keys: 0,
        first_audio_pts: None,
        first_video_au: None,
    };
    let mut last_dts = MediaTime::from_micros(i64::MIN);
    loop {
        match demux.next_event().expect("no demux error") {
            StreamEvent::Format(_, Format::Video { codec, .. }) => {
                assert_eq!(codec, VideoCodec::H264);
                s.video_formats += 1;
            }
            StreamEvent::Format(_, Format::Audio { codec, .. }) => {
                assert_eq!(codec, AudioCodec::Aac);
                s.audio_formats += 1;
            }
            StreamEvent::Au(au) => {
                assert!(au.dts >= last_dts, "AUs interleaved in decode order");
                last_dts = au.dts;
                assert_eq!(au.generation, Generation(1));
                if au.data.starts_with(&[0, 0, 0, 1]) {
                    // Annex-B start code marks the video track's AUs.
                    s.video_aus += 1;
                    if au.key {
                        s.video_keys += 1;
                    }
                    if s.first_video_au.is_none() {
                        s.first_video_au = Some(au.data);
                    }
                } else {
                    s.audio_aus += 1;
                    if s.first_audio_pts.is_none() {
                        s.first_audio_pts = Some(au.pts);
                    }
                }
            }
            StreamEvent::Eos(reason) => {
                assert_eq!(reason, EosReason::Natural);
                return s;
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}

#[test]
fn faststart_demuxes_the_full_fixture() {
    let mut demux = open("h264-aac-640x360-30fps.mp4");
    assert_eq!(demux.take_notes(), Vec::<String>::new());
    let duration = demux.duration().expect("duration known");
    assert!((duration.as_millis() - 6000).abs() < 100, "{duration}");

    let s = drain(&mut demux);
    assert_eq!((s.video_formats, s.audio_formats), (1, 1));
    // The ffprobe-verified packet counts for this fixture.
    assert_eq!(s.video_aus, 180);
    assert_eq!(s.audio_aus, 283);
    assert_eq!(s.video_keys, 3, "6 s at GOP 60 / 30 fps");
    // The edit list shifts the priming AU ahead of the origin.
    assert_eq!(s.first_audio_pts, Some(MediaTime::from_micros(-21333)));
}

#[test]
fn audio_format_reconstructs_the_asc() {
    let mut demux = open("h264-aac-640x360-30fps.mp4");
    loop {
        if let StreamEvent::Format(
            _,
            Format::Audio {
                sample_rate,
                channels,
                codec_private,
                ..
            },
        ) = demux.next_event().expect("event")
        {
            assert_eq!(sample_rate, 48000);
            assert_eq!(channels, 2);
            // AOT 2 (LC), frequency index 3 (48 kHz), channel config 2.
            assert_eq!(codec_private, vec![0x11, 0x90]);
            return;
        }
    }
}

#[test]
fn all_layouts_demux_identically() {
    let baseline = drain(&mut open("h264-aac-640x360-30fps.mp4"));
    for layout in ["h264-aac-moov-trailing.mp4", "h264-aac-frag.mp4"] {
        let s = drain(&mut open(layout));
        assert_eq!(s.video_aus, baseline.video_aus, "{layout}");
        assert_eq!(s.audio_aus, baseline.audio_aus, "{layout}");
        assert_eq!(s.first_video_au, baseline.first_video_au, "{layout}");
    }
}

#[test]
fn seek_lands_on_a_keyframe() {
    // Every moov layout seeks the same way (the seek matrix rows).
    for layout in [
        "h264-aac-640x360-30fps.mp4",
        "h264-aac-moov-trailing.mp4",
        "h264-aac-frag.mp4",
    ] {
        seek_lands_on_a_keyframe_in(layout);
    }
}

fn seek_lands_on_a_keyframe_in(layout: &str) {
    let mut demux = open(layout);
    let landed = demux
        .seek(MediaTime::from_secs(3), Generation(2))
        .expect("seek");
    assert!(landed <= MediaTime::from_secs(3));
    assert!(landed >= MediaTime::ZERO);

    // First video AU after the seek is a keyframe with the new generation.
    loop {
        match demux.next_event().expect("event") {
            StreamEvent::Au(au) if au.data.starts_with(&[0, 0, 0, 1]) => {
                assert!(au.key, "seek must land keyframe-clean");
                assert_eq!(au.generation, Generation(2));
                assert_eq!(au.pts, landed);
                return;
            }
            StreamEvent::Au(_) => {}
            StreamEvent::Eos(_) => panic!("hit EOS before a video AU"),
            _ => {}
        }
    }
}

#[test]
fn truncated_metadata_is_a_typed_error() {
    let mut bytes = fixture("h264-aac-640x360-30fps.mp4");
    bytes.truncate(4000); // Mid-moov.
    let result = Mp4Demuxer::open(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation(1),
    );
    assert!(result.is_err());
}

#[test]
fn metadata_budget_trips_as_an_error() {
    let result = Mp4Demuxer::open(
        Box::new(MemSource(fixture("h264-aac-640x360-30fps.mp4"))),
        DemuxLimits {
            max_metadata_bytes: 1024,
            ..DemuxLimits::default()
        },
        Generation(1),
    );
    assert!(result.is_err());
}

#[test]
fn video_only_fixture_still_demuxes() {
    let mut demux = open("h264-640x360-30fps.mp4");
    let s = drain(&mut demux);
    assert_eq!((s.video_formats, s.audio_formats), (1, 0));
    assert!(s.video_aus > 0);
    assert_eq!(s.audio_aus, 0);
}
