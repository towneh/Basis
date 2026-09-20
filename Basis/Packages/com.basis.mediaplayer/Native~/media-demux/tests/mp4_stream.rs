//! Streaming MP4 demuxer tests over the committed fixtures: the three
//! layouts (faststart, trailing moov, fragmented) must demux identically,
//! events must interleave in decode order, and hostile input must produce
//! typed errors.

mod common;

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

/// A file whose fragments are as far apart as a real long video's: the
/// walk has to pay for the headers it parses, not for a cache block per
/// fragment, or the budget runs out part-way down the file.
#[test]
fn many_fragment_files_open_when_their_fragments_are_spread_out() {
    for name in ["h264-aac-manyfrag.mp4", "h264-aac-manyfrag-sidx.mp4"] {
        let baseline = drain(&mut open(name));
        let inflated = common::inflate(&fixture(name));
        let counters = inflated.counters();
        let len = inflated.len();
        assert!(
            len > DemuxLimits::default().max_metadata_bytes,
            "{name} must outgrow the budget a block per fragment would charge ({len} bytes)"
        );

        let mut demux = Mp4Demuxer::open(Box::new(inflated), DemuxLimits::default(), Generation(1))
            .unwrap_or_else(|e| panic!("{name} spread out must open: {e:?}"));
        let fetched = counters.bytes();
        assert!(
            fetched < 16 * 1024 * 1024,
            "{name} open fetched {fetched} bytes"
        );

        // The padding is past every sample, so the streams are the ones
        // the plain fixture holds.
        let s = drain(&mut demux);
        assert_eq!(s.video_aus, baseline.video_aus, "{name}");
        assert_eq!(s.audio_aus, baseline.audio_aus, "{name}");
        assert_eq!(s.video_keys, baseline.video_keys, "{name}");
        assert_eq!(s.first_video_au, baseline.first_video_au, "{name}");
        assert_eq!(s.first_audio_pts, baseline.first_audio_pts, "{name}");
    }
}

/// Every access unit a demuxer yields from here, in order.
fn access_units(demux: &mut Mp4Demuxer) -> Vec<(u32, MediaTime, MediaTime, bool, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        match demux.next_event().expect("no demux error") {
            StreamEvent::Au(au) => out.push((au.track.0, au.pts, au.dts, au.key, au.data)),
            StreamEvent::Eos(_) => return out,
            _ => {}
        }
    }
}

/// The next `n` access units, or as many as the file has left.
fn first_access_units(
    demux: &mut Mp4Demuxer,
    n: usize,
) -> Vec<(u32, MediaTime, MediaTime, bool, Vec<u8>)> {
    let mut out = Vec::new();
    while out.len() < n {
        match demux.next_event().expect("no demux error") {
            StreamEvent::Au(au) => out.push((au.track.0, au.pts, au.dts, au.key, au.data)),
            StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    out
}

/// The same file read the way one with no usable index is: its index is
/// left describing the layout before the fragments were spread out, so
/// it covers none of the file and the whole of it is walked instead.
fn open_walked(name: &str) -> Mp4Demuxer {
    Mp4Demuxer::open(
        Box::new(common::inflate_untiled(&fixture(name))),
        DemuxLimits::default(),
        Generation(1),
    )
    .unwrap_or_else(|e| panic!("{name} must open by the walk: {e:?}"))
}

fn open_spread(name: &str) -> (Mp4Demuxer, common::Counters) {
    let source = common::inflate(&fixture(name));
    let counters = source.counters();
    let demux = Mp4Demuxer::open(Box::new(source), DemuxLimits::default(), Generation(1))
        .unwrap_or_else(|e| panic!("{name} spread out must open: {e:?}"));
    (demux, counters)
}

/// A file with an index it can be believed on is opened from the index:
/// `ftyp`, `moov` and the index, then the first fragment. Nothing else is
/// read before playback, and what comes out is what a walk of every
/// fragment yields, access unit for access unit.
#[test]
fn an_indexed_file_opens_from_its_index() {
    let (mut indexed, counters) = open_spread("h264-aac-manyfrag-sidx.mp4");
    let fetched = counters.bytes();
    assert!(
        fetched < 1024 * 1024,
        "opening from the index fetched {fetched} bytes"
    );

    let (mut walked, walk_counters) = open_spread("h264-aac-manyfrag.mp4");
    assert!(
        walk_counters.bytes() > fetched * 2,
        "the fixture without an index must cost more to open, or this row \
         proves nothing: {} against {fetched}",
        walk_counters.bytes()
    );
    assert_eq!(
        indexed.duration().map(|d| d.as_millis() / 100),
        walked.duration().map(|d| d.as_millis() / 100),
    );
    assert_eq!(access_units(&mut indexed), access_units(&mut walked));
}

/// The demuxer holds a fragment's worth of sample references, not the
/// file's worth, however far into the file playback has reached.
/// The committed fixture is covered as it stands as well as spread out:
/// its index stops at the `mfra` it ends with, so believing the index at
/// all means reading that box's length off the end of the file.
#[test]
fn reading_a_fragment_at_a_time_does_not_accumulate() {
    for mut demux in [
        open_spread("h264-aac-manyfrag-sidx.mp4").0,
        open("h264-aac-manyfrag-sidx.mp4"),
    ] {
        // To the end of the file, or the claim is only about the part
        // of it that was read: what accumulates does so as the file goes
        // on. The count bounds a hung test, nothing more.
        let mut most = 0usize;
        let mut ended = false;
        for _ in 0..100_000 {
            if matches!(demux.next_event().expect("event"), StreamEvent::Eos(_)) {
                ended = true;
                break;
            }
            most = most.max(demux.held_samples());
        }
        assert!(ended, "the file did not reach its end");
        assert!(most > 0, "the file is read a fragment at a time");
        assert!(most < 200, "held {most} sample references");
    }
}

/// Seeking by the index has to land where a seek over the whole table
/// lands: the same sync sample, and the same stream after it. Swept
/// across the file rather than sampled at a few points, because what a
/// seek lands on turns on where the target falls inside its subsegment.
#[test]
fn an_index_seek_lands_where_the_whole_table_does() {
    // Fragments opening on a keyframe, and fragments whose keyframes are
    // inside them, where the landing is part-way through a fragment and
    // the samples ahead of it belong before it.
    for (name, until) in [
        ("h264-aac-manyfrag-sidx.mp4", 40_500i64),
        ("h264-aac-longfrag-sidx.mp4", 20_500),
    ] {
        let mut targets = 0;
        for ms in (0..until).step_by(137) {
            let target = MediaTime::from_millis(ms);
            let mut indexed = open_spread(name).0;
            let mut walked = open_walked(name);

            let by_index = indexed.seek(target, Generation(2)).expect("index seek");
            let by_table = walked.seek(target, Generation(2)).expect("table seek");
            assert_eq!(by_index, by_table, "{name} landing for {target}");
            assert_eq!(
                first_access_units(&mut indexed, 24),
                first_access_units(&mut walked, 24),
                "{name} stream after {target}"
            );
            targets += 1;
        }
        assert!(targets > 100, "{name}: only {targets} targets swept");

        // And to the end from the start, the middle, and past the last
        // picture, where the audio track runs on alone.
        for ms in [0, until / 2, until * 2] {
            let target = MediaTime::from_millis(ms);
            let mut indexed = open_spread(name).0;
            let mut walked = open_walked(name);
            assert_eq!(
                indexed.seek(target, Generation(3)).expect("index seek"),
                walked.seek(target, Generation(3)).expect("table seek"),
                "{name} landing for {target}"
            );
            assert_eq!(
                access_units(&mut indexed),
                access_units(&mut walked),
                "{name} to the end from {ms} ms"
            );
        }
    }
}

/// An index that does not account for the whole file says nothing
/// trustworthy about where its fragments are, so the file is read the
/// way one with no index at all is.
#[test]
fn an_index_that_covers_nothing_is_not_used() {
    let source = common::inflate_untiled(&fixture("h264-aac-manyfrag-sidx.mp4"));
    let counters = source.counters();
    let mut demux = Mp4Demuxer::open(Box::new(source), DemuxLimits::default(), Generation(1))
        .expect("the file still opens");
    let (mut walked, _) = open_spread("h264-aac-manyfrag.mp4");
    assert!(
        counters.bytes() > 4 * 1024 * 1024,
        "the whole file was walked, not opened from the index"
    );
    assert_eq!(access_units(&mut demux), access_units(&mut walked));
}

/// Opening a file fetches the metadata it parses and nothing past it.
/// A progressive file's `moov` holds every sample table, so on a long
/// one it is megabytes and the box behind it sits that far into the
/// file: reading its header to find out whether this file has fragments
/// costs a whole cache block of media, and answers nothing that `moov`
/// has not already said.
#[test]
fn opening_a_progressive_file_reads_no_further_than_its_moov() {
    let fixture = fixture("h264-aac-640x360-30fps.mp4");
    let baseline = drain(&mut open("h264-aac-640x360-30fps.mp4"));

    // A `moov` four cache blocks long, so anything read past it shows.
    let source = common::pad_moov(&fixture, 1024 * 1024);
    let counters = source.counters();
    let mut demux = Mp4Demuxer::open(Box::new(source), DemuxLimits::default(), Generation(1))
        .expect("a padded moov still opens");
    let fetched = counters.bytes();

    assert_eq!(
        demux.duration().map(|d| d.as_millis() / 100),
        Some(60),
        "the padded file is still the fixture"
    );
    assert_eq!(demux.take_notes(), Vec::<String>::new());
    assert!(
        fetched <= 512 * 1024,
        "open fetched {fetched} bytes; the moov and what the parse needs of it is one block,          so this is a block of media read to look at a box header"
    );
    // The tracks are the fixture's: padding `moov` does not disturb what
    // it states, only where the media behind it sits.
    assert_eq!(baseline.video_aus, 180);
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
