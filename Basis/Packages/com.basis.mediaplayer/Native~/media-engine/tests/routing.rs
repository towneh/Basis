//! What the engine decides a URL is. The classifier picks the transport,
//! and "matched nothing I know" is a refusal rather than a filesystem
//! open — so these rows are about which lane a string reaches, and about
//! the strings that must reach none of them.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use media_engine::{ErrorCategory, OpenRequest, Session, State};

/// Open `url` and wait for the session to settle, then report its state
/// and error category. Nothing here is expected to play, so a short
/// settle is enough; the transports fail fast on an unroutable address.
fn settle(url: &str, allow_local: bool) -> (u32, u32) {
    let mut request = OpenRequest::new(url.to_owned());
    request.allow_local_addresses = allow_local;
    let mut session = Session::open(request);
    let shared = session.shared().clone();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let state = shared.state.load(Ordering::Relaxed);
        if state == State::Error as u32 || state == State::Playing as u32 {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let state = shared.state.load(Ordering::Relaxed);
    let category = shared.last_error_category.load(Ordering::Relaxed);
    session.close();
    (state, category)
}

/// A scheme the engine does not carry is refused as configuration, not
/// handed to the filesystem. Before this, every one of these was opened
/// as a path — which is how a remote peer's string became a local file
/// read, and how a UNC path became an outbound SMB connect.
#[test]
fn an_unknown_scheme_is_a_typed_refusal() {
    for url in [
        "ftp://attacker.example/clip.ts",
        "file:///etc/passwd",
        "gopher://attacker.example/clip.ts",
        "javascript:alert(1)",
        "data:text/plain,hello",
        "smb://attacker.example/share/clip.ts",
    ] {
        let (state, category) = settle(url, false);
        assert_eq!(
            state,
            State::Error as u32,
            "{url:?} settled in state {state} rather than refusing"
        );
        assert_eq!(
            category,
            ErrorCategory::Config as u32,
            "{url:?} refused as category {category}, not a config refusal"
        );
    }
}

/// Schemes are case-insensitive (RFC 3986 §3.1) and the managed
/// classifier compares them that way, so the engine must agree: an
/// uppercase URL is the same request as a lowercase one, and above all
/// is not a local path. Pinned by the error category — these addresses
/// are unroutable, so the lane fails, but it fails as I/O from the
/// transport rather than as a config refusal or a missing file.
#[test]
fn an_uppercase_scheme_routes_where_its_lowercase_twin_does() {
    // A pair per thread. `settle` allows twenty seconds per URL, and
    // these eight addresses are unroutable rather than quick to refuse
    // on every host — a host that drops the SYN reaches the deadline,
    // and run one after another that is over two minutes for one row.
    // Each pair stays sequential on its own thread, which is what keeps
    // its two spellings comparable.
    let pairs = [
        ("HTTP://127.0.0.1:9/clip.ts", "http://127.0.0.1:9/clip.ts"),
        ("HTTPS://127.0.0.1:9/clip.ts", "https://127.0.0.1:9/clip.ts"),
        ("RTSP://127.0.0.1:9/clip", "rtsp://127.0.0.1:9/clip"),
        ("RTSPT://127.0.0.1:9/clip", "rtspt://127.0.0.1:9/clip"),
    ];
    let settled: Vec<_> = pairs
        .into_iter()
        .map(|(upper, lower)| {
            thread::spawn(move || (upper, settle(upper, true), lower, settle(lower, true)))
        })
        .collect();
    for handle in settled {
        let (upper, (upper_state, upper_category), lower, (lower_state, lower_category)) =
            handle.join().expect("the pair settles on its own thread");
        // Settled, not merely finished waiting: `settle` returns on its
        // deadline as well, and two categories compared after a timeout
        // would agree without either URL having reached a transport.
        assert_eq!(upper_state, State::Error as u32, "{upper:?} never settled");
        assert_eq!(lower_state, State::Error as u32, "{lower:?} never settled");
        // The category rather than the settled pair. Both spellings make a
        // real attempt against an unroutable address, and whether each one
        // lands on a refusal or a timeout is the box's to decide under
        // load; the lane they took is what this row is about.
        assert_eq!(
            upper_category, lower_category,
            "{upper:?} and {lower:?} took different lanes"
        );
        assert_ne!(
            upper_category,
            ErrorCategory::Config as u32,
            "{upper:?} was refused outright rather than routed"
        );
    }
}

/// A UNC path is a host, not a place on this machine, and opening one is
/// a network connection the address gate never sees. Refused unless the
/// session has explicitly opted out of that gate, which world content
/// never does. Every spelling Windows accepts is a row: it takes either
/// separator in either of the two leading positions, and all four
/// pairings open the same share.
#[test]
fn a_network_share_path_is_refused_without_the_local_opt_out() {
    for url in [
        r"\\attacker.example\share\clip.ts",
        "//attacker.example/share/clip.ts",
        r"\\?\UNC\attacker.example\share\clip.ts",
        r"\/attacker.example/share/clip.ts",
        r"/\attacker.example\share\clip.ts",
    ] {
        assert!(
            url.starts_with('\\') || url.starts_with('/'),
            "the row's own input lost its leading separators: {url:?}"
        );
        let (state, category) = settle(url, false);
        assert_eq!(
            state,
            State::Error as u32,
            "{url:?} settled in state {state} rather than refusing"
        );
        assert_eq!(
            category,
            ErrorCategory::Config as u32,
            "{url:?} refused as category {category}, not a config refusal"
        );
    }
}

// The rest of this file needs a picture out of the pipeline, so it is
// Windows-only like every other decode-dependent engine row: the Linux
// backend carries no H.264 or AAC decoder, and the fixture cannot decode
// there whatever the routing does. The rows above are decode-free and
// assert the error *category*, so they run on every host.

/// The whole fixture in memory, as the impairment harness wraps a
/// source: the caller owns the bytes and the request's URL never names
/// anything the engine opens.
#[cfg(windows)]
struct SuppliedSource(Vec<u8>);

#[cfg(windows)]
impl media_demux::ByteSource for SuppliedSource {
    fn size(&mut self) -> Result<Option<u64>, media_demux::SourceError> {
        Ok(Some(self.0.len() as u64))
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, media_demux::SourceError> {
        // Past the end is end of stream. Checked in u64 before the cast,
        // which would otherwise discard the upper bits on a 32-bit target
        // and read from a wrapped position instead.
        if offset >= self.0.len() as u64 {
            return Ok(0);
        }
        let from = offset as usize;
        let n = buf.len().min(self.0.len() - from);
        buf[..n].copy_from_slice(&self.0[from..from + n]);
        Ok(n)
    }
}

/// `open_with_source` states that the request's URL is display-only, so
/// a harness may label its source with a string that names no transport
/// at all. Classification must neither refuse the session over a label
/// nor read one as a location: `"case 4"` parses as a relative path, and
/// treating it as one would hand a caller-supplied playlist a filesystem
/// arm rooted at the working directory.
#[cfg(windows)]
#[test]
fn a_caller_supplied_source_plays_under_a_label_that_names_no_transport() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4");
    let bytes = std::fs::read(&fixture).expect("fixture readable");

    for label in ["impairment://case-4", "fixture:burst", "case 4"] {
        let request = OpenRequest::new(label.to_owned());
        let mut session =
            Session::open_with_source(request, Box::new(SuppliedSource(bytes.clone())));
        let shared = session.shared().clone();
        let start = Instant::now();
        let mut played = false;
        while start.elapsed() < Duration::from_secs(30) {
            let state = shared.state.load(Ordering::Relaxed);
            assert_ne!(
                state,
                State::Error as u32,
                "{label:?} was refused, but it labels a source rather than naming one"
            );
            if state == State::Playing as u32 && shared.frames_decoded.load(Ordering::Relaxed) > 0 {
                played = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        session.close();
        assert!(
            played,
            "{label:?} never decoded a frame from the given source"
        );
    }
}

/// An ordinary local file still opens and plays: the file route is a
/// case of the classifier now rather than its fallthrough, and that must
/// not have cost it anything.
#[cfg(windows)]
#[test]
fn an_ordinary_local_file_still_plays() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4");
    let mut session = Session::open(OpenRequest::new(
        fixture.to_str().expect("fixture path is UTF-8").to_owned(),
    ));
    let shared = session.shared().clone();
    let start = Instant::now();
    let mut played = false;
    while start.elapsed() < Duration::from_secs(30) {
        let state = shared.state.load(Ordering::Relaxed);
        assert_ne!(state, State::Error as u32, "the file lane must still play");
        if state == State::Playing as u32 && shared.frames_decoded.load(Ordering::Relaxed) > 0 {
            played = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    session.close();
    assert!(played, "never decoded a frame from a plain local file");
}
