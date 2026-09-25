# Testing the media engine

How to run and add the engine's tests, and what each test checks. The managed
side has its own guide at [`../TESTING.md`](../TESTING.md), and
[`DIAGNOSTICS.md`](DIAGNOSTICS.md) explains the captures. Run the rows for any
area you change.

## Prerequisites

- **Rust through rustup.** `rust-toolchain.toml` pins the version and
  components; the first `cargo` command installs them.
- **cargo-deny and cargo-vet:** `cargo install --locked cargo-deny cargo-vet`.
- **Windows:** PowerShell 7, NASM on `PATH`, and a GPU with hardware video
  decode for the Direct3D 11, Direct3D 12 and Media Foundation tests. Set `CARGO_TARGET_DIR`
  to a short path such as `C:/bm-target` to stay under Windows' path-length
  limit.
- **Optional:** `ffprobe` for the conformance check; librist built into
  `third_party/librist/` by `tools/build-librist.ps1` or `.sh` (needs meson and
  ninja) for RIST; `rustup target add aarch64-linux-android` and Unity's Android
  NDK (`tools/android-env.ps1` finds it) for the Android build check. Without
  each, the gate skips that step.
- **For some by-hand rows:** Python 3 and ffmpeg (fixture scripts and local
  servers in `tools/`), and nightly Rust with `cargo-fuzz` on Linux (see
  `fuzz/README.md`).

## Running the tests

```
.\tools\ci.ps1        # Windows
tools/ci.sh           # Linux
```

This is the gate: run it before every commit. It runs these steps in order and
stops at the first failure. The first run builds everything and takes a while;
after that it takes a few minutes.

| Step | Checks | Needs |
| --- | --- | --- |
| `cargo fmt --check` | Formatting is `rustfmt`'s default | |
| `cargo clippy` | No lint warnings anywhere, tests and examples included | |
| `cargo test` | Every test in the workspace | Windows for the session, Media Foundation and GPU tests |
| RIST | The RIST transport lints clean and its tests pass | librist built, see [`third_party/librist/`](third_party/librist/README.md) |
| Android (Windows only) | The engine compiles and lints for Quest | the `aarch64-linux-android` Rust target and an Android NDK |
| `cargo deny` | Licences, security advisories, banned crates and crate sources, as set in `deny.toml` | network access |
| `cargo vet` | Every dependency is audited or exempted in `supply-chain/` | network access |
| Conformance | Each MP4 and TS fixture demuxes to exactly what ffprobe reads from it | `ffprobe` |
| Software decode | AV1 and Opus play through the whole engine without a GPU | |
| Impairment | A recorded bad-network profile replayed through the engine at 1x; playback has to keep going within the buffer model | H.264 and AAC decoders, so Linux skips it today |
| Split source | Video and audio from two files play as one session | H.264 and AAC decoders, so Linux skips it today |

**RIST, Android, Conformance, Impairment and Split source print `SKIPPED:` in
yellow when what they need is missing, and the run still ends green**, so check
for those lines before trusting a pass. Every other step fails the run if its
tool is missing.
`-Fuzz` (`--fuzz` on Linux) also builds the fuzz targets, which needs nightly
Rust on Linux or WSL (see [`fuzz/`](fuzz/README.md)).

On Windows, `cargo vet` rewrites the files in `supply-chain/` with Windows line
endings. `git diff` shows no change, and `git checkout -- supply-chain` puts
them back.

```
cargo test -p media-demux                          # one crate
cargo test -p media-engine --test session          # one test file
cargo test -p media-engine --test session caption  # matching tests
```

Session, Media Foundation and GPU tests need Windows. Tests marked
`#[ignore]` need a network source named by an environment variable (see the
matrix) and run with `-- --ignored`.

`bm-probe` runs the engine without Unity (`cargo run -p bm-probe -- <command>`,
with `--release` for timing):

| Command | Does |
| --- | --- |
| `probe <src>` | Reports container, codecs and first-frame timing |
| `play <src> --csv out.csv` | Plays through the engine, writing the engine capture. `--audio-out`, `--seek-to-ms` and `--decode` are optional |
| `bench <src>` | Measures time to first frame and seek time |
| `caps` | Prints what this machine can play |
| `conformance fixtures` | Compares each fixture's demuxed stream with ffprobe |
| `impair <src> --profile <name>` | Replays a recorded network profile and grades the buffering |

## Layout

| Where | What |
| --- | --- |
| `src/` of each crate | Unit tests |
| `media-demux/tests/` | Container demuxers against the fixtures |
| `media-engine/tests/` | Whole sessions (mostly Windows) |
| `media-io`, `media-hls`, `media-rtp`, `media-rtsp`, `media-whep` `tests/` | Network sources against local scripted servers |
| `media-bank/tests/`, `media-clock/tests/` | Buffering and the clock, including property tests |
| `media-present/tests/` | The Direct3D 11 conversion pass and the Direct3D 12 handoff |
| `media-ffi/tests/` | The plugin boundary |
| `media-testkit/` | Recorded network-delay profiles and the impairment source |
| `fixtures/`, `tools/gen-*.py` | Test media and the scripts that generate it |
| `fuzz/` | Fuzz targets |

## Adding a test

1. Use the lowest level that shows the behaviour: a unit test, then a crate
   integration test, then a session test. By-hand `bm-probe` rows are for what
   needs a real network, a device or a listener.
2. Break the code the test covers and check the test fails with a clear
   message, then restore it.
3. Assert the required value in the test, not the engine's own constant.
4. Generate fixtures from synthetic sources (`testsrc2`, sine tones) with a
   script in `tools/`; do not commit recorded media. New MP4 and TS fixtures
   must pass `bm-probe conformance fixtures`. Keep TS audio at 48 kHz with an
   ADTS track.
5. Add the row to the matrix in the same commit. Untested features go under
   [Not yet run](#not-yet-run). Column changes also update
   [`DIAGNOSTICS.md`](DIAGNOSTICS.md).

## Network sources

Some by-hand rows need a network source of a given kind: a live RTSP stream, a
WHEP endpoint, a RIST sender, or files over HTTPS with byte ranges. Any source
of that kind works. `tools/live-ts-server.py` and `tools/live-hls-server.py`
serve live MPEG-TS and HLS locally, mediamtx serves RTSP and WHEP, and any
static server with range requests serves files.

A server that takes one client at a time (ffmpeg's `-listen 1`) cannot test
live-or-on-demand detection, because the player's probe uses the only
connection. Set **Liveness** to Live for those.

## What is checked

**CI** runs in the gate on every platform, **CI, Windows** only on Windows,
**By hand** needs a person, and **Device (Quest)** a headset.

### Foundations, lifecycle and hostile input

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Unit/property tests | Bank sizing, clock correction, jitter filtering, frame-pool leasing, audio ring, demux fixtures, address blocklist and HTTP source behave as specified. | `cargo test --workspace` | CI |
| Session lifecycle | Pause freezes position, seeks settle, and Ended waits for the audio ring's tail, including seeks after Ended or mid-drain. | `cargo test -p media-engine --test session` | CI |
| ABI record padding | Records copied into a caller's buffer have no padding bytes; a compile-time assert fails the build if one appears. | `cargo build -p media-ffi` | CI |
| Session teardown ordering | Closing a session, even mid-open, returns only after every thread it spawned has been joined. | `cargo test -p media-engine --lib close_tests` | CI |
| Seeks land on their target | A seek shows its target within a frame across picture, sound, captions and SEI, handles targets past the end, and discards no audio. | `cargo test -p media-engine --test session between_keyframes`; by hand: `cargo test --release -p media-engine --test session a_long_decode_forward -- --ignored` (set `BASIS_MEDIA_TEST_SPARSE_KEYFRAMES_URL` to an H.264 MP4 over HTTPS whose last keyframe before 17 s is several seconds earlier) | CI, Windows; by hand |
| Seek matrix | Seeks land on every MP4 layout, HLS VOD, Matroska and raw audio; raw TS and live HLS refuse with Unsupported. | `cargo test -p media-demux --test mp4_stream` + `--test ts_stream` + `--test hls` + `--test raw_audio`; `bm-probe bench <lane>` | CI; bench by hand |
| Conformance (ffprobe oracle) | Demuxed access units for the MP4 and MPEG-TS/m2ts fixtures match ffprobe's packets in count, timestamps, payload hashes and keyframe flags. | `cargo run -p bm-probe -- conformance fixtures` | CI |
| Fuzz | The demuxers, HLS playlist parser and caption decoder never panic or read out of bounds on hostile input. | `cargo +nightly fuzz run mp4_stream` (likewise `ts_stream`, `hls_playlist`, `mkv_stream`, `flac_stream`, `mp3_stream`, `adts_stream`, `ogg_stream`, `caption_scan`) | By hand (Linux, nightly) |
| Unsafe confinement | `unsafe` appears only in the ABI boundary, platform decode adapters and present layer; other crates refuse it at compile time. | `cargo build --workspace` | CI |
| Compiler floor | The declared `rust-version` matches the real minimum compiler the dependency graph needs. | `rustup run 1.92 cargo check --workspace --all-targets` | By hand |
| Raw-pointer obligations | Functions dereferencing caller-supplied raw pointers are `unsafe fn`, and call sites without a documented `unsafe` block fail to compile. | `cargo build --workspace` plus the aarch64 Android graph | CI, Android build |

### Demux and containers

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| TS demuxer unit rows | The TS demuxer gets expected counts, joins mid-stream on an SPS keyframe, unwraps 33-bit timestamps, and handles m2ts and LPCM. | `cargo test -p media-demux --test ts_stream` | CI |
| TS table parsing | Each PAT/PMT section is parsed once per version, and malformed, bad-CRC, not-yet-applicable or other-program sections never bind the wrong streams. | `cargo test -p media-demux --test ts_stream`; `cargo run -p bm-probe -- conformance fixtures` | CI |
| Demux note caps | Diagnostic notes from stream content (TS, HLS, Matroska, MP4, Ogg) are deduplicated and capped at 64. | `cargo test -p media-demux --lib demuxer` + `--test ts_stream` + `--test mkv_stream` + `cargo test -p media-hls --test hls` | CI |
| Fragmented MP4 opened from its index | Fragmented MP4 opens from a trustworthy `sidx`, or else from the `tfra` in a trailing `mfra`, loads fragments on demand and reads each one's data in one pass, seeks like a full parse, otherwise walking the file. | `cargo test -p media-demux --lib mp4` + `--lib mp4_index` + `--lib mp4_fragment` + `--test mp4_stream` | CI |
| MP4 AAC config | Each `mp4a` track's AudioSpecificConfig reaches the decoder byte for byte, so HE-AAC (SBR, parametric stereo, explicit or backward-compatible) is configured at its output rate and channel count. An `esds` the walk cannot reach falls back to the fields the box parser kept, with a note; a config that ends early, or an object type that is not an AAC core, is refused with a note. | `cargo test -p media-bitstream asc` + `cargo test -p media-demux --lib mp4_esds` + `--test mp4_stream`; by hand: `bm-probe play <an HE-AAC 5.1 MP4> --duration 20 --audio-out out.f32`, every channel carrying sound | CI; by hand |
| Matroska demuxer rows | Matroska/WebM gets expected counts, converts H.264 to Annex-B, announces VP9 and Opus, applies CodecDelay and cue-seeks to keyframes. | `cargo test -p media-demux --test mkv_stream` | CI |
| Raw audio demuxer rows | FLAC, Ogg Opus, MP3, ADTS and WAV demux with exact timestamps, report duration, seek, and refuse unsupported layouts with a typed error. | `cargo test -p media-demux --test raw_audio` + `cargo test -p media-demux --lib` | CI |
| Embedded cover art | Cover art is extracted undecoded from FLAC, Ogg, ID3v2 and MP4 tags, refusing hostile lengths and preferring the front cover. | `cargo test -p media-demux --lib artwork` + `--test raw_audio`; by hand: play an audio-only source with an asymmetric picture and check it on the output texture | CI; by hand |
| Matroska stated geometry | Matroska stating a NaN, infinite or huge audio rate, channel count or duration skips that track or reports unknown duration; video plays. | `cargo test -p media-demux --test mkv_stream` | CI |
| MKV/WebM playback | H.264+AAC, VP9/Opus, AV1/Opus and H.265+AAC Matroska/WebM files play end to end with both tracks. | `cargo run -p bm-probe -- play fixtures/mkv/h264-aac.mkv --duration 8` | By hand |
| Audio track selection | Each audio track in MP4 and Matroska is listed with its language and playable by index; out-of-range picks the first. | `cargo test -p media-demux --test audio_tracks`; `cargo run -p bm-probe -- play fixtures/h264-multiaudio.mp4 --audio-track {0,1} --audio-out out.f32` | CI; by hand |
| Audio tracks with no metadata | A file with several untagged audio tracks lists and binds each one, labelled by track position. | `cargo test -p media-demux --test audio_tracks untagged` | CI |
| HLS scheduler unit rows | HLS playlists parse or refuse unsupported features, and the scheduler handles window refresh, variant choice, join point and VOD seeks. | `cargo test -p media-demux --test hls` | CI |
| CEA-608 caption lane | In-band CEA-608 captions in H.264 decode into timed pop-on, roll-up, special-character and clear cues, and seeks reset the decoder. | `cargo test -p media-bitstream` + `cargo test -p media-engine --test session caption`; by hand: `bm-probe play fixtures/h264-608-640x360-30fps.ts --duration 10` | CI; by hand |
| SEI user-data lane | H.264/H.265 SEI `user_data_unregistered` data arrives in order with UUID and timestamp, in bounded drop-oldest storage that seeks clear. | `cargo test -p media-bitstream` + `cargo test -p media-engine --lib user_data_ring` + `cargo test -p media-engine --test session user_data`; by hand: `bm-probe play fixtures/h264-sei-userdata-640x360-30fps.ts --duration 10` | CI; by hand |
| SEI user data: managed delivery at the playhead | `BasisMediaPlayer.UserDataReceived` fires once per message, in order, at its timestamp; a late subscriber still gets messages not yet due. | the VRSL-URP package's Basis tests (Test Runner, assembly `Towneh.VRSL.URP.Basis.Tests`) | By hand (Unity) |

### Decode

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Software decode adapters | claxon FLAC, libopus Opus and rav1d AV1 decode fixtures with correct timestamps; surround Opus and broken headers get typed errors. | `cargo test -p decode-sw` | CI |
| PCM adapter | WAV and Blu-ray 16- and 24-bit PCM converts to float identically in WAV channel order, and unsupported formats are refused. | `cargo test -p decode-sw` | CI |
| MF adapter contracts | H.264, AAC and MP3 decode through Windows' built-in Media Foundation decoders, and VP9 and AV1 through installed Store extensions. | `cargo test -p decode-mf` | CI, Windows |
| Strided plane copies | Decoder-reported strides, sizes and dimensions are checked before plane copies, and negative, short, overflowing or odd-sized geometry is refused. | `cargo test -p decode-mf --lib` + `cargo test -p decode-sw --lib` | CI (decode-mf half Windows only) |
| Windows hardware decode | DXVA decodes on the GPU with no CPU copy and matches software byte for byte on the visible frame for H.264, VP9 and AV1. | `cargo test -p decode-mf --test dxva_decode` + `cargo test -p media-present --test gpu_pass present_slice` | CI, Windows |
| Media Foundation decoder fed only when dry | A submit never blocks inside a Media Foundation decoder; both adapters offer input only after the decoder asks for more. | `cargo test -p decode-mf --test dxva_decode av1_submit_never_waits`; by hand: `bm-probe play <an AV1 MP4 with muxed audio> --duration 40 --csv x.csv` | CI, Windows; by hand |
| Live decode at low latency | On a live source, the Windows H.264 decoders (DXVA and the in-box MFT) hand out each frame as soon as the stream's reordering allows, with the same frames in the same order as normal decode, so a live join presents in step with the audio-led clock. | `cargo test -p decode-mf --test dxva_decode live_h264`; live: `bm-probe play <a live RTSP stream, then a live HTTP-TS stream> --duration 30` presents nearly every decoded frame, with pool drops in single figures | CI, Windows; by hand |
| Slow video costs video, picture stays in step | With a too-slow decoder, audio stays whole, position follows audio, late video skips to a keyframe, and no frame shows over 40 ms late. | `cargo test -p media-engine --test slow_video`; by hand: `BASIS_MEDIA_SLOW_VIDEO_DECODE_MS=80 bm-probe play fixtures/h264-aac-320x180-30s.mp4 --duration 25 --csv x.csv` | CI, Windows; by hand |
| Forced hardware fallback | With `BASIS_MEDIA_DISABLE_HW_DECODE` set, the default plays through in software and `hardware_only` gives a typed error while audio plays. | `cargo test -p media-engine --test hw_fallback` | CI |
| Decode preference | `decode_preference` (hardware with fallback, hardware only, software only) picks the decode route; an unavailable route gives a typed error. | Covered by the hw_fallback rows; by hand: `bm-probe play <url> --decode fallback\|hardware\|software` | CI; by hand |
| Software-route cap | Software decode accepts up to 1080p60 and refuses larger with a typed error before building a decoder, on either route. | `cargo test -p media-engine --lib route` | CI |
| Capability contract | The capability report keeps its exact JSON shape, lists only what this build decodes, and uses the size-then-fill buffer convention. | `cargo test -p media-engine --test capabilities` + `cargo test -p media-ffi --test capabilities`; by hand: `cargo run -p bm-probe -- caps` | CI, Windows |

### Present, audio output and clock

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Clock correction law | Clock correction is proportional to the error, capped wide for 1.2 s after a snap then at 2%, with out-of-range ceilings bounded. | `cargo test -p media-clock` | CI |
| GPU conversion pass | The D3D11 NV12-to-BGRA pass matches the CPU reference for every colour matrix and range, on synthetic sweeps and decoded frames. | `cargo test -p media-present --test gpu_pass` | CI, Windows |
| Shared-texture handle lifetime | The presenter always closes its shared texture handle; the consumer reopens on a handle change, retrying failed opens up to eight times. | `cargo test -p media-present --test gpu_pass dropping_the_presenter`; by hand: play an HLS ladder whose renditions change resolution across a discontinuity and check the picture stays live | CI, Windows; by hand |
| Direct3D 12 handoff | Converted frames reach a D3D12 texture unchanged; the newest finished frame is copied, never an older one after it; a slot with a copy pending is not converted over; a destination of another size is refused; no copy is recorded while the host cannot give its next frame-fence value; a dropped consumer keeps its objects until its last copy completes. | `cargo test -p media-present --test d3d12_handoff` | CI, Windows |
| Multichannel interleave order | Multichannel PCM is interleaved in WAV channel-mask order (FL FR C LFE BL BR), checked with a 5.1 tone-per-speaker fixture. | `cargo test -p media-engine --test session multichannel_interleave` | CI, Windows |
| PTS-annotated ring serve | Ring timestamp markers track media time, audio over 300 ms late is trimmed in bounded steps, and an on-time full ring is not. | `cargo test -p media-engine --lib audio`; live: `bm-probe play <a live RTSP stream> --duration 75` | CI; by hand |
| Audio ring generation swap | Audio ring resets are atomic with the consumer swap, and a refused decoder retires its producer and consumer. | `cargo test -p media-engine --lib audio` | CI |
| Audio pts-marker budget | The 1024 timestamp markers survive tiny audio chunks: contiguous chunks need none, and without a free slot the producer waits. | `cargo test -p media-engine --lib audio` | CI |
| Audio ring sizing | Whatever rate and channel count is announced, the audio ring's allocation stays under 6 Mi samples, rounded to whole frames. | `cargo test -p media-engine --lib audio` | CI |
| A/V output-latency compensation | The device's reported output latency shifts the audio clock back, clamped to 0 to 500 ms and applied by slewing. | `cargo test -p media-engine --lib audio` + `--test session audio_latency`; on device: listen for sync on a fixture and on the RTSP stereo stream | CI; Device (Quest) |
| Render-event frame selection | The render event shows each 24 fps frame once at 72 Hz; the video thread takes over without events. On Direct3D 12, where the copy lands one event later, frames are chosen and judged late one refresh further ahead. | `cargo test -p media-engine --lib present` | CI |
| Output-texture ownership | Closing and reopening a source releases the output texture, and the texture count stays level across open and close cycles. | In the Editor or a standalone build, open, close and reopen a source half a dozen times with the Profiler's Memory module on Texture2D count | By hand |
| Headless audio lane | `bm-probe` writes decoded PCM out as raw interleaved f32. | `bm-probe play <src> --audio-out out.f32` | By hand |

### Buffering, pacing and resilience

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Live join starts on audio | A live join clocks from the first buffered audio, shows video from its keyframe, and neither snaps the clock nor discards audio. | `cargo test -p media-engine --test session`; live: `bm-probe play <a live RTSP stream> --duration 60 --csv out.csv` | CI; by hand |
| Live-vs-on-demand inference | On Auto liveness, a finite HTTP source answering range requests is on-demand, otherwise live, with its total from `Content-Range`. | `cargo test -p media-io --test http_source seekability` | CI |
| Startup burst (VOD) | The Bank (demux-to-decode buffer) releases the first 2 s unpaced at startup and after seeks, then paces at real time. | `cargo test -p media-bank` | CI |
| Priming join (live) | Live lanes prime the decoder, hold presentation until target depth arrives, then release at real time without pausing. | `cargo test -p media-bank --test priming` | CI |
| Per-track release | A full decode channel holds back only its track in the Bank, and a live join's early audio is kept. | `cargo test -p media-bank --test gated` | CI |
| A seek straight after open | A seek that lands before a track's Format has left the Bank keeps the Format, so both tracks still decode and the new timeline re-arms the A/V offset. | `cargo test -p media-bank --test gated a_seek_keeps` + `cargo test -p media-engine --test session a_seek_clears_the_offset`; the session row only meets the window under load, so run it with the other `a_` rows (`cargo test -p media-engine --test session a_`) | CI |
| Audio-leading start | Live sessions start at the first buffered audio, on-demand sessions start audio and video together, and stale `"audio_leading"` is ignored. | `bm-probe bench <rtsp-lane> --live` | By hand |
| Impairment, CI lane | Under the worst profile (+300 ms round trip, 0.05% loss) at 3 s depth, the session keeps presenting and stalls within the sizing model. | `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000` | CI, Windows |
| Impairment, full profiles | All recorded profiles at full length and several depths: file lanes stay within the sizing model, live lanes keep presenting. | `bm-probe impair <file.ts\|live-url> --profile <name> [--depth-ms N] [--csv out.csv]` | By hand |
| Reconnect/resilience | A dropped live connection reconnects with jittered backoff, keeping its buffer; exhausted attempts end the session (Ended for EOF, Error for I/O loss). | `cargo test -p media-engine --test reconnect`; by hand: play a feeder with `--live` and restart it mid-run | CI; by hand |

### Transports and sources

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Headless playback, file | A local MP4 plays at 1x with expected decode and present counts, no pool drops and hardware-cadence audio pulls. | `cargo run -p media-engine --example smoke -- fixtures/h264-aac-640x360-30fps.mp4 5` | By hand |
| Headless playback, TS file | A local MPEG-TS file plays likewise, with the container sniffed and stream ids read from the programme map table. | `cargo run -p bm-probe -- play fixtures/h264-aac-640x360-30fps.ts --duration 5` | By hand |
| Auto liveness costs one connection | A live URL on Auto liveness opens one connection, the live lane adopting the liveness probe's response as its stream. | `cargo test -p media-engine --test reconnect` | CI, Windows |
| Headless playback, HTTP | A file served over local HTTP plays through media-io, with range requests and a pinned connection. | serve `fixtures/` locally, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/h264-aac-640x360-30fps.mp4 --duration 5 --allow-local` | By hand |
| Headless playback, HTTP-TS live | A live HTTP TS stream given `--live` plays sequentially with per-read stall detection and the Bank in live lag mode. | `python tools/live-ts-server.py <file.ts> <duration_s> <port>`, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/live --live --allow-local --duration 12` | By hand |
| Live-source unit rows | The live source streams sequentially, re-reads its head cache, reports typed stall errors, cancels its connect, re-vets redirects and applies the address gate. | `cargo test -p media-io --test live_source` | CI |
| On-demand source cancellation | Closing a session cancels an on-demand HTTP open or read against a quiet server within about 200 ms. | `cargo test -p media-io --test http_source` | CI |
| One connection per sequential open | A 200 answer to the range probe becomes the stream; a quiet body gives a typed error within the read timeout. | `cargo test -p media-io --test http_source` | CI |
| A chunk outlives an idle consumer | Long pauses and low-bitrate files do not fail ranged chunk requests; a silent server still errors within the read timeout. | `cargo test -p media-io --test http_source`; by hand: `bm-probe play <an MP3 over HTTPS>` | CI; by hand |
| Ranged requests sized to the reader | A jump asks for 64 KiB or the read's size, reads that carry on double up to the chunk size, a skip of up to 128 KiB reads on, and a walk of jumps keeps one connection. | `cargo test -p media-io --test http_source`; by hand: `bm-probe probe` a fragmented MP4 without `sidx` over HTTPS and `bm-probe bench` an Ogg Opus file, comparing the bytes in the server's access log with the file size | CI; by hand |
| A dropped connection is reopened | A failed ranged read retries once from the byte reached, and a connection dropped mid-file reads back byte for byte. | `cargo test -p media-io --test http_source` | CI |
| A redirect met mid-file is followed | Ranged requests follow redirects from the caller's URL, gate-vetting each hop, and a 206's `Content-Range` must start at the requested byte. | `cargo test -p media-io --test http_source` | CI |
| Transport errors name their cause | HTTP and WHEP transport failures report the full error chain, including the operating system's reason for a refused connection. | `cargo test -p media-io --test http_source` + `cargo test -p media-whep --test signal` | CI |
| Zero-length reads | An on-demand read into an empty buffer returns at once without touching the network or discarding its held chunk. | `cargo test -p media-io --test http_source` | CI |
| Resolve ceiling and open cancellation | DNS runs under one time ceiling off the reactor, WHEP opens cancel on close, and failed opens release tokens and server sessions. | `cargo test -p media-io --lib` + `cargo test -p media-whep --test signal` | CI |
| Resource fetch caps | Playlist fetches over HTTP and file are byte-capped, accepting exactly the cap and refusing one byte more. | `cargo test -p media-io --test resource_fetcher` | CI |
| Playlist origin confinement | A network-fetched playlist can never read local files, however URIs are spelt; a disk playlist keeps file and network access. | `cargo test -p media-io --test resource_fetcher` + `cargo test -p media-engine --test hls_origin` | CI |
| Source routing | Schemes route normalised and case-insensitively; unsupported schemes are config errors, UNC paths need the address gate off, and local paths play. | `cargo test -p media-engine --lib classify_tests` + `cargo test -p media-engine --test routing` | CI |
| Playlist URI scheme | Playlist entries must resolve to `http` or `https`; `file:`, `ftp:`, `data:` and drive-letter forms are refused, relative and cross-host entries work. | `cargo test -p media-hls --test hls` | CI |
| Playlist directory confinement | A disk playlist reaches only plain relative files beside it; absolute, `..`, drive-relative, UNC and planted-link paths are refused. | `cargo test -p media-hls --test hls` + `cargo test -p media-io --test resource_fetcher` | CI |
| HLS VOD playback | HLS from file or HTTP plays to Ended without pool drops, chaining TS segments and timing fMP4 by its timestamps. | `cargo run -p bm-probe -- play fixtures/hls/ts/index.m3u8 --duration 8` and `…/hls/fmp4/index.m3u8` | By hand |
| HLS live | Live HLS joins three segments back with the Bank in lag mode, slides in real time, crosses discontinuities and ends on ENDLIST. | `python tools/live-hls-server.py fixtures/hls/ts <duration_s> <port>`, then `bm-probe play http://127.0.0.1:<port>/live.m3u8 --allow-local --duration 25` | By hand |
| HLS over real HTTPS | HLS VOD with ranged segments and 5.1 audio plays from a real HTTPS origin, as does a public master playlist. | an on-demand HLS playlist with TS segments and one with fMP4 segments, served over HTTPS with ranges and carrying 5.1 audio; the public `https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8` | By hand |
| RTSP lanes | `rtsp://` falls back from UDP to TCP, `rtspt://` stays on TCP; stereo, 5.1, slow-join, restart and forced-fallback lanes play with sender-report alignment. | live RTSP streams in stereo, in 5.1, with no audio, and with keyframes about 10 s apart, each over `rtsp://` and `rtspt://`; the public `rtsp://stream.vrcdn.live/live/vrcdn` | By hand |
| RTP timestamp scaling | RTP timestamp and sender-report conversion to microseconds saturates at both signs, and ordinary spans convert unchanged. | `cargo test -p media-rtp --test receiver` | CI |
| Depacketizer drain discipline | WHEP and RTSP-UDP lanes drain the depacketizer after every refused packet, survive crafted sequences without panicking, and count refusals. | `cargo test -p media-whep --lib` + `cargo test -p media-rtsp --lib` | CI |
| AAC access units fragmented by marker bit | Multichannel AAC split across RTP packets reassembles up to the marker bit, size-bounded, loss discarding the partial unit; 5.1 decodes cleanly. | `cargo test -p media-rtsp --test aac_reassembly`; by hand: `cargo run -p bm-probe --release -- play <a live RTSP stream with 5.1 AAC> --duration 20 --audio-out out.f32` | CI; by hand |
| WHEP lanes | `whep://` and `wheps://` play H.264 and Opus over WebRTC with vetted HTTP signalling, gate-checked media addresses and reconnect on publisher restart. | `cargo test -p media-whep`; by hand: `bm-probe play <a WHEP endpoint carrying H.264 and Opus>` | CI; by hand |
| WHEP signalling body cap | WHEP signalling answers are capped at 256 KiB while arriving, accepting exactly the cap and refusing more, including endless chunked bodies. | `cargo test -p media-whep --test signal` | CI |
| RIST lanes | `rist://` plays plain and AES-128 streams from a gate-vetted host; wrong secrets and RIST-less builds give typed errors, callback panics are reported. | `cargo run -p bm-probe --features rist -- play rist://127.0.0.1:11968 --duration 12 --allow-local` (with an ffmpeg RIST sender on loopback); by hand: `bm-probe play rist://<host>:<port> --duration 20` against a RIST sender | CI, rist feature; by hand |
| RIST on Android | The Android arm64 plugin builds with RIST linked in, within its size budget and adding no shared-library dependency. | `bash tools/build-librist-android.sh` then `.\tools\stage-android-plugin.ps1` | By hand |
| Split sources | Split video and audio sources stay within 100 ms, seek together, and unsupported or mismatched pairs get typed errors. | `cargo test -p media-engine --test split_source`; `bm-probe play fixtures/split/h264-640x360-30fps-video.mp4 --audio-url fixtures/split/aac-48k-stereo-audio.m4a --duration 9` | CI |
| Audio-only file lanes | Raw FLAC, Opus, PCM, MP3 and ADTS AAC play to End at their stored duration, audio driving clock and position. | `cargo run -p bm-probe -- play fixtures/sine-48k-stereo.{flac,mp3,aac,opus} --duration 9` | By hand |
| Codec breadth over HTTPS | FLAC (16/24-bit, 5.1, variable blocksize), MP3 (CBR, VBR), Opus, VP9 and AV1 files each play over HTTPS at 1x without pool drops. | `bm-probe play <file URL>` for each | By hand |

### Shared playback and sync

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Sync soft target | The engine ignores a follower's sync error under 150 ms, slews up to 2% above that and seeks past 2 s; live ignores it. | `cargo test -p media-engine --test session sync_target` | CI |
| Divergence bound | `max_divergence_ms` caps how far behind live the Bank runs, Auto's growth included, and a larger explicit depth gives a typed error. | Covered by the Bank's config validation rows; by hand: open a live lane with `"max_divergence_ms"` set and check Auto depth stays inside it in the capture CSV | CI; by hand |

### Platforms

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Linux headless lane | Headless on linux-x64, the engine decodes AV1, FLAC and Opus in software and gives typed errors for platform-only codecs. | `tools/ci.sh` (on a Linux host) | By hand (Linux host) |

### Harness and instrumentation

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Bench lane | Startup-to-first-frame and seek-to-settled times are measured per lane and aggregated over several runs. | `bm-probe bench <src\|url> [--runs N] [--seek-to-ms N] [--live]` | By hand |
| Capture recorder | The diagnostics timeline is written as a CSV with a stable column contract. | `bm-probe play <src> --csv out.csv` | By hand |
| Engine A/V offset is measurable | Video-minus-audio offset reaches the ABI snapshot, both captures and `bm-probe`, reading "unknown" until a frame is presented or with no session. | `cargo test -p media-engine --lib presentation_arms`. By hand: `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 20 --depth-ms 3000 --csv x.csv`, then count rows whose `av_offset_us` is not `i32::MIN` | CI; by hand |
| Serve-trim total is a capture column | Cumulative serve-trim discards reach the capture as `audio_trimmed_frames` and do not fall across a seek. | `cargo test -p media-diag`. By hand: `bm-probe play <live-src> --csv out.csv` and confirm the summary's trimmed figure equals the last row's final column | CI; by hand |
| Capture reaches the file | The capture writer flushes and errors when rows cannot reach the target, and a truncated capture fails the run. | `cargo test -p media-diag`. By hand on Linux: `bm-probe play fixtures/h264-aac-320x180-30s.ts --duration 9 --interval-ms 2000 --csv /dev/full` | CI; by hand (Linux) |
| Impairment run owns its capture | A `bm-probe impair` run whose CSV capture failed grades as a failure, exits non-zero and still prints its grade. | `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000 --csv <an existing directory>` | By hand |
| Diagnostic log sink | Every textual engine diagnostic goes through one replaceable sink, defaulting to stderr and also reaching `OutputDebugString` on Windows. | `cargo test -p media-diag`. By hand: `bm-probe play fixtures/does-not-exist.mp4 --duration 3` prints `[basis-media] session error: …` | CI; by hand |
| An event drain is a backlog, not a loss | `bm_session_drain_events` hands back at most the requested number of events and leaves the rest queued for the next call. | `cargo test -p media-diag` | CI |
| Refused events are reported | The count of events the session log refused reaches the ABI snapshot as `events_dropped`, saturating at the field's limit. | `cargo test -p media-ffi --lib` | CI |
| Bank schedule readings are capture columns | The Bank's lag, target lag, reanchor and stall totals appear as engine capture columns and in `bm-probe play`'s `bank:` line. | `cargo test -p media-diag`. By hand: `bm-probe play <live-src> --duration 60 --csv out.csv` | CI; by hand |
| Join surplus is returned, not held | Media arriving after a live join anchors is decayed back downstream as surplus, pausing while the release thread is blocked. | `cargo test -p media-bank`. Live: `bm-probe play <a live RTSP stream> --duration 90 --csv out.csv` | CI; by hand |
| A live source does not pause | A pause on a live session is ignored and logged once, and a pause arriving during opening is dropped. | `cargo test -p media-engine --test session pause` | CI |
| A pause holds across a seek | Pause and seek in either order show the target frame, stay paused there, and `play` resumes from it, with or without video. | `cargo test -p media-engine --test session a_` | CI |
| Auto-inferred HTTP live lane runs as live | A live HTTP source detected by automatic liveness runs the Bank in live mode, the same as one declared live. | `cargo test -p media-engine --test reconnect`. Live: `bm-probe play <a live MPEG-TS stream over HTTPS> --csv out.csv` | CI; by hand |
| Free text reaches a drain, not only a sink | Every free-text diagnostic line also lands in a drainable process-wide ring that evicts oldest when full and counts evictions. | `cargo test -p media-diag` | CI |
| The process log crosses the ABI | `bm_drain_log` needs no session handle, leaves lines past the cap queued, and truncates on a UTF-8 boundary. | `cargo test -p media-ffi --test log_drain` | CI |
| A cut detail says it was cut | A truncated detail ends in `…` without overrunning its buffer or splitting a multi-byte character, in event and log drains. | `cargo test -p media-ffi --no-fail-fast` | CI |
| Unity end to end | The engine and managed component play a source in Unity, with the capture graded against healthy ranges in [`DIAGNOSTICS.md`](DIAGNOSTICS.md). | `Basis > Tools > Media Player > Run Smoke Test`, or headless: `Unity -batchmode -projectPath <project> -logFile - -executeMethod BasisMediaSmokeTest.RunBatch` | By hand |

## Not yet run

These have never been run and do not count as coverage.

| Row | What it would check | How to run it |
| --- | --- | --- |
| SEI user data: managed seek and loop | A seek drops the managed SEI user-data queue, and a looping file drops what the previous pass left queued. | a scene script subscribing and logging `ptsUs` against `fixtures/h264-sei-userdata-640x360-30fps.ts` served over HTTP: seek back and confirm the frame indices restart from the landed frame with nothing from the old position in between; loop the file and confirm the same across the wrap |
| A fragmented MP4 hours long | A multi-hour, multi-gigabyte fragmented MP4 over ranged HTTP opens from its segment index, and a seek near the end plays. | `ffmpeg -stream_loop -1 -i <clip> -t 7200 -c copy -movflags +frag_keyframe+empty_moov+default_base_moof+global_sidx -frag_duration 2000000 out.mp4`, serve it, then `bm-probe play <url> --duration 30 --seek-to-ms 6600000` |
| RIST on Android | Plain and AES-128 RIST streams connect and play on a Quest client build. | a Quest build playing a plain and an AES-128 RIST stream |
| Android audio geometry refusal | Out-of-range Matroska audio rate or channel count gets a typed, logged refusal: the audio mutes while video plays. | a Quest build playing a crafted Matroska file; `adb logcat -s basis-media` |
| FFI callback panic fences | MediaCodec, Vulkan and librist callbacks record panics without unwinding into foreign code, and a mid-render close leaks no Vulkan objects. | a Quest build playing a fixture, closed mid-playback |
| MediaCodec frame geometry refusal | A failed or non-positive NDK frame-size query is refused with a typed error, and ordinary playback is unchanged. | a Quest build playing a fixture |
| Vulkan present-layer lifetimes | Closing the last player is validation-clean and the next open presents; reopens get a fresh `RenderTexture`; logcat shows no device address. | a Quest build: close the last player, reopen, then `adb logcat -s basis-media` |
| Vulkan device-creation feature guard | The Vulkan hook enables `samplerYcbcrConversion` only where advertised, video still shows, and the log carries the appended-extension line. | a Quest build playing a fixture, then `adb logcat -s basis-media` |
| Engine diagnostics in logcat | Engine diagnostics reach logcat under `basis-media`, including `session error:` for an unresolvable URL and the transport line when playing. | a Quest build opening an unresolvable URL and then a live stream, with `adb logcat -s basis-media` |
| Proton / Wine | Under Proton, ranged VOD and live HTTP-TS classify as on Windows, and https uses Wine's certificate store. | the Windows client under Proton on a Linux box, one ranged VOD and one live HTTP-TS lane |

## Android devices

A Quest on USB with debugging authorised. Stage the plugin with
`tools\stage-android-plugin.ps1`, build a Quest client of a test scene, play the
row's source, and read the captures and `adb logcat -s basis-media`.

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Android build lane | The whole engine graph compiles and lints clean for `aarch64-linux-android`, without the AV1 software decoder. | the gate's `Android (aarch64)` step (prints SKIPPED without an NDK). By hand: `. .\tools\android-env.ps1` then `cargo clippy --target aarch64-linux-android -p media-ffi -p decode-mediacodec -- -D warnings` | CI, Android build |
| Engine .so | `libbasis_media.so` links only against `libmediandk`, `liblog` and the C runtime, and exports the ABI plus `UnityPluginLoad` and `JNI_OnLoad`. | `. .\tools\android-env.ps1; cargo build --target aarch64-linux-android -p media-ffi --release`, then `llvm-readelf -d`/`--dyn-syms` on `target/aarch64-linux-android/release/libbasis_media.so` | By hand |
| Playback on device | MediaCodec hardware decode presents via Vulkan into a `RenderTexture` under OpenXR, with AAC audio, a mid-run seek and natural end. | a Quest build playing a fixture with a seek | Device (Quest) |
| AAC from MP4 on device | AAC in MP4 whose config carries the SBR sync extension with SBR absent, as ffmpeg writes it, plays with sound in stereo and 5.1; an HE-AAC MP4 keeps its SBR. | a Quest build playing `fixtures/aac-48k-stereo.m4a`, `fixtures/sine-48k-51.m4a` and an HE-AAC 5.1 MP4 | Device (Quest) |
| Managed package on device | The package plays with Vulkan output, audio resampled to the device rate, and position advancing one second per second. | a Quest build playing a fixture; grade the frame capture | Device (Quest) |
| Android https | An `https://` lane connects and plays on device using the bundled root certificates. | a Quest build playing any `https://` source | Device (Quest) |
| Diagnostics captures | The engine writes its diagnostics CSV on close, and the frame capture writes one row per Unity frame. | `cargo test -p media-engine --test session diag_csv_written_on_close`. On device: `adb pull` both captures from the app's files directory after a run | CI; Device (Quest) |
| Steady-lane cadence | On a steady live stereo stream, frames hold for the ideal number of display refreshes through audio-callback jitter, skipping none. | a Quest build playing a steady live stereo RTSP stream over `rtspt://`; grade frame holds over a window after the join | Device (Quest) |
| Live lanes on device | RTSP (stereo, 5.1, UDP and TCP), WHEP and live HLS lanes each decode, present and play audio on device. | a Quest build playing each kind of stream | Device (Quest) |

## Known gaps

- No test covers the ordering that stops the A/V offset reading a new timeline
  against the old one after a seek.
- A fragmented MP4 with neither a segment index nor an `mfra` reads every
  fragment header at open, which is slow on a long file over HTTP.
- A seek in a fragmented MP4 opened from its index fetches the target's
  fragment, and can fetch its neighbour, before it lands; over a long
  round trip that is slower than a file whose whole table was read at open.
- A seek into a fragmented file whose audio and video fragments are cut at
  different points does not replay audio from the fragment before the landing.
- A hostname lookup cannot be cancelled once started; a queued lookup is
  refused at the same time limit.
- The engine's 7.1 tests stop at the decoder; speaker output and mix-down are
  in [`../TESTING.md`](../TESTING.md).
- A split pair whose sources start at different times by more than the decoder
  cushion is refused.
- Raw TS cannot be seeked. HLS has no encryption, byte-range segments,
  keyframe-only playlists or adaptive switching, and fetches whole segments of
  up to 64 MiB.
- Raw audio seeks are approximate except WAV and FLAC.
- No VP9 software decoder; Opus is mono or stereo; AV1 above 8-bit 4:2:0 is
  refused.
- RTSP carries H.264 and AAC only, without credentials or multicast.
- WHEP carries H.264 and Opus only, ignores the server's STUN and TURN entries,
  and sends no authentication. Its fuzz target needs `--features whep` and
  cmake.
