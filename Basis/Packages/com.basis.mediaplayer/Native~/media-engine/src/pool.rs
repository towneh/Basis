//! The leased FramePool (§6.8): a small fixed pool of decoder-format
//! frames between decode and present. The decode side blocks (bounded,
//! stop-aware) when the pool is exhausted; the present side never blocks —
//! it takes the newest due frame or nothing. Waiting is one-directional by
//! construction, which is what makes the backpressure deadlock-safe.
//!
//! Slots carry owned [`VideoFrame`]s, so an opaque (decoder-native GPU)
//! frame rides the pool exactly like a CPU frame; recycling a slot drops
//! the payload, which for an opaque frame returns the buffer to the
//! adapter's image reader — the pool depth is therefore part of the
//! adapter's outstanding-image budget.

use std::sync::{Arc, Condvar, Mutex};

use media_clock::MediaTime;
use media_decode::VideoFrame;

pub const POOL_SLOTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Ready,
    /// Taken by the present side; frees on release.
    Leased,
}

struct Slot {
    state: SlotState,
    pts: MediaTime,
    frame: Option<VideoFrame>,
    /// The timeline the frame was decoded on. A flush clears the pool, but
    /// not before the render thread can take a lease published under the
    /// previous generation, so the frame carries its own answer.
    generation: u64,
}

struct PoolState {
    slots: Vec<Slot>,
    /// Monotonic publish counter so "newest" is well-defined.
    published: u64,
    /// Publish sequence per slot (parallel to `slots`).
    seq: Vec<u64>,
    dropped: u64,
}

pub struct FramePool {
    state: Mutex<PoolState>,
    freed: Condvar,
}

/// A filled frame currently owned by the present side.
pub struct Lease {
    pub pts: MediaTime,
    /// The generation this frame was published under; see [`Slot`].
    pub generation: u64,
    frame: Option<VideoFrame>,
    slot: usize,
}

impl Lease {
    pub fn frame(&self) -> Option<&VideoFrame> {
        self.frame.as_ref()
    }

    /// Move the frame out (the Android sink hands it to the render
    /// event); the lease still frees its slot on release.
    pub fn take_frame(&mut self) -> Option<VideoFrame> {
        self.frame.take()
    }
}

impl FramePool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState {
                slots: (0..POOL_SLOTS)
                    .map(|_| Slot {
                        state: SlotState::Free,
                        pts: MediaTime::ZERO,
                        frame: None,
                        generation: 0,
                    })
                    .collect(),
                published: 0,
                seq: vec![0; POOL_SLOTS],
                dropped: 0,
            }),
            freed: Condvar::new(),
        })
    }

    /// Decode side: publish one frame if a slot is free; a full pool hands
    /// the frame back as backpressure — the caller holds it and keeps
    /// presenting; it never blocks, because on M2's folded thread the
    /// presenter is the only thing that frees slots.
    pub fn try_publish(&self, frame: VideoFrame, generation: u64) -> Result<(), VideoFrame> {
        let mut state = self.state.lock().expect("pool lock");
        let Some(slot_index) = state.slots.iter().position(|s| s.state == SlotState::Free) else {
            return Err(frame);
        };

        let seq = state.published + 1;
        state.published = seq;
        state.seq[slot_index] = seq;
        let slot = &mut state.slots[slot_index];
        slot.state = SlotState::Ready;
        slot.pts = MediaTime::from_micros(frame.pts_us());
        slot.frame = Some(frame);
        slot.generation = generation;
        Ok(())
    }

    /// Present side: take the newest Ready frame due at `now` (pts <= now),
    /// discarding older due frames (counted as drops). Never blocks.
    pub fn take_due(&self, now: MediaTime) -> Option<Lease> {
        let state = self.state.lock().expect("pool lock");
        self.take_due_locked(state, now)
    }

    /// `take_due` for the render thread: a try-lock, so a publish in flight
    /// on the video thread costs a re-present, never a wait (§6.3 — the
    /// render thread never blocks on a media-path lock).
    pub fn try_take_due(&self, now: MediaTime) -> Option<Lease> {
        let state = self.state.try_lock().ok()?;
        self.take_due_locked(state, now)
    }

    fn take_due_locked(
        &self,
        mut state: std::sync::MutexGuard<'_, PoolState>,
        now: MediaTime,
    ) -> Option<Lease> {
        let mut due: Vec<usize> = (0..state.slots.len())
            .filter(|&i| state.slots[i].state == SlotState::Ready && state.slots[i].pts <= now)
            .collect();
        due.sort_by_key(|&i| state.seq[i]);
        let newest = due.pop()?;
        // Older due frames lost the race to the clock: recycle them.
        for &stale in &due {
            state.slots[stale].state = SlotState::Free;
            state.slots[stale].frame = None;
            state.dropped += 1;
        }
        let slot = &mut state.slots[newest];
        slot.state = SlotState::Leased;
        let lease = Lease {
            pts: slot.pts,
            generation: slot.generation,
            frame: slot.frame.take(),
            slot: newest,
        };
        if !due.is_empty() {
            self.freed.notify_all();
        }
        Some(lease)
    }

    /// The pts of the oldest Ready frame, if any — the restart point for a
    /// parked clock.
    pub fn first_ready_pts(&self) -> Option<MediaTime> {
        let state = self.state.lock().expect("pool lock");
        (0..state.slots.len())
            .filter(|&i| state.slots[i].state == SlotState::Ready)
            .min_by_key(|&i| state.seq[i])
            .map(|i| state.slots[i].pts)
    }

    /// Present side, on teardown/flush inspection: how many frames wait.
    pub fn ready_count(&self) -> usize {
        let state = self.state.lock().expect("pool lock");
        state
            .slots
            .iter()
            .filter(|s| s.state == SlotState::Ready)
            .count()
    }

    pub fn dropped(&self) -> u64 {
        self.state.lock().expect("pool lock").dropped
    }

    /// Free a lease's slot. The frame itself is the caller's to keep or
    /// drop — an opaque frame may need to outlive the lease until the
    /// render thread has consumed it.
    pub fn release(&self, lease: Lease) {
        let mut state = self.state.lock().expect("pool lock");
        state.slots[lease.slot].state = SlotState::Free;
        drop(state);
        self.freed.notify_all();
    }

    /// Flush every waiting frame (seek/teardown). A leased slot stays
    /// leased until its holder releases it.
    pub fn clear(&self) {
        let mut state = self.state.lock().expect("pool lock");
        for slot in &mut state.slots {
            if slot.state == SlotState::Ready {
                slot.state = SlotState::Free;
                slot.frame = None;
            }
        }
        drop(state);
        self.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_decode::{ColorInfo, Nv12Frame};

    fn frame(pts_ms: i64) -> VideoFrame {
        VideoFrame::Nv12(Nv12Frame {
            width: 2,
            height: 2,
            pts_us: MediaTime::from_millis(pts_ms).as_micros(),
            color: ColorInfo::default(),
            data: vec![pts_ms as u8; 8],
        })
    }

    #[test]
    fn newest_due_frame_wins_and_older_are_recycled() {
        let pool = FramePool::new();
        for ms in [0, 33, 66] {
            assert!(pool.try_publish(frame(ms), 0).is_ok());
        }
        // At t=50ms, frames 0 and 33 are due; 33 wins, 0 is dropped.
        let lease = pool.take_due(MediaTime::from_millis(50)).expect("due");
        assert_eq!(lease.pts, MediaTime::from_millis(33));
        assert_eq!(pool.dropped(), 1);
        // 66 is not due yet.
        assert!(pool.take_due(MediaTime::from_millis(50)).is_none());
        pool.release(lease);
        let lease = pool.take_due(MediaTime::from_millis(70)).expect("66 due");
        assert_eq!(lease.pts, MediaTime::from_millis(66));
        pool.release(lease);
    }

    #[test]
    fn full_pool_is_backpressure_not_a_drop() {
        let pool = FramePool::new();
        for ms in 0..POOL_SLOTS as i64 {
            assert!(pool.try_publish(frame(ms * 33), 0).is_ok());
        }
        assert!(pool.try_publish(frame(999), 0).is_err());
        assert_eq!(pool.dropped(), 0);
        // Present frees slots (all four due: newest wins, three recycled);
        // the publish then lands.
        let lease = pool.take_due(MediaTime::from_secs(10)).expect("due");
        assert_eq!(
            lease.pts,
            MediaTime::from_millis((POOL_SLOTS as i64 - 1) * 33)
        );
        assert_eq!(pool.dropped(), 3);
        pool.release(lease);
        assert!(pool.try_publish(frame(999), 0).is_ok());
    }
}
