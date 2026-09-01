//! Per-session PCM output (§6.9): a lock-free SPSC ring of interleaved f32
//! frames written by the audio decode thread and drained by the Unity audio
//! thread through the ABI, PTS-annotated at chunk granularity. The consumer
//! side never takes a lock on the pull path; the playhead derives from the
//! media timeline (chunk pts markers interpolated by frames removed) and
//! feeds the session clock as its master when audio is present, so a
//! source whose sample count drifts against its pts timeline cannot drag
//! the clock. The serve trims a head that runs late against the session
//! clock for the same reason: such a source delivers more samples than
//! its timeline claims, and without the trim the surplus saturates the
//! ring and gaps upstream.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use media_clock::MediaTime;
use media_diag::diag_warn;

/// Ring capacity in frames, sized in seconds of audio.
const RING_SECONDS: u32 = 2;
/// Hard ceiling on the ring, in samples. The geometry that sizes it is
/// announced by the container or by a platform decoder, so neither factor
/// is trusted to name an allocation on its own. Above two seconds of
/// 384 kHz 7.1, which is past anything the codec table carries.
const MAX_RING_SAMPLES: usize = 6 * 1024 * 1024;
/// How late (µs) the ring head's pts may run against the session clock
/// before the serve trims it. Depth alone is normal (the VOD startup
/// burst fills the ring by design, live joins bank legitimately); a head
/// that stays late against the clock means the source delivers more
/// samples than its timeline claims, and holding the surplus saturates
/// the ring and gaps the track upstream. Sized above the ladder's snap
/// threshold's practical wobble and the pull cadence, below anything a
/// viewer would read as drift.
const TRIM_LATE_US: i64 = 300_000;
/// Per-pull trim bound, frames: small steps so the master playhead jumps
/// by at most ~21 ms at a time (the ladder absorbs that without a snap).
const TRIM_MAX_FRAMES: usize = 1024;
/// Chunk pts markers buffered between producer and consumer. Only chunks
/// whose pts the consumer could not already work out take a slot, so this
/// counts discontinuities in flight rather than chunks, and a stream has
/// to gap on more than a thousand of them before the ring is the binding
/// constraint on depth.
const PTS_MARKERS: usize = 1024;

#[derive(Clone, Copy)]
struct PtsMarker {
    /// Absolute frame index (frames pushed before this chunk) the pts
    /// applies to.
    index: u64,
    pts_us: i64,
}

pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u32,
}

/// State shared between the producer, the consumer and pollers.
pub struct AudioShared {
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    /// Frames handed to the consumer since the current generation's origin.
    pub consumed_frames: AtomicU64,
    /// Media time of the first frame written this generation, µs.
    pub base_pts_us: AtomicI64,
    /// Wall time (engine µs) of the last consumer pull that made progress.
    pub last_pull_wall_us: AtomicI64,
    /// Managed sink output latency (µs): the chain after the pull (DSP
    /// buffers + HAL). Subtracted from the playhead so the clock master is
    /// the *audible* position and video paces to match. 0 = uncompensated.
    pub output_latency_us: AtomicI64,
    /// Media time of the ring head as of the last pull (µs), derived from
    /// the chunk pts markers — the playhead's timeline authority once set.
    /// `i64::MIN` until the first marker is consumed.
    pub playhead_pts_us: AtomicI64,
    /// Lock-free mirror of the session clock for the pull path (which
    /// must never take the clock lock): position + the wall it was read
    /// at, written by the audio thread each tick while the clock plays;
    /// `i64::MIN` while parked, which disables the serve trim (parked
    /// spans — startup, seeks, join backlogs — legitimately hold depth).
    pub clock_now_us: AtomicI64,
    pub clock_wall_us: AtomicI64,
    /// Interleaved frames pushed this generation (production counter).
    pub pushed_frames: AtomicU64,
    /// Frames discarded by the serve-side lateness trim, this generation.
    /// Reset with the rest of the block, because the ring's own accounting
    /// invariant — served plus trimmed equals pushed — only holds inside one
    /// generation.
    pub trimmed_frames: AtomicU64,
    /// The same trim, counted for the life of the session and never reset.
    ///
    /// Diagnostics want a figure that only ever climbs: a capture column that
    /// drops after a seek loses the trims before it, and the event beside it is
    /// rate-limited against a high-water mark that a reset leaves stranded above
    /// the counter, silencing the event until the new generation passes the old
    /// session total.
    pub trimmed_frames_total: AtomicU64,
}

impl AudioShared {
    fn new() -> Self {
        Self {
            sample_rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            consumed_frames: AtomicU64::new(0),
            base_pts_us: AtomicI64::new(0),
            last_pull_wall_us: AtomicI64::new(i64::MIN),
            output_latency_us: AtomicI64::new(0),
            playhead_pts_us: AtomicI64::new(i64::MIN),
            clock_now_us: AtomicI64::new(i64::MIN),
            clock_wall_us: AtomicI64::new(i64::MIN),
            pushed_frames: AtomicU64::new(0),
            trimmed_frames: AtomicU64::new(0),
            trimmed_frames_total: AtomicU64::new(0),
        }
    }

    /// The audio playhead: the ring head's media time as of the last pull
    /// (chunk pts markers interpolated by frames removed — the pts
    /// timeline, not the sample count, is the authority), extrapolated by
    /// the time since that pull (the consumer drains in DSP-buffer quanta,
    /// so the raw position stair-steps one buffer behind real time).
    /// Meaningful only once a format is set and consumption has begun.
    pub fn playhead(&self, wall: MediaTime) -> Option<MediaTime> {
        let rate = self.sample_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return None;
        }
        let consumed = self.consumed_frames.load(Ordering::Relaxed);
        if consumed == 0 {
            return None;
        }
        let since_pull = (wall
            - MediaTime::from_micros(self.last_pull_wall_us.load(Ordering::Relaxed)))
        .clamp(MediaTime::ZERO, MediaTime::from_millis(40));
        let latency = MediaTime::from_micros(self.output_latency_us.load(Ordering::Relaxed));
        let position = match self.playhead_pts_us.load(Ordering::Relaxed) {
            i64::MIN => {
                // No marker consumed yet: fall back to the sample-counted
                // form (identical on any timeline until the first chunk's
                // marker lands, which the first progressing pull consumes).
                let base = self.base_pts_us.load(Ordering::Relaxed);
                base + (consumed as i64) * 1_000_000 / i64::from(rate)
            }
            pts => pts,
        };
        Some(MediaTime::from_micros(position) + since_pull - latency)
    }
}

/// Decode-thread half.
pub struct AudioProducer {
    ring: rtrb::Producer<f32>,
    markers: rtrb::Producer<PtsMarker>,
    shared: Arc<AudioShared>,
    channels: u32,
    sample_rate: u32,
    base_set: bool,
    pushed_frames: u64,
    /// The newest marker written, which is the base the consumer will
    /// extrapolate from once it reaches it.
    last_marker: Option<PtsMarker>,
}

impl AudioProducer {
    /// Push presentable interleaved samples stamped with the first frame's
    /// pts. Frames before the media origin (negative pts — encoder priming)
    /// must already be dropped by the caller. Returns the number of
    /// *samples* written; the rest is backpressure the caller retries after
    /// a wait (with the pts advanced past what was written).
    pub fn push(&mut self, pts_us: i64, samples: &[f32]) -> usize {
        if !self.base_set {
            self.shared.base_pts_us.store(pts_us, Ordering::Relaxed);
            self.base_set = true;
        }
        let free = self.ring.slots();
        // Whole frames only, so the interleave never shears.
        let channels = self.channels.max(1) as usize;
        let take = (free / channels * channels).min(samples.len() / channels * channels);
        if take == 0 {
            return 0;
        }
        // Marker before the samples: the consumer drains markers up to
        // its removed count, so one must never describe frames that
        // could be consumed before it is visible.
        if let Some(marker) = self.marker_for(pts_us) {
            if self.markers.push(marker).is_err() {
                // The chunk's own pts, and nothing else, says where it
                // sits on the timeline. Losing it leaves the consumer
                // extrapolating an older chunk's timeline over this one,
                // which lands on the serve-side lateness trim as an
                // arbitrary error and costs audio. Take the back-pressure
                // instead, exactly as a full sample ring does: the caller
                // retries once the consumer has drained.
                return 0;
            }
            self.last_marker = Some(marker);
        }
        for &s in &samples[..take] {
            let _ = self.ring.push(s);
        }
        self.pushed_frames += (take / channels) as u64;
        self.shared
            .pushed_frames
            .store(self.pushed_frames, Ordering::Relaxed);
        take
    }

    /// The marker this chunk needs, or `None` when the consumer would
    /// already put the chunk where it belongs.
    ///
    /// Chunk cadence is the stream's to choose and the marker ring is
    /// fixed, so a stream of minimum-size access units — FLAC blocks go
    /// down to sixteen samples — would otherwise exhaust the budget on
    /// nothing but cadence. A contiguous chunk carries no information the
    /// consumer does not already have: it interpolates from the marker it
    /// holds by frames removed, which is this chunk's pts. Comparing
    /// against the retained marker rather than the previous chunk keeps
    /// the error bounded by the tolerance instead of letting it accrue.
    fn marker_for(&self, pts_us: i64) -> Option<PtsMarker> {
        let marker = PtsMarker {
            index: self.pushed_frames,
            pts_us,
        };
        let rate = i64::from(self.sample_rate.max(1));
        // One sample period: below the resolution the consumer's own
        // interpolation works in.
        let tolerance = (1_000_000 / rate).max(1);
        let redundant = self.last_marker.is_some_and(|last| {
            // Saturating throughout: the pts is the stream's to state, and
            // a hostile one must cost a marker rather than a panic.
            let ahead = i64::try_from(marker.index.saturating_sub(last.index)).unwrap_or(i64::MAX);
            let predicted = last
                .pts_us
                .saturating_add(ahead.saturating_mul(1_000_000) / rate);
            pts_us.saturating_sub(predicted).saturating_abs() <= tolerance
        });
        (!redundant).then_some(marker)
    }

    pub fn free_frames(&self) -> usize {
        self.ring.slots() / self.channels.max(1) as usize
    }

    /// True once the consumer has pulled everything pushed so far.
    pub fn is_drained(&self) -> bool {
        self.ring.slots() == self.ring.buffer().capacity()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }
}

/// Unity-audio-thread half. Lock-free pull.
pub struct AudioConsumer {
    ring: rtrb::Consumer<f32>,
    markers: rtrb::Consumer<PtsMarker>,
    shared: Arc<AudioShared>,
    channels: u32,
    sample_rate: u32,
    /// Frames removed from the ring (served + trimmed) — the index space
    /// the pts markers map onto.
    removed_frames: u64,
    /// The newest marker at or before `removed_frames`.
    anchor: Option<PtsMarker>,
}

impl AudioConsumer {
    /// Advance the pts anchor to the newest marker at or before the ring
    /// head, and return the head's media time (µs) when a marker has been
    /// seen.
    fn head_pts(&mut self) -> Option<i64> {
        loop {
            let due = self
                .markers
                .peek()
                .map(|m| m.index <= self.removed_frames)
                .unwrap_or(false);
            if !due {
                break;
            }
            if let Ok(marker) = self.markers.pop() {
                self.anchor = Some(marker);
            }
        }
        self.anchor.map(|anchor| {
            let ahead = (self.removed_frames - anchor.index) as i64;
            anchor.pts_us + ahead * 1_000_000 / i64::from(self.sample_rate.max(1))
        })
    }

    /// Fill `out` with as many whole frames as are ready; the remainder is
    /// zeroed (silence). Returns frames written. `wall_us` stamps consumer
    /// liveness for the master-selection heuristic.
    pub fn pull(&mut self, out: &mut [f32], wall_us: i64) -> usize {
        let channels = self.channels.max(1) as usize;
        let rate = i64::from(self.sample_rate.max(1));

        // Serve-side trim: a head that runs late against the session
        // clock means the source delivers more samples than its timeline
        // claims (depth alone is normal — the startup burst and live-join
        // backlogs bank legitimately, and a parked clock disables the
        // check entirely). Discard the late span in bounded steps: it has
        // no slot in the timeline, and holding it saturates the ring and
        // gaps the track upstream.
        let clock_now = self.shared.clock_now_us.load(Ordering::Relaxed);
        if clock_now != i64::MIN
            && let Some(head) = self.head_pts()
        {
            let since =
                (wall_us - self.shared.clock_wall_us.load(Ordering::Relaxed)).clamp(0, 100_000);
            let late = (clock_now + since) - head - TRIM_LATE_US;
            if late > 0 {
                let trim = ((late * rate / 1_000_000) as usize)
                    .min(TRIM_MAX_FRAMES)
                    .min(self.ring.slots() / channels);
                for _ in 0..trim * channels {
                    let _ = self.ring.pop();
                }
                self.removed_frames += trim as u64;
                self.shared
                    .trimmed_frames
                    .fetch_add(trim as u64, Ordering::Relaxed);
                // Both, at the one site that trims. Deriving the session figure
                // from the per-generation one would need whoever installs a
                // generation to snapshot it first, and the install resets the
                // block from inside.
                self.shared
                    .trimmed_frames_total
                    .fetch_add(trim as u64, Ordering::Relaxed);
            }
        }

        let want = out.len() / channels * channels;
        let ready = self.ring.slots() / channels * channels;
        let take = want.min(ready);
        for slot in out[..take].iter_mut() {
            *slot = self.ring.pop().unwrap_or(0.0);
        }
        out[take..].fill(0.0);
        if take > 0 {
            self.removed_frames += (take / channels) as u64;
            self.shared
                .consumed_frames
                .fetch_add((take / channels) as u64, Ordering::Relaxed);
            self.shared
                .last_pull_wall_us
                .store(wall_us, Ordering::Relaxed);
            if let Some(head) = self.head_pts() {
                self.shared.playhead_pts_us.store(head, Ordering::Relaxed);
            }
        }
        take / channels
    }
}

/// Install a fresh ring for one generation's format — a seek's flush or a
/// mid-stream format change — and return the producer half.
///
/// The slot's own lock is taken here rather than by the caller, because
/// the shared block's reset and the swap have to be one critical section
/// and a signature the caller can satisfy without holding that lock does
/// not say so. Holding the slot is what makes the swap safe: a pull holds
/// it for its whole duration, so the reset cannot land underneath one
/// already in flight. Were it able to, that pull's own stores would go
/// back on top of the reset — the retired generation's playhead, consumed
/// count and pull wall — and the clock, which masters on the playhead,
/// would read the previous timeline's absolute position against the new
/// one: an error the size of the seek, which the ladder answers with a
/// snap to a position nothing has decoded behind. The pull path serves
/// silence on a failed `try_lock`, so the contention this adds costs one
/// block.
pub fn install_audio_generation(
    slot: &Mutex<Option<AudioConsumer>>,
    format: AudioFormatInfo,
    shared: Arc<AudioShared>,
) -> AudioProducer {
    // A pull that panicked while holding the slot poisons it. What is
    // behind the lock is replaced wholesale here, so nothing this call
    // does depends on the previous holder having finished — and panicking
    // instead would take the opener thread down on every later seek and
    // format change, where `read_audio` merely serves silence.
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    let (producer, consumer) = audio_pair(format, shared);
    *slot = Some(consumer);
    producer
}

/// Create the pair for one generation's format, resetting the shared
/// block to that generation's origin. Private because the reset is only
/// safe with the consumer slot held: go through `install_audio_generation`.
fn audio_pair(format: AudioFormatInfo, shared: Arc<AudioShared>) -> (AudioProducer, AudioConsumer) {
    // Saturating throughout: the announced rate reaching u32::MAX wraps a
    // plain multiply, and the wrap is silent in a release build. The
    // announced values still go to the shared state below verbatim — this
    // bounds the allocation, not the timeline.
    let frames = format.sample_rate.max(8000).saturating_mul(RING_SECONDS);
    // The cap counts samples while both ends of the ring work in whole
    // frames, so it is rounded down to one: a channel count that does not
    // divide it would otherwise leave slots no push or pull can use.
    //
    // A geometry whose single frame is wider than the cap has no working
    // answer: neither the rounded-down zero nor the flat cap holds one
    // frame, so nothing can ever be pushed. The flat cap keeps the
    // allocation bounded, which is what matters here, and the line says
    // the lane is inert rather than leaving it to look like a stall. It
    // is not reachable from a decoder — this is sized from what one
    // reports, and every adapter screens its own channel count far below
    // the cap (8 on the MF and software audio routes, 2 for Opus, 64 on
    // MediaCodec) against a cap of six million samples.
    let channels = format.channels.max(1) as usize;
    let cap = match MAX_RING_SAMPLES / channels * channels {
        0 => {
            diag_warn!(
                "audio ring: one frame of {channels} channels exceeds the {MAX_RING_SAMPLES}-sample cap, so no audio can be banked"
            );
            MAX_RING_SAMPLES
        }
        whole => whole,
    };
    let capacity = (frames.saturating_mul(format.channels.max(1)) as usize).min(cap);
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    let (marker_tx, marker_rx) = rtrb::RingBuffer::new(PTS_MARKERS);
    shared
        .sample_rate
        .store(format.sample_rate, Ordering::Relaxed);
    shared.channels.store(format.channels, Ordering::Relaxed);
    shared.consumed_frames.store(0, Ordering::Relaxed);
    shared.base_pts_us.store(0, Ordering::Relaxed);
    shared.playhead_pts_us.store(i64::MIN, Ordering::Relaxed);
    shared.clock_now_us.store(i64::MIN, Ordering::Relaxed);
    shared.pushed_frames.store(0, Ordering::Relaxed);
    shared.trimmed_frames.store(0, Ordering::Relaxed);
    // trimmed_frames_total is deliberately not reset here. It is what the
    // diagnostics quote, and a session total that restarts at a seek reports
    // less loss than the session actually had.
    (
        AudioProducer {
            ring: producer,
            markers: marker_tx,
            shared: Arc::clone(&shared),
            channels: format.channels,
            sample_rate: format.sample_rate,
            base_set: false,
            pushed_frames: 0,
            last_marker: None,
        },
        AudioConsumer {
            ring: consumer,
            markers: marker_rx,
            shared,
            channels: format.channels,
            sample_rate: format.sample_rate,
            removed_frames: 0,
            anchor: None,
        },
    )
}

pub fn new_audio_shared() -> Arc<AudioShared> {
    Arc::new(AudioShared::new())
}

/// The C player's priming rule: given a chunk starting at `pts_us` with
/// `frames` frames at `rate`, how many leading frames precede the media
/// origin and must be dropped (rounding up, so a partially primed frame is
/// dropped rather than half-played).
pub fn frames_before_origin(pts_us: i64, frames: usize, rate: u32) -> usize {
    if pts_us >= 0 || frames == 0 || rate == 0 {
        return 0;
    }
    if pts_us == i64::MIN {
        return frames;
    }
    let drop = ((-pts_us) * i64::from(rate) + 999_999) / 1_000_000;
    usize::try_from(drop).map_or(frames, |d| d.min(frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priming_frames_are_counted_conservatively() {
        assert_eq!(frames_before_origin(0, 1024, 48000), 0);
        assert_eq!(frames_before_origin(100, 1024, 48000), 0);
        // One full AAC frame ahead of the origin.
        assert_eq!(frames_before_origin(-21333, 1024, 48000), 1024);
        // Half a frame rounds up.
        assert_eq!(frames_before_origin(-10667, 1024, 48000), 513);
        assert_eq!(frames_before_origin(i64::MIN, 7, 48000), 7);
    }

    /// The sizing factors are announced by the stream, so the ring must
    /// stay bounded whatever they say. Unbounded, a saturated rate wraps
    /// the frame count — silently, in a release build — and lands a
    /// multi-gigabyte request; the shared state still carries the
    /// announced values, which the playhead maths reads verbatim.
    #[test]
    fn a_hostile_geometry_cannot_size_the_ring() {
        for (sample_rate, channels) in [
            (u32::MAX, u32::MAX),
            (u32::MAX, 2),
            (5_000_000_000u64 as u32, 8),
            // Counts that do not divide the cap, so the clamp has to round
            // rather than land on it.
            (u32::MAX, 3),
            (u32::MAX, 5),
            (u32::MAX, 7),
        ] {
            let shared = new_audio_shared();
            let (producer, _consumer) = audio_pair(
                AudioFormatInfo {
                    sample_rate,
                    channels,
                },
                Arc::clone(&shared),
            );
            assert!(
                producer.ring.slots() <= MAX_RING_SAMPLES,
                "{sample_rate} Hz / {channels} ch sized {} slots",
                producer.ring.slots()
            );
            // Whole frames only: the ring is pushed and pulled a frame at
            // a time, so a remainder is capacity nothing can reach.
            let slots = producer.ring.slots();
            if slots >= channels as usize {
                assert_eq!(
                    slots % channels as usize,
                    0,
                    "{sample_rate} Hz / {channels} ch left {} slots over",
                    slots % channels as usize
                );
            }
            assert_eq!(shared.sample_rate.load(Ordering::Relaxed), sample_rate);
            assert_eq!(shared.channels.load(Ordering::Relaxed), channels);
        }
        // A real geometry is unaffected.
        let shared = new_audio_shared();
        let (producer, _consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48_000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        assert_eq!(producer.ring.slots(), 48_000 * 2 * 2);
    }

    /// The marker ring is fixed and chunk cadence is the stream's to
    /// choose, so a source can offer more gapped chunks than there are
    /// slots. Dropping the overflow would leave the consumer extrapolating
    /// a stale chunk's timeline over the ones that followed, which reaches
    /// the serve-side lateness trim as an arbitrary error and costs audio.
    #[test]
    fn a_gapped_stream_cannot_outrun_the_pts_marker_ring() {
        const FRAMES: usize = 16;
        const RATE: i64 = 48_000;
        // A whole chunk of silence between each: every chunk's pts is a
        // discontinuity, so no marker can be worked out from another.
        const STEP_US: i64 = 2 * FRAMES as i64 * 1_000_000 / RATE;

        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: RATE as u32,
                channels: 1,
            },
            Arc::clone(&shared),
        );

        let chunk = vec![0.0f32; FRAMES];
        let mut pushed: Vec<i64> = Vec::new();
        let mut pts = 0i64;
        while pushed.len() < PTS_MARKERS * 4 && producer.push(pts, &chunk) == FRAMES {
            pushed.push(pts);
            pts += STEP_US;
        }
        // Drain a chunk at a time. The playhead names the chunk at the
        // ring head, which is the one after the chunk just served.
        let mut out = vec![0.0f32; FRAMES];
        for (i, next) in pushed.iter().skip(1).enumerate() {
            consumer.pull(&mut out, 1_000 + i as i64);
            assert_eq!(
                shared.playhead_pts_us.load(Ordering::Relaxed),
                *next,
                "playhead lost the timeline at chunk {}",
                i + 1
            );
        }
        assert_eq!(
            pushed.len(),
            PTS_MARKERS,
            "the marker ring, not the sample ring, has to be what stops this"
        );
        assert!(
            producer.free_frames() > FRAMES,
            "the sample ring still had room, so the stop was back-pressure"
        );
    }

    /// Back-pressure on the marker ring must not cost depth on an ordinary
    /// stream: a contiguous chunk tells the consumer nothing it cannot
    /// already interpolate, so however small the chunks are the ring still
    /// fills to its two seconds.
    #[test]
    fn a_contiguous_stream_of_tiny_chunks_still_fills_the_ring() {
        const FRAMES: usize = 16;
        const RATE: i64 = 48_000;

        let shared = new_audio_shared();
        let (mut producer, _consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: RATE as u32,
                channels: 1,
            },
            Arc::clone(&shared),
        );

        let chunk = vec![0.0f32; FRAMES];
        let mut chunks = 0u64;
        loop {
            let pts = (chunks * FRAMES as u64) as i64 * 1_000_000 / RATE;
            if producer.push(pts, &chunk) != FRAMES {
                break;
            }
            chunks += 1;
        }
        assert!(
            chunks > PTS_MARKERS as u64 * 4,
            "only {chunks} chunks of {FRAMES} frames fit — the marker budget is capping depth"
        );
        assert!(
            producer.free_frames() < FRAMES,
            "the sample ring is what filled, not the marker ring"
        );
    }

    /// A generation swap — a seek's flush, or a mid-stream format change —
    /// rebuilds the ring and resets the shared block. A pull holds the
    /// consumer slot for its whole duration, so one already in flight must
    /// not be able to land the retired generation's playhead, consumed
    /// count and pull wall on top of that reset: the clock masters on the
    /// playhead, and the previous timeline's absolute position against the
    /// new one is the whole seek distance of error.
    #[test]
    fn a_generation_swap_cannot_be_undone_by_a_pull_in_flight() {
        let format = || AudioFormatInfo {
            sample_rate: 48_000,
            channels: 2,
        };
        let shared = new_audio_shared();
        let slot: Arc<Mutex<Option<AudioConsumer>>> = Arc::new(Mutex::new(None));
        let mut producer = install_audio_generation(&slot, format(), Arc::clone(&shared));

        // The retired generation, mid-file: pushed and part-consumed, so
        // the shared block carries a playhead of its own.
        producer.push(5_000_000, &vec![0.0f32; 4096]);
        let mut out = vec![0.0f32; 2048];
        slot.lock()
            .expect("slot")
            .as_mut()
            .expect("consumer")
            .pull(&mut out, 1_000);
        assert_ne!(shared.playhead_pts_us.load(Ordering::Relaxed), i64::MIN);

        // Hold the slot the way a pull in flight does, then swap.
        let in_flight = slot.lock().expect("slot");
        let at_the_swap = Arc::new(std::sync::Barrier::new(2));
        let swapping = {
            let slot = Arc::clone(&slot);
            let shared = Arc::clone(&shared);
            let at_the_swap = Arc::clone(&at_the_swap);
            std::thread::spawn(move || {
                at_the_swap.wait();
                install_audio_generation(&slot, format(), shared)
            })
        };
        // The barrier says the swap is under way. What orders it behind
        // the pull is the slot guard, not the moment the thread reaches
        // the lock, so there is nothing here for a loaded machine to get
        // wrong — and a timer long enough to be sure on a quiet one is a
        // timer that proves nothing on a busy one.
        at_the_swap.wait();
        let mut in_flight = in_flight;
        in_flight.as_mut().expect("consumer").pull(&mut out, 2_000);
        drop(in_flight);
        drop(swapping.join().expect("swap thread"));

        assert_eq!(
            shared.playhead_pts_us.load(Ordering::Relaxed),
            i64::MIN,
            "the retired generation's playhead survived the swap"
        );
        assert_eq!(
            shared.consumed_frames.load(Ordering::Relaxed),
            0,
            "the retired generation's consumed count survived the swap"
        );
    }

    #[test]
    fn playhead_subtracts_output_latency() {
        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        let chunk = vec![0.0f32; 2048];
        assert_eq!(producer.push(0, &chunk), 2048);
        let mut out = vec![0.0f32; 512];
        consumer.pull(&mut out, 1_000);

        let wall = MediaTime::from_micros(1_000);
        let uncompensated = shared.playhead(wall).unwrap();
        shared.output_latency_us.store(60_000, Ordering::Relaxed);
        assert_eq!(
            shared.playhead(wall).unwrap(),
            uncompensated - MediaTime::from_millis(60)
        );
    }

    /// The whole ladder inherits the latency offset — with a sink
    /// latency reported mid-play, the clock slews back by exactly that
    /// offset (within the dead band) and settles there.
    #[test]
    fn ladder_inherits_the_latency_offset() {
        use media_clock::{ClockConfig, Correction, Generation, Master, MediaClock};

        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        let cfg = ClockConfig::default();
        let dead_band = cfg.dead_band;
        let mut clock = MediaClock::new(cfg, MediaTime::ZERO, MediaTime::ZERO, Generation(0));
        clock.set_playing(MediaTime::ZERO, true);
        clock.set_master(MediaTime::ZERO, Master::Audio);

        // The consumer pulls one 20 ms DSP block per tick, exactly at 1x,
        // from a source whose pts timeline is sample-exact.
        let block = vec![0.0f32; 960 * 2];
        let mut out = vec![0.0f32; 960 * 2];
        let mut push_pts = 0i64;
        let mut pull = |wall_us: i64| {
            producer.push(push_pts, &block);
            push_pts += 960 * 1_000_000 / 48_000;
            consumer.pull(&mut out, wall_us);
        };

        // Converged at zero latency: observations inside the dead band.
        let mut wall_us = 0;
        for _ in 0..10 {
            wall_us += 20_000;
            pull(wall_us);
            let wall = MediaTime::from_micros(wall_us);
            clock.observe_master(wall, shared.playhead(wall).unwrap());
        }
        let wall = MediaTime::from_micros(wall_us);
        let before = clock.now(wall) - shared.playhead(wall).unwrap();
        assert!(before.abs() <= dead_band, "unconverged baseline: {before}");

        // Sink latency arrives: the master shifts back 100 ms; the clock
        // must slew (never snap — 100 ms is under the snap threshold)
        // until it sits within the dead band of the shifted master.
        shared.output_latency_us.store(100_000, Ordering::Relaxed);
        let mut slewed = false;
        for _ in 0..600 {
            wall_us += 20_000;
            pull(wall_us);
            let wall = MediaTime::from_micros(wall_us);
            match clock.observe_master(wall, shared.playhead(wall).unwrap()) {
                Correction::Slew { .. } => slewed = true,
                Correction::Snap { error } => panic!("latency must never snap: {error}"),
                Correction::None => {}
            }
        }
        assert!(slewed, "a 100 ms offset must engage the slew rung");
        let wall = MediaTime::from_micros(wall_us);
        let error = clock.now(wall) - shared.playhead(wall).unwrap();
        assert!(
            error.abs() <= dead_band,
            "clock should settle onto the compensated master, error {error}"
        );
        // And the settled position is ~the latency behind the raw pull
        // playhead: video due-times shifted to the audible timeline.
        let raw = shared.playhead(wall).unwrap() + MediaTime::from_millis(100);
        let shift = raw - clock.now(wall);
        assert!(
            (shift - MediaTime::from_millis(100)).abs() <= dead_band,
            "expected ~100 ms shift, got {shift}"
        );
    }

    /// The pts timeline, not the sample count, owns the playhead. A
    /// source stamping 1024-sample chunks ~1010 samples of pts apart (the
    /// ms-quantised passthrough class) must not drag the master ahead of
    /// its own timeline.
    #[test]
    fn pts_timeline_owns_the_playhead() {
        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        let chunk = vec![0.0f32; 1024 * 2];
        let mut out = vec![0.0f32; 1024 * 2];
        let mut pts_us = 0i64;
        for i in 0..40 {
            assert_eq!(producer.push(pts_us, &chunk), 1024 * 2);
            pts_us += 1010 * 1_000_000 / 48_000;
            consumer.pull(&mut out, (i + 1) * 21_000);
        }
        // 40 chunks consumed: sample count says 40*1024 frames = 853 ms;
        // the timeline says the head sits at chunk 40's pts.
        let playhead = shared
            .playhead(MediaTime::from_micros(40 * 21_000))
            .unwrap();
        let sample_counted = MediaTime::from_micros(40 * 1024 * 1_000_000 / 48_000);
        // The drained head extrapolates from the newest marker by sample
        // count, so the playhead sits within one chunk of the timeline.
        assert!(
            (playhead - MediaTime::from_micros(pts_us)).abs() < MediaTime::from_millis(2),
            "playhead must sit on the pts timeline, got {playhead} vs {pts_us}us"
        );
        assert!(
            sample_counted - playhead > MediaTime::from_millis(10),
            "the drift this guards against must be visible in the fixture"
        );
    }

    /// A head that runs late against the session clock trims in
    /// bounded steps instead of saturating the ring — the surplus samples
    /// have no slot in the timeline. The clock mirror is driven at wall
    /// rate while the fixture's pts advance at half the sample count (the
    /// ms-quantised passthrough class, exaggerated).
    #[test]
    fn serve_trims_a_head_late_against_the_clock() {
        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        let chunk = vec![0.0f32; 1024 * 2];
        let mut pts_us = 0i64;
        let mut pushed = 0usize;
        loop {
            let wrote = producer.push(pts_us, &chunk);
            pushed += wrote / 2;
            if wrote < chunk.len() {
                break;
            }
            pts_us += 512 * 1_000_000 / 48_000;
        }

        let mut out = vec![0.0f32; 512 * 2];
        let mut served = 0u64;
        let mut wall = 0i64;
        let mut last_trimmed = 0u64;
        for _ in 0..1000 {
            wall += 10_000;
            shared.clock_now_us.store(wall, Ordering::Relaxed);
            shared.clock_wall_us.store(wall, Ordering::Relaxed);
            served += consumer.pull(&mut out, wall) as u64;
            let trimmed = shared.trimmed_frames.load(Ordering::Relaxed);
            assert!(
                trimmed - last_trimmed <= 1024,
                "trims must step in bounded increments"
            );
            last_trimmed = trimmed;
        }
        let trimmed = shared.trimmed_frames.load(Ordering::Relaxed);
        assert!(trimmed > 0, "a chronically late head must trim");
        assert_eq!(
            served + trimmed,
            pushed as u64,
            "served + trimmed must account for every pushed frame"
        );
    }

    /// The session trim total survives a generation change; the
    /// per-generation counter does not.
    ///
    /// A seek reinstalls the audio generation, which resets the shared block.
    /// Diagnostics quote the total, so if that reset reached it the capture
    /// column would fall after a seek and report less loss than the session
    /// had — while the event beside it, rate-limited against a high-water
    /// mark held across generations, would go quiet until the new generation
    /// passed the old total.
    #[test]
    fn the_session_trim_total_survives_a_generation_change() {
        fn trim_some(shared: &Arc<AudioShared>) {
            let (mut producer, mut consumer) = audio_pair(
                AudioFormatInfo {
                    sample_rate: 48000,
                    channels: 2,
                },
                Arc::clone(shared),
            );
            let chunk = vec![0.0f32; 1024 * 2];
            let mut pts_us = 0i64;
            loop {
                let wrote = producer.push(pts_us, &chunk);
                if wrote < chunk.len() {
                    break;
                }
                // Half the sample count, so the head runs late against the
                // clock and the serve trim engages.
                pts_us += 512 * 1_000_000 / 48_000;
            }
            let mut out = vec![0.0f32; 512 * 2];
            let mut wall = 0i64;
            for _ in 0..200 {
                wall += 10_000;
                shared.clock_now_us.store(wall, Ordering::Relaxed);
                shared.clock_wall_us.store(wall, Ordering::Relaxed);
                consumer.pull(&mut out, wall);
            }
        }

        let shared = new_audio_shared();

        trim_some(&shared);
        let first_generation = shared.trimmed_frames.load(Ordering::Relaxed);
        let first_total = shared.trimmed_frames_total.load(Ordering::Relaxed);
        assert!(first_generation > 0, "the fixture must actually trim");
        assert_eq!(first_generation, first_total, "one generation, one figure");

        // The seek. audio_pair is what install_audio_generation calls, and the
        // reset lives there.
        trim_some(&shared);
        let second_generation = shared.trimmed_frames.load(Ordering::Relaxed);
        let second_total = shared.trimmed_frames_total.load(Ordering::Relaxed);

        assert!(
            second_total > first_total,
            "the session total only climbs: {second_total} against {first_total}"
        );
        // The two together, which is the whole point: the per-generation counter
        // restarted and carries only this generation's share, while the total
        // carries both. Either one alone passes with the reset reaching the wrong
        // counter.
        assert_eq!(
            second_total,
            first_total + second_generation,
            "the total is every generation's trim summed"
        );
    }

    /// A completely full ring on an honest timeline (the VOD startup-burst
    /// shape) must never trim: depth is not the signal, lateness is.
    #[test]
    fn full_ring_on_an_honest_timeline_never_trims() {
        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        let chunk = vec![0.0f32; 1024 * 2];
        let mut pts_us = 0i64;
        let mut pushed = 0usize;
        loop {
            let wrote = producer.push(pts_us, &chunk);
            pushed += wrote / 2;
            pts_us += (wrote / 2) as i64 * 1_000_000 / 48_000;
            if wrote < chunk.len() {
                break;
            }
        }
        let mut out = vec![0.0f32; 512 * 2];
        let mut served = 0u64;
        let mut wall = 0i64;
        for _ in 0..1000 {
            wall += 10_000;
            shared.clock_now_us.store(wall, Ordering::Relaxed);
            shared.clock_wall_us.store(wall, Ordering::Relaxed);
            served += consumer.pull(&mut out, wall) as u64;
        }
        assert_eq!(shared.trimmed_frames.load(Ordering::Relaxed), 0);
        assert_eq!(served, pushed as u64, "every frame plays");
    }

    #[test]
    fn ring_round_trips_and_tracks_playhead() {
        let shared = new_audio_shared();
        let (mut producer, mut consumer) = audio_pair(
            AudioFormatInfo {
                sample_rate: 48000,
                channels: 2,
            },
            Arc::clone(&shared),
        );
        assert!(
            shared.playhead(MediaTime::ZERO).is_none(),
            "no consumption yet"
        );

        let chunk: Vec<f32> = (0..2048).map(|i| i as f32).collect();
        assert_eq!(producer.push(0, &chunk), 2048);

        let mut out = vec![0.0f32; 512];
        assert_eq!(consumer.pull(&mut out, 1_000), 256);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[511], 511.0);
        // 256 frames at 48 kHz = 5333 µs.
        assert_eq!(
            shared.playhead(MediaTime::from_micros(1_000)),
            Some(MediaTime::from_micros(5333))
        );

        // Drain past what is banked: remainder must be silence.
        let mut out = vec![1.0f32; 4096];
        let frames = consumer.pull(&mut out, 2_000);
        assert_eq!(frames, (2048 - 512) / 2);
        assert_eq!(out[2048 - 512], 0.0);
    }
}
