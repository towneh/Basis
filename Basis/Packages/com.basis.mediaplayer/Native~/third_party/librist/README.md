# librist

Where the librist static library that `media-rist` links is staged, for builds
with the `rist` feature. Without it, the engine builds with no RIST support.

- Pinned to librist v0.2.11 (https://code.videolan.org/rist/librist),
  BSD-2-Clause, with its bundled mbedTLS (Apache-2.0) linked into the archive.
  No local patches.
- `include/librist/` holds the pinned version's public headers, committed so
  `media-rist`'s FFI declarations and the layout check in `media-rist/csrc/`
  compile against the exact API.
- The libraries are built from source and not committed:

  | Platform | File | Script |
  | --- | --- | --- |
  | Windows x64 | `win-x64/rist.lib` | `tools/build-librist.ps1` |
  | Linux x64 | `linux-x64/librist.a` | `tools/build-librist.sh` |
  | Android arm64 | `android-arm64/librist.a` | `tools/build-librist-android.sh` |

To move the pin, pass the new tag to the scripts (`-LibristRef`, or
`LIBRIST_REF` for the shell scripts), which also re-stage the headers, then run
`cargo test -p media-rist --features librist`, which fails if the struct
layout changed.
