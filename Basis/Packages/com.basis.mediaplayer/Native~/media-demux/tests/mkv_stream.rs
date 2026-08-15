//! Matroska demuxer rows: pinned AU counts against the source fixture,
//! codec announces for tracks without decode adapters, keyframe-clean
//! cue seeks, Annex-B conversion of stored H.264.

use media_clock::{Generation, MediaTime};
use media_demux::{
    AudioCodec, DemuxLimits, Demuxer, Format, MemSource, MkvDemuxer, StreamEvent, VideoCodec,
};

fn open(name: &str) -> MkvDemuxer {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/mkv");
    let bytes = std::fs::read(path.join(name)).expect("fixture");
    MkvDemuxer::open(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation(0),
    )
    .expect("open")
}

#[test]
fn mkv_h264_aac_matches_the_source_counts() {
    let mut demux = open("h264-aac.mkv");
    let mut video = None;
    let mut audio = None;
    let (mut video_aus, mut audio_aus, mut keys) = (0, 0, 0);
    loop {
        match demux.next_event().expect("event") {
            StreamEvent::Format(
                track,
                Format::Video {
                    codec, coded_width, ..
                },
            ) => {
                assert_eq!(codec, VideoCodec::H264);
                assert_eq!(coded_width, 640);
                video = Some(track);
            }
            StreamEvent::Format(
                track,
                Format::Audio {
                    codec, sample_rate, ..
                },
            ) => {
                assert_eq!(codec, AudioCodec::Aac);
                assert_eq!(sample_rate, 48_000);
                audio = Some(track);
            }
            StreamEvent::Au(au) if Some(au.track) == video => {
                // Stored length-prefixed; emitted Annex B.
                assert!(au.data.starts_with(&[0, 0, 0, 1]), "Annex-B start code");
                if au.key {
                    keys += 1;
                    // Keyframes lead with SPS (NAL type 7 after the code).
                    assert_eq!(au.data[4] & 0x1F, 7, "SPS prepended on keyframes");
                }
                video_aus += 1;
            }
            StreamEvent::Au(au) if Some(au.track) == audio => audio_aus += 1,
            StreamEvent::Au(_) => {}
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    // The remux carries the source fixture's AU counts (GOP 60 ⇒ 3 keys).
    assert_eq!(video_aus, 180);
    assert_eq!(audio_aus, 283);
    assert_eq!(keys, 3);
}

#[test]
fn webm_vp9_opus_announces_without_adapters() {
    let mut demux = open("vp9-opus.webm");
    let mut saw_vp9 = false;
    let mut saw_opus = false;
    let mut aus = 0;
    loop {
        match demux.next_event().expect("event") {
            StreamEvent::Format(_, Format::Video { codec, .. }) => {
                assert_eq!(codec, VideoCodec::Vp9);
                saw_vp9 = true;
            }
            StreamEvent::Format(
                _,
                Format::Audio {
                    codec,
                    codec_private,
                    ..
                },
            ) => {
                assert_eq!(codec, AudioCodec::Opus);
                assert!(codec_private.starts_with(b"OpusHead"));
                saw_opus = true;
            }
            StreamEvent::Au(_) => aus += 1,
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    assert!(saw_vp9 && saw_opus);
    assert!(aus > 100, "frames flow even without decode adapters: {aus}");
}

#[test]
fn seek_lands_keyframe_clean_at_or_before_target() {
    let mut demuxer = open("h264-aac.mkv");
    // Drain the format announces first.
    let video = loop {
        match demuxer.next_event().expect("event") {
            StreamEvent::Format(track, Format::Video { .. }) => break track,
            StreamEvent::Eos(_) => panic!("no video format"),
            _ => {}
        }
    };
    // The remux's first video pts is 21 ms: a target before any content
    // legitimately lands there, everything else at or before the target.
    let first_pts = MediaTime::from_micros(21_000);
    for target_ms in [0i64, 1000, 2000, 3000, 5000] {
        let target = MediaTime::from_millis(target_ms);
        let landed = demuxer.seek(target, Generation(2)).expect("seek");
        assert!(
            landed <= target.max(first_pts) + MediaTime::from_millis(1),
            "seek({target_ms}ms) landed late at {landed}"
        );
        // The first video AU from the landing point must be a keyframe
        // with pts at or before the target.
        loop {
            match demuxer.next_event().expect("event") {
                StreamEvent::Au(au) if au.track == video => {
                    assert!(au.key, "seek({target_ms}ms): first video AU not a keyframe");
                    assert!(
                        au.pts <= target.max(first_pts) + MediaTime::from_millis(1),
                        "seek({target_ms}ms): first video AU at {}",
                        au.pts
                    );
                    break;
                }
                StreamEvent::Eos(_) => panic!("seek({target_ms}ms): hit EOS before video"),
                _ => {}
            }
        }
    }
}
