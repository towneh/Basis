# librist (pinned) — build staging

Staging area for the librist static that `media-rist` links when built with
the `rist` cargo feature (spec §5.5 / §9: librist pinned and watched, behind
its build flag).

- **Pin: librist v0.2.11** (https://code.videolan.org/rist/librist), BSD-2-Clause,
  with its bundled mbedTLS (Apache-2.0) linked into the archive. No local patches.
- `include/librist/` — the pinned tag's public headers, **committed** so the
  hand-written FFI declarations and the layout-check shim in
  `media-rist/csrc/` always compile against the exact pinned API.
- `win-x64/rist.lib`, `linux-x64/librist.a` — built from source by
  `tools/build-librist.ps1`, **not committed** (gitignored).

To change the pin: pass `-LibristRef` to the build script, re-stage the
headers, and re-run the `media-rist` layout test (`cargo test -p media-rist
--features librist`) — it fails the build if the struct layout moved.
