//! HTTP(S) byte source over positioned reads.
//!
//! Open resolves the host once, vets *every* returned address through the
//! gate, and pins the connection to the vetted set (`resolve_to_addrs`)
//! with Host/SNI carried by the URL — the resolve-then-reconnect TOCTOU
//! is closed by construction. Redirects are handled manually so each
//! hop re-runs the same vetting and re-pins.
//!
//! Reads on a range-capable server are chunked ranged requests under a
//! per-request timeout, so a stalled link surfaces as a typed error rather
//! than a silent hang; sequential reads ride one pooled connection, and a
//! positioned read elsewhere costs one request. A server that answers the
//! opening probe with a 200 has handed over the whole entity, so that
//! response *is* the sequential stream (forward reads discard, backward
//! reads restart) and the open costs one connection — which is all the
//! origins that answer this way tend to have. No total timeout bounds
//! that body, only the client's read timeout, so every request and every
//! read races the session's [`CancelToken`] as well: the opener and demux
//! threads this source runs on are both joined by a closing session, and a
//! server that stops answering must hold neither of them.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use media_demux::{ByteSource, SourceError};
use url::Url;

use crate::cancel::CancelToken;
use crate::runtime::runtime;
use crate::{AddressGate, IoError, IoErrorKind, IoLimits};

pub(crate) const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// What one URL still needs looked up before its client can be pinned.
enum VetTarget {
    /// An IP literal, vetted by the screen that produced this.
    Literal,
    Domain {
        domain: String,
        port: u16,
    },
}

/// The half of vetting that touches no network: scheme screen, and the
/// literal hosts settled against the gate on the spot.
fn vet_target(url: &Url, gate: &dyn AddressGate) -> Result<VetTarget, IoError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(IoError::new(
            IoErrorKind::Url,
            format!("unsupported scheme: {}", url.scheme()),
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| IoError::new(IoErrorKind::Url, "URL without host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| IoError::new(IoErrorKind::Url, "URL without port"))?;

    match host {
        url::Host::Ipv4(ip) => {
            if !gate.permit(ip.into()) {
                return Err(IoError::new(IoErrorKind::Blocked, format!("{ip} blocked")));
            }
            Ok(VetTarget::Literal)
        }
        url::Host::Ipv6(ip) => {
            if !gate.permit(ip.into()) {
                return Err(IoError::new(IoErrorKind::Blocked, format!("{ip} blocked")));
            }
            Ok(VetTarget::Literal)
        }
        url::Host::Domain(domain) => Ok(VetTarget::Domain {
            domain: domain.to_string(),
            port,
        }),
    }
}

/// The other half: permit *every* returned address (a mixed
/// public/private answer is the rebinding shape, refused whole) and hand
/// back the pinned set for `resolve_to_addrs`.
fn vet_addrs(
    domain: String,
    addrs: Vec<SocketAddr>,
    gate: &dyn AddressGate,
) -> Result<(String, Vec<SocketAddr>), IoError> {
    if addrs.is_empty() {
        return Err(IoError::new(
            IoErrorKind::Resolve,
            format!("{domain}: no addresses"),
        ));
    }
    for addr in &addrs {
        if !gate.permit(addr.ip()) {
            return Err(IoError::new(
                IoErrorKind::Blocked,
                format!("{domain} resolves to blocked {}", addr.ip()),
            ));
        }
    }
    Ok((domain, addrs))
}

/// Scheme/host vetting: resolve once under the resolver ceiling, vet the
/// whole answer, and hand back the pinned set for `resolve_to_addrs`.
/// `None` means the host was an IP literal, already vetted. The resolve
/// is a real await point, so a `select!` racing this against a cancel
/// token can observe the cancel. Public for transports that run their own
/// HTTP requests over the same discipline (WHEP signalling).
pub async fn vet_url_async(
    url: &Url,
    gate: &dyn AddressGate,
) -> Result<Option<(String, Vec<SocketAddr>)>, IoError> {
    match vet_target(url, gate)? {
        VetTarget::Literal => Ok(None),
        VetTarget::Domain { domain, port } => {
            let addrs = crate::resolve::resolve_async(&domain, port).await?;
            vet_addrs(domain, addrs, gate).map(Some)
        }
    }
}

pub struct HttpSource {
    client: reqwest::Client,
    url: Url,
    limits: IoLimits,
    len: Option<u64>,
    ranges: bool,
    /// Whether the server will serve byte ranges at all, which is a wider
    /// question than [`Self::ranges`]: that one records a 206 we actually
    /// got and so drives the reads, while a server can answer 200 to a
    /// range request and still advertise `Accept-Ranges: bytes`.
    rangeable: bool,
    /// Whether the response stated any length — the total, or just this
    /// body's. Chunked delivery with no length at all is the live shape.
    finite: bool,
    stream: Option<StreamState>,
    cancel: CancelToken,
}

struct StreamState {
    response: reqwest::Response,
    pos: u64,
    /// Exclusive end of the current ranged chunk; `None` for an unbounded
    /// sequential body.
    end: Option<u64>,
    /// The chunk being served and how far into it the reads have got. A
    /// body yields whole chunks rather than filling a caller's buffer, so
    /// the remainder is held here between reads.
    chunk: Option<(Bytes, usize)>,
}

impl StreamState {
    fn new(response: reqwest::Response, pos: u64, end: Option<u64>) -> Self {
        Self {
            response,
            pos,
            end,
            chunk: None,
        }
    }

    /// Copy out of the held chunk; `0` once it is spent.
    fn serve(&mut self, buf: &mut [u8]) -> usize {
        let Some((bytes, taken)) = self.chunk.as_mut() else {
            return 0;
        };
        let remaining = &bytes[*taken..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        *taken += n;
        if *taken >= bytes.len() {
            self.chunk = None;
        }
        self.pos += n as u64;
        n
    }

    /// One read's worth of body, `0` at the end of it.
    fn read(&mut self, cancel: &CancelToken, buf: &mut [u8]) -> Result<usize, IoError> {
        // Guarded here as well as at the trait boundary: this is the loop
        // that reads a zero from `serve` as a spent chunk, and it should
        // not depend on every caller having checked first.
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.serve(buf);
            if n > 0 {
                return Ok(n);
            }
            let next = awaiting(cancel, IoErrorKind::Read, "read cancelled", async {
                self.response
                    .chunk()
                    .await
                    .map_err(|e| IoError::new(IoErrorKind::Read, e.to_string()))
            })?;
            match next {
                Some(bytes) if !bytes.is_empty() => self.chunk = Some((bytes, 0)),
                // An empty chunk is not the end of the body.
                Some(_) => {}
                None => return Ok(0),
            }
        }
    }
}

impl std::fmt::Debug for HttpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSource")
            .field("url", &self.url.as_str())
            .field("len", &self.len)
            .field("ranges", &self.ranges)
            .finish_non_exhaustive()
    }
}

impl HttpSource {
    pub fn open(
        url: &str,
        limits: IoLimits,
        gate: Arc<dyn AddressGate>,
        cancel: CancelToken,
    ) -> Result<Self, IoError> {
        if url.len() > limits.max_url_len {
            return Err(IoError::new(IoErrorKind::Cap, "URL length cap exceeded"));
        }
        let mut current =
            Url::parse(url).map_err(|e| IoError::new(IoErrorKind::Url, format!("{url}: {e}")))?;

        for _hop in 0..=limits.max_redirects {
            let probe_end = limits.chunk_bytes - 1;
            let (client, response) = awaiting(
                &cancel,
                IoErrorKind::Connect,
                "open cancelled",
                pinned_get(
                    &current,
                    &limits,
                    gate.as_ref(),
                    Some((0, probe_end)),
                    IoErrorKind::Connect,
                ),
            )?;

            let status = response.status().as_u16();
            if REDIRECT_STATUSES.contains(&status) {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        IoError::new(IoErrorKind::Redirect, format!("{status} without Location"))
                    })?;
                current = current
                    .join(location)
                    .map_err(|e| IoError::new(IoErrorKind::Redirect, format!("{location}: {e}")))?;
                continue;
            }
            if !response.status().is_success() {
                return Err(IoError {
                    kind: IoErrorKind::Http,
                    status: Some(status),
                    detail: format!("GET {current}"),
                });
            }

            if status == 206 {
                // On a 206 the total can only come from Content-Range: a
                // range-capping proxy makes Content-Length the part, not
                // the whole. A 206 that states no total still paces as
                // on-demand, it just reports an unknown size.
                let len = content_range_total(&response);
                let finite = len.is_some() || response.content_length().is_some();
                let end = response.content_length().map(|n| n.min(limits.chunk_bytes));
                return Ok(Self {
                    client,
                    url: current,
                    limits,
                    len,
                    ranges: true,
                    rangeable: true,
                    finite,
                    stream: Some(StreamState::new(response, 0, end)),
                    cancel,
                });
            }

            // No range support: a 200 answers with the whole entity from
            // byte 0, so this response is the sequential stream and the
            // open is done on the one connection it already holds.
            let len = response.content_length();
            // A server can decline this range request and still honour
            // ranges generally; the advertised header is the second arm.
            let advertises_ranges = response
                .headers()
                .get("accept-ranges")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"));
            return Ok(Self {
                client,
                url: current,
                limits,
                len,
                ranges: false,
                rangeable: advertises_ranges,
                finite: len.is_some(),
                stream: Some(StreamState::new(response, 0, None)),
                cancel,
            });
        }
        Err(IoError::new(
            IoErrorKind::Redirect,
            format!("redirect cap ({}) exceeded", limits.max_redirects),
        ))
    }

    /// The URL the source actually reads from (after redirects).
    pub fn final_url(&self) -> &Url {
        &self.url
    }

    /// Whether this source reads as on-demand rather than a live edge:
    /// finite and rangeable. A known total is deliberately not required —
    /// that is a separate question from whether the source can be paced
    /// and seeked, and demanding one misclassifies servers that serve
    /// ranges without stating a total.
    pub fn is_seekable(&self) -> bool {
        self.finite && self.rangeable
    }

    fn reopen_at(&mut self, offset: u64) -> Result<(), IoError> {
        self.stream = None;
        if self.ranges {
            let last = offset + self.limits.chunk_bytes - 1;
            let response = awaiting(
                &self.cancel,
                IoErrorKind::Read,
                "read cancelled",
                send_get(
                    &self.client,
                    &self.url,
                    Some((offset, last)),
                    Some(self.limits.request_timeout),
                    IoErrorKind::Read,
                ),
            )?;
            let status = response.status().as_u16();
            if status != 206 {
                return Err(IoError {
                    kind: IoErrorKind::Read,
                    status: Some(status),
                    detail: format!("ranged GET at {offset} for {}", self.url),
                });
            }
            let end = response.content_length().map(|n| offset + n);
            self.stream = Some(StreamState::new(response, offset, end));
            return Ok(());
        }

        // Sequential fallback: restart and discard forward to the offset.
        let response = awaiting(
            &self.cancel,
            IoErrorKind::Read,
            "read cancelled",
            send_get(&self.client, &self.url, None, None, IoErrorKind::Read),
        )?;
        if !response.status().is_success() {
            return Err(IoError {
                kind: IoErrorKind::Read,
                status: Some(response.status().as_u16()),
                detail: format!("GET {}", self.url),
            });
        }
        let mut stream = StreamState::new(response, 0, None);
        discard_until(&mut stream, &self.cancel, offset)?;
        self.stream = Some(stream);
        Ok(())
    }
}

impl ByteSource for HttpSource {
    fn size(&mut self) -> Result<Option<u64>, SourceError> {
        Ok(self.len)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        // A caller that asked for nothing gets nothing. `serve` copies zero
        // bytes and reports it as a spent chunk, so the loop below would
        // replace the chunk it is still holding — losing its unread
        // remainder — and go on pulling until the body ends.
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(len) = self.len
            && offset >= len
        {
            return Ok(0);
        }
        let cancel = self.cancel.clone();
        // Two passes at most: a positioned stream that has reached its chunk
        // boundary reopens once and reads again.
        for _ in 0..2 {
            let usable = match &self.stream {
                Some(s) if s.pos == offset => s.end.is_none_or(|end| offset < end),
                Some(s) => !self.ranges && offset >= s.pos,
                None => false,
            };
            if !usable {
                self.reopen_at(offset)?;
            } else if let Some(stream) = &mut self.stream
                && offset > stream.pos
                && let Err(e) = discard_until(stream, &cancel, offset)
            {
                self.stream = None;
                return Err(e.into());
            }

            let stream = self.stream.as_mut().expect("stream present after reopen");
            // A failed read retires the response with it. Left installed,
            // it stays usable to the check above, and a later read at the
            // same position polls a `chunk()` future that was dropped
            // mid-poll on the way out. Nothing reaches that today because
            // the session's cancel token latches, so every later call
            // resolves the cancel branch first — which makes this path
            // safe by the token's behaviour rather than by its own state.
            // One reconnect on a transient error is what that costs.
            let n = match stream.read(&cancel, buf) {
                Ok(n) => n,
                Err(e) => {
                    self.stream = None;
                    return Err(e.into());
                }
            };
            if n > 0 {
                return Ok(n);
            }
            // Chunk exhausted at the boundary: reopen; a true end of file
            // (pos at or past the known length) is a clean 0.
            let pos = stream.pos;
            if self.len.is_some_and(|len| pos < len) && stream.end == Some(pos) {
                self.stream = None;
                continue;
            }
            return Ok(0);
        }
        Ok(0)
    }
}

/// Drive one network operation on the shared runtime, racing the session's
/// cancel token. The live source gets that race from the `select!` around
/// its own open; this lane stays synchronous all the way down to the demux
/// thread, so the race belongs at each call instead.
fn awaiting<T>(
    cancel: &CancelToken,
    kind: IoErrorKind,
    detail: &'static str,
    work: impl Future<Output = Result<T, IoError>>,
) -> Result<T, IoError> {
    runtime().block_on(async {
        tokio::select! {
            _ = cancel.cancelled() => Err(IoError::new(kind, detail)),
            outcome = work => outcome,
        }
    })
}

/// Build the pinned client for `url` and send one GET through it. The
/// client comes back alongside the response because the source keeps it
/// for every later positioned read.
async fn pinned_get(
    url: &Url,
    limits: &IoLimits,
    gate: &dyn AddressGate,
    range: Option<(u64, u64)>,
    kind: IoErrorKind,
) -> Result<(reqwest::Client, reqwest::Response), IoError> {
    let client = build_pinned_client(url, limits, gate).await?;
    // Untimed: the opening probe's response becomes the body on a server
    // that answers 200, and a total timeout would cut that body off.
    let response = send_get(&client, url, range, None, kind).await?;
    Ok((client, response))
}

/// One GET, with `timeout` bounding the whole exchange — headers and
/// body both. Only a request whose body is bounded in advance can carry
/// one; an open-ended body is held to the client's read timeout and the
/// session's cancel token instead.
async fn send_get(
    client: &reqwest::Client,
    url: &Url,
    range: Option<(u64, u64)>,
    timeout: Option<Duration>,
    kind: IoErrorKind,
) -> Result<reqwest::Response, IoError> {
    let mut request = client.get(url.clone());
    if let Some((first, last)) = range {
        request = request.header("Range", format!("bytes={first}-{last}"));
    }
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    request
        .send()
        .await
        .map_err(|e| IoError::new(kind, e.to_string()))
}

async fn build_pinned_client(
    url: &Url,
    limits: &IoLimits,
    gate: &dyn AddressGate,
) -> Result<reqwest::Client, IoError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(limits.connect_timeout)
        // Per read rather than per request, so a link that has gone quiet
        // still surfaces as a typed error on the one client that has to
        // carry an unbounded sequential body as well as bounded chunks.
        .read_timeout(limits.request_timeout)
        .no_proxy()
        .user_agent("basis-media/0.1");

    if let Some((domain, addrs)) = vet_url_async(url, gate).await? {
        builder = builder.resolve_to_addrs(&domain, &addrs);
    }

    builder
        .build()
        .map_err(|e| IoError::new(IoErrorKind::Connect, e.to_string()))
}

fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get("content-range")?.to_str().ok()?;
    let total = value.rsplit('/').next()?;
    total.parse().ok()
}

fn discard_until(
    stream: &mut StreamState,
    cancel: &CancelToken,
    offset: u64,
) -> Result<(), IoError> {
    let mut scratch = [0u8; 64 * 1024];
    while stream.pos < offset {
        let want = scratch.len().min((offset - stream.pos) as usize);
        let n = stream.read(cancel, &mut scratch[..want])?;
        if n == 0 {
            return Err(IoError::new(
                IoErrorKind::Read,
                format!("source ended at {} before offset {offset}", stream.pos),
            ));
        }
    }
    Ok(())
}
