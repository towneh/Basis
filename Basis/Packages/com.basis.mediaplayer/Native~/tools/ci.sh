#!/bin/sh
# Local CI: the gate a hosted pipeline will eventually mirror. Run before
# committing; everything here must pass clean.
#
#   ./tools/ci.sh          # fmt, clippy, tests, deny, vet
#   ./tools/ci.sh --fuzz   # additionally build the fuzz targets (needs
#                          # nightly + cargo-fuzz)
set -eu
cd "$(dirname "$0")/.."

echo "== cargo fmt --check"
cargo fmt --check
echo "== cargo clippy"
cargo clippy --workspace --all-targets --examples -- -D warnings
echo "== cargo test"
cargo test --workspace
# RIST feature graph: only when the librist static is staged — the default
# build stays librist-free.
if [ -f third_party/librist/linux-x64/librist.a ]; then
    echo "== clippy (rist feature)"
    cargo clippy -p media-rist -p media-engine --features media-engine/rist --all-targets -- -D warnings
    echo "== test (rist feature)"
    cargo test -p media-rist --features librist
else
    echo "SKIPPED: rist feature — librist not staged"
fi
echo "== cargo deny check"
cargo deny check
echo "== cargo vet"
cargo vet
if command -v ffprobe >/dev/null 2>&1; then
    echo "== conformance (ffprobe oracle)"
    cargo run -q -p bm-probe -- conformance fixtures
else
    echo "SKIPPED: conformance — ffprobe not on PATH"
fi
# The software-decode lane (§12.4): the in-process floors (rav1d + Opus)
# through the full headless pipeline — the row every platform can run,
# including GPU-less CI hosts.
echo "== software decode (AV1+Opus headless)"
cargo run -q -p bm-probe -- play fixtures/mkv/av1-opus.webm --duration 8
# The impairment lane (§12.2): the worst phase-0 jitter profile replayed
# through the full engine over a 1x-paced fixture, graded against the
# sizing model — bounded to keep the per-commit gate quick; TESTING.md
# carries the full-length rows. The fixture is H.264+AAC, so the row
# needs the platform decoders — headless hosts skip it (the Windows
# ci.ps1 gate carries it per-commit).
if cargo run -q -p bm-probe -- caps --compact | grep -q '"h264"'; then
    echo "== impairment (phase-0 replay)"
    cargo run -q -p bm-probe -- impair fixtures/h264-aac-320x180-30s.ts \
        --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000
else
    echo "SKIPPED: impairment — no H.264 decode on this platform"
fi
if [ "${1:-}" = "--fuzz" ]; then
    echo "== cargo fuzz build"
    cargo +nightly fuzz build
fi
echo "CI green"
