//! The `sidx` segment index (ISO/IEC 14496-12 8.16.3): where each of a
//! fragmented file's subsegments begins, how long it is, and what span of
//! media it holds. A file that carries one says in its first hundred
//! kilobytes everything an opener needs to start anywhere in it, however
//! many hours long it is.
//!
//! An index is believed only when it accounts for the whole file: every
//! reference is to media rather than to another index, and the end of the
//! box plus `first_offset` plus the sizes lands exactly on the end of the
//! media. A short or shifted index would otherwise put a seek in the
//! middle of a box, and hostile input is the normal case for a player
//! that opens arbitrary URLs.

/// One subsegment: a `moof` and the `mdat` it indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    /// Absolute offset of the subsegment's first byte.
    pub offset: u64,
    pub size: u32,
    /// Start time, in the index's timescale.
    pub time: u64,
    pub duration: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SegmentIndex {
    /// The track whose presentation times the index states. A muxed file
    /// written with per-track indexes carries one of these per track.
    pub reference_id: u32,
    pub timescale: u32,
    pub entries: Vec<IndexEntry>,
    /// One past the last byte any reference covers.
    pub end: u64,
}

impl SegmentIndex {
    /// Whether the index covers the file's media to the byte. `media_end`
    /// is the end of the source, or the start of a trailing `mfra`.
    pub fn tiles(&self, media_end: u64) -> bool {
        self.end == media_end
    }

    /// Whether the first reference is the first fragment the box walk
    /// found, which pins the index to this file's layout rather than to a
    /// copy it was written for.
    pub fn starts_at(&self, first_fragment: u64) -> bool {
        self.entries
            .first()
            .is_some_and(|entry| entry.offset == first_fragment)
    }

    /// The last subsegment beginning at or before `time`, in the index's
    /// timescale; the first when `time` precedes the index.
    pub fn floor(&self, time: u64) -> usize {
        self.entries
            .partition_point(|entry| entry.time <= time)
            .saturating_sub(1)
    }

    /// Media the index spans, in its own timescale.
    pub fn span(&self) -> u64 {
        self.entries
            .last()
            .map_or(0, |last| last.time.saturating_add(u64::from(last.duration)))
            .saturating_sub(self.entries.first().map_or(0, |first| first.time))
    }
}

/// Parse a `sidx` body — the box contents past its eight-byte header —
/// whose last byte is at `after_box - 1`.
pub(crate) fn parse(body: &[u8], after_box: u64) -> Result<SegmentIndex, &'static str> {
    let Some(&version) = body.first() else {
        return Err("sidx has no body");
    };
    // The version sizes `earliest_presentation_time` and `first_offset`;
    // the reference table follows them.
    let head = match version {
        0 => 20usize,
        1 => 28usize,
        _ => return Err("sidx version is not 0 or 1"),
    };
    if body.len() < head + 4 {
        return Err("sidx shorter than its reference count");
    }
    let reference_id = be32(&body[4..8]);
    let timescale = be32(&body[8..12]);
    if timescale == 0 {
        return Err("sidx timescale is zero");
    }
    let (earliest, first_offset) = if head == 20 {
        (
            u64::from(be32(&body[12..16])),
            u64::from(be32(&body[16..20])),
        )
    } else {
        (be64(&body[12..20]), be64(&body[20..28]))
    };

    let count = usize::from(u16::from_be_bytes([body[head + 2], body[head + 3]]));
    if count == 0 {
        return Err("sidx references nothing");
    }
    let table = &body[head + 4..];
    if table.len() < count * 12 {
        return Err("sidx reference count overruns the box");
    }

    let mut offset = after_box
        .checked_add(first_offset)
        .ok_or("sidx first offset runs past the file")?;
    let mut time = earliest;
    let mut entries = Vec::with_capacity(count);
    for reference in table.as_chunks::<12>().0.iter().take(count) {
        let word = be32(&reference[0..4]);
        if word >> 31 == 1 {
            return Err("sidx references another index");
        }
        let size = word & 0x7FFF_FFFF;
        if size == 0 {
            return Err("sidx reference is empty");
        }
        let duration = be32(&reference[4..8]);
        entries.push(IndexEntry {
            offset,
            size,
            time,
            duration,
        });
        offset = offset
            .checked_add(u64::from(size))
            .ok_or("sidx sizes run past the file")?;
        time = time
            .checked_add(u64::from(duration))
            .ok_or("sidx durations run past the end of time")?;
    }

    Ok(SegmentIndex {
        reference_id,
        timescale,
        entries,
        end: offset,
    })
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four bytes"))
}

fn be64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("eight bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `sidx` body with `references` as `(size, duration)` pairs, every
    /// reference to media and starting with a SAP.
    fn body(version: u8, first_offset: u64, references: &[(u32, u32)]) -> Vec<u8> {
        let mut out = vec![version, 0, 0, 0];
        out.extend_from_slice(&1u32.to_be_bytes()); // reference_ID
        out.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        if version == 0 {
            out.extend_from_slice(&0u32.to_be_bytes()); // earliest pts
            out.extend_from_slice(&(first_offset as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&0u64.to_be_bytes());
            out.extend_from_slice(&first_offset.to_be_bytes());
        }
        out.extend_from_slice(&0u16.to_be_bytes()); // reserved
        out.extend_from_slice(&(references.len() as u16).to_be_bytes());
        for (size, duration) in references {
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(&duration.to_be_bytes());
            out.extend_from_slice(&0x9000_0000u32.to_be_bytes()); // SAP type 1
        }
        out
    }

    #[test]
    fn both_versions_describe_the_same_segments() {
        for version in [0u8, 1u8] {
            let index = parse(&body(version, 0, &[(100, 10), (200, 20)]), 1000)
                .expect("a well-formed index parses");
            assert_eq!(index.reference_id, 1);
            assert_eq!(index.timescale, 1000);
            assert_eq!(
                index.entries,
                vec![
                    IndexEntry {
                        offset: 1000,
                        size: 100,
                        time: 0,
                        duration: 10
                    },
                    IndexEntry {
                        offset: 1100,
                        size: 200,
                        time: 10,
                        duration: 20
                    },
                ],
                "version {version}"
            );
            assert_eq!(index.end, 1300);
            assert_eq!(index.span(), 30);
        }
    }

    /// ffmpeg's `+global_sidx` gives the first of two indexes an offset
    /// that steps over the second, so a non-zero one is ordinary.
    #[test]
    fn a_first_offset_steps_over_what_follows_the_box() {
        let index = parse(&body(0, 40, &[(100, 10)]), 1000).expect("parses");
        assert_eq!(index.entries[0].offset, 1040);
        assert!(index.tiles(1140));
        assert!(index.starts_at(1040));
        assert!(!index.starts_at(1000));
    }

    #[test]
    fn an_index_short_of_the_media_is_not_trusted() {
        let index = parse(&body(0, 0, &[(100, 10), (200, 20)]), 1000).expect("parses");
        assert!(index.tiles(1300));
        assert!(!index.tiles(1301), "a fragment past the index");
        assert!(!index.tiles(1299));
    }

    #[test]
    fn the_floor_is_the_subsegment_holding_the_time() {
        let index = parse(&body(0, 0, &[(10, 100), (10, 100), (10, 100)]), 0).expect("parses");
        assert_eq!(index.floor(0), 0);
        assert_eq!(index.floor(99), 0);
        assert_eq!(index.floor(100), 1);
        assert_eq!(index.floor(250), 2);
        assert_eq!(index.floor(u64::MAX), 2);
    }

    #[test]
    fn hostile_indexes_are_refused() {
        assert!(parse(&[], 0).is_err(), "no body at all");

        let short = &body(0, 0, &[(100, 10)])[..8];
        assert!(parse(short, 0).is_err(), "a truncated box");

        let mut version = body(0, 0, &[(100, 10)]);
        version[0] = 2;
        assert!(parse(&version, 0).is_err(), "an unknown version");

        let mut timescale = body(0, 0, &[(100, 10)]);
        timescale[8..12].copy_from_slice(&0u32.to_be_bytes());
        assert!(parse(&timescale, 0).is_err(), "a zero timescale");

        let mut count = body(0, 0, &[(100, 10)]);
        count[22..24].copy_from_slice(&64u16.to_be_bytes());
        assert!(parse(&count, 0).is_err(), "a count past the box");

        let mut none = body(0, 0, &[(100, 10)]);
        none[22..24].copy_from_slice(&0u16.to_be_bytes());
        assert!(parse(&none, 0).is_err(), "no references at all");

        let mut hierarchical = body(0, 0, &[(100, 10)]);
        hierarchical[24..28].copy_from_slice(&0x8000_0064u32.to_be_bytes());
        assert!(parse(&hierarchical, 0).is_err(), "a reference to an index");

        let empty = body(0, 0, &[(0, 10)]);
        assert!(parse(&empty, 0).is_err(), "a zero-length subsegment");

        let huge = body(1, u64::MAX, &[(100, 10)]);
        assert!(parse(&huge, 1000).is_err(), "a first offset that wraps");

        let wrapping = body(0, 0, &[(0x7FFF_FFFF, 10); 3]);
        assert!(
            parse(&wrapping, u64::MAX - 1).is_err(),
            "sizes that wrap the file"
        );
    }
}
