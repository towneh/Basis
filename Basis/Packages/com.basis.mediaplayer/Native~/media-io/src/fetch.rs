//! Whole-resource fetcher for playlist-driven lanes (HLS): every fetch is
//! an independent, vetted, cancellable GET — each URL re-runs the resolve →
//! vet → pinned-connect discipline (§9.3) — or a local file read for
//! fixture playback.
//!
//! Which of those two a fetcher can do is fixed when it is built, from
//! where the session's playlist came from, and never widens afterwards. A
//! playlist fetched over the network gets [`ResourceFetcher::remote`] and
//! has no filesystem arm at all, so no URI it names can reach the disk
//! however the URI is spelled; a playlist opened from disk gets
//! [`ResourceFetcher::local`] and keeps both, since reaching the network
//! from there is what the address gate already covers.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use media_demux::{ByteSource, SourceError};
use media_hls::SegmentFetcher;

use crate::{AddressGate, CancelToken, HttpLiveSource, IoError, IoErrorKind, IoLimits};

/// The directory a disk-origin playlist's reads are confined to, kept in
/// both forms because the two screens need different ones: the path as
/// the caller gave it, which the resolved resource is a prefix-extension
/// of, and the canonicalised one, which symlinks resolve into.
struct LocalRoot {
    given: PathBuf,
    canonical: PathBuf,
}

pub struct ResourceFetcher {
    limits: IoLimits,
    gate: Arc<dyn AddressGate>,
    cancel: CancelToken,
    /// The directory local reads resolve within. `None` is a
    /// network-origin playlist, which has no local arm.
    root: Option<LocalRoot>,
}

/// The file `url` names, confined to `root`, or a refusal.
///
/// Two screens. The lexical one compares components against the root as
/// given and touches the filesystem not at all, so nothing can race it;
/// it is what catches an absolute path, a drive-relative one, and a walk
/// back out through `..`. The canonicalised one then resolves symlinks
/// and re-checks, which catches a link planted inside the root that
/// points outside it.
///
/// What is opened is the canonicalised path, so defeating the second
/// screen means replacing a directory component of an already-resolved
/// path between the check and the open. Closing that window needs
/// openat-style handle-relative resolution, which is neither on stable
/// Rust for Windows (`windows_by_handle`, rust-lang/rust#63010) nor
/// reachable from a crate that forbids unsafe. The narrower residual is
/// the deliberate trade: without the second screen the same attacker
/// succeeds without having to win any race at all.
fn confine(root: &LocalRoot, url: &str) -> Result<PathBuf, SourceError> {
    let path = Path::new(url);
    // Strip the root and judge only what the playlist contributed: the
    // root itself is the caller's and may legitimately carry `..` or any
    // other shape. An absolute or drive-relative resource fails to strip
    // at all, which is the refusal it deserves.
    let Ok(contributed) = path.strip_prefix(&root.given) else {
        return Err(format!("playlist resource outside the playlist's directory: {url}").into());
    };
    if contributed
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("playlist resource walks out of its directory: {url}").into());
    }
    let canonical = std::fs::canonicalize(path).map_err(|e| format!("{url}: {e}"))?;
    if !canonical.starts_with(&root.canonical) {
        return Err(
            format!("playlist resource resolves outside the playlist's directory: {url}").into(),
        );
    }
    Ok(canonical)
}

impl ResourceFetcher {
    /// A fetcher for a playlist that came off the network: http(s) only,
    /// every URL address-vetted, anything else refused.
    pub fn remote(limits: IoLimits, gate: Arc<dyn AddressGate>, cancel: CancelToken) -> Self {
        Self {
            limits,
            gate,
            cancel,
            root: None,
        }
    }

    /// A fetcher for a playlist opened from disk beside `root` — the
    /// fixture lane. http(s) URIs still fetch and are still vetted.
    /// Fails when `root` cannot be canonicalised, since a root that does
    /// not resolve is not one reads can be judged against.
    pub fn local(
        root: &Path,
        limits: IoLimits,
        gate: Arc<dyn AddressGate>,
        cancel: CancelToken,
    ) -> Result<Self, IoError> {
        let canonical = std::fs::canonicalize(root)
            .map_err(|e| IoError::new(IoErrorKind::File, format!("{}: {e}", root.display())))?;
        Ok(Self {
            limits,
            gate,
            cancel,
            root: Some(LocalRoot {
                given: root.to_path_buf(),
                canonical,
            }),
        })
    }
}

impl SegmentFetcher for ResourceFetcher {
    fn fetch(&mut self, url: &str, cap: u64) -> Result<Vec<u8>, SourceError> {
        if self.cancel.is_cancelled() {
            return Err("fetch cancelled".into());
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            let mut source = HttpLiveSource::open(
                url,
                self.limits.clone(),
                Arc::clone(&self.gate),
                self.cancel.child(),
            )
            .map_err(|e| Box::new(e) as SourceError)?;
            let mut bytes = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = source.read_at(bytes.len() as u64, &mut buf)?;
                if n == 0 {
                    return Ok(bytes);
                }
                if bytes.len() as u64 + n as u64 > cap {
                    return Err(format!("resource exceeds the {cap}-byte cap: {url}").into());
                }
                bytes.extend_from_slice(&buf[..n]);
            }
        } else {
            let Some(root) = self.root.as_ref() else {
                return Err(format!(
                    "a playlist fetched over the network may not name a local resource: {url}"
                )
                .into());
            };
            let path = confine(root, url)?;
            // Length off the handle, not the name: a second resolution
            // leaves the target free to grow or be swapped between the
            // two calls, so the read is bounded whatever the size said.
            let file = std::fs::File::open(&path).map_err(|e| format!("{url}: {e}"))?;
            let stated = file.metadata().map_err(|e| format!("{url}: {e}"))?.len();
            if stated > cap {
                return Err(format!("resource exceeds the {cap}-byte cap: {url}").into());
            }
            let mut bytes = Vec::new();
            file.take(cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{url}: {e}"))?;
            if bytes.len() as u64 > cap {
                return Err(format!("resource exceeds the {cap}-byte cap: {url}").into());
            }
            Ok(bytes)
        }
    }

    fn wait(&mut self, duration: Duration) {
        // Sliced so teardown never waits out a full refresh interval.
        let mut remaining = duration;
        let slice = Duration::from_millis(50);
        while remaining > Duration::ZERO && !self.cancel.is_cancelled() {
            let step = remaining.min(slice);
            std::thread::sleep(step);
            remaining = remaining.saturating_sub(step);
        }
    }
}
