# basis-media

The native engine of the `com.basis.mediaplayer` package: a media playback engine
for [Basis](https://github.com/BasisVR/Basis), written in Rust. It decodes through
the platform's own decoders (Media Foundation with D3D11VA on Windows, MediaCodec
on Android), hands frames to Unity on the GPU without a CPU copy, and does all its
network-facing parsing (MP4, MPEG-TS, MKV/WebM, HLS, RTSP/RTP, WHEP, RIST, raw
audio) in `#![forbid(unsafe_code)]` crates. The managed components live one level
up in the package's `Runtime/`, and the built binaries ship committed in
`Runtime/Plugins/`.

## Workspace

| Crate | Role |
| --- | --- |
| `media-ffi` | the plugin boundary Unity loads: session handles, the snapshot poll, the render event |
| `media-engine` | sessions: opens a source, assembles the pipeline, runs its threads |
| `media-clock` | the media clock: one time type, the choice of master, drift correction |
| `media-bank` | the buffer between demux and decode: depth, pacing, Auto sizing |
| `media-demux` | the stream event model and the container demuxers |
| `media-bitstream` | elementary-stream parsing shared by the demuxers (Annex-B, captions, SEI) |
| `media-hls` | HLS playlists and segment chaining |
| `media-io` | every socket and file the engine opens, with the address rules applied |
| `media-rtp`, `media-rtsp` | RTP reordering and receiver reports, and RTSP sessions on top |
| `media-whep` | WHEP (WebRTC receive) sessions |
| `media-rist` | RIST receive, through librist |
| `media-decode` | the decoder trait, with `decode-mf` (Windows), `decode-mediacodec` (Android) and `decode-sw` (FLAC, Opus, AV1, PCM in software) |
| `media-present` | handing frames to Unity: a Direct3D 11 shared texture on Windows, Vulkan on Android |
| `media-diag` | stage counters, the event log and the capture writer |
| `media-testkit` | test support: recorded network-delay profiles and the impairment source that replays them |
| `bm-probe` | a headless command-line player for running the engine without Unity |

Try it:

```sh
# headless playback of a fixture, with the frames handed to a second
# Direct3D 11 device the way Unity receives them (Windows)
cargo run -p media-engine --example smoke -- fixtures/h264-640x360-30fps.mp4

# probe a file: container and codec report, plus first-frame decode timing
cargo run -p bm-probe -- probe fixtures/h264-640x360-30fps.mp4 --decode
```

## Testing

[`TESTING.md`](TESTING.md) covers what you need installed, running the gate
(`tools/ci.ps1` or `tools/ci.sh`) and single tests, how the tests are laid out,
how to add one, and what each row checks. [`DIAGNOSTICS.md`](DIAGNOSTICS.md)
says how to read the captures a run writes. Fuzz targets live in `fuzz/`
(nightly, Linux; see `fuzz/README.md`).

The buffering is held to measured data: recorded delivery-gap distributions from
impaired live-stream captures are committed as fixtures
(`media-testkit/fixtures/phase0/`), and the sizing table built on them runs as
tests in `media-bank/tests/sizing_table.rs`.

## Licence

MIT OR Apache-2.0, at your option.
