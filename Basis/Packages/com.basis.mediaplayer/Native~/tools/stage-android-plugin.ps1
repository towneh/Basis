# Build the Android engine and stage the stripped .so into the package
# Runtime/Plugins, where every project referencing com.basis.mediaplayer
# picks it up (the Android twin of the x86_64 dll copy in TESTING.md).
# Run from the workspace root (Native~) after any engine change:
#
#   .\tools\stage-android-plugin.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot

. (Join-Path $PSScriptRoot "android-env.ps1") | Out-Null
if ($env:BM_ANDROID_ENV_OK -ne "1") {
    Write-Host "stage-android-plugin: no NDK (see android-env.ps1)" -ForegroundColor Red
    exit 1
}

Push-Location $root
try {
    cargo build --target aarch64-linux-android -p media-ffi --release
    if ($LASTEXITCODE -ne 0) { exit 1 }
} finally {
    Pop-Location
}

$strip = Join-Path $env:ANDROID_NDK "toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-strip.exe"
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$built = Join-Path $targetDir "aarch64-linux-android\release\libbasis_media.so"
$dest = Join-Path $root "..\Runtime\Plugins\Android\arm64-v8a\libbasis_media.so"
# --strip-all: .dynsym (the dlopen surface) survives; .symtab and debug
# sections go. The unstripped artefact stays in target/ for symbolication.
& $strip --strip-all -o $dest $built
if ($LASTEXITCODE -ne 0) { exit 1 }
Write-Host "staged $dest ($([math]::Round((Get-Item $dest).Length / 1MB, 1)) MB)" -ForegroundColor Green
