# The engine gate on Windows. Run before committing; everything here must pass.
# Steps whose tool is missing print SKIPPED and do not fail the run.
#
#   .\tools\ci.ps1          # fmt, clippy, tests, the RIST and Android builds,
#                           # deny, vet, and the headless playback checks
#   .\tools\ci.ps1 -Fuzz    # additionally build the fuzz targets (needs
#                           # nightly + cargo-fuzz; Linux/WSL only)

param([switch]$Fuzz)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Step($name, $block) {
    Write-Host "== $name" -ForegroundColor Cyan
    & $block
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $name" -ForegroundColor Red
        exit 1
    }
}

Step "cargo fmt --check"   { cargo fmt --check }
Step "cargo clippy"        { cargo clippy --workspace --all-targets --examples -- -D warnings }
Step "cargo test"          { cargo test --workspace }
# RIST feature graph: only when the librist static is staged (built from
# source by tools/build-librist.ps1) — the default build stays librist-free.
if (Test-Path "third_party/librist/win-x64/rist.lib") {
    Step "clippy (rist feature)" { cargo clippy -p media-rist -p media-engine --features media-engine/rist --all-targets -- -D warnings }
    Step "test (rist feature)"   { cargo test -p media-rist --features librist }
} else {
    Write-Host "SKIPPED: rist feature — librist not staged (tools/build-librist.ps1)" -ForegroundColor Yellow
}
# Android: the aarch64 graph must keep compiling on every commit.
# Needs the rust target plus an NDK (android-env.ps1 finds Unity's);
# skipped loudly when either is absent. Runs in a child shell so the
# toolchain env does not leak into later steps.
$androidTarget = (rustup target list --installed) -contains "aarch64-linux-android"
if ($androidTarget) {
    Step "android check (aarch64)" {
        pwsh -NoProfile -Command {
            Set-Location $args[0]
            . .\tools\android-env.ps1 | Out-Null
            if ($env:BM_ANDROID_ENV_OK -ne "1") {
                Write-Host "SKIPPED: android check — no NDK found" -ForegroundColor Yellow
                exit 0
            }
            # RIST rides the shipping Android build, so lint it there too
            # when the static is staged — skipping it loudly is how the
            # transport stayed absent from Quest while shipping elsewhere.
            $rist = Join-Path (Get-Location) "third_party\librist\android-arm64\librist.a"
            if (Test-Path $rist) {
                cargo clippy --target aarch64-linux-android -p media-ffi -p decode-mediacodec --features rist -- -D warnings
            } else {
                Write-Host "NOTE: android clippy without --features rist (no staged librist; run tools/build-librist-android.sh)" -ForegroundColor Yellow
                cargo clippy --target aarch64-linux-android -p media-ffi -p decode-mediacodec -- -D warnings
            }
            exit $LASTEXITCODE
        } -args (Get-Location).Path
    }
} else {
    Write-Host "SKIPPED: android check — aarch64-linux-android target not installed" -ForegroundColor Yellow
}
Step "cargo deny check"    { cargo deny check }
Step "cargo vet"           { cargo vet }
if (Get-Command ffprobe -ErrorAction SilentlyContinue) {
    Step "conformance (ffprobe oracle)" { cargo run -q -p bm-probe -- conformance fixtures }
} else {
    Write-Host "SKIPPED: conformance — ffprobe not on PATH" -ForegroundColor Yellow
}
# Impairment: the worst recorded network-delay profile replayed through the
# whole engine over a fixture paced at 1x, graded against the buffer sizing
# model. Kept short here; TESTING.md has the full-length runs.
Step "impairment (phase-0 replay)" {
    cargo run -q -p bm-probe -- impair fixtures/h264-aac-320x180-30s.ts `
        --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000
}
# A split pair through the whole engine: video off one source, audio off
# another, both metered by the one Bank. The unit rows pin the pieces; this
# is the lane end to end.
Step "split source (two legs, one session)" {
    cargo run -q -p bm-probe -- play fixtures/split/h264-640x360-30fps-video.mp4 `
        --audio-url fixtures/split/aac-48k-stereo-audio.m4a --duration 9
}
if ($Fuzz) {
    Step "cargo fuzz build" { cargo +nightly fuzz build }
}
Write-Host "CI green" -ForegroundColor Green
