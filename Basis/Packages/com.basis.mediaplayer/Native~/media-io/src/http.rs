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
//! positioned read elsewhere costs one request. Servers without range
//! support get an untimed sequential stream (forward reads discard,
//! backward reads restart) so faststart files still play; the per-read
//! stall detector for that path arrives with the async I/O domain at M3.

use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use media_demux::{ByteSource, SourceError};
use url::Url;

use crate::{AddressGate, IoError, IoErrorKind, IoLimits};

pub(crate) const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// Scheme/host vetting shared by the blocking and async clients: resolve
/// once, permit *every* returned address (a mixed public/private answer is
/// the rebinding shape, refused whole), and hand back the pinned set for
/// `resolve_to_addrs`. `None` means the host was an IP literal, already
/// vetted. Public for transports that run their own HTTP requests over
/// the same discipline (WHEP signalling).
pub fn vet_url(
    url: &Url,
    gate: &dyn AddressGate,
) -> Result<Option<(String, Vec<SocketAddr>)>, IoError> {
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
            Ok(None)
        }
        url::Host::Ipv6(ip) => {
            if !gate.permit(ip.into()) {
                return Err(IoError::new(IoErrorKind::Blocked, format!("{ip} blocked")));
            }
            Ok(None)
        }
        url::Host::Domain(domain) => {
            let addrs: Vec<SocketAddr> = (domain, port)
                .to_socket_addrs()
                .map_err(|e| IoError::new(IoErrorKind::Resolve, format!("{domain}: {e}")))?
                .collect();
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
            Ok(Some((domain.to_string(), addrs)))
        }
    }
}

pub struct HttpSource {
    client: reqwest::blocking::Client,
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
}

struct StreamState {
    response: reqwest::blocking::Response,
    pos: u64,
    /// Exclusive end of the current ranged chunk; `None` for an unbounded
    /// sequential body.
    end: Option<u64>,
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
    pub fn open(url: &str, limits: IoLimits, gate: Arc<dyn AddressGate>) -> Result<Self, IoError> {
        if url.len() > limits.max_url_len {
            return Err(IoError::new(IoErrorKind::Cap, "URL length cap exceeded"));
        }
        let mut current =
            Url::parse(url).map_err(|e| IoError::new(IoErrorKind::Url, format!("{url}: {e}")))?;

        for _hop in 0..=limits.max_redirects {
            let client = build_pinned_client(&current, &limits, gate.as_ref(), true)?;
            let probe_end = limits.chunk_bytes - 1;
            let response = client
                .get(current.clone())
                .header("Range", format!("bytes=0-{probe_end}"))
                .send()
                .map_err(|e| IoError::new(IoErrorKind::Connect, e.to_string()))?;

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
                    stream: Some(StreamState {
                        response,
                        pos: 0,
                        end,
                    }),
                });
            }

            // No range support: switch to an untimed client so the one
            // long-lived body is not cut off by the request timeout.
            let len = response.content_length();
            // A server can decline this range request and still honour
            // ranges generally; the advertised header is the second arm.
            let advertises_ranges = response
                .headers()
                .get("accept-ranges")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"));
            drop(response);
            let client = build_pinned_client(&current, &limits, gate.as_ref(), false)?;
            let response = client
                .get(current.clone())
                .send()
                .map_err(|e| IoError::new(IoErrorKind::Connect, e.to_string()))?;
            if !response.status().is_success() {
                return Err(IoError {
                    kind: IoErrorKind::Http,
                    status: Some(response.status().as_u16()),
                    detail: format!("GET {current}"),
                });
            }
            let len = len.or(response.content_length());
            return Ok(Self {
                client,
                url: current,
                limits,
                len,
                ranges: false,
                rangeable: advertises_ranges,
                finite: len.is_some(),
                stream: Some(StreamState {
                    response,
                    pos: 0,
                    end: None,
                }),
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
            let response = self
                .client
                .get(self.url.clone())
                .header("Range", format!("bytes={offset}-{last}"))
                .send()
                .map_err(|e| IoError::new(IoErrorKind::Read, e.to_string()))?;
            let status = response.status().as_u16();
            if status != 206 {
                return Err(IoError {
                    kind: IoErrorKind::Read,
                    status: Some(status),
                    detail: format!("ranged GET at {offset} for {}", self.url),
                });
            }
            let end = response.content_length().map(|n| offset + n);
            self.stream = Some(StreamState {
                response,
                pos: offset,
                end,
            });
            return Ok(());
        }

        // Sequential fallback: restart and discard forward to the offset.
        let response = self
            .client
            .get(self.url.clone())
            .send()
            .map_err(|e| IoError::new(IoErrorKind::Read, e.to_string()))?;
        if !response.status().is_success() {
            return Err(IoError {
                kind: IoErrorKind::Read,
                status: Some(response.status().as_u16()),
                detail: format!("GET {}", self.url),
            });
        }
        let mut stream = StreamState {
            response,
            pos: 0,
            end: None,
        };
        discard_until(&mut stream, offset)?;
        self.stream = Some(stream);
        Ok(())
    }
}

impl ByteSource for HttpSource {
    fn size(&mut self) -> Result<Option<u64>, SourceError> {
        Ok(self.len)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if let Some(len) = self.len
            && offset >= len
        {
            return Ok(0);
        }
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
            {
                discard_until(stream, offset)?;
            }

            let stream = self.stream.as_mut().expect("stream present after reopen");
            let n = stream
                .response
                .read(buf)
                .map_err(|e| IoError::new(IoErrorKind::Read, e.to_string()))?;
            if n > 0 {
                stream.pos += n as u64;
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

fn build_pinned_client(
    url: &Url,
    limits: &IoLimits,
    gate: &dyn AddressGate,
    timed: bool,
) -> Result<reqwest::blocking::Client, IoError> {
    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(limits.connect_timeout)
        .timeout(if timed {
            Some(limits.request_timeout)
        } else {
            None
        })
        .no_proxy()
        .user_agent("basis-media/0.1");

    if let Some((domain, addrs)) = vet_url(url, gate)? {
        builder = builder.resolve_to_addrs(&domain, &addrs);
    }

    builder
        .build()
        .map_err(|e| IoError::new(IoErrorKind::Connect, e.to_string()))
}

fn content_range_total(response: &reqwest::blocking::Response) -> Option<u64> {
    let value = response.headers().get("content-range")?.to_str().ok()?;
    let total = value.rsplit('/').next()?;
    total.parse().ok()
}

fn discard_until(stream: &mut StreamState, offset: u64) -> Result<(), IoError> {
    let mut scratch = [0u8; 64 * 1024];
    while stream.pos < offset {
        let want = scratch.len().min((offset - stream.pos) as usize);
        let n = stream
            .response
            .read(&mut scratch[..want])
            .map_err(|e| IoError::new(IoErrorKind::Read, e.to_string()))?;
        if n == 0 {
            return Err(IoError::new(
                IoErrorKind::Read,
                format!("source ended at {} before offset {offset}", stream.pos),
            ));
        }
        stream.pos += n as u64;
    }
    Ok(())
}
