//! Raw audio demuxer rows: sniffed routing, pinned AU counts against
//! ffprobe's packet view, exact pts arithmetic, metadata-frame and
//! pre-skip handling.

use media_clock::{Generation, MediaTime};
use media_demux::{
    Au, AudioCodec, ContainerKind, DemuxLimits, Format, MemSource, StreamEvent, open_auto,
    sniff_container,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../fixtures/");
    std::fs::read(format!("{path}{name}")).expect("fixture readable")
}

struct Run {
    format: Format,
    aus: Vec<Au>,
}

fn run(name: &str) -> Run {
    let bytes = fixture(name);
    let mut demuxer = open_auto(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation::default(),
    )
    .expect("open");
    let mut format = None;
    let mut aus = Vec::new();
    loop {
        match demuxer.next_event().expect("event") {
            StreamEvent::Format(_, f) => format = Some(f),
            StreamEvent::Au(au) => aus.push(au),
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    Run {
        format: format.expect("a format announce"),
        aus,
    }
}

#[test]
fn sniffs_route_each_container() {
    assert_eq!(
        sniff_container(&fixture("sine-48k-stereo.flac")[..64]),
        Some(ContainerKind::Flac)
    );
    assert_eq!(
        sniff_container(&fixture("sine-48k-stereo.opus")[..64]),
        Some(ContainerKind::Ogg)
    );
    // LAME writes no ID3 by default via ffmpeg; the bare-sync path must
    // classify by layer bits.
    let mp3 = fixture("sine-48k-stereo.mp3");
    assert_eq!(
        sniff_container(&mp3[..64.min(mp3.len())]),
        Some(ContainerKind::Mp3)
    );
    assert_eq!(
        sniff_container(&fixture("sine-48k-stereo.aac")[..64]),
        Some(ContainerKind::Adts)
    );
    // An ID3v2 header routes to the MP3 demuxer.
    let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
    tagged.extend_from_slice(&[0u8; 32]);
    assert_eq!(sniff_container(&tagged), Some(ContainerKind::Mp3));
}

#[test]
fn flac_frames_match_ffprobe_and_carry_exact_pts() {
    let run = run("sine-48k-stereo.flac");
    let Format::Audio {
        codec,
        sample_rate,
        channels,
        codec_private,
    } = &run.format
    else {
        panic!("audio format expected");
    };
    assert_eq!(*codec, AudioCodec::Flac);
    assert_eq!(*sample_rate, 48_000);
    assert_eq!(*channels, 2);
    assert!(codec_private.starts_with(b"fLaC"));
    // ffprobe: 63 packets over 6 s.
    assert_eq!(run.aus.len(), 63);
    assert_eq!(run.aus[0].pts, MediaTime::ZERO);
    // ffmpeg's flac encoder uses 4608-sample blocks: frame 1 at 96 ms.
    assert_eq!(run.aus[1].pts, MediaTime::from_micros(96_000));
    let last = run.aus.last().expect("frames");
    assert_eq!(
        last.pts,
        MediaTime::from_micros(62 * 4608 * 1_000_000 / 48_000)
    );
}

/// The 7.1 lane. FLAC carries 8 channels through the demuxer
/// unscreened (claxon's cap is 8, the decoder takes it), while ADTS AAC
/// with channel_configuration 7 refuses typed at open — wider than the
/// 1..=6 screen the AAC lanes share.
#[test]
fn flac_71_demuxes_eight_channels() {
    let run = run("sine-48k-71.flac");
    let Format::Audio {
        codec,
        sample_rate,
        channels,
        ..
    } = &run.format
    else {
        panic!("audio format expected");
    };
    assert_eq!(*codec, AudioCodec::Flac);
    assert_eq!(*sample_rate, 48_000);
    assert_eq!(*channels, 8);
    // ffprobe: 63 packets over 6 s, same blocking as the stereo fixture.
    assert_eq!(run.aus.len(), 63);
}

#[test]
fn adts_71_refuses_typed_on_the_channel_screen() {
    let bytes = fixture("sine-48k-71.aac");
    let result = open_auto(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation::default(),
    );
    match result {
        Err(media_demux::DemuxError::Unsupported(what)) => {
            assert!(
                what.contains("channel configuration"),
                "refusal named {what:?}"
            );
        }
        Err(other) => panic!("expected a typed channel-screen refusal, got {other:?}"),
        Ok(_) => panic!("expected a typed channel-screen refusal, got an open demuxer"),
    }
}

#[test]
fn mp3_skips_the_xing_frame_and_counts_samples() {
    let run = run("sine-48k-stereo.mp3");
    let Format::Audio {
        codec,
        sample_rate,
        channels,
        ..
    } = &run.format
    else {
        panic!("audio format expected");
    };
    assert_eq!(*codec, AudioCodec::Mp3);
    assert_eq!(*sample_rate, 48_000);
    assert_eq!(*channels, 2);
    // ffprobe also sees 251 packets: its demuxer consumes the leading
    // Info frame as metadata exactly as this one must (the fixture's
    // stored stream has 252 frames).
    assert_eq!(run.aus.len(), 251);
    assert_eq!(run.aus[0].pts, MediaTime::ZERO);
    // The first AU is real audio, not the Info frame.
    assert_ne!(&run.aus[0].data[36..40], b"Info");
    // 1152 samples per MPEG-1 Layer III frame.
    assert_eq!(
        run.aus[1].pts,
        MediaTime::from_micros(1_152 * 1_000_000 / 48_000)
    );
}

#[test]
fn adts_strips_headers_and_reconstructs_the_asc() {
    let run = run("sine-48k-stereo.aac");
    let Format::Audio {
        codec,
        sample_rate,
        channels,
        codec_private,
    } = &run.format
    else {
        panic!("audio format expected");
    };
    assert_eq!(*codec, AudioCodec::Aac);
    assert_eq!(*sample_rate, 48_000);
    assert_eq!(*channels, 2);
    // AAC-LC (AOT 2), 48 kHz (index 3), stereo (config 2).
    assert_eq!(codec_private, &vec![0x11u8, 0x90]);
    assert_eq!(run.aus.len(), 283);
    // Payloads carry no ADTS sync of their own.
    assert!(run.aus[0].data.len() > 4);
    assert_eq!(
        run.aus[1].pts,
        MediaTime::from_micros(1_024 * 1_000_000 / 48_000)
    );
}

#[test]
fn ogg_opus_applies_pre_skip_as_negative_lead_in() {
    let run = run("sine-48k-stereo.opus");
    let Format::Audio {
        codec,
        sample_rate,
        channels,
        codec_private,
    } = &run.format
    else {
        panic!("audio format expected");
    };
    assert_eq!(*codec, AudioCodec::Opus);
    assert_eq!(*sample_rate, 48_000);
    assert_eq!(*channels, 2);
    assert!(codec_private.starts_with(b"OpusHead"));
    assert_eq!(run.aus.len(), 301);
    // Pre-skip pushes the first packets before the origin; playback time
    // reaches zero once the priming samples are consumed.
    assert!(run.aus[0].pts < MediaTime::ZERO);
    let pre_skip = i64::from(u16::from_le_bytes([codec_private[10], codec_private[11]]));
    assert_eq!(
        run.aus[0].pts,
        MediaTime::from_micros(-pre_skip * 1_000_000 / 48_000)
    );
    // 20 ms packets: consecutive pts step by exactly 960 samples.
    let step = run.aus[1].pts - run.aus[0].pts;
    assert_eq!(step, MediaTime::from_micros(20_000));
    // The stream must cross the origin and cover ~6 s of audio.
    let last = run.aus.last().expect("packets");
    assert!(last.pts > MediaTime::from_micros(5_900_000));
}
