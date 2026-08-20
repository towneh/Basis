//! What an HLS playlist may reach, end to end through the engine: a
//! playlist served over HTTP names a real, readable, playable local file
//! and must not get it. The fetcher's own rows pin the refusal; these
//! pin the wiring that decides which fetcher the lane is given, which is
//! the half a unit test cannot see.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use media_engine::{ErrorCategory, OpenRequest, Session, SourceLiveness, State};

/// Serve `playlist` at `/index.m3u8` and **nothing else**, 200 with a
/// length, ranges ignored. Everything other than the playlist path is a
/// 404 on purpose: serving the playlist body for any path would let a
/// segment URI that resolved back to this origin be answered with
/// playlist bytes, so the session would fail at demux and a row would
/// pass while asserting nothing about where the URI was allowed to go.
fn spawn_playlist_server(playlist: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let body = Arc::new(playlist.into_bytes());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let body = Arc::clone(&body);
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                let mut wants_playlist = false;
                let mut request_line = true;
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line.trim_end().is_empty() {
                        break;
                    }
                    if request_line {
                        wants_playlist = line.contains(" /index.m3u8 ");
                        request_line = false;
                    }
                    line.clear();
                }
                if !wants_playlist {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });
    format!("http://127.0.0.1:{port}/index.m3u8")
}

/// A copy of the TS fixture somewhere off the playlist's origin. Real,
/// readable, and playable — so a session that reaches it plays, and the
/// test can tell "refused" from "failed for some other reason".
///
/// Removed on drop rather than at the end of the row, because a failed
/// assertion panics past any cleanup written after it — and the
/// directory is named for the process, so a run that fails would leave
/// one behind every time.
struct Planted(std::path::PathBuf);

impl Drop for Planted {
    fn drop(&mut self) {
        let dir = self.0.parent().map(std::path::Path::to_path_buf);
        let _ = std::fs::remove_file(&self.0);
        if let Some(dir) = dir {
            // Refuses while it holds anything, so a sibling row's own
            // planted file is safe from this.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn planted_fixture(name: &str) -> Planted {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.ts");
    let dir = std::env::temp_dir().join(format!("bm-hls-origin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let planted = dir.join(name);
    std::fs::copy(&fixture, &planted).expect("plant the fixture");
    Planted(planted)
}

/// Run a one-segment VOD playlist naming `segment` and report the state
/// the session settles in, how many frames it decoded, and which error
/// category it settled under.
fn play_playlist_naming(segment: &str) -> (u32, u64, u32) {
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:7\n#EXTINF:6.0,\n{segment}\n#EXT-X-ENDLIST\n"
    );
    let mut request = OpenRequest::new(spawn_playlist_server(playlist));
    // The address gate is not what is under test — leave it wide open, so
    // a pass cannot be the gate refusing 127.0.0.1 by accident.
    request.allow_local_addresses = true;
    // Stated, so the lane is the ranged on-demand one every time rather
    // than liveness inference deciding it from the server's answer.
    request.liveness = SourceLiveness::Vod;
    let mut session = Session::open(request);
    let shared = session.shared().clone();

    let start = Instant::now();
    let mut state = shared.state.load(Ordering::Relaxed);
    while start.elapsed() < Duration::from_secs(30) {
        state = shared.state.load(Ordering::Relaxed);
        if state == State::Error as u32
            || state == State::Playing as u32
            || state == State::Ended as u32
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // A moment for any frame that was going to arrive to arrive.
    thread::sleep(Duration::from_millis(250));
    let decoded = shared.frames_decoded.load(Ordering::Relaxed);
    let category = shared.last_error_category.load(Ordering::Relaxed);
    session.close();
    (state, decoded, category)
}

/// A `file:` URL for the planted fixture, in the spelling the host uses.
///
/// The temp directory sits under a user profile, whose name this test
/// does not choose — a space in it would make the URL name something
/// other than the planted file, and the row would then assert a refusal
/// for a reason that is not the origin routing it is about. `%` goes
/// first, or it would re-encode the escape the space just produced.
fn file_url(path: &std::path::Path) -> String {
    let text = path.to_str().expect("scratch path is UTF-8");
    let escaped = text.replace('%', "%25").replace(' ', "%20");
    if cfg!(windows) {
        format!("file:///{}", escaped.replace('\\', "/"))
    } else {
        format!("file://{escaped}")
    }
}

/// Every spelling names the *same planted, playable* copy of the TS
/// fixture, so a row that passes has refused rather than merely failed
/// to find anything — that distinction is the whole assertion.
///
/// Which spellings can name a local file at all depends on the host, so
/// they are listed per platform rather than filtered — a row that cannot
/// bite on the host running it is worse than absent, because it reads as
/// coverage.
///
/// On Windows a bare path is drive-absolute, and URL joining turns it
/// into a one-character scheme; `c://…` carries the `://` that once
/// marked a URI absolute; `c:/…` is the same drive path again. All three
/// reach the filesystem if nothing stops them.
///
/// Off Windows none of that applies. A POSIX absolute path joined onto
/// an http base is simply a **root-relative URL** — `/tmp/x` becomes
/// `http://origin/tmp/x`, which names no local file and is not the thing
/// this row is about. Only the `file:` URL is a genuine local naming
/// there, so that is the only spelling listed.
#[test]
fn a_network_playlist_cannot_play_a_local_file() {
    let planted = planted_fixture("planted.ts");
    let planted = &planted.0;
    #[cfg(windows)]
    let path = planted.to_str().expect("scratch path is UTF-8").to_owned();

    #[cfg(windows)]
    let spellings = vec![
        path.clone(),
        file_url(planted),
        path.replacen(':', "://", 1),
        path.replacen(":\\", ":/", 1),
    ];
    #[cfg(not(windows))]
    let spellings = vec![file_url(planted)];

    for segment in &spellings {
        let (state, decoded, category) = play_playlist_naming(segment);
        assert_eq!(
            state,
            State::Error as u32,
            "{segment:?} settled in state {state} rather than refusing"
        );
        assert_eq!(
            decoded, 0,
            "{segment:?} decoded {decoded} frames off the local file"
        );
        // The refusal has to come from the source, not from the decoder.
        // On a host with no H.264 decoder — the Linux lane — a session
        // that *did* read the planted file would also settle in Error
        // with nothing decoded, so state alone cannot tell the two
        // apart and this row would pass without proving anything.
        assert_ne!(
            category,
            ErrorCategory::Decode as u32,
            "{segment:?} was refused by the decoder, so the read was not prevented"
        );
    }
}

/// The fetcher wiring behind that lane, on every host. What the row
/// above proves needs a picture out of the pipeline, so it is gated to
/// Windows — but the part most likely to regress is the confinement in
/// `ResourceFetcher::local`, which is platform-independent and was
/// therefore covered nowhere else.
///
/// What a host without a decoder can still reach is the Bank: media
/// arrives there demuxed, so anything in it proves the fetcher was asked
/// for the playlist's own segment and delivered it. Asserted positively
/// for that reason — a session that never settles, which is what this
/// one does off Windows, would satisfy any assertion phrased against the
/// error category without the fetcher having been reached at all.
#[test]
fn a_disk_playlist_reaches_its_own_segments_without_a_decoder() {
    let playlist =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/hls/ts/index.m3u8");
    let mut request =
        OpenRequest::new(playlist.to_str().expect("fixture path is UTF-8").to_owned());
    request.liveness = SourceLiveness::Vod;
    let mut session = Session::open(request);
    let shared = session.shared().clone();

    let start = Instant::now();
    let mut banked = 0;
    while start.elapsed() < Duration::from_secs(20) {
        banked = shared.banked_us.load(Ordering::Relaxed);
        if banked > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let category = shared.last_error_category.load(Ordering::Relaxed);
    session.close();
    assert!(
        banked > 0,
        "nothing from the playlist's own segment reached the Bank \
         (error category {category})"
    );
}

/// The other direction still works: a playlist opened from disk plays the
/// segments sitting beside it. The refusal above is about where the
/// playlist came from, not a blanket ban on local media.
///
/// Windows only, like every other row here that needs a picture out of
/// the pipeline: the Linux backend carries no H.264 or AAC decoder, so
/// the fixture cannot decode there whatever the routing does.
#[cfg(windows)]
#[test]
fn a_disk_playlist_still_plays_its_own_segments() {
    let playlist =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/hls/ts/index.m3u8");
    let mut request =
        OpenRequest::new(playlist.to_str().expect("fixture path is UTF-8").to_owned());
    request.liveness = SourceLiveness::Vod;
    let mut session = Session::open(request);
    let shared = session.shared().clone();

    let start = Instant::now();
    let mut played = false;
    while start.elapsed() < Duration::from_secs(30) {
        let state = shared.state.load(Ordering::Relaxed);
        assert_ne!(
            state,
            State::Error as u32,
            "the fixture lane must still play"
        );
        if state == State::Playing as u32 && shared.frames_decoded.load(Ordering::Relaxed) > 0 {
            played = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    session.close();
    assert!(
        played,
        "never decoded a frame from the local fixture playlist"
    );
}
