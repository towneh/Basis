//! Network and I/O layer (§6.10): every socket the engine opens, opened
//! here. Byte sources implement the demux layer's [`ByteSource`] seam;
//! SSRF policy is *bind what you resolve* — resolve once, vet every
//! address, connect to the vetted IPs with Host/SNI carried separately,
//! and re-vet on every redirect. The address gate exists exactly once, in
//! this crate; the managed layer holds policy (consent, allowlists), not
//! mechanism.

#![forbid(unsafe_code)]

mod cancel;
mod fetch;
mod file;
mod gate;
mod http;
mod live;
mod runtime;

pub use cancel::CancelToken;
pub use fetch::ResourceFetcher;
pub use file::FileSource;
pub use gate::{AddressGate, AllowAllGate, PublicAddressGate, resolve_vetted, vet_host};
pub use http::{HttpSource, vet_url};
pub use live::HttpLiveSource;
pub use runtime::io_runtime_handle;

use std::fmt;
use std::time::Duration;

/// Edge caps drawn from the session budget (§9.2).
#[derive(Debug, Clone)]
pub struct IoLimits {
    pub max_redirects: u32,
    pub connect_timeout: Duration,
    /// Ceiling on one ranged request, connect to last body byte. Ranged
    /// reads are chunked (`chunk_bytes`) so this doubles as the stall
    /// detector; it is also the worst-case teardown wait while the async
    /// I/O domain (M3) is not yet underneath this crate.
    pub request_timeout: Duration,
    /// Size of one ranged request. Bounds both the per-request timeout's
    /// meaning and the bytes wasted by a discarded stream.
    pub chunk_bytes: u64,
    /// Per-read stall detector on sequential live sources: this long with
    /// no bytes at all is a dead link, surfaced as a typed error for the
    /// resilience path.
    pub read_stall: Duration,
    pub max_url_len: usize,
}

impl Default for IoLimits {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            connect_timeout: Duration::from_secs(8),
            request_timeout: Duration::from_secs(20),
            chunk_bytes: 4 * 1024 * 1024,
            read_stall: Duration::from_secs(10),
            max_url_len: 4096,
        }
    }
}

/// Structured error surface (§7): a typo, a 404, a TLS failure, a blocked
/// address and a cap hit are distinguishable in a field report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoErrorKind {
    /// Malformed or unsupported URL.
    Url,
    /// The address gate refused every resolved address.
    Blocked,
    /// DNS resolution failed.
    Resolve,
    /// TCP/TLS connect failed or timed out.
    Connect,
    /// The server answered with a non-success status.
    Http,
    /// Redirect handling failed (limit, missing/invalid Location).
    Redirect,
    /// A mid-stream read failed or timed out.
    Read,
    /// A cap from [`IoLimits`] tripped.
    Cap,
    /// Local file I/O failed.
    File,
}

#[derive(Debug)]
pub struct IoError {
    pub kind: IoErrorKind,
    /// HTTP status where one applies.
    pub status: Option<u16>,
    pub detail: String,
}

impl IoError {
    fn new(kind: IoErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{:?} ({status}): {}", self.kind, self.detail),
            None => write!(f, "{:?}: {}", self.kind, self.detail),
        }
    }
}

impl std::error::Error for IoError {}
