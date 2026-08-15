//! The pull-based demuxer contract (§6.2): the engine pulls, the demuxer
//! owns nothing downstream and is never re-entered.

use media_clock::{Generation, MediaTime};

use crate::{DemuxError, StreamEvent, TrackId};

/// Parse-time caps drawn from the session budget (§6.6): enforced inside
/// the demuxer, not around it.
#[derive(Debug, Clone)]
pub struct DemuxLimits {
    /// Ceiling on bytes the metadata parse may pull (boxes, sample tables).
    pub max_metadata_bytes: u64,
    /// Ceiling on a single compressed access unit — far above any real one,
    /// so a hostile size field cannot drive a huge allocation.
    pub max_au_bytes: u64,
}

impl Default for DemuxLimits {
    fn default() -> Self {
        Self {
            max_metadata_bytes: 64 * 1024 * 1024,
            max_au_bytes: 64 * 1024 * 1024,
        }
    }
}

pub trait Demuxer: Send {
    /// Pull the next event. Emits `Format` events first, then interleaved
    /// `Au`s in decode order; returns `Eos` at the end (and keeps returning
    /// it — the engine stops pulling).
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError>;

    /// Reposition to the keyframe-clean point at or before `target`, adopt
    /// `generation` for everything emitted from here, and return the actual
    /// position. Unseekable (live) demuxers return `Unsupported`.
    fn seek(&mut self, target: MediaTime, generation: Generation) -> Result<MediaTime, DemuxError>;

    /// Total duration where the container states one.
    fn duration(&self) -> Option<MediaTime>;

    /// The selected video track, once known. TS demuxers learn it from the
    /// PMT, so it can be `None` until the first packets are pulled.
    fn video_track(&self) -> Option<TrackId> {
        None
    }

    /// The selected audio track, once known (see [`Self::video_track`]).
    fn audio_track(&self) -> Option<TrackId> {
        None
    }

    /// Drain per-track findings (skipped tracks, refused layouts) for the
    /// engine to surface as diagnostics.
    fn take_notes(&mut self) -> Vec<String> {
        Vec::new()
    }
}
