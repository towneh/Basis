#!/bin/sh
# The engine gate on Linux. Run before committing; everything here must pass.
# Steps whose tool is missing print SKIPPED and do not fail the run.
#
#   ./tools/ci.sh          # fmt, clippy, tests, the RIST build, deny, vet,
#                          # and the headless playback checks
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
# Software decode: rav1d and Opus through the whole headless pipeline, which
# any host can run, GPU or not.
echo "== software decode (AV1+Opus headless)"
cargo run -q -p bm-probe -- play fixtures/mkv/av1-opus.webm --duration 8
# Impairment: the worst recorded network-delay profile replayed through the
# whole engine over a fixture paced at 1x, graded against the buffer sizing
# model. Kept short here; TESTING.md has the full-length runs. The fixture is
# H.264 and AAC, so a host without those decoders skips it.
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
