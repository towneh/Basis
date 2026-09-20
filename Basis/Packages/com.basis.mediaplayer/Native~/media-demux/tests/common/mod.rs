//! Shared pieces for the fragmented-MP4 rows.
//!
//! A real long video puts megabytes between one fragment header and the
//! next, which is what makes the open-time walk expensive; the committed
//! fixtures put kilobytes between them so a single cache block covers
//! dozens. [`inflate`] restores the spacing by padding every `mdat` with
//! bytes no sample points at, and [`SparseSource`] serves those bytes as
//! zeros so the file costs its real size in memory rather than its
//! declared one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use media_demux::{ByteSource, SourceError};

/// Bytes added inside every `mdat`. Past the demuxer's 256 KiB cache
/// block, so no two fragment headers share one.
pub const FRAGMENT_PAD: u64 = 320 * 1024;

/// What a source was asked for, readable after the demuxer has taken it.
#[derive(Clone, Default)]
pub struct Counters {
    pub reads: Arc<AtomicU64>,
    pub bytes: Arc<AtomicU64>,
}

impl Counters {
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// A source of real byte runs separated by virtual zeros.
pub struct SparseSource {
    /// `(start, bytes)` in ascending, non-overlapping order.
    runs: Vec<(u64, Vec<u8>)>,
    len: u64,
    counters: Counters,
}

impl SparseSource {
    pub fn new(runs: Vec<(u64, Vec<u8>)>, len: u64) -> Self {
        Self {
            runs,
            len,
            counters: Counters::default(),
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn counters(&self) -> Counters {
        self.counters.clone()
    }
}

impl ByteSource for SparseSource {
    fn size(&mut self) -> Result<Option<u64>, SourceError> {
        Ok(Some(self.len))
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        // The run at or before `offset`, and the one after it: inside the
        // first the bytes are real, between them they are zeros.
        let next = self.runs.partition_point(|(start, _)| *start <= offset);
        let available = match next.checked_sub(1).map(|i| &self.runs[i]) {
            Some((start, bytes)) if offset < start + bytes.len() as u64 => {
                let from = (offset - start) as usize;
                let n = buf.len().min(bytes.len() - from);
                buf[..n].copy_from_slice(&bytes[from..from + n]);
                n
            }
            _ => {
                let until = self.runs.get(next).map_or(self.len, |(start, _)| *start);
                let n = buf.len().min((until - offset) as usize);
                buf[..n].fill(0);
                n
            }
        };
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes
            .fetch_add(available as u64, Ordering::Relaxed);
        Ok(available)
    }
}

/// Spread a fragmented fixture's fragments [`FRAGMENT_PAD`] further apart
/// without changing a sample. The padding goes inside each `mdat` past
/// the samples, which `default-base-is-moof` offsets do not reach; each
/// `sidx` reference grows by the same amount so an index still tiles the
/// file; and the `mfra`, whose offsets are absolute, is dropped.
pub fn inflate(data: &[u8]) -> SparseSource {
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut run: Vec<u8> = Vec::new();
    let mut run_start = 0u64;
    let mut out = 0u64;
    let mut pos = 0usize;

    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("four bytes")) as usize;
        assert!(
            size >= 8 && pos + size <= data.len(),
            "box at {pos} declares {size} bytes"
        );
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().expect("four bytes");
        let mut boxed = data[pos..pos + size].to_vec();
        pos += size;

        if &kind == b"mfra" {
            continue;
        }
        if &kind == b"sidx" {
            pad_index(&mut boxed);
        }
        if &kind == b"mdat" {
            let grown =
                u32::try_from(size as u64 + FRAGMENT_PAD).expect("padded mdat stays 32-bit");
            boxed[..4].copy_from_slice(&grown.to_be_bytes());
        }
        run.extend_from_slice(&boxed);
        out += size as u64;
        if &kind == b"mdat" {
            runs.push((run_start, std::mem::take(&mut run)));
            out += FRAGMENT_PAD;
            run_start = out;
        }
    }
    assert_eq!(pos, data.len(), "boxes do not tile the fixture");
    if !run.is_empty() {
        runs.push((run_start, run));
    }
    SparseSource::new(runs, out)
}

/// Grow every reference in a `sidx` by the padding its subsegment gained.
fn pad_index(boxed: &mut [u8]) {
    let version = boxed[8];
    let mut p = if version == 0 { 28 } else { 36 };
    let count = usize::from(u16::from_be_bytes([boxed[p + 2], boxed[p + 3]]));
    p += 4;
    for _ in 0..count {
        let word = u32::from_be_bytes(boxed[p..p + 4].try_into().expect("four bytes"));
        assert_eq!(word >> 31, 0, "hierarchical reference in a fixture index");
        let grown = (word & 0x7FFF_FFFF) + FRAGMENT_PAD as u32;
        boxed[p..p + 4].copy_from_slice(&grown.to_be_bytes());
        p += 12;
    }
}
