//! HttpSource against an in-process HTTP/1.1 server: range serving,
//! sequential fallback, redirect re-vetting, and the address gate. No
//! external network anywhere.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use media_demux::{ByteSource, DemuxLimits, Demuxer, Mp4Demuxer, StreamEvent};
use media_io::{AllowAllGate, CancelToken, HttpSource, IoErrorKind, IoLimits, PublicAddressGate};

/// How long a cancelled request may take to come back. The request
/// timeout it stands against is 20 s, so this is loose enough not to
/// flake on a host running the rest of the suite alongside it.
const CANCEL_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ranges,
    /// Ignores Range headers: always a 200 with the full body.
    Sequential,
    /// Declines the range request but advertises `Accept-Ranges: bytes`,
    /// which is the second arm of "will this server serve ranges".
    SequentialAdvertised,
    /// A live edge: 200, no length of any kind, closed when done.
    Unbounded,
}

/// Serve `body` at `/media`, with `/hop1` -> `/hop2` -> `/media` redirects
/// and `/loop` redirecting to itself. Returns the base URL.
fn spawn_server(body: Vec<u8>, mode: Mode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let body = Arc::new(body);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let body = Arc::clone(&body);
            thread::spawn(move || serve_connection(stream, &body, mode));
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn serve_connection(stream: TcpStream, body: &[u8], mode: Mode) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut stream = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let mut range: Option<(u64, Option<u64>)> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(spec) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                let mut parts = spec.split('-');
                let start = parts.next().and_then(|s| s.parse().ok());
                let end = parts.next().and_then(|s| s.parse().ok());
                if let Some(start) = start {
                    range = Some((start, end));
                }
            }
        }

        let response = match path.as_str() {
            "/hop1" => redirect("/hop2"),
            "/hop2" => redirect("/media"),
            "/loop" => redirect("/loop"),
            _ => match (mode, range) {
                (Mode::Ranges, Some((start, end))) if start <= body.len() as u64 => {
                    let start = start as usize;
                    // Inclusive range end, clamped to the body.
                    let stop = end
                        .map(|e| (e as usize + 1).min(body.len()))
                        .unwrap_or(body.len());
                    let slice = &body[start..stop];
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\n\r\n",
                        start,
                        stop.saturating_sub(1),
                        body.len(),
                        slice.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(slice);
                    r
                }
                (Mode::Unbounded, _) => {
                    let mut r = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                    r.extend_from_slice(body);
                    let _ = stream.write_all(&r);
                    return;
                }
                _ => {
                    let accept = if mode == Mode::SequentialAdvertised {
                        "Accept-Ranges: bytes\r\n"
                    } else {
                        ""
                    };
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\n{accept}Content-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
            },
        };
        if stream.write_all(&response).is_err() {
            return;
        }
    }
}

/// A server that answers a ranged GET with `serve` bytes of a body it
/// never finishes, or with `serve` as `None` never answers at all. Both
/// leave the client waiting on a socket nothing will write to again,
/// which only the read timeout and the cancel token end.
fn start_stalling_server(total: u64, serve: Option<usize>) -> (String, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // Held so the connections stay open rather than being closed by
        // the drop, which the client would read as a finished body.
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.trim_end().is_empty() {
                    break;
                }
            }
            if let Some(n) = serve {
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{total}\r\n\
                     Content-Length: {total}\r\n\r\n",
                    // A zero total is not a shape any row wants, but it
                    // would underflow here and stop the server thread
                    // accepting, which reads as a hung client.
                    total.saturating_sub(1)
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&vec![0u8; n]);
                let _ = stream.flush();
            }
            let _ = tx.send(());
            held.push(stream);
        }
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

/// A server that accepts exactly one connection, answers it with the
/// whole body however it was asked for, and then stops listening so a
/// second connection is refused. This is the shape of the one-client-per-
/// slot feeders the live rig runs, and of any origin that answers a
/// ranged GET with a plain 200.
fn spawn_single_slot_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        drop(listener);
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut stream = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line.trim_end().is_empty() {
                break;
            }
        }
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(&body);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    });
    format!("http://127.0.0.1:{port}")
}

fn redirect(to: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name);
    std::fs::read(path).expect("fixture readable")
}

fn open(url: &str) -> Result<HttpSource, media_io::IoError> {
    HttpSource::open(
        url,
        IoLimits::default(),
        Arc::new(AllowAllGate),
        CancelToken::new(),
    )
}

fn count_aus(source: HttpSource) -> (u32, u32) {
    let mut demux = Mp4Demuxer::open(
        Box::new(source),
        DemuxLimits::default(),
        media_clock::Generation(1),
    )
    .expect("demux opens over http");
    let (mut video, mut audio) = (0u32, 0u32);
    loop {
        match demux.next_event().expect("event") {
            StreamEvent::Au(au) => {
                if au.data.starts_with(&[0, 0, 0, 1]) {
                    video += 1;
                } else {
                    audio += 1;
                }
            }
            StreamEvent::Eos(_) => return (video, audio),
            _ => {}
        }
    }
}

#[test]
fn range_server_demuxes_trailing_moov() {
    let base = spawn_server(fixture("h264-aac-moov-trailing.mp4"), Mode::Ranges);
    let mut source = open(&format!("{base}/media")).expect("open");
    assert_eq!(
        source.size().expect("size"),
        Some(fixture("h264-aac-moov-trailing.mp4").len() as u64)
    );
    assert_eq!(count_aus(source), (180, 283));
}

#[test]
fn small_chunks_span_the_whole_file() {
    let base = spawn_server(fixture("h264-aac-640x360-30fps.mp4"), Mode::Ranges);
    let source = HttpSource::open(
        &format!("{base}/media"),
        IoLimits {
            // Force many chunk-boundary reopens across the ~670 KiB body.
            chunk_bytes: 64 * 1024,
            ..IoLimits::default()
        },
        Arc::new(AllowAllGate),
        CancelToken::new(),
    )
    .expect("open");
    assert_eq!(count_aus(source), (180, 283));
}

#[test]
fn sequential_server_demuxes_faststart() {
    let base = spawn_server(fixture("h264-aac-640x360-30fps.mp4"), Mode::Sequential);
    let source = open(&format!("{base}/media")).expect("open");
    assert_eq!(count_aus(source), (180, 283));
}

/// An origin that answers the opening ranged probe with a 200 has
/// already handed over the whole entity from byte 0, so that response is
/// the sequential stream. Opening a second one would cost a second
/// connection, and the origins that answer this way are the ones least
/// able to give it: every live edge answers 200, and a feeder serving one
/// client at a time has nothing left once the probe has taken its slot.
#[test]
fn a_sequential_open_costs_one_connection() {
    let base = spawn_single_slot_server(fixture("h264-aac-640x360-30fps.mp4"));
    let source = open(&format!("{base}/media")).expect("open rides the connection it already has");
    assert_eq!(count_aus(source), (180, 283));
}

/// The opening request carries no total timeout, because its response is
/// the body itself on a server that answers 200 and no bound can be put
/// on how long that runs. A link that simply goes quiet is held by the
/// client's read timeout instead, which measures the wait rather than the
/// exchange; without it the only way out of this read is the session's
/// cancel token.
#[test]
fn a_quiet_link_surfaces_as_a_read_error() {
    const SERVED: usize = 32;
    let (base, _requested) = start_stalling_server(4 * 1024 * 1024, Some(SERVED));
    let mut source = HttpSource::open(
        &format!("{base}/media"),
        IoLimits {
            request_timeout: Duration::from_millis(500),
            ..IoLimits::default()
        },
        Arc::new(AllowAllGate),
        CancelToken::new(),
    )
    .expect("the open lands on the bytes the server did write");

    let mut buf = [0u8; 16];
    let mut got = 0usize;
    while got < SERVED {
        let n = source
            .read_at(got as u64, &mut buf)
            .expect("the served head reads back");
        assert!(n > 0, "the server wrote {SERVED} bytes");
        got += n;
    }

    // Read on a worker: without a bound on the wait this never comes
    // back at all, and a row that hangs the suite says less than one
    // that fails it.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome = source
            .read_at(got as u64, &mut buf)
            .map_err(|e| e.to_string());
        let _ = tx.send(outcome);
    });
    let err = rx
        .recv_timeout(CANCEL_WINDOW)
        .expect("the read comes back on the timeout, not on the socket")
        .expect_err("the quiet link is given up on");
    assert!(
        err.starts_with("Read:"),
        "a stalled body is a read error: {err}"
    );
}

#[test]
fn redirects_are_followed_and_capped() {
    let base = spawn_server(fixture("h264-aac-640x360-30fps.mp4"), Mode::Ranges);
    let source = open(&format!("{base}/hop1")).expect("redirect chain resolves");
    assert!(source.final_url().path().ends_with("/media"));
    assert_eq!(count_aus(source), (180, 283));

    let err = open(&format!("{base}/loop")).expect_err("redirect loop trips the cap");
    assert_eq!(err.kind, IoErrorKind::Redirect);
}

#[test]
fn public_gate_blocks_loopback() {
    let base = spawn_server(b"irrelevant".to_vec(), Mode::Ranges);
    let err = HttpSource::open(
        &format!("{base}/media"),
        IoLimits::default(),
        Arc::new(PublicAddressGate),
        CancelToken::new(),
    )
    .expect_err("loopback must be blocked");
    assert_eq!(err.kind, IoErrorKind::Blocked);
}

/// `Session::close` joins the opener thread from the client's main
/// thread, so an open that cannot be abandoned freezes the client for the
/// request budget — six redirect hops' worth, in the worst case.
#[test]
fn a_close_during_open_abandons_the_request() {
    let (base, requested) = start_stalling_server(1024, None);
    let cancel = CancelToken::new();
    let closing = cancel.clone();
    // `None` afterwards means the open never reached a request, so the
    // cancel would have proved nothing.
    let cancelled_at = Arc::new(Mutex::new(None));
    let stamp = Arc::clone(&cancelled_at);
    thread::spawn(move || {
        // Building the pinned client loads the OS trust store, which is
        // not quick; cancelling on a timer alone can beat the request
        // onto the wire.
        requested.recv().expect("the server reads the request");
        thread::sleep(Duration::from_millis(200));
        *stamp.lock().expect("stamp") = Some(Instant::now());
        closing.cancel();
    });

    let err = HttpSource::open(
        &format!("{base}/media"),
        IoLimits::default(),
        Arc::new(AllowAllGate),
        cancel,
    )
    .map(|_| ())
    .expect_err("a cancelled open cannot succeed");

    let cancelled_at = cancelled_at
        .lock()
        .expect("stamp")
        .expect("the request was in flight when the cancel landed");
    assert_eq!(err.kind, IoErrorKind::Connect);
    assert!(
        err.detail.contains("cancelled"),
        "the refusal names the cancel, not a transport error: {err}"
    );
    assert!(
        cancelled_at.elapsed() < CANCEL_WINDOW,
        "returned on the cancel, not after the request timeout: {:?}",
        cancelled_at.elapsed()
    );
}

/// The same for the demux thread, which `close` joins too and which is
/// where a session spends the whole of its life.
#[test]
fn a_close_during_a_read_abandons_it() {
    const SERVED: usize = 64 * 1024;
    let (base, _requested) = start_stalling_server(4 * 1024 * 1024, Some(SERVED));
    let cancel = CancelToken::new();
    let mut source = HttpSource::open(
        &format!("{base}/media"),
        IoLimits::default(),
        Arc::new(AllowAllGate),
        cancel.clone(),
    )
    .expect("the open lands on the bytes the server did write");

    // Drain exactly what was written; the read after this one has nothing
    // to wait for.
    let mut buf = vec![0u8; 32 * 1024];
    let mut got = 0usize;
    while got < SERVED {
        let n = source
            .read_at(got as u64, &mut buf)
            .expect("the served head reads back");
        assert!(n > 0, "the server wrote {SERVED} bytes");
        got += n;
    }

    let closing = cancel.clone();
    let cancelled_at = Arc::new(Mutex::new(None));
    let stamp = Arc::clone(&cancelled_at);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        *stamp.lock().expect("stamp") = Some(Instant::now());
        closing.cancel();
    });

    let err = source
        .read_at(got as u64, &mut buf)
        .expect_err("the stalled read is abandoned");

    let cancelled_at = cancelled_at
        .lock()
        .expect("stamp")
        .expect("the read was waiting when the cancel landed");
    assert!(
        err.to_string().contains("cancelled"),
        "the refusal names the cancel, not a transport error: {err}"
    );
    assert!(
        cancelled_at.elapsed() < CANCEL_WINDOW,
        "returned on the cancel, not after the request timeout: {:?}",
        cancelled_at.elapsed()
    );
}

/// `read_at` is trait surface, so the buffer's length is the caller's to
/// choose and zero is a length. The held chunk has to survive it: served
/// as a spent one, the unread remainder is dropped and the loop pulls
/// until the body ends, which on a live or stalled source is not a thing
/// that happens. Pinned against a server that goes quiet, so a read that
/// went to the network rather than to the chunk in hand cannot come back
/// inside the window.
#[test]
fn a_zero_length_read_keeps_the_chunk_it_is_holding() {
    // Small and written in one call, so the whole of it is one chunk on
    // loopback and the first read provably leaves a remainder inside it.
    // Sized from a 64 KiB body the row would be resting on where the
    // transport happened to split it, and a short first chunk would send
    // the second read to the network and read as a regression that is
    // not one. Small also makes the failure loud rather than slow: the
    // server writes this much and then never writes again, so a source
    // that dropped the chunk has nothing to refetch and the read waits
    // out the request timeout instead of quietly succeeding.
    const SERVED: usize = 32;
    let (base, _requested) = start_stalling_server(4 * 1024 * 1024, Some(SERVED));
    let mut source = open(&format!("{base}/media")).expect("open");

    let mut buf = [0u8; 16];
    assert_eq!(
        source
            .read_at(0, &mut buf)
            .expect("the served head reads back"),
        buf.len(),
        "the first read leaves a remainder in the chunk"
    );

    let started = Instant::now();
    assert_eq!(
        source.read_at(16, &mut []).expect("a zero-length read"),
        0,
        "nothing was asked for and nothing is served"
    );
    assert!(
        started.elapsed() < CANCEL_WINDOW,
        "the zero-length read went to the network: {:?}",
        started.elapsed()
    );

    let started = Instant::now();
    assert_eq!(
        source
            .read_at(16, &mut buf)
            .expect("the remainder reads back"),
        buf.len(),
        "the chunk the zero-length read was holding is still there"
    );
    assert!(
        started.elapsed() < CANCEL_WINDOW,
        "the remainder was refetched rather than served: {:?}",
        started.elapsed()
    );
}

#[test]
fn unsupported_scheme_is_a_url_error() {
    let err = open("ftp://example.invalid/media").expect_err("ftp refused");
    assert_eq!(err.kind, IoErrorKind::Url);
}

/// The liveness inference reads two things off the source: whether the
/// server will serve byte ranges, and whether it stated any length. The
/// composition — finite and rangeable is on-demand — is what keeps a VOD
/// off the jitter-buffer path, where it would play at delivery speed.
#[test]
fn seekability_reads_ranges_and_length_not_guesses() {
    let body = fixture("h264-aac-640x360-30fps.mp4");

    // A real 206 answer, which is the signal — not an advertised header.
    let ranged = spawn_server(body.clone(), Mode::Ranges);
    assert!(
        open(&format!("{ranged}/media"))
            .expect("open")
            .is_seekable()
    );

    // 200 to a range request, no advertisement: nothing says ranges work.
    let plain = spawn_server(body.clone(), Mode::Sequential);
    assert!(!open(&format!("{plain}/media")).expect("open").is_seekable());

    // 200 to a range request but `Accept-Ranges: bytes` advertised. The
    // C player accepted this arm and so does this one, which is the only
    // gap between them.
    let advertised = spawn_server(body.clone(), Mode::SequentialAdvertised);
    assert!(
        open(&format!("{advertised}/media"))
            .expect("open")
            .is_seekable()
    );

    // No ranges and no length at all: the live-edge shape.
    let unbounded = spawn_server(body, Mode::Unbounded);
    assert!(
        !open(&format!("{unbounded}/media"))
            .expect("open")
            .is_seekable()
    );
}
