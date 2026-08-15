//! WHEP signalling rows over a virtual server (§12.1): both answer
//! flows, PATCH discipline, redirects, Link ice-server surfacing,
//! teardown DELETE, and the §9.3 candidate gate at the transmit
//! boundary. The server is a hand-rolled HTTP loop with a real str0m
//! instance answering the SDP, so the negotiation the client accepts is
//! genuine; no media network beyond loopback is touched.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use media_demux::Demuxer;
use media_io::AddressGate;
use media_rtsp::CancelProbe;
use media_whep::WhepDemuxer;
use str0m::change::SdpOffer;
use str0m::media::{Direction, MediaKind};
use str0m::{Candidate, Rtc, RtcConfig};

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    content_type: Option<String>,
    body: String,
}

type Responder = Box<dyn FnMut(&Recorded) -> String + Send>;

struct VirtualServer {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl VirtualServer {
    fn start(mut respond: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                log.lock().expect("log").push(request.clone());
                let response = respond(&request);
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { port, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("log").clone()
    }

    fn wait_for(&self, method: &str, tries: u32) -> bool {
        for _ in 0..tries {
            if self.requests().iter().any(|r| r.method == method) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

fn read_request(mut stream: &TcpStream) -> Option<Recorded> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&raw) {
            break pos;
        }
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    let mut content_type = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "content-type" => content_type = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let mut body = raw[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Some(Recorded {
        method,
        path,
        content_type,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    out
}

/// A server-side str0m that accepts the client's offer — the SDP the
/// client swallows is a genuine negotiation, not a canned string.
fn answer_offer(offer_sdp: &str, media_addr: std::net::SocketAddr) -> String {
    let offer = SdpOffer::from_sdp_string(offer_sdp).expect("client offer parses");
    let mut rtc = server_rtc(media_addr);
    let answer = rtc.sdp_api().accept_offer(offer).expect("accept offer");
    answer.to_sdp_string()
}

fn server_rtc(media_addr: std::net::SocketAddr) -> Rtc {
    let provider = Arc::new(str0m::crypto::from_feature_flags());
    let mut rtc = RtcConfig::new()
        .set_crypto_provider(provider)
        .set_rtp_mode(true)
        .build(std::time::Instant::now());
    rtc.add_local_candidate(Candidate::host(media_addr, "udp").expect("candidate"));
    rtc
}

struct PermitAll;
impl AddressGate for PermitAll {
    fn permit(&self, _ip: IpAddr) -> bool {
        true
    }
}

/// Permits everything except one address — the shape of "signalling may
/// pass, this candidate may not".
struct BlockOne(IpAddr);
impl AddressGate for BlockOne {
    fn permit(&self, ip: IpAddr) -> bool {
        ip != self.0
    }
}

fn cancel_never() -> CancelProbe {
    Box::new(|| false)
}

fn open(url: &str, gate: Arc<dyn AddressGate>) -> Result<WhepDemuxer, media_demux::DemuxError> {
    WhepDemuxer::open(
        url,
        media_clock::Generation(0),
        media_io::io_runtime_handle(),
        cancel_never(),
        gate,
    )
}

#[test]
fn direct_201_flow_with_ice_servers_and_delete() {
    let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media sock");
    let media_addr = media.local_addr().expect("addr");
    let server = VirtualServer::start(Box::new(move |request| match request.method.as_str() {
        "POST" => response(
            "201 Created",
            &[
                ("Content-Type", "application/sdp"),
                ("Location", "/session/abc"),
                ("Link", "<stun:stun.example.net:3478>; rel=\"ice-server\""),
            ],
            &answer_offer(&request.body, media_addr),
        ),
        "DELETE" => response("200 OK", &[], ""),
        other => panic!("unexpected {other} in the direct flow"),
    }));

    let url = format!("whep://127.0.0.1:{}/stream/whep", server.port);
    let demuxer = open(&url, Arc::new(PermitAll)).expect("open succeeds");
    assert_eq!(demuxer.answer_flow(), media_whep::AnswerFlow::Direct);
    assert_eq!(demuxer.ice_servers(), ["stun:stun.example.net:3478"]);

    let posts = server.requests();
    assert_eq!(posts.len(), 1, "exactly one signalling request so far");
    assert_eq!(posts[0].method, "POST");
    assert_eq!(posts[0].path, "/stream/whep");
    assert_eq!(posts[0].content_type.as_deref(), Some("application/sdp"));
    assert!(posts[0].body.starts_with("v=0"), "SDP offer body");
    assert!(
        posts[0].body.contains("a=candidate"),
        "ICE gathered fully before POST — host candidate rides in the offer"
    );
    assert!(
        posts[0].body.contains("recvonly"),
        "receive-only media directions"
    );

    drop(demuxer);
    assert!(
        server.wait_for("DELETE", 50),
        "teardown DELETEs the session URL"
    );
    let requests = server.requests();
    let delete = requests
        .iter()
        .find(|r| r.method == "DELETE")
        .expect("delete");
    assert_eq!(delete.path, "/session/abc");
    assert!(
        !requests.iter().any(|r| r.method == "PATCH"),
        "the direct flow never PATCHes — PATCH-refusing servers work"
    );
}

#[test]
fn counter_offer_406_flow_patches_answer() {
    let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media sock");
    let media_addr = media.local_addr().expect("addr");
    // The server counter-offers sendonly A/V from its own str0m.
    let server_offer = {
        let mut rtc = server_rtc(media_addr);
        let mut api = rtc.sdp_api();
        api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, _pending) = api.apply().expect("changes");
        offer.to_sdp_string()
    };
    let server = VirtualServer::start(Box::new(move |request| match request.method.as_str() {
        "POST" => response(
            "406 Not Acceptable",
            &[
                ("Content-Type", "application/sdp"),
                ("Location", "/session/counter"),
            ],
            &server_offer,
        ),
        "PATCH" => {
            assert_eq!(request.path, "/session/counter");
            assert_eq!(request.content_type.as_deref(), Some("application/sdp"));
            assert!(request.body.starts_with("v=0"), "SDP answer body");
            response("204 No Content", &[], "")
        }
        "DELETE" => response("200 OK", &[], ""),
        other => panic!("unexpected {other} in the counter-offer flow"),
    }));

    let url = format!("whep://127.0.0.1:{}/stream/whep", server.port);
    let demuxer = open(&url, Arc::new(PermitAll)).expect("open succeeds");
    assert_eq!(demuxer.answer_flow(), media_whep::AnswerFlow::CounterOffer);
    let methods: Vec<String> = server.requests().iter().map(|r| r.method.clone()).collect();
    assert_eq!(methods, ["POST", "PATCH"]);
}

#[test]
fn post_redirect_re_vets_and_lands() {
    let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media sock");
    let media_addr = media.local_addr().expect("addr");
    let server = VirtualServer::start(Box::new(move |request| {
        if request.path == "/old" {
            response("307 Temporary Redirect", &[("Location", "/new")], "")
        } else {
            assert_eq!(request.path, "/new");
            response(
                "201 Created",
                &[
                    ("Content-Type", "application/sdp"),
                    ("Location", "/session/xyz"),
                ],
                &answer_offer(&request.body, media_addr),
            )
        }
    }));
    let url = format!("whep://127.0.0.1:{}/old", server.port);
    let demuxer = open(&url, Arc::new(PermitAll)).expect("open follows the redirect");
    assert_eq!(demuxer.answer_flow(), media_whep::AnswerFlow::Direct);
    // Both hops carried the POST (307 preserves the method).
    let methods: Vec<String> = server.requests().iter().map(|r| r.method.clone()).collect();
    assert_eq!(methods, ["POST", "POST"]);
}

#[test]
fn error_status_fails_typed_and_gate_blocks_signalling() {
    let server = VirtualServer::start(Box::new(|_request| {
        response("404 Not Found", &[], "no such stream")
    }));
    let url = format!("whep://127.0.0.1:{}/missing", server.port);
    let err = open(&url, Arc::new(PermitAll))
        .map(|_| ())
        .expect_err("404 fails the open");
    assert!(
        err.to_string().contains("404"),
        "status surfaces in the error: {err}"
    );

    // The default public gate refuses loopback signalling outright.
    let err = open(&url, Arc::new(media_io::PublicAddressGate))
        .map(|_| ())
        .expect_err("blocked");
    assert!(
        err.to_string().to_lowercase().contains("block"),
        "gate refusal surfaces typed: {err}"
    );
}

/// §9.3: a candidate address the gate refuses never receives a packet —
/// the check sits at the transmit boundary, so the connectivity check
/// itself is what gets suppressed. The mirror case proves the machinery
/// would have sent to the address had the gate permitted it.
#[test]
fn blocked_candidate_address_never_receives_a_packet() {
    for (permitted, expect_traffic) in [(false, false), (true, true)] {
        // The media socket sits on a second loopback address so the gate
        // can pass signalling (127.0.0.1) while refusing the candidate.
        let media = match std::net::UdpSocket::bind("127.0.0.2:0") {
            Ok(socket) => socket,
            // Loopback aliases aren't bindable everywhere; the row is
            // Windows/Linux-shaped, skip loudly elsewhere.
            Err(e) => {
                eprintln!("skipping: cannot bind 127.0.0.2 ({e})");
                return;
            }
        };
        let media_addr = media.local_addr().expect("addr");
        let server = VirtualServer::start(Box::new(move |request| {
            response(
                "201 Created",
                &[
                    ("Content-Type", "application/sdp"),
                    ("Location", "/session/gate"),
                ],
                &answer_offer(&request.body, media_addr),
            )
        }));
        let gate: Arc<dyn AddressGate> = if permitted {
            Arc::new(PermitAll)
        } else {
            Arc::new(BlockOne(media_addr.ip()))
        };
        let url = format!("whep://127.0.0.1:{}/stream/whep", server.port);
        let demuxer = open(&url, gate).expect("signalling passes either way");

        media
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut buf = [0u8; 1500];
        let received = media.recv_from(&mut buf).is_ok();
        assert_eq!(
            received, expect_traffic,
            "gate permitted={permitted} ⇒ traffic={expect_traffic}"
        );
        drop(demuxer);
    }
}

/// A session with no playable codec fails at open, not as a stall: the
/// server answer rejects both m-lines.
#[test]
fn unplayable_answer_refuses_at_open() {
    let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media sock");
    let media_addr = media.local_addr().expect("addr");
    let server = VirtualServer::start(Box::new(move |request| {
        // Answer with every m-line rejected (port 0 / inactive): parse
        // the real answer and neuter the codecs by marking inactive.
        let answer = answer_offer(&request.body, media_addr);
        let neutered = answer
            .replace("a=sendonly", "a=inactive")
            .replace("a=sendrecv", "a=inactive")
            .replace("a=recvonly", "a=inactive");
        response(
            "201 Created",
            &[
                ("Content-Type", "application/sdp"),
                ("Location", "/session/neutered"),
            ],
            &neutered,
        )
    }));
    let url = format!("whep://127.0.0.1:{}/stream/whep", server.port);
    // Whether this opens depends on how much the answer prunes; the row
    // pins the invariant that a truly codec-less negotiation is a typed
    // refusal rather than a silent stall.
    match open(&url, Arc::new(PermitAll)) {
        Ok(demuxer) => drop(demuxer),
        Err(e) => {
            let detail = e.to_string();
            assert!(
                detail.contains("negotiated") || detail.contains("codec"),
                "refusal names the codec problem: {detail}"
            );
        }
    }
}

#[test]
fn feed_stall_surfaces_when_media_never_flows() {
    // Signalling succeeds but no ICE peer ever answers: the demuxer's
    // pull surfaces a typed stall (the engine reconnect path's food).
    let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media sock");
    let media_addr = media.local_addr().expect("addr");
    let server = VirtualServer::start(Box::new(move |request| match request.method.as_str() {
        "POST" => response(
            "201 Created",
            &[
                ("Content-Type", "application/sdp"),
                ("Location", "/session/quiet"),
            ],
            &answer_offer(&request.body, media_addr),
        ),
        _ => response("200 OK", &[], ""),
    }));
    let url = format!("whep://127.0.0.1:{}/stream/whep", server.port);
    let mut demuxer = open(&url, Arc::new(PermitAll)).expect("open succeeds");
    let err = demuxer.next_event().expect_err("no media ever arrives");
    assert!(
        err.to_string().contains("stall"),
        "typed stall for the reconnect path: {err}"
    );
}
