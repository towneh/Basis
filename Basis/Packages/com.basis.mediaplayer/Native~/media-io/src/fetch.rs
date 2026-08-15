//! Whole-resource fetcher for playlist-driven lanes (HLS): every fetch is
//! an independent, vetted, cancellable GET — each URL re-runs the resolve →
//! vet → pinned-connect discipline (§9.3) — or a local file read for
//! fixture playback.

use std::sync::Arc;
use std::time::Duration;

use media_demux::{ByteSource, SourceError};
use media_hls::SegmentFetcher;

use crate::{AddressGate, CancelToken, HttpLiveSource, IoLimits};

pub struct ResourceFetcher {
    limits: IoLimits,
    gate: Arc<dyn AddressGate>,
    cancel: CancelToken,
}

impl ResourceFetcher {
    pub fn new(limits: IoLimits, gate: Arc<dyn AddressGate>, cancel: CancelToken) -> Self {
        Self {
            limits,
            gate,
            cancel,
        }
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
            let metadata = std::fs::metadata(url).map_err(|e| format!("{url}: {e}"))?;
            if metadata.len() > cap {
                return Err(format!("resource exceeds the {cap}-byte cap: {url}").into());
            }
            std::fs::read(url).map_err(|e| format!("{url}: {e}").into())
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
