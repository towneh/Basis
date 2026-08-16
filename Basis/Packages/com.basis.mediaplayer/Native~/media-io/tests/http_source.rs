//! HttpSource against an in-process HTTP/1.1 server: range serving,
//! sequential fallback, redirect re-vetting, and the address gate. No
//! external network anywhere.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use media_demux::{ByteSource, DemuxLimits, Demuxer, Mp4Demuxer, StreamEvent};
use media_io::{AllowAllGate, HttpSource, IoErrorKind, IoLimits, PublicAddressGate};

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
    HttpSource::open(url, IoLimits::default(), Arc::new(AllowAllGate))
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
    )
    .expect_err("loopback must be blocked");
    assert_eq!(err.kind, IoErrorKind::Blocked);
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
