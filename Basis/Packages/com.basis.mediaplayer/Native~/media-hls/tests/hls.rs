//! HLS scheduler + chaining tests over a virtual fetcher: VOD both
//! container flavours, live window advance, window fall-out,
//! stated discontinuities, master variant choice, feature refusals, and
//! seek-to-segment. Segment bytes come from the committed HLS fixtures;
//! time is virtual (recorded waits, no sleeps).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use media_clock::{Generation, MediaTime};
use media_demux::{DemuxError, DemuxLimits, Demuxer, Format, SourceError, StreamEvent};
use media_hls::{HlsDemuxer, ParsedPlaylist, SegmentFetcher, looks_like_playlist};

fn fixture_dir(kind: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../fixtures/hls/{kind}"))
}

#[derive(Default)]
struct FetchLog {
    fetched: Vec<String>,
    waits: Vec<Duration>,
}

/// Virtual fetcher: static resources plus a sequence of playlist bodies
/// served for successive fetches of the playlist URL (a live refresh).
struct MockFetcher {
    resources: HashMap<String, Vec<u8>>,
    playlist_url: String,
    refreshes: Vec<Vec<u8>>,
    refresh_cursor: usize,
    log: Arc<Mutex<FetchLog>>,
}

impl MockFetcher {
    fn new(playlist_url: &str, refreshes: Vec<Vec<u8>>) -> (Self, Arc<Mutex<FetchLog>>) {
        let log = Arc::new(Mutex::new(FetchLog::default()));
        (
            Self {
                resources: HashMap::new(),
                playlist_url: playlist_url.to_string(),
                refreshes,
                refresh_cursor: 0,
                log: Arc::clone(&log),
            },
            log,
        )
    }

    fn with_file(mut self, url: &str, path: PathBuf) -> Self {
        self.resources
            .insert(url.to_string(), std::fs::read(path).expect("fixture bytes"));
        self
    }
}

impl SegmentFetcher for MockFetcher {
    fn fetch(&mut self, url: &str, _cap: u64) -> Result<Vec<u8>, SourceError> {
        self.log.lock().unwrap().fetched.push(url.to_string());
        if url == self.playlist_url {
            let body = self.refreshes[self.refresh_cursor.min(self.refreshes.len() - 1)].clone();
            self.refresh_cursor += 1;
            return Ok(body);
        }
        self.resources
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no such resource: {url}").into())
    }

    fn wait(&mut self, duration: Duration) {
        self.log.lock().unwrap().waits.push(duration);
    }
}

const BASE: &str = "https://test/index.m3u8";

fn open(playlist: &str, fetcher: MockFetcher) -> Result<HlsDemuxer, DemuxError> {
    HlsDemuxer::open(
        BASE,
        playlist.as_bytes().to_vec(),
        Box::new(fetcher),
        DemuxLimits::default(),
        Generation(0),
    )
}

struct Drained {
    /// Video dts per AU (decode order; pts reorders under B-frames).
    video_aus: Vec<MediaTime>,
    max_video_pts: MediaTime,
    audio_aus: usize,
    video_formats: usize,
    audio_formats: usize,
    discontinuities: usize,
}

fn drain(demuxer: &mut dyn Demuxer) -> Result<Drained, DemuxError> {
    let mut out = Drained {
        video_aus: Vec::new(),
        max_video_pts: MediaTime::ZERO,
        audio_aus: 0,
        video_formats: 0,
        audio_formats: 0,
        discontinuities: 0,
    };
    let mut video_track = None;
    let mut audio_track = None;
    loop {
        match demuxer.next_event()? {
            StreamEvent::Format(track, Format::Video { .. }) => {
                video_track = Some(track);
                out.video_formats += 1;
            }
            StreamEvent::Format(track, Format::Audio { .. }) => {
                audio_track = Some(track);
                out.audio_formats += 1;
            }
            StreamEvent::Au(au) if Some(au.track) == video_track => {
                out.max_video_pts = out.max_video_pts.max(au.pts);
                out.video_aus.push(au.dts);
            }
            StreamEvent::Au(au) if Some(au.track) == audio_track => out.audio_aus += 1,
            StreamEvent::Au(_) => {}
            StreamEvent::Discontinuity(..) => out.discontinuities += 1,
            StreamEvent::Eos(_) => return Ok(out),
            _ => {}
        }
    }
}

fn ts_fetcher(refreshes: Vec<Vec<u8>>) -> MockFetcher {
    let dir = fixture_dir("ts");
    let (fetcher, _) = MockFetcher::new(BASE, refreshes);
    fetcher
        .with_file("https://test/seg000.ts", dir.join("seg000.ts"))
        .with_file("https://test/seg001.ts", dir.join("seg001.ts"))
        .with_file("https://test/seg002.ts", dir.join("seg002.ts"))
}

const VOD_TS: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nseg000.ts\n#EXTINF:2.0,\nseg001.ts\n#EXTINF:2.0,\nseg002.ts\n#EXT-X-ENDLIST\n";

#[test]
fn vod_ts_plays_every_segment_through_one_demuxer() {
    let mut demuxer = open(VOD_TS, ts_fetcher(vec![])).expect("open");
    assert!(!demuxer.is_live());
    assert_eq!(demuxer.duration(), Some(MediaTime::from_secs(6)));
    let drained = drain(&mut demuxer).expect("drain");
    // The conformance counts for the source fixture: chaining must not
    // lose an AU at any segment boundary.
    assert_eq!(drained.video_aus.len(), 180);
    assert_eq!(drained.audio_aus, 283);
    assert_eq!(drained.video_formats, 1, "one video announce, deduped");
    assert_eq!(drained.audio_formats, 1);
    assert_eq!(drained.discontinuities, 0);
    let mut sorted = drained.video_aus.clone();
    sorted.sort();
    assert_eq!(sorted, drained.video_aus, "video dts monotonic");
}

#[test]
fn vod_fmp4_plays_every_segment_with_absolute_timestamps() {
    let dir = fixture_dir("fmp4");
    let (fetcher, _) = MockFetcher::new(BASE, vec![]);
    let fetcher = fetcher
        .with_file("https://test/init.mp4", dir.join("init.mp4"))
        .with_file("https://test/seg000.m4s", dir.join("seg000.m4s"))
        .with_file("https://test/seg001.m4s", dir.join("seg001.m4s"))
        .with_file("https://test/seg002.m4s", dir.join("seg002.m4s"));
    // EXT-X-MAP appears once and applies to every later segment.
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:2.0,\nseg000.m4s\n#EXTINF:2.0,\nseg001.m4s\n#EXTINF:2.0,\nseg002.m4s\n#EXT-X-ENDLIST\n";
    let mut demuxer = open(playlist, fetcher).expect("open");
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(drained.video_aus.len(), 180);
    assert_eq!(drained.audio_aus, 283);
    assert_eq!(drained.video_formats, 1);
    let mut sorted = drained.video_aus.clone();
    sorted.sort();
    assert_eq!(sorted, drained.video_aus, "tfdt keeps dts absolute");
    let last = drained.max_video_pts;
    assert!(last > MediaTime::from_millis(5900), "tail reached: {last}");
}

#[test]
fn live_window_advances_and_ends() {
    // One segment visible per refresh; ENDLIST arrives with the last.
    let win = |segments: &str, end: bool| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n{segments}{}",
            if end { "#EXT-X-ENDLIST\n" } else { "" }
        )
        .into_bytes()
    };
    let refreshes = vec![
        win("#EXTINF:2.0,\nseg000.ts\n#EXTINF:2.0,\nseg001.ts\n", false),
        win(
            "#EXTINF:2.0,\nseg000.ts\n#EXTINF:2.0,\nseg001.ts\n#EXTINF:2.0,\nseg002.ts\n",
            true,
        ),
    ];
    let initial = win("#EXTINF:2.0,\nseg000.ts\n", false);
    let fetcher = ts_fetcher(refreshes);
    let log = Arc::clone(&fetcher.log);
    let mut demuxer = open(std::str::from_utf8(&initial).unwrap(), fetcher).expect("open");
    assert!(demuxer.is_live());
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(drained.video_aus.len(), 180, "all three segments played");
    assert_eq!(drained.discontinuities, 0);
    let log = log.lock().unwrap();
    assert!(!log.waits.is_empty(), "refreshes waited between fetches");
    assert!(
        log.waits.iter().all(|w| *w >= Duration::from_millis(500)),
        "refresh cadence respects the floor"
    );
}

#[test]
fn live_join_starts_three_segments_from_the_edge() {
    // Five segments in the initial window: the join point is sequence 2.
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nmissing0.ts\n#EXTINF:2.0,\nmissing1.ts\n\
#EXTINF:2.0,\nseg000.ts\n#EXTINF:2.0,\nseg001.ts\n#EXTINF:2.0,\nseg002.ts\n";
    let end = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nmissing0.ts\n#EXTINF:2.0,\nmissing1.ts\n\
#EXTINF:2.0,\nseg000.ts\n#EXTINF:2.0,\nseg001.ts\n#EXTINF:2.0,\nseg002.ts\n#EXT-X-ENDLIST\n";
    let fetcher = ts_fetcher(vec![end.as_bytes().to_vec()]);
    let log = Arc::clone(&fetcher.log);
    let mut demuxer = open(playlist, fetcher).expect("open");
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(drained.video_aus.len(), 180, "joined at the edge backoff");
    let fetched = log.lock().unwrap().fetched.clone();
    assert_eq!(
        fetched.first().map(String::as_str),
        Some("https://test/seg000.ts"),
        "first fetch is the join point, not the window start: {fetched:?}"
    );
    assert!(
        !fetched.iter().any(|u| u.contains("missing")),
        "segments behind the join point are never fetched"
    );
}

#[test]
fn window_fallout_jumps_forward_with_a_discontinuity() {
    // The window races past the cursor: refresh drops straight to
    // sequence 2.
    let refreshes = vec![
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:2\n\
#EXTINF:2.0,\nseg002.ts\n#EXT-X-ENDLIST\n"
            .as_bytes()
            .to_vec(),
    ];
    let initial = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nseg000.ts\n";
    let fetcher = ts_fetcher(refreshes);
    let mut demuxer = open(initial, fetcher).expect("open");
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(
        drained.discontinuities, 1,
        "the jump surfaces as a discontinuity"
    );
    assert_eq!(drained.video_aus.len(), 120, "seg000 + seg002 played");
    let notes = demuxer.take_notes();
    assert!(
        notes.iter().any(|n| n.contains("window advanced")),
        "fall-out noted: {notes:?}"
    );
}

#[test]
fn stated_discontinuity_rebuilds_and_reports() {
    // The same segment twice with a stated splice: timestamps restart,
    // the TS demuxer rebuilds, downstream hears about it.
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nseg000.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:2.0,\nseg000.ts\n#EXT-X-ENDLIST\n";
    let mut demuxer = open(playlist, ts_fetcher(vec![])).expect("open");
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(drained.discontinuities, 1);
    assert_eq!(drained.video_aus.len(), 120);
}

#[test]
fn master_playlist_picks_the_highest_bandwidth_variant() {
    let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nhigh.m3u8\n";
    let (fetcher, log) = MockFetcher::new(BASE, vec![]);
    let dir = fixture_dir("ts");
    let mut fetcher = fetcher
        .with_file("https://test/seg000.ts", dir.join("seg000.ts"))
        .with_file("https://test/seg001.ts", dir.join("seg001.ts"))
        .with_file("https://test/seg002.ts", dir.join("seg002.ts"));
    fetcher
        .resources
        .insert("https://test/high.m3u8".into(), VOD_TS.as_bytes().to_vec());
    let mut demuxer = open(master, fetcher).expect("open");
    let notes = demuxer.take_notes();
    assert!(
        notes.iter().any(|n| n.contains("800000")),
        "variant choice noted: {notes:?}"
    );
    let drained = drain(&mut demuxer).expect("drain");
    assert_eq!(drained.video_aus.len(), 180);
    let fetched = log.lock().unwrap().fetched.clone();
    assert!(fetched.iter().any(|u| u.ends_with("high.m3u8")));
    assert!(!fetched.iter().any(|u| u.ends_with("low.m3u8")));
}

#[test]
fn unsupported_features_refuse_at_open() {
    for (playlist, what) in [
        (
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:2.0,\nseg000.ts\n#EXT-X-ENDLIST\n",
            "encryption",
        ),
        (
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n\
#EXTINF:2.0,\n#EXT-X-BYTERANGE:1000@0\nseg000.ts\n#EXT-X-ENDLIST\n",
            "byte ranges",
        ),
        (
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-I-FRAMES-ONLY\n\
#EXTINF:2.0,\nseg000.ts\n#EXT-X-ENDLIST\n",
            "iframe-only",
        ),
    ] {
        match open(playlist, ts_fetcher(vec![])) {
            Err(DemuxError::Unsupported(_)) => {}
            Err(other) => panic!("{what}: expected Unsupported, got {other:?}"),
            Ok(_) => panic!("{what}: expected a typed refusal, got a demuxer"),
        }
    }
}

#[test]
fn vod_seek_lands_on_the_target_segment() {
    let mut demuxer = open(VOD_TS, ts_fetcher(vec![])).expect("open");
    // Pull a few events to learn the timeline origin.
    let mut origin = None;
    while origin.is_none() {
        if let StreamEvent::Au(au) = demuxer.next_event().expect("event") {
            origin = Some(au.pts);
        }
    }
    let origin = origin.unwrap();

    let landed = demuxer
        .seek(origin + MediaTime::from_millis(3500), Generation(1))
        .expect("seek supported on HLS VOD");
    assert_eq!(landed, origin + MediaTime::from_secs(2), "segment start");

    // The next video AUs come from segment 1 with the new generation.
    // Formats are deduped across the seek, so track identity comes from
    // the demuxer, not a re-announce.
    let video_track = demuxer.video_track();
    assert!(video_track.is_some(), "video track learned before the seek");
    loop {
        match demuxer.next_event().expect("event") {
            StreamEvent::Au(au) if Some(au.track) == video_track => {
                assert_eq!(au.generation, Generation(1));
                assert!(
                    au.pts >= origin + MediaTime::from_millis(1900),
                    "resumed at segment 1, got {}",
                    au.pts
                );
                break;
            }
            StreamEvent::Eos(_) => panic!("ended before the post-seek AU"),
            _ => {}
        }
    }
}

#[test]
fn seek_on_a_live_playlist_is_refused() {
    let initial = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXTINF:2.0,\nseg000.ts\n";
    let mut demuxer = open(initial, ts_fetcher(vec![])).expect("open");
    match demuxer.seek(MediaTime::from_secs(1), Generation(1)) {
        Err(DemuxError::Unsupported(_)) => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn playlist_sniff_tolerates_bom_and_whitespace() {
    assert!(looks_like_playlist(b"#EXTM3U\n#EXT-X-VERSION:3\n"));
    assert!(looks_like_playlist(b"\xEF\xBB\xBF#EXTM3U\n"));
    assert!(looks_like_playlist(b"\r\n#EXTM3U\n"));
    assert!(!looks_like_playlist(b"{\"not\": \"a playlist\"}"));
    assert!(!looks_like_playlist(&[0x47, 0x40, 0x00, 0x10]));
}

/// Fuzz-found (first hls_playlist campaign): a hostile EXTINF duration
/// must be a typed cap refusal, not a MediaTime overflow in the
/// cumulative-duration folds. Stated on a playlist that is otherwise
/// well-formed, so the duration is what the refusal is about — the
/// pinned campaign input below carries several hostile shapes at once
/// and is not a witness for any one of them.
#[test]
fn hostile_extinf_duration_is_a_cap_refusal() {
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:2679\n\
#EXTINF:44444444444444445.988,\nseg2680.ts\n#EXT-X-ENDLIST\n";
    match media_hls::parse_playlist(playlist.as_bytes(), BASE) {
        Err(DemuxError::Cap(_)) => {}
        Err(other) => panic!("expected a cap refusal, got {other:?}"),
        Ok(_) => panic!("hostile duration parsed"),
    }
}

/// The campaign input itself: whatever the parser makes of it, the
/// answer is a typed error rather than a panic or an unbounded
/// allocation. Which refusal fires is not pinned — the input carries a
/// hostile duration, a corrupt tag and a garbage URI together, and the
/// first one reached is an implementation detail that has moved before.
#[test]
fn the_pinned_campaign_input_refuses_without_panicking() {
    let bytes = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/hls-playlist/extinf-overflow.m3u8"),
    )
    .expect("pinned input");
    assert!(
        media_hls::parse_playlist(&bytes, BASE).is_err(),
        "the campaign input must refuse"
    );
}

/// A playlist opened from disk names its resources beside itself. Path
/// joining would drop the base entirely for an absolute URI and walk out
/// of it for a `..`, so resolution refuses anything that is not a plain
/// relative name rather than handing the fetcher a path to disown. A
/// plain relative URI, with or without a `./`, still resolves.
///
/// Every row here holds on every host, which is the point of the list
/// rather than an accident of it. `Path` parses only the syntax of the
/// platform it was built for, so a Unix build reads the Windows rows as
/// ordinary filenames and would take a playlist written to attack a
/// Windows client; those shapes are screened as text so that the answer
/// does not depend on who is running the test.
#[test]
fn a_disk_playlist_refuses_a_uri_outside_its_directory() {
    let local_base = "/srv/fixtures/index.m3u8";
    let playlist = |uri: &str| {
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{uri}\n#EXT-X-ENDLIST\n")
            .into_bytes()
    };

    for uri in [
        // Recognised by Path on any host.
        "/etc/passwd",
        "../../../etc/passwd",
        "sub/../../escape.ts",
        // Windows syntax, refused off Windows too.
        r"C:\Windows\win.ini",
        "c:win.ini",
        "C:/Windows/win.ini",
        r"\\attacker.example\share\clip.ts",
        r"sub\..\..\escape.ts",
    ] {
        match media_hls::parse_playlist(&playlist(uri), local_base) {
            Err(DemuxError::Parse(detail)) => assert!(
                detail.contains("outside the playlist's directory"),
                "{uri:?} refused for the wrong reason: {detail}"
            ),
            Err(other) => panic!("{uri:?}: expected a resolution refusal, got {other:?}"),
            Ok(_) => panic!("{uri:?} resolved instead of refusing"),
        }
    }

    for uri in ["seg000.ts", "./seg000.ts", "nested/seg000.ts"] {
        media_hls::parse_playlist(&playlist(uri), local_base)
            .unwrap_or_else(|e| panic!("{uri:?} is a plain relative URI and must resolve: {e:?}"));
    }
}

/// A playlist named without a directory — `bm-probe play index.m3u8`
/// from inside the fixture directory — resolves its segments against the
/// current directory explicitly. The fetcher is built confined to a
/// directory and judges what it is handed against that root, so a bare
/// name coming back here would be disowned there and the playlist would
/// open and then fetch nothing.
#[test]
fn a_playlist_named_without_a_directory_resolves_against_the_current_one() {
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg000.ts\n#EXT-X-ENDLIST\n";
    let parsed = media_hls::parse_playlist(playlist.as_bytes(), "index.m3u8").expect("resolves");
    let ParsedPlaylist::Media(window) = parsed else {
        panic!("media playlist expected")
    };
    let resolved = std::path::Path::new(&window.segments[0].url);
    assert!(
        resolved.strip_prefix(".").is_ok(),
        "must sit under the same root the fetcher is given: {:?}",
        window.segments[0].url
    );
}

/// A playlist served over the network may not name resources of a
/// different kind. Every URI resolves through URL joining and the result
/// has to be one the fetcher can actually go and get, so a scheme that
/// would instead land on its filesystem arm is refused at resolution.
/// The drive-letter forms are the ones that matter on Windows: `c://x`
/// carries the `://` that used to mark a URI as absolute and pass it
/// through untouched, and `c:/x` reads as a one-character scheme.
#[test]
fn a_network_playlist_cannot_change_a_uri_to_an_unfetchable_scheme() {
    let playlist = |uri: &str| {
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{uri}\n#EXT-X-ENDLIST\n")
            .into_bytes()
    };

    for uri in [
        "c://Windows/win.ini",
        "c:/Windows/win.ini",
        "C://Windows//System32/drivers/etc/hosts",
        "file:///etc/passwd",
        "ftp://attacker.example/clip.ts",
        "data:text/plain,hello",
    ] {
        match media_hls::parse_playlist(&playlist(uri), BASE) {
            Err(DemuxError::Parse(detail)) => assert!(
                detail.contains("unfetchable"),
                "{uri:?} refused for the wrong reason: {detail}"
            ),
            Err(other) => panic!("{uri:?}: expected a scheme refusal, got {other:?}"),
            Ok(_) => panic!("{uri:?} resolved instead of refusing"),
        }
    }

    // What a real playlist does, all still fine: relative, root-relative,
    // absolute onto another host (a CDN split across origins), and a
    // scheme change between the two fetchable ones.
    for uri in [
        "seg000.ts",
        "/other/seg000.ts",
        "https://cdn.example/seg000.ts",
        "http://cdn.example/seg000.ts",
    ] {
        media_hls::parse_playlist(&playlist(uri), BASE)
            .unwrap_or_else(|e| panic!("{uri:?} is an ordinary playlist URI: {e:?}"));
    }
}

/// A playlist on disk may still name network resources — that is an
/// ordinary local fixture pointing at a CDN, and the address gate vets
/// it at the fetcher. Only the schemes that would reach the filesystem
/// arm are refused.
#[test]
fn a_disk_playlist_may_still_name_network_resources() {
    let playlist = |uri: &str| {
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{uri}\n#EXT-X-ENDLIST\n")
            .into_bytes()
    };
    let local_base = "/srv/fixtures/index.m3u8";

    for uri in ["https://cdn.example/seg000.ts", "http://cdn.example/seg.ts"] {
        let parsed = media_hls::parse_playlist(&playlist(uri), local_base)
            .unwrap_or_else(|e| panic!("{uri:?} must resolve from a disk playlist: {e:?}"));
        match parsed {
            ParsedPlaylist::Media(window) => assert_eq!(
                window.segments[0].url, uri,
                "the URI stays whole for the fetcher to vet"
            ),
            ParsedPlaylist::Master(_) => panic!("media playlist expected"),
        }
    }

    // A scheme that would land on the filesystem arm is still refused.
    for uri in ["file:///etc/passwd", "c://Windows/win.ini"] {
        assert!(
            media_hls::parse_playlist(&playlist(uri), local_base).is_err(),
            "{uri:?} must not resolve from a disk playlist"
        );
    }
}

/// The scheduler's notes are drained once, before playback starts, so
/// nothing empties them again for the life of a live session — and a live
/// session records one on every window jump and every segment it has to
/// skip. Over hours that is unbounded growth on the demux thread, so the
/// collection is capped like the demuxers' own.
#[test]
fn scheduler_notes_stay_bounded_over_a_long_live_session() {
    // Three segments per window, so the live join point is the window
    // start and every one of them is reached; none of them is fetchable,
    // so each is a skip note. Each refresh moves the window far past the
    // cursor, which is a jump note on top.
    let window = |first: u64| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{first}\n\
             #EXTINF:2.0,\ngone{first}a.ts\n#EXTINF:2.0,\ngone{first}b.ts\n\
             #EXTINF:2.0,\ngone{first}c.ts\n"
        )
    };
    // Every refresh offers at least its own window-jump note, so asking
    // for one more window than the cap holds overruns it whatever the cap
    // is set to. Derived rather than fixed: a count chosen against today's
    // cap turns a raised cap into a failing row about nothing.
    let windows = media_demux::MAX_NOTES as u64 + 1;
    let refreshes: Vec<Vec<u8>> = (1..=windows)
        .map(|i| window(i * 100).into_bytes())
        .collect();
    let (fetcher, _log) = MockFetcher::new(BASE, refreshes);
    let mut demuxer = open(&window(0), fetcher).expect("a live playlist opens");
    // One jump note per window, plus a skip note per segment in it.
    for _ in 0..10_000 {
        match demuxer.next_event() {
            Ok(StreamEvent::Eos(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let notes = demuxer.take_notes();
    assert_eq!(
        notes.len(),
        media_demux::MAX_NOTES,
        "filled and stopped rather than growing with the session: {notes:?}"
    );
}
