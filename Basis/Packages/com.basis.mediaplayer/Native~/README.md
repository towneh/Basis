# basis-media

The native engine of the `com.basis.mediaplayer` package: a media playback engine
for [Basis](https://github.com/BasisVR/Basis), written in Rust. Native decode
through the platform's decoders (Media Foundation with D3D11VA on Windows,
MediaCodec on Android), zero-copy GPU handoff into Unity, and network-facing
parsing (MP4, MPEG-TS, MKV/WebM, HLS, RTSP/RTP, WHEP, RIST, raw audio) in
`#![forbid(unsafe_code)]` crates. The managed component family lives one level
up in the package `Runtime/`; the built binaries ship committed in the package
`Runtime/Plugins/`.

## Workspace

| Crate | Role |
| --- | --- |
| `media-ffi` | cdylib plugin boundary (generational handles, snapshot poll, render event) |
| `media-engine` | sessions, pipeline assembly |
| `media-clock` | the media clock: one time type, master selection, drift ladder (dead band / bounded slew / snap) |
| `media-bank` | the AU bank + pacer: one configured depth, debt bound, decay, Auto sizing |
| `media-demux` | `StreamEvent` model + container demux (`forbid(unsafe_code)`) |
| `media-decode` / `decode-mf` | decode trait + Media Foundation adapter |
| `media-present` | shared-texture handoff into the host |
| `media-diag` | stage counters, structured event log, capture recorder |
| `media-testkit` | impairment/replay harness + phase-0 capture fixtures |
| `bm-probe` | headless CLI harness player (no Unity, no C ABI in the loop) |

Further crates (HLS, RTSP/RTP, WHEP, RIST, IO, bitstream, metadata, more decode
adapters) join the workspace at the milestone that gives them real content.

Try it:

```sh
# the M0 slice, headless (decode at 1x with the shared-texture handoff
# consumed on a second D3D11 device)
cargo run -p media-engine --example smoke -- fixtures/h264-640x360-30fps.mp4

# probe a file: container/codec report plus first-frame decode timing
cargo run -p bm-probe -- probe fixtures/h264-640x360-30fps.mp4 --decode
```

## Verification

`./tools/ci.ps1` (or `tools/ci.sh`) is the local gate: rustfmt, clippy
(`-D warnings`), the test suite, `cargo deny` (licence allowlist + advisories) and
`cargo vet`. Fuzz targets live in `fuzz/` (nightly, Linux/WSL; see `fuzz/README.md`).

The Bank's buffering behaviour is held to measured data: recorded delivery-gap
distributions from impaired live-stream captures are committed as fixtures
(`media-testkit/fixtures/phase0/`), and its sizing table runs as tests in
`media-bank/tests/sizing_table.rs`.

## Licence

MIT OR Apache-2.0, at your option.
