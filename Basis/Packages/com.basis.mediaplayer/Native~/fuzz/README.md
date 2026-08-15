# Fuzzing

Fuzz targets for the parser crates. Policy (spec §12.1): a demuxer/parser
crate does not merge without a `cargo-fuzz` target, a seed corpus, and CI
replay of pinned crashes.

Needs a nightly toolchain and [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz).
libFuzzer targets do not build on `*-pc-windows-msvc`; run under WSL or a
Linux host (the VPS lane), where the same corpus and targets work unchanged.

```sh
cargo +nightly fuzz run mp4_stream        # fuzz with the committed corpus
cargo +nightly fuzz run mp4_stream corpus/mp4_stream crashes/  # replay pinned crashes
```

- `corpus/<target>/` — seed corpus, committed. The C player's corpus
  fixtures carry over as seeds as their formats gain demuxers here.
- `crashes/` — pinned crash reproducers, committed once found and fixed;
  CI replays them.

## Known contained panics (mp4_stream)

The MP4 open boundary deliberately fences re_mp4's panic paths with
`catch_unwind`, so on the shipped (panic=unwind) build a hostile file is
a typed refusal. Fuzz builds run panic=abort, where those same paths
read as crashes: any `mp4_stream` crash whose backtrace lands inside
`re_mp4` is that class, not an escape. Pin such inputs under
`media-demux/tests/data/re-mp4-panics/` (the cargo test asserts the
fence converts them to typed errors) and keep them OUT of the seed
corpus, or every later campaign dies on them at startup. Worth carrying
upstream to re_mp4 when a batch accumulates.

## Known contained panics (rtp_session)

The webrtc-rs parsers have panic paths on hostile length fields (first
found: rtp 0.17.2 advances by the header-extension word count before
checking the remaining buffer). media-rtp screens RTP header geometry
up front and fences both `unmarshal` calls with `catch_unwind`, so the
shipped build turns the whole class into typed rejections — but under
the fuzz build's abort-on-panic hook any *fenced* panic still reads as
a crash. Any `rtp_session` crash whose backtrace lands inside the `rtp`
or `rtcp` crates is that class, not an escape. Pin such inputs under
`media-rtp/tests/data/rtp-panics/` (the cargo test replays them and
asserts typed rejection) and keep them OUT of the seed corpus.
Upstream-report candidates to webrtc-rs when a batch accumulates.

## whep_signal (feature-gated)

The WHEP signalling surfaces a hostile server controls: the crate's own
Link-header walker and scheme mapper, plus str0m's SDP offer/answer
parsers (str0m's stated posture is that user input must never panic —
any panic found here is an upstream report). The target is behind the
`whep` cargo feature because media-whep's Linux build pulls str0m's
rust-crypto backend, which compiles `aws-lc-sys` (needs cmake); the gate
keeps the other campaigns independent of that toolchain:

```sh
cargo +nightly fuzz run whep_signal --features whep
```

## Known contained panics (mkv_stream)

Same posture for the Matroska boundary: matroska-demuxer has panic
paths on hostile input (block-timestamp overflow in `parse_timestamp`,
for one), and MkvDemuxer fences both its open and its frame walk with
`catch_unwind`. Any `mkv_stream` crash whose backtrace lands inside
`matroska_demuxer` is that class. Pin such inputs under
`media-demux/tests/data/mkv-panics/` (the cargo test asserts
containment) and keep them OUT of the seed corpus. Timeout-class
inputs (the seek-head read loops) are different: those are fixed by
the SourceIo budgets, live in `tests/data/slow-mkv/`, and DO belong in
the seed corpus. Upstream reports tracked with the cue-seek issue.
