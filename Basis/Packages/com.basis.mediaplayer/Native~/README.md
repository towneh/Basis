# basis-media

The Rust engine behind `com.basis.mediaplayer`. It decodes with the platform's
decoders (Media Foundation with D3D11VA on Windows, MediaCodec on Android) and
hands frames to Unity on the GPU. Container and protocol parsing (MP4,
MPEG-TS, MKV/WebM, HLS, RTSP/RTP, WHEP, RIST, raw audio) is
`#![forbid(unsafe_code)]`. The package's managed code is in `../Runtime/`, and
the built binaries are committed in `../Runtime/Plugins/`.

| Crate | Role |
| --- | --- |
| `media-ffi` | The plugin boundary Unity loads |
| `media-engine` | Sessions and their pipelines |
| `media-clock` | The media clock and drift correction |
| `media-bank` | The buffer between demux and decode |
| `media-demux` | Container demuxers |
| `media-bitstream` | Elementary-stream parsing (Annex-B, captions, SEI) |
| `media-hls` | HLS playlists and segments |
| `media-io` | Sockets and files, with the address rules |
| `media-rtp`, `media-rtsp` | RTP and RTSP |
| `media-whep` | WHEP (WebRTC receive) |
| `media-rist` | RIST, through librist |
| `media-decode` | The decoder trait, with `decode-mf` (Windows), `decode-mediacodec` (Android) and `decode-sw` (FLAC, Opus, AV1, PCM) |
| `media-present` | Frame hand-off to Unity: Direct3D 11 on Windows, Vulkan on Android |
| `media-diag` | Counters, the event log and the capture writer |
| `media-testkit` | Recorded network-delay profiles for tests |
| `bm-probe` | A command-line player for running the engine without Unity |

```sh
cargo build --release -p media-ffi --features rist       # the plugin
cargo run -p bm-probe -- probe fixtures/h264-640x360-30fps.mp4 --decode
```

[`TESTING.md`](TESTING.md) covers prerequisites and testing, and
[`DIAGNOSTICS.md`](DIAGNOSTICS.md) the captures. Fuzz targets are in `fuzz/`.

Licensed MIT OR Apache-2.0, at your option.
