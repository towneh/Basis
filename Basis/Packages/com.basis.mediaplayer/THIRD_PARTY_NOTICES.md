# Third-Party Notices

The native engine (`basis_media`) is a Rust binary that links its dependencies
statically, so the shipped `.dll` / `.so` embeds them. Everything in the graph is
permissively licensed and every licence in it requires attribution, which is
what this file provides.

Nothing here is copyleft. `Native~/deny.toml` holds the allowed licence set and
`Native~/tools/ci.ps1` fails the build on anything outside it, so a dependency
carrying an unexpected licence cannot land quietly.

## What ships

| Binary | Contains |
| --- | --- |
| `Runtime/Plugins/x86_64/basis_media.dll` | the Rust graph below, plus Media Foundation and Direct3D from the OS |
| `Runtime/Plugins/Android/arm64-v8a/libbasis_media.so` | the Rust graph below, plus MediaCodec and Vulkan from the OS |
| `Runtime/Plugins/Linux/x86_64/libbasis_media.so` | the Rust graph below; software decode only, no OS codec framework |

Operating-system frameworks carry no attribution obligation. The Rust graph
does.

## Licences in the shipped graph

Resolved from the engine's own `Cargo.lock` for the shipping library, per
target: 302 crates on Windows x64, 281 on Android arm64, 289 on Linux x64. The
Windows graph is the largest and is broken down here; the other two are
subsets of the same licence set.

| Licence | Crates |
| --- | --- |
| MIT or Apache-2.0 (either, at your option) | 207 |
| MIT | 40 |
| Unicode-3.0 | 18 |
| Unlicense or MIT | 7 |
| BSD-3-Clause | 4 |
| ISC | 4 |
| BSD-2-Clause, or Apache-2.0, or MIT | 3 |
| Apache-2.0, or ISC, or MIT | 3 |
| Apache-2.0 | 3 |
| MIT, Apache-2.0 or Zlib | 2 |
| one each: BSD-2-Clause; CC0-1.0; CDLA-Permissive-2.0; Apache-2.0 and ISC; Apache-2.0 or BSL-1.0; and four further permissive combinations | 11 |

Where a crate offers a choice, the permissive option is the one taken, and
`Native~/deny.toml` lists what may be chosen.

To regenerate the exact list, from `Native~/`:

```sh
cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc
```

Each package object carries `name`, `version` and `license`. Filtering by
platform matters: without it the resolver reports crates for targets this
engine is never built for, and their licences are not obligations here.

## Libraries worth naming

The graph is mostly small Rust crates. These are the ones that do the heavy
lifting, or whose licence differs from the MIT/Apache norm:

| Library | Licence | What it does |
| --- | --- | --- |
| **rav1d** | BSD-2-Clause | AV1 video decoding in software. A Rust port of dav1d |
| **libopus** (via `audiopus_sys`, ISC) | BSD-3-Clause | Opus audio decoding. Built from vendored C source and linked statically |
| **claxon** | Apache-2.0 | FLAC audio decoding |
| **retina** | MIT or Apache-2.0 | RTSP client and RTP depacketisation. Vendored, see below |
| **matroska-demuxer** | Zlib, or MIT, or Apache-2.0 | Matroska/WebM parsing. Vendored, see below |
| **re_mp4** | MIT | MP4 and fragmented-MP4 parsing |
| **m3u8-rs** | MIT | HLS playlist parsing |
| **str0m** | MIT or Apache-2.0 | WebRTC (the WHEP receive path) |
| **webrtc-rs** `rtp` / `rtcp` / `webrtc-util` | MIT or Apache-2.0 | RTP and RTCP packet formats |
| **rustls** | Apache-2.0, or ISC, or MIT | TLS |
| **ring** | Apache-2.0 and ISC | Cryptographic primitives under rustls. Embeds BoringSSL-derived assembly |
| **aws-lc-rs** / **aws-lc-sys** | ISC and (Apache-2.0 or ISC), with further terms | Cryptography reachable from the WebRTC stack. Windows uses the OS provider instead |
| **webpki-roots** | CDLA-Permissive-2.0 | The CCADB trust-anchor bundle, used on Android, which has no readable CA store |
| **tokio** | MIT | Async runtime for the network transports |
| **ash** | MIT or Apache-2.0 | Vulkan bindings, Android only |
| **jni** | MIT or Apache-2.0 | JNI bindings, Android only |
| **windows** | MIT or Apache-2.0 | Windows API bindings, Windows only |
| **to_method** | CC0-1.0 | A small conversion helper, pulled in by rav1d |

Unicode-3.0 covers the ICU data crates that reach the graph through URL and
IDNA handling.

## Vendored sources

Three dependencies are vendored under `Native~/third_party/` rather than taken
from crates.io, each with its patches and the reason for them documented in a
`PATCHES.md` beside the source:

- **retina** (MIT or Apache-2.0) — patched for servers that advertise an
  all-zero SSRC, for AAC access units delivered without the RTP marker bit, for
  UDP socket binding against Windows' excluded port ranges, and for the seams
  the UDP transport needs.
- **matroska-demuxer** (Zlib, or MIT, or Apache-2.0) — patched so cue-based
  seeking resolves against the right cluster and lands on the keyframe at or
  before the requested time.
- **librist** (BSD-2-Clause), which vendors **mbedTLS** (Apache-2.0) — the RIST
  live-ingest transport. Built from source by
  `Native~/tools/build-librist.ps1` on Windows and `build-librist.sh` on Linux;
  the static library is not committed.

## RIST, per platform

RIST is behind a Cargo feature, and every shipped binary is now built with it
on — Windows, Linux and Android alike — so librist and mbedTLS are statically
linked into all three and the attribution above applies to each. Android's
librist is cross-compiled against the NDK by
`Native~/tools/build-librist-android.sh`; as on the other platforms the static
library itself is not committed.

## Test clips

`Native~/fixtures/captest/A.mp4` through `D.mp4` are test content for the
session-cap pass, not shipped runtime assets — `Native~` is invisible to
Unity, so nothing imports them. The video is generated; the music is:

> Kevin MacLeod (incompetech.com) — licensed under Creative Commons: By
> Attribution 4.0, <https://creativecommons.org/licenses/by/4.0/>
>
> - `A.mp4` — *Blue Ska*
> - `B.mp4` — *Achaidh Cheide*
> - `C.mp4` — *Vibe Ace*
> - `D.mp4` — *Bass Walker*

Every other fixture under `Native~/fixtures` is generated by ffmpeg from
synthetic sources and carries no third-party content.

## This package

Licensed under either of MIT or Apache-2.0, at your option. See `LICENSE.md`.
