# Vendored matroska-demuxer 0.8.1

Vendored copy of the `matroska-demuxer` crate (Zlib OR MIT OR Apache-2.0,
see the LICENSE files), applied via `[patch.crates-io]` in the workspace
root. Lockfile dropped; source otherwise identical to the crates.io
release except three seek changes in `src/lib.rs`:

- `seek_broad_phase`: `CueRelativePosition` is resolved against the data
  offset of the cluster the cue point references, not the segment's
  first cluster. The released code passes `cluster_start` (the first
  cluster) into `get_cluster_offset_and_timestamp`, so every cue seek
  with relative positions lands at `first_cluster_data + relative` —
  wrong for every cue but the first (observed: seeks past the first
  cluster return EOF or misposition).
- `seek_broad_phase`, linear fallback: cluster offsets handed to the
  narrow phase are the cluster *element* positions (captured before
  `next_element`), matching what the cue path provides — the released
  code returned data offsets, which the narrow phase cannot enter, and
  fell back to file offset 0 (the EBML header) when the first cluster
  already overshot the target.
- New `seek_to_cue_point` method: like `seek`, but when a cue point was
  used the narrow phase targets the cue's own time, so the reader lands
  on the keyframe at or before the target instead of mid-GOP. `seek`
  itself keeps the released semantics (first block at or after the
  requested time; past-the-end runs to EOF) — the crate's own test
  suite pins those, and it still passes here (the 8 `parse_testN_mkv`
  failures are files the published package does not ship; same result
  on the pristine 0.8.1).

`MkvDemuxer` uses `seek_to_cue_point`. All three changes are candidates
for an upstream report/PR; drop the vendored copy when a release
carries fixes.
