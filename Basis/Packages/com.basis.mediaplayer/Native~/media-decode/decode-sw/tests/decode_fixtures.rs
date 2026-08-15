//! Adapter fixture rows (§12.1's L8 discipline, software edition): each
//! decoder is driven with the AUs its real demuxer produces from the
//! committed sine fixture, and the PCM out is checked for count and
//! content, not just absence of errors.

use decode_sw::{FlacDecoder, OpusDecoder};
use media_clock::Generation;
use media_decode::{AudioDecoder, PcmChunk, SubmitOutcome};
use media_demux::{DemuxLimits, Format, MemSource, StreamEvent, open_auto};

struct Demuxed {
    codec_private: Vec<u8>,
    aus: Vec<(Vec<u8>, i64)>,
}

fn demux(name: &str) -> Demuxed {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/");
    let bytes = std::fs::read(format!("{path}{name}")).expect("fixture readable");
    let mut demuxer = open_auto(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation::default(),
    )
    .expect("open");
    let mut codec_private = Vec::new();
    let mut aus = Vec::new();
    loop {
        match demuxer.next_event().expect("event") {
            StreamEvent::Format(
                _,
                Format::Audio {
                    codec_private: p, ..
                },
            ) => codec_private = p,
            StreamEvent::Au(au) => aus.push((au.data, au.pts.as_micros())),
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    Demuxed { codec_private, aus }
}

fn decode_all(decoder: &mut dyn AudioDecoder, aus: &[(Vec<u8>, i64)]) -> Vec<PcmChunk> {
    let mut chunks = Vec::new();
    for (au, pts) in aus {
        loop {
            match decoder.submit(au, *pts).expect("submit") {
                SubmitOutcome::Accepted => break,
                SubmitOutcome::NotAccepting => {
                    if let Some(chunk) = decoder.try_output().expect("output") {
                        chunks.push(chunk);
                    }
                }
            }
        }
    }
    decoder.begin_drain().expect("drain");
    while let Some(chunk) = decoder.try_output().expect("output") {
        chunks.push(chunk);
    }
    chunks
}

fn sanity(chunks: &[PcmChunk], min_frames: u64, max_frames: u64) {
    let mut frames = 0u64;
    let mut peak = 0.0f32;
    for chunk in chunks {
        assert_eq!(chunk.sample_rate, 48_000);
        assert_eq!(chunk.channels, 2);
        frames += chunk.data.len() as u64 / 2;
        for &s in &chunk.data {
            assert!(s.is_finite());
            peak = peak.max(s.abs());
        }
    }
    assert!(
        (min_frames..=max_frames).contains(&frames),
        "decoded {frames} frames, expected {min_frames}..={max_frames}"
    );
    // A sine fixture has real signal without clipping.
    assert!(peak > 0.05 && peak <= 1.0, "peak {peak} out of range");
}

#[test]
fn flac_fixture_decodes_bit_for_bit_shaped_pcm() {
    let demuxed = demux("sine-48k-stereo.flac");
    let mut decoder = FlacDecoder::new(&demuxed.codec_private).expect("decoder");
    assert_eq!(decoder.output_format(), (48_000, 2));
    let chunks = decode_all(&mut decoder, &demuxed.aus);
    // FLAC is lossless: exactly 6 s at 48 kHz.
    sanity(&chunks, 288_000, 288_000);
    // Chunk pts mirror the demuxer's frame pts.
    assert_eq!(chunks[0].pts_us, 0);
    assert_eq!(chunks[1].pts_us, 96_000);
}

#[test]
fn opus_fixture_decodes_with_pre_skip_before_the_origin() {
    let demuxed = demux("sine-48k-stereo.opus");
    let mut decoder = OpusDecoder::new(&demuxed.codec_private).expect("decoder");
    assert_eq!(decoder.output_format(), (48_000, 2));
    let chunks = decode_all(&mut decoder, &demuxed.aus);
    // 301 packets × 960 samples; the leading pre-skip's worth carries
    // negative pts and is the engine's to drop.
    sanity(&chunks, 301 * 960, 301 * 960);
    assert!(chunks[0].pts_us < 0);
    // Priming is consumed within the first packets: pts crosses zero.
    assert!(chunks.iter().any(|c| c.pts_us >= 0));
}

/// Claxon's channel cap is 8 — the 7.1 fixture decodes to
/// eight-channel PCM end to end. (Surfacing beyond the ring's interleave
/// is the managed splitter's job, not the adapter's.)
#[test]
fn flac_71_fixture_decodes_eight_channels() {
    let demuxed = demux("sine-48k-71.flac");
    let mut decoder = FlacDecoder::new(&demuxed.codec_private).expect("decoder");
    assert_eq!(decoder.output_format(), (48_000, 8));
    let chunks = decode_all(&mut decoder, &demuxed.aus);
    let mut frames = 0u64;
    let mut peak = 0.0f32;
    for chunk in &chunks {
        assert_eq!(chunk.sample_rate, 48_000);
        assert_eq!(chunk.channels, 8);
        frames += chunk.data.len() as u64 / 8;
        for &s in &chunk.data {
            assert!(s.is_finite());
            peak = peak.max(s.abs());
        }
    }
    // Lossless: exactly 6 s at 48 kHz.
    assert_eq!(frames, 288_000);
    assert!(peak > 0.05 && peak <= 1.0, "peak {peak} out of range");
}

#[test]
fn flac_refuses_a_broken_header() {
    assert!(FlacDecoder::new(b"not flac").is_err());
    assert!(OpusDecoder::new(b"not opus").is_err());
}

#[test]
fn opus_refuses_surround_mapping() {
    // OpusHead with mapping family 1 (surround): typed refusal until a
    // multistream decoder exists.
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 6, 0, 0, 128, 187, 0, 0, 0, 0, 1]);
    assert!(OpusDecoder::new(&head).is_err());
}

#[test]
fn av1_fixture_decodes_through_rav1d() {
    use media_decode::VideoDecoder;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/mkv/av1-opus.webm"
    );
    let bytes = std::fs::read(path).expect("fixture readable");
    let mut demuxer = open_auto(
        Box::new(MemSource(bytes)),
        DemuxLimits::default(),
        Generation::default(),
    )
    .expect("open");
    let mut video = None;
    let mut aus = Vec::new();
    loop {
        match demuxer.next_event().expect("event") {
            StreamEvent::Format(track, Format::Video { .. }) => video = Some(track),
            StreamEvent::Au(au) if Some(au.track) == video => {
                aus.push((au.data, au.pts.as_micros()))
            }
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    let mut decoder = decode_sw::SwAv1Decoder::new().expect("decoder");
    let mut frames = 0usize;
    let mut last_pts = i64::MIN;
    for (au, pts) in &aus {
        loop {
            match decoder.submit(au, *pts).expect("submit") {
                SubmitOutcome::Accepted => break,
                SubmitOutcome::NotAccepting => {
                    if let Some(frame) = decoder.try_output().expect("output") {
                        assert!(frame.pts_us() >= last_pts, "pts must not regress");
                        last_pts = frame.pts_us();
                        frames += 1;
                    }
                }
            }
        }
        while let Some(frame) = decoder.try_output().expect("output") {
            let nv12 = frame.as_nv12().expect("nv12");
            assert!(nv12.width > 0 && !nv12.data.is_empty());
            assert!(frame.pts_us() >= last_pts, "pts must not regress");
            last_pts = frame.pts_us();
            frames += 1;
        }
    }
    decoder.begin_drain().expect("drain");
    while let Some(_frame) = decoder.try_output().expect("output") {
        frames += 1;
    }
    assert!(
        frames >= aus.len() - 2,
        "decoded {frames} of {} frames",
        aus.len()
    );
}
