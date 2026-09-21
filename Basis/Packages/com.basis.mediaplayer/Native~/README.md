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
| `media-present` | Frame hand-off to Unity: Direct3D 11 and 12 on Windows, Vulkan on Android |
| `media-diag` | Counters, the event log and the capture writer |
| `media-testkit` | Recorded network-delay profiles for tests |
| `bm-probe` | A command-line player for running the engine without Unity |

```sh
cargo build --release -p media-ffi --features rist       # the plugin
cargo run -p bm-probe -- probe fixtures/h264-640x360-30fps.mp4 --decode
```

The built plugins are committed under `../Runtime/Plugins/`: copy
`basis_media.dll` to `x86_64/` on Windows, run `tools/stage-android-plugin.ps1`
for `Android/arm64-v8a/`, and on a Linux host copy `libbasis_media.so`, stripped,
to `Linux/x86_64/`. Each needs librist staged first (see
[`third_party/librist/`](third_party/librist/README.md)).

| Tool | Does |
| --- | --- |
| `tools/ci.ps1`, `tools/ci.sh` | The gate: formatting, lints, tests, licence and supply-chain audits, and headless playback checks |
| `tools/android-env.ps1` | Finds an Android NDK (Unity's by default) and sets cargo up for `aarch64-linux-android`; dot-source it |
| `tools/stage-android-plugin.ps1` | Builds the Android plugin and copies it into the package |
| `tools/build-librist.ps1`, `build-librist.sh`, `build-librist-android.sh` | Build the librist library for Windows, Linux and Android |
| `tools/gen-*.py` | Generate the test fixtures in `fixtures/` |
| `tools/live-ts-server.py`, `tools/live-hls-server.py` | Serve a fixture as a live MPEG-TS or HLS stream, for tests by hand |

[`TESTING.md`](TESTING.md) covers prerequisites and testing, and
[`DIAGNOSTICS.md`](DIAGNOSTICS.md) the captures. Fuzz targets are in `fuzz/`.

Licensed MIT OR Apache-2.0, at your option.
