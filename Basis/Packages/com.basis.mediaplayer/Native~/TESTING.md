# Testing the media engine

How to run the engine's tests, how they are laid out, and how to add one,
followed by the matrix of what is checked and how. The managed side (the
panel, shared playback, the session cap, the prefabs) has its own guide at
[`../TESTING.md`](../TESTING.md), and [`DIAGNOSTICS.md`](DIAGNOSTICS.md) says
how to read the captures a run produces.

When a change touches a row's area, run that row. A row that was not run is not
a row that passed.

## Before you start

- **Rust through rustup.** `rust-toolchain.toml` pins the toolchain (1.98.0,
  with clippy, rustfmt and the `aarch64-linux-android` target), so the first
  `cargo` command installs the right one.
- **cargo-deny and cargo-vet**: `cargo install --locked cargo-deny cargo-vet`.
  `supply-chain/config.toml` records the cargo-vet version the audits were
  written with.
- **On Windows:** PowerShell 7 for `tools/ci.ps1`, NASM on `PATH` (rav1d's
  assembly needs it), and a GPU with hardware video decode for the Direct3D 11
  and Media Foundation rows. Set `CARGO_TARGET_DIR` to a short path such as
  `C:/bm-target`: the package's folder is deep enough that build paths can pass
  Windows' path-length limit.
- **Optional, but each one skips part of the gate without it:**
  - `ffprobe` on `PATH`, for the conformance check against the fixtures;
  - librist, built into `third_party/librist/` by `tools/build-librist.ps1`
    (Windows, needs meson and ninja) or `tools/build-librist.sh`, for the RIST
    rows;
  - the Android NDK that ships with Unity, found by `tools/android-env.ps1`,
    for the Android build check.
- **For some by-hand rows:** Python 3 (the fixture generators and the local
  test servers in `tools/`), ffmpeg (the fixture generators), and nightly Rust
  with `cargo-fuzz` on Linux for fuzzing (see `fuzz/README.md`).

## Running the tests

### The gate

```
.\tools\ci.ps1        # Windows
tools/ci.sh           # Linux
```

This is what every commit has to pass: `cargo fmt --check`, clippy with
`-D warnings`, the whole test suite, the RIST feature's lint and tests, the
Android build check, `cargo deny`, `cargo vet`, the ffprobe conformance check,
a network-impairment replay and a split-source run.

**A skipped step is not a pass.** Without librist, ffprobe or the Android NDK
the gate prints a yellow `SKIPPED:` line for that step and carries on, and the
run still ends green. Read the output for those lines before trusting it.

### One crate, one test

```
cargo test -p media-demux                          # one crate
cargo test -p media-engine --test session          # one test file
cargo test -p media-engine --test session caption  # tests whose names match
```

The end-to-end session tests (`media-engine/tests/session.rs` and
`slow_video.rs`), the Media Foundation decoder tests and the GPU pass tests
need Windows; on other platforms they are not built or they skip. A few tests
are `#[ignore]`d because they need a network source; the matrix says which,
and they run with `-- --ignored` once the environment variable they name is
set.

### The headless player

`bm-probe` runs the engine without Unity, which is how most by-hand rows are
run:

| Command | What it does |
| --- | --- |
| `bm-probe probe <src>` | Opens a source and reports its container, codecs and first-frame timing |
| `bm-probe play <src> --csv out.csv` | Plays it through the whole engine for a set time, writing the engine capture; `--audio-out` writes the decoded audio, `--seek-to-ms` seeks part-way, `--decode` picks the decode route |
| `bm-probe bench <src>` | Measures time to first frame and seek-to-settled, repeated and averaged |
| `bm-probe caps` | Prints what this machine can play, as the JSON the player reads |
| `bm-probe conformance fixtures` | Compares each fixture's demuxed stream with ffprobe's view of it |
| `bm-probe impair <src> --profile <name>` | Replays a recorded network profile over a source and grades the buffering against the model |

Run it as `cargo run -p bm-probe -- <command> …`, adding `--release` for timing
work.

## How the tests are laid out

| Where | What is tested there |
| --- | --- |
| each crate's `src/` (`#[cfg(test)]`) | unit tests of that crate's own logic |
| `media-demux/tests/` | the container demuxers against the fixtures: MP4, MPEG-TS, Matroska, raw audio, audio tracks |
| `media-engine/tests/` | whole sessions: playback, seeking, captions, user data, split sources, reconnects, slow video (mostly Windows) |
| `media-io/tests/`, `media-hls/tests/`, `media-rtp/tests/`, `media-rtsp/tests/`, `media-whep/tests/` | the network sources and protocols, against local scripted servers |
| `media-bank/tests/`, `media-clock/tests/` | buffering and the clock, including property tests and the sizing table |
| `media-present/tests/` | the Direct3D 11 conversion pass (Windows, GPU) |
| `media-ffi/tests/` | the plugin boundary: capabilities and the log drain |
| `media-testkit/` | test support: recorded network-delay profiles (`fixtures/phase0/`) and the impairment source that replays them |
| `fixtures/` | the media files the tests read, all generated |
| `tools/gen-*.py` | the scripts that generate those fixtures |
| `fuzz/` | fuzz targets for the demuxers, the playlist parser and the caption decoder |

## Adding a test

1. **Use the lowest level that can show the behaviour.** A unit test in the
   crate where the logic lives; an integration test in that crate's `tests/`
   when it needs a fixture or a local server; a session test in
   `media-engine/tests/` when only a whole pipeline shows it. A by-hand
   `bm-probe` row is for what needs a real network, a device or a person
   listening.
2. **See it fail first.** Break the code the test guards, run the test alone,
   confirm it fails with a message that names the fault, then restore the code.
   A test that has never failed has not shown that it tests anything.
3. **State the requirement in the test itself.** Assert the number the
   behaviour has to meet, not the engine's own constant: a test that reads the
   constant still passes when someone changes the constant to switch the
   behaviour off.
4. **Generate fixtures; do not record them.** Committed media is made from
   synthetic sources (`testsrc2` and sine tones) by a script in `tools/`, so the
   repository carries no third-party content and anyone can regenerate it. A
   new MP4 or TS fixture has to pass `bm-probe conformance fixtures`; keep TS
   audio at 48 kHz, and give a TS fixture a proper ADTS audio track, since the
   conformance check picks up every `fixtures/*.ts`.
5. **Add its row to the matrix below in the same commit**, named for what it
   proves. Something that exists but has never been run goes under
   [Not yet run](#not-yet-run) until it has been. A change to what a capture
   column carries changes [`DIAGNOSTICS.md`](DIAGNOSTICS.md) too.

## The test server

Rows that name `<test-host>` ran against a test server that is not public. To
run them elsewhere, serve the same kinds of source from a host of your own:
on-demand files under `/vod/` over HTTPS with byte ranges, live RTSP on port
8090 (paths `imax51`, `imaxstereo`, `imaxsilent` and `imaxslowjoin`), WHEP on
8091, RIST on 5000 (plain) and 5001 (AES-128), and single-client MPEG-TS
feeders on 8093 and 8094. Each row names the file or stream it used and what
about it mattered (a 5.1 track, a keyframe at 9.75 s, a variable FLAC
blocksize), which is what a substitute has to match. Public sources are named
as they are.

A feeder that serves one client at a time (ffmpeg's `-listen 1`) cannot be
used to test live-or-on-demand inference: the player's probe takes the only
connection and the real open is refused. Set **Liveness** to Live for those,
and use a server that accepts several clients for anything testing inference.

## What is checked

"Runs in" says where a row runs: **CI** is part of the gate on every platform,
**CI, Windows** is the gate on Windows only, **By hand** needs a person, and
**Device (Quest)** needs a headset.

### Foundations, lifecycle and hostile input

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Unit/property tests | Bank sizing and properties, the clock's correction ladder and its jitter filter, frame-pool leasing, the audio ring and playhead, demux fixtures, the address blocklist and the HTTP source all behave as specified. | `cargo test --workspace` | CI |
| Session lifecycle | Pause freezes position, seeks settle cleanly, and audio-only and audio-plus-video sessions only report Ended once the audio ring's tail has been consumed, including seeks issued after Ended or during the end-of-stream drain. | `cargo test -p media-engine --test session` | CI |
| ABI record padding | No record the engine copies into a caller's buffer has padding bytes that could carry leftover stack memory across the boundary; a compile-time assert fails the build if one appears. | `cargo build -p media-ffi` | CI |
| Session teardown ordering | Closing a session, even mid-open, does not return until every thread the session spawned has been joined and none still touches its shared state. | `cargo test -p media-engine --lib close_tests` | CI |
| Seeks land on their target | A seek decodes forward from the keyframe and shows the requested position (within a frame) for picture, sound, captions and SEI data, handles targets past the last frame, and discards no audio while landing. | `cargo test -p media-engine --test session between_keyframes`; by hand: `cargo test --release -p media-engine --test session a_long_decode_forward -- --ignored` (set `BASIS_MEDIA_TEST_IMAX_URL` to `imax_51.mp4` on the test server) | CI, Windows; by hand |
| Seek matrix | Seeks land correctly on every MP4 layout, HLS VOD, Matroska and each raw audio format, while raw TS and live HLS refuse seeking with a typed Unsupported error. | `cargo test -p media-demux --test mp4_stream` + `--test ts_stream` + `--test hls` + `--test raw_audio`; `bm-probe bench <lane>` | CI; bench by hand |
| Conformance (ffprobe oracle) | The demuxed access units for the MP4 and MPEG-TS/m2ts fixtures match ffprobe's packets in count, timestamps, payload hashes and keyframe flags. | `cargo run -p bm-probe -- conformance fixtures` | CI |
| Fuzz | The demuxers, the HLS playlist parser and the caption decoder never panic or read out of bounds on hostile input. | `cargo +nightly fuzz run mp4_stream` (likewise `ts_stream`, `hls_playlist`, `mkv_stream`, `flac_stream`, `mp3_stream`, `adts_stream`, `ogg_stream`, `caption_scan`) | By hand (Linux, nightly) |
| Unsafe confinement | `unsafe` code is confined to the ABI boundary, the three platform decode adapters and the present layer; every other crate refuses it at compile time. | `cargo build --workspace` | CI |
| Compiler floor | The declared `rust-version` matches the real minimum compiler the dependency graph needs. | `rustup run 1.92 cargo check --workspace --all-targets` | By hand |
| Raw-pointer obligations | Every function that dereferences a caller-supplied raw pointer is an `unsafe fn`, and a call site without a documented `unsafe` block does not compile. | `cargo build --workspace` plus the aarch64 Android graph | CI, Android build |

### Demux and containers

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| TS demuxer unit rows | The MPEG-TS demuxer gives the expected access-unit and keyframe counts, waits for a keyframe with SPS on a mid-stream join, unwraps 33-bit timestamps, handles m2ts and LPCM, and survives pinned crash inputs. | `cargo test -p media-demux --test ts_stream` | CI |
| TS table parsing | Each PAT/PMT table section is parsed once per version, and malformed, corrupted (bad CRC), not-yet-applicable or other-program sections never bind the wrong streams or block the correct copy. | `cargo test -p media-demux --test ts_stream`; `cargo run -p bm-probe -- conformance fixtures` | CI |
| Demux note caps | Diagnostic notes generated from stream content (TS, HLS, Matroska, MP4, Ogg) are deduplicated and capped at 64, however many a hostile source provokes. | `cargo test -p media-demux --lib demuxer` + `--test ts_stream` + `--test mkv_stream` + `cargo test -p media-hls --test hls` | CI |
| Fragmented MP4 opened from its index | A fragmented MP4 with a trustworthy `sidx` index opens from the index alone, reads fragments on demand, seeks to the same place as a full parse, and falls back to a full walk if the index is untrustworthy. | `cargo test -p media-demux --lib mp4` + `--lib mp4_index` + `--lib mp4_fragment` + `--test mp4_stream` | CI |
| Matroska demuxer rows | Matroska/WebM gives the expected counts, converts stored H.264 to Annex-B, announces VP9 and Opus, applies CodecDelay to audio timestamps and lands cue seeks on keyframes. | `cargo test -p media-demux --test mkv_stream` | CI |
| Raw audio demuxer rows | FLAC, Ogg Opus, MP3, ADTS and WAV files are sniffed, demuxed with exact timestamps, report duration and seek correctly, and refuse unsupported layouts with a typed error. | `cargo test -p media-demux --test raw_audio` + `cargo test -p media-demux --lib` | CI |
| Embedded cover art | Cover art is extracted undecoded from FLAC, Ogg, ID3v2 and MP4 tags, hostile lengths are refused, and the front cover is chosen when a file holds several. | `cargo test -p media-demux --lib artwork` + `--test raw_audio`; by hand: play an audio-only source with an asymmetric picture and check it on the output texture | CI; by hand |
| Matroska stated geometry | Matroska files stating a NaN, infinite or huge sample rate, channel count or duration have the audio track skipped or the duration reported as unknown, while the video still plays. | `cargo test -p media-demux --test mkv_stream` | CI |
| MKV/WebM playback | H.264+AAC, VP9/Opus, AV1/Opus and H.265+AAC Matroska/WebM files play end to end with both tracks. | `cargo run -p bm-probe -- play fixtures/mkv/h264-aac.mkv --duration 8` | By hand |
| Audio track selection | Multi-audio MP4 and Matroska list every audio track with its language, the chosen index plays that track, and an out-of-range index falls back to the first. | `cargo test -p media-demux --test audio_tracks`; `cargo run -p bm-probe -- play fixtures/h264-multiaudio.mp4 --audio-track {0,1} --audio-out out.f32` | CI; by hand |
| Audio tracks with no metadata | A file with several untagged audio tracks (a typical recording) still lists and binds each track, and labels fall back to track position. | `cargo test -p media-demux --test audio_tracks untagged` | CI |
| HLS scheduler unit rows | HLS playlists parse and refuse unsupported features, and the scheduler handles live window refresh and jumps, variant choice, join point and VOD seeking. | `cargo test -p media-demux --test hls` | CI |
| CEA-608 caption lane | In-band CEA-608 captions in H.264 are decoded into timed cues covering pop-on, roll-up, special characters and clears, and seeks reset the decoder. | `cargo test -p media-bitstream` + `cargo test -p media-engine --test session caption`; by hand: `bm-probe play fixtures/h264-608-640x360-30fps.ts --duration 10` | CI; by hand |
| SEI user-data lane | Application data carried in H.264/H.265 SEI `user_data_unregistered` messages arrives in order with its UUID and timestamp, within bounded storage that drops oldest, and seeks clear it. | `cargo test -p media-bitstream` + `cargo test -p media-engine --lib user_data_ring` + `cargo test -p media-engine --test session user_data`; by hand: `bm-probe play fixtures/h264-sei-userdata-640x360-30fps.ts --duration 10` | CI; by hand |
| SEI user data: managed delivery at the playhead | `BasisMediaPlayer.UserDataReceived` fires once per message, in order, when playback reaches its timestamp, and a late subscriber still receives every message not yet due. | the VRSL-URP package's Basis tests (Test Runner, assembly `Towneh.VRSL.URP.Basis.Tests`) | By hand (Unity) |

### Decode

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Software decode adapters | claxon FLAC, libopus Opus and rav1d AV1 decode their fixtures completely with correct timestamps, and surround Opus or broken headers are refused with a typed error. | `cargo test -p decode-sw` | CI |
| PCM adapter | 16- and 24-bit integer PCM from WAV (little-endian) and Blu-ray LPCM (big-endian) converts to float identically, Blu-ray channel layouts are reordered to WAV order, and formats it cannot serve are refused. | `cargo test -p decode-sw` | CI |
| MF adapter contracts | H.264, AAC and MP3 decode through Windows' built-in Media Foundation decoders, and VP9 and AV1 through the Store extensions when installed (the rows skip loudly when an extension is absent). | `cargo test -p decode-mf` | CI, Windows |
| Strided plane copies | Row strides, mapped sizes and frame dimensions reported by a decoder are checked before planes are copied: negative, short, overflowing or odd-sized geometry is refused rather than read or written out of bounds. | `cargo test -p decode-mf --lib` + `cargo test -p decode-sw --lib` | CI (decode-mf half Windows only) |
| Windows hardware decode | The DXVA route decodes on the GPU and hands frames to the conversion pass without a CPU copy, and its output matches the software route byte for byte over the visible frame for H.264, VP9 and AV1. | `cargo test -p decode-mf --test dxva_decode` + `cargo test -p media-present --test gpu_pass present_slice` | CI, Windows |
| Media Foundation decoder fed only when dry | A submit never blocks inside a Media Foundation decoder: both adapters offer input only after the decoder has asked for more (the Store AV1 decoder otherwise waits about a second). | `cargo test -p decode-mf --test dxva_decode av1_submit_never_waits`; by hand: `bm-probe play https://<test-host>/vod/tos_av1_frag.mp4 --duration 40 --csv x.csv` | CI, Windows; by hand |
| Slow video costs video, picture stays in step | A decoder too slow for its stream costs video frames, not audio: position follows the audio clock, late video is skipped to the next keyframe, and no frame goes up more than 40 ms late. | `cargo test -p media-engine --test slow_video`; by hand: `BASIS_MEDIA_SLOW_VIDEO_DECODE_MS=80 bm-probe play fixtures/h264-aac-320x180-30s.mp4 --duration 25 --csv x.csv` | CI, Windows; by hand |
| Forced hardware fallback | With `BASIS_MEDIA_DISABLE_HW_DECODE` set, the default preference falls back to software decode and plays to the end, while `hardware_only` refuses with a typed error and audio plays on. | `cargo test -p media-engine --test hw_fallback` | CI |
| Decode preference | The `decode_preference` setting (hardware with fallback, hardware only, software only) picks the decode route, and a route the platform lacks refuses with a typed error. | Covered by the hw_fallback rows; by hand: `bm-probe play <url> --decode fallback\|hardware\|software` | CI; by hand |
| Software-route cap | Software decode accepts up to 1080p60 and refuses anything larger with a typed error before building a decoder, on the software-only route and the fallback alike. | `cargo test -p media-engine --lib route` | CI |
| Capability contract | The engine's capability report keeps its exact JSON shape, lists only what this build can actually decode, and follows the size-then-fill buffer convention over the ABI. | `cargo test -p media-engine --test capabilities` + `cargo test -p media-ffi --test capabilities`; by hand: `cargo run -p bm-probe -- caps` | CI, Windows |

### Present, audio output and clock

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Clock correction law | The clock's correction rate is proportional to the error and stays inside the configured ceiling (wide for 1.2 s after a snap, then 2%), with out-of-range ceiling values bounded where they are used. | `cargo test -p media-clock` | CI |
| GPU conversion pass | The D3D11 NV12 to BGRA conversion matches the CPU reference converter for every colour matrix and range, on synthetic sweeps and on real decoded frames. | `cargo test -p media-present --test gpu_pass` | CI, Windows |
| Shared-texture handle lifetime | The presenter closes its shared texture handle on every path, and the consumer reopens when the handle changes, retrying a failed open up to eight times. | `cargo test -p media-present --test gpu_pass dropping_the_presenter`; by hand: play an HLS ladder whose renditions change resolution across a discontinuity and check the picture stays live | CI, Windows; by hand |
| Multichannel interleave order | Multichannel PCM is interleaved in WAV channel-mask order (FL FR C LFE BL BR) end to end, checked with a 5.1 fixture carrying a different tone per speaker. | `cargo test -p media-engine --test session multichannel_interleave` | CI, Windows |
| PTS-annotated ring serve | The audio ring carries timestamp markers so the playhead follows media time, and audio running over 300 ms late is trimmed in bounded steps while an on-time full ring is never trimmed. | `cargo test -p media-engine --lib audio`; live: `bm-probe play rtspt://<test-host>:8090/imax51 --duration 75` | CI; by hand |
| Audio ring generation swap | Resetting the audio ring for a seek or format change happens atomically with the consumer swap, so an in-flight pull cannot restore the old timeline, and a refused decoder retires its producer and consumer. | `cargo test -p media-engine --lib audio` | CI |
| Audio pts-marker budget | Very small audio chunks cannot exhaust the 1024 timestamp markers: contiguous chunks need no new marker, and a needed marker with no free slot makes the producer wait rather than drop it. | `cargo test -p media-engine --lib audio` | CI |
| Audio ring sizing | Whatever sample rate and channel count a container or decoder announces, the audio ring's allocation stays under a fixed ceiling of 6 Mi samples, rounded to whole frames. | `cargo test -p media-engine --lib audio` | CI |
| A/V output-latency compensation | The audio device's reported output latency shifts the audio clock back so video matches what is heard, clamped to 0 to 500 ms and applied by slewing rather than snapping. | `cargo test -p media-engine --lib audio` + `--test session audio_latency`; on device: listen for sync on a fixture and on the RTSP stereo stream | CI; Device (Quest) |
| Render-event frame selection | Unity's render event chooses the due frame at display cadence, showing each frame of a 24 fps stream exactly once at 72 Hz, and hands over to the video thread when no events arrive. | `cargo test -p media-engine --lib present` | CI |
| Output-texture ownership | Closing and reopening a source releases the output texture, and the texture count stays level across open and close cycles. | In the Editor or a standalone build, open, close and reopen a source half a dozen times with the Profiler's Memory module on Texture2D count | By hand |
| Headless audio lane | `bm-probe` writes decoded PCM out as raw interleaved f32 so it can be inspected or played back. | `bm-probe play <src> --audio-out out.f32` | By hand |

### Buffering, pacing and resilience

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Live join starts on audio | A live session starts its clock at the first buffered audio and shows video from its keyframe against it, so the join causes no clock snap and discards no audio. | `cargo test -p media-engine --test session`; live: `bm-probe play rtspt://<test-host>:8090/imax51 --duration 60 --csv out.csv` | CI; by hand |
| Live-vs-on-demand inference | With liveness on Auto, an HTTP source is treated as on-demand when it is finite and answers range requests, and as live otherwise, reading the total from `Content-Range`. | `cargo test -p media-io --test http_source seekability` | CI |
| Startup burst (VOD) | The Bank (the buffer between demux and decode) releases the first 2 s unpaced at startup and after every seek, then returns to real-time pacing. | `cargo test -p media-bank` | CI |
| Priming join (live) | Live lanes prime the decoder during the startup hold, keep presentation back until the target depth has arrived, then release at real time without pausing to drain what was released early. | `cargo test -p media-bank --test priming` | CI |
| Per-track release | A full decode channel on one track holds back only that track in the Bank; the other keeps releasing, and a live join's early audio is kept. | `cargo test -p media-bank --test gated` | CI |
| Audio-leading start | Every live session starts audible at the first buffered audio with video joining at its keyframe, on-demand sessions start both together, and a stale `"audio_leading"` field is ignored. | `bm-probe bench <rtsp-lane> --live` | By hand |
| Impairment, CI lane | Under the worst recorded network profile (300 ms extra round trip, 0.05% loss) at 3 s depth, the session stays alive, keeps presenting, and stalls no more than the sizing model allows. | `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000` | CI, Windows |
| Impairment, full profiles | Every recorded network profile at full length and several depths: file lanes stay within the sizing model, and live URL lanes stay alive and presenting. | `bm-probe impair <file.ts\|live-url> --profile <name> [--depth-ms N] [--csv out.csv]` | By hand |
| Reconnect/resilience | A dropped live connection reconnects with jittered backoff, keeps its buffer and rejoins mid-GOP, and once attempts run out the session ends (Ended for EOF, Error for I/O loss). | `cargo test -p media-engine --test reconnect`; by hand: play a feeder with `--live` and restart it mid-run | CI; by hand |

### Transports and sources

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Headless playback, file | A local MP4 plays through the full pipeline at 1x with the expected decode and present counts, no pool drops and audio pulled at hardware cadence. | `cargo run -p media-engine --example smoke -- fixtures/h264-aac-640x360-30fps.mp4 5` | By hand |
| Headless playback, TS file | A local MPEG-TS file plays the same way, with the container sniffed and its stream ids (PIDs) read from the programme map table. | `cargo run -p bm-probe -- play fixtures/h264-aac-640x360-30fps.ts --duration 5` | By hand |
| Auto liveness costs one connection | A live URL left on Auto liveness is probed once and the live lane adopts the probe's response as its stream, so a live TS origin sees exactly one connection. | `cargo test -p media-engine --test reconnect` | CI, Windows |
| Headless playback, HTTP | A file served over local HTTP plays through media-io, with range requests and a pinned connection. | serve `fixtures/` locally, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/h264-aac-640x360-30fps.mp4 --duration 5 --allow-local` | By hand |
| Headless playback, HTTP-TS live | A live HTTP TS stream plays as a sequential source with per-read stall detection and the Bank in live lag mode, liveness stated with `--live`. | `python tools/live-ts-server.py <file.ts> <duration_s> <port>`, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/live --live --allow-local --duration 12` | By hand |
| Live-source unit rows | The live source streams sequentially, re-reads from its head cache, reports a stall as a typed error, cancels its connect, re-vets redirects and applies the address gate. | `cargo test -p media-io --test live_source` | CI |
| On-demand source cancellation | Closing a session cancels an on-demand HTTP open or read against a server that has gone quiet within about 200 ms, rather than waiting for the request timeout. | `cargo test -p media-io --test http_source` | CI |
| One connection per sequential open | An origin that answers the opening range probe with a 200 is read from that one response, and a body that goes quiet comes back as a typed read error within the read timeout. | `cargo test -p media-io --test http_source` | CI |
| A chunk outlives an idle consumer | A long pause or a low-bitrate file does not fail a ranged chunk request, which has no total timeout, while a server that never answers still errors within the read timeout. | `cargo test -p media-io --test http_source`; by hand: `bm-probe play https://<test-host>/vod/music_cbr.mp3` | CI; by hand, test server |
| A dropped connection is reopened | A ranged read that fails is retried once with a range request from the byte it had reached, and a dropped connection mid-file reads back byte for byte. | `cargo test -p media-io --test http_source` | CI |
| A redirect met mid-file is followed | Every ranged request walks redirects from the caller's URL, vetting each hop through the address gate, and a 206 must state a `Content-Range` starting at the byte asked for. | `cargo test -p media-io --test http_source` | CI |
| Transport errors name their cause | HTTP and WHEP transport failures report the full error chain, so a refused connection names the operating system's reason rather than a generic send error. | `cargo test -p media-io --test http_source` + `cargo test -p media-whep --test signal` | CI |
| Zero-length reads | An on-demand read into an empty buffer returns at once without touching the network or discarding the chunk it holds. | `cargo test -p media-io --test http_source` | CI |
| Resolve ceiling and open cancellation | DNS lookups run under one time ceiling off the async reactor, WHEP opens cancel promptly on session close, and failed opens release their cancel tokens and server sessions. | `cargo test -p media-io --lib` + `cargo test -p media-whep --test signal` | CI |
| Resource fetch caps | Whole-resource fetches for playlists are capped in bytes on both the HTTP and file paths, serving a resource at exactly the cap and refusing one byte past it. | `cargo test -p media-io --test resource_fetcher` | CI |
| Playlist origin confinement | A playlist fetched over the network can never read local files, whatever spelling its URIs use, while a playlist opened from disk keeps both file and network access. | `cargo test -p media-io --test resource_fetcher` + `cargo test -p media-engine --test hls_origin` | CI |
| Source routing | A session URL is routed by its normalised, case-insensitive scheme; unsupported schemes are refused as config errors, UNC paths are refused unless the address gate is off, and local paths still play. | `cargo test -p media-engine --lib classify_tests` + `cargo test -p media-engine --test routing` | CI |
| Playlist URI scheme | Playlist entries resolve by URL joining and must end up `http` or `https`, so `file:`, `ftp:`, `data:` and drive-letter forms are refused while ordinary relative and cross-host entries work. | `cargo test -p media-hls --test hls` | CI |
| Playlist directory confinement | A playlist on disk can only reach plain relative files beside itself; absolute, `..`, drive-relative, UNC and planted-link paths are refused. | `cargo test -p media-hls --test hls` + `cargo test -p media-io --test resource_fetcher` | CI |
| HLS VOD playback | HLS playlists are sniffed and routed from file or HTTP, with TS segments chained and fMP4 segments timed from their own timestamps, playing to Ended with no pool drops. | `cargo run -p bm-probe -- play fixtures/hls/ts/index.m3u8 --duration 8` and `…/hls/fmp4/index.m3u8` | By hand |
| HLS live | A live playlist puts the Bank in lag mode, joins three segments back, slides its window in real time, plays through discontinuity splices and ends on ENDLIST. | `python tools/live-hls-server.py fixtures/hls/ts <duration_s> <port>`, then `bm-probe play http://127.0.0.1:<port>/live.m3u8 --allow-local --duration 25` | By hand |
| HLS over real HTTPS | HLS VOD lanes play from a real HTTPS origin (ranged segments, 5.1 audio), and a public multi-variant master playlist plays. | `https://<test-host>/vod/hls_imax/index.m3u8` + `…/hls_fmp4/index.m3u8`; `https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8` | By hand, test server |
| RTSP lanes | `rtsp://` tries UDP and falls back to TCP, `rtspt://` stays on TCP; stereo, 5.1, slow-join, publisher restart and forced fallback all play with sender-report A/V alignment. | `rtsp://<test-host>:8090/{imaxstereo,imax51,imaxsilent,imaxslowjoin}` + the same paths via `rtspt://`; public: `rtsp://stream.vrcdn.live/live/vrcdn` | By hand, test server |
| RTP timestamp scaling | Converting RTP timestamps and sender-report anchors to microseconds saturates instead of overflowing, at both signs, while ordinary spans convert unchanged. | `cargo test -p media-rtp --test receiver` | CI |
| Depacketizer drain discipline | The WHEP and RTSP-UDP lanes always drain the depacketizer after a refused packet, so a crafted packet sequence cannot panic the session, and refusals are counted and reported. | `cargo test -p media-whep --lib` + `cargo test -p media-rtsp --lib` | CI |
| AAC access units fragmented by marker bit | Multichannel AAC split across RTP packets is reassembled up to the marker bit, with a size bound and loss discarding the partial unit, and 5.1 audio decodes cleanly. | `cargo test -p media-rtsp --test aac_reassembly`; by hand: `cargo run -p bm-probe --release -- play rtsp://<test-host>:8090/imax51 --duration 20 --audio-out out.f32` | CI; by hand, test server |
| WHEP lanes | `whep://` and `wheps://` sub-second WebRTC playback signals over vetted HTTP, runs ICE/DTLS/SRTP, gate-checks every media address and reconnects on publisher restart, carrying H.264 and Opus. | `cargo test -p media-whep`; by hand: `bm-probe play whep://<test-host>:8091/whepav/whep` | CI; by hand, test server |
| WHEP signalling body cap | The WHEP signalling answer is capped at 256 KiB as it arrives, reading a body at the cap whole and refusing anything larger, including an endless chunked body. | `cargo test -p media-whep --test signal` | CI |
| RIST lanes | `rist://` plays plain and AES-128 streams through librist with the host gate-vetted; a wrong secret fails typed, a build without RIST refuses typed, and callback panics are reported. | `cargo run -p bm-probe --features rist -- play rist://127.0.0.1:11968 --duration 12 --allow-local` (with an ffmpeg RIST sender on loopback); by hand: `bm-probe play rist://<test-host>:5000 --duration 20` | CI, rist feature; by hand, test server |
| RIST on Android | The Android arm64 plugin builds and stages with the RIST transport linked in, inside the size budget and with no new shared-library dependency. | `bash tools/build-librist-android.sh` then `.\tools\stage-android-plugin.ps1` | By hand |
| Split sources | A video-only source plays against a separate audio-only source, with both legs kept within 100 ms, seeks taking both, and unsupported or mismatched pairs refused typed. | `cargo test -p media-engine --test split_source`; `bm-probe play fixtures/split/h264-640x360-30fps-video.mp4 --audio-url fixtures/split/aac-48k-stereo-audio.m4a --duration 9` | CI |
| Audio-only file lanes | Each raw audio container (FLAC, Opus, PCM, MP3, ADTS AAC) plays to a natural End at its stored duration, with the audio thread driving the clock and position. | `cargo run -p bm-probe -- play fixtures/sine-48k-stereo.{flac,mp3,aac,opus} --duration 9` | By hand |
| Codec batch (test server /vod/) | Every file in the test server's codec batch (FLAC, MP3, Opus, VP9, AV1 variants) plays over real HTTPS at 1x with no pool drops. | `bm-probe play https://<test-host>/vod/<file>` for each file in the batch | By hand, test server |

### Shared playback and sync

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Sync soft target | Followers pass the owner's position to the engine, which ignores errors under 150 ms, slews up to 2% beyond that and seeks only past 2 s; live sessions ignore the target. | `cargo test -p media-engine --test session sync_target` | CI |
| Divergence bound | A live lane's `max_divergence_ms` caps how far behind live the Bank may run, including Auto's depth growth, and an explicit depth beyond it fails with a typed error. | Covered by the Bank's config validation rows; by hand: open a live lane with `"max_divergence_ms"` set and check Auto depth stays inside it in the capture CSV | CI; by hand |

### Platforms

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Linux headless lane | The engine builds and runs headless on linux-x64, decoding AV1, FLAC and Opus in software, refusing platform-only codecs with a typed error, and reporting only what it can decode. | `tools/ci.sh` (on a Linux host) | By hand (Linux host) |

### Harness and instrumentation

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Bench lane | Startup-to-first-frame and seek-to-settled times are measured per lane and aggregated over several runs, for checking the engine's timing budgets. | `bm-probe bench <src\|url> [--runs N] [--seek-to-ms N] [--live]` | By hand |
| Capture recorder | The diagnostics timeline is written as a CSV with a stable column contract that analysis tooling can read. | `bm-probe play <src> --csv out.csv` | By hand |
| Engine A/V offset is measurable | The presented video timestamp minus the audio playhead reaches the ABI snapshot, both captures and `bm-probe`, and reads as "unknown" until the current timeline has presented a frame or when no session is open. | `cargo test -p media-engine --lib presentation_arms`. By hand: `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 20 --depth-ms 3000 --csv x.csv`, then count rows whose `av_offset_us` is not `i32::MIN` | CI; by hand |
| Serve-trim total is a capture column | The session's cumulative count of audio frames discarded by the serve trim reaches the capture as `audio_trimmed_frames` and survives a seek without falling. | `cargo test -p media-diag`. By hand: `bm-probe play <live-src> --csv out.csv` and confirm the summary's trimmed figure equals the last row's final column | CI; by hand, test server |
| Capture reaches the file | The capture writer flushes and reports an error when the rows cannot reach the target: a truncated capture fails the run instead of reporting success. | `cargo test -p media-diag`. By hand on Linux: `bm-probe play fixtures/h264-aac-320x180-30s.ts --duration 9 --interval-ms 2000 --csv /dev/full` | CI; by hand (Linux) |
| Impairment run owns its capture | A `bm-probe impair` run whose CSV capture did not land grades as a failure and exits non-zero, while still printing its grade. | `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000 --csv <an existing directory>` | By hand |
| Diagnostic log sink | Every textual engine diagnostic goes through one replaceable sink, defaulting to stderr, and on Windows also reaches `OutputDebugString` where a GUI process has no readable console. | `cargo test -p media-diag`. By hand: `bm-probe play fixtures/does-not-exist.mp4 --duration 3` prints `[basis-media] session error: …` | CI; by hand |
| An event drain is a backlog, not a loss | `bm_session_drain_events` hands back at most the requested number of events and leaves the rest queued for the next call. | `cargo test -p media-diag` | CI |
| Refused events are reported | The count of events the session log refused reaches the ABI snapshot as `events_dropped`, saturating rather than wrapping when it outgrows the field. | `cargo test -p media-ffi --lib` | CI |
| Bank schedule readings are capture columns | The Bank's lag, target lag, reanchor and stall totals reach the engine capture as named columns and `bm-probe play`'s `bank:` line, where the release schedule's decisions can be read. | `cargo test -p media-diag`. By hand: `bm-probe play <live-src> --duration 60 --csv out.csv` | CI; by hand, test server |
| Join surplus is returned, not held | Media that arrives after a live join's schedule is anchored counts as surplus and is decayed back downstream, and decay pauses while the release thread is blocked on a full downstream channel. | `cargo test -p media-bank`. Live: `bm-probe play rtsp://<test-host>:8090/imax51 --duration 90 --csv out.csv` | CI; by hand, test server |
| A live source does not pause | A pause request on a live session is ignored and logged once, and a pause arriving while the session is still opening is dropped rather than carried into it. | `cargo test -p media-engine --test session pause` | CI |
| A pause holds across a seek | Pause and seek compose in either order: the session lands on the seek target, presents that frame, stays paused with its position held, and `play` resumes from there, with or without video. | `cargo test -p media-engine --test session a_` | CI |
| Auto-inferred HTTP live lane runs as live | A live HTTP source reached through automatic liveness detection runs the Bank in its live mode, the same as one declared live. | `cargo test -p media-engine --test reconnect`. Live: `bm-probe play https://<test-host>/live/imax.ts --csv out.csv` | CI; by hand, test server |
| Free text reaches a drain, not only a sink | Every free-text diagnostic line also lands in a process-wide ring a host can drain, which evicts its oldest line when full and counts what it evicted. | `cargo test -p media-diag` | CI |
| The process log crosses the ABI | `bm_drain_log` hands process-log lines to a host with no session handle needed, leaves lines beyond the cap queued, and truncates long details on a UTF-8 character boundary. | `cargo test -p media-ffi --test log_drain` | CI |
| A cut detail says it was cut | A detail truncated to fit its record ends in `…`, placed without overrunning the buffer or splitting a multi-byte character, in both the event and log drains. | `cargo test -p media-ffi --no-fail-fast` | CI |
| Unity end to end | The engine and the managed component play a source in Unity, and the capture is graded against the healthy ranges in [`DIAGNOSTICS.md`](DIAGNOSTICS.md). | `Basis > Tools > Media Player > Run Smoke Test`, or headless: `Unity -batchmode -projectPath <project> -logFile - -executeMethod BasisMediaSmokeTest.RunBatch` | By hand |

## Not yet run

These have a surface but have never been exercised, and are not counted as
coverage.

| Row | What it would check | How to run it |
| --- | --- | --- |
| SEI user data: managed seek and loop | A seek drops the managed SEI user-data queue, and a looping file drops what the previous pass left queued instead of delivering it ahead of the new pass. | a scene script subscribing and logging `ptsUs` against `fixtures/h264-sei-userdata-640x360-30fps.ts` served over HTTP: seek back and confirm the frame indices restart from the landed frame with nothing from the old position in between; loop the file and confirm the same across the wrap |
| A fragmented MP4 hours long | A fragmented MP4 of several hours and gigabytes, served over HTTP with ranges, opens from its segment index and a seek near its far end lands and plays on cleanly. | `ffmpeg -stream_loop -1 -i <clip> -t 7200 -c copy -movflags +frag_keyframe+empty_moov+default_base_moof+global_sidx -frag_duration 2000000 out.mp4`, serve it, then `bm-probe play <url> --duration 30 --seek-to-ms 6600000` |
| RIST on Android | Plain and AES-128 RIST lanes (`rist://<test-host>:5000` and `rist://<test-host>:5001`) connect and play on a Quest client build. | a Quest build playing each lane, with the test server's streams up |
| Android audio geometry refusal | A Matroska file stating an out-of-range audio sample rate or channel count gets a typed refusal: the audio track mutes with the refusal in the log while video plays on. | a Quest build playing a crafted Matroska file; `adb logcat -s basis-media` |
| FFI callback panic fences | The MediaCodec, Vulkan and librist callbacks absorb and record a panic instead of unwinding into foreign code, with ordinary playback unchanged and a mid-render close tearing down without leaking Vulkan objects. | a Quest build playing a fixture, closed mid-playback |
| MediaCodec frame geometry refusal | A failed or non-positive frame-size query from the NDK is refused with a typed error rather than presenting a garbled picture, and ordinary playback is unchanged. | a Quest build playing a fixture |
| Vulkan present-layer lifetimes | Closing the last player tears down without a validation complaint and the next open still presents, a reopened session presents into a fresh `RenderTexture`, and logcat carries no device address. | a Quest build: close the last player, reopen, then `adb logcat -s basis-media` |
| Vulkan device-creation feature guard | The Vulkan hook enables `samplerYcbcrConversion` in Unity's device only where the driver advertises it, so ordinary playback still shows video and the log carries the appended-extension line. | a Quest build playing a fixture, then `adb logcat -s basis-media` |
| Engine diagnostics in logcat | Engine diagnostics appear in logcat under the `basis-media` tag, including `session error:` for an unresolvable URL and the transport line for a playing stream. | a Quest build opening an unresolvable URL and then a live stream, with `adb logcat -s basis-media` |
| Proton / Wine | Under Proton, a ranged VOD and a live HTTP-TS lane classify the same as on Windows, and https connects using Wine's certificate store. | the Windows client under Proton on a Linux box, one ranged VOD and one live HTTP-TS lane |

## Android devices

None of these run in CI. They need a Quest connected over USB with debugging
authorised. After any engine change, rebuild and stage the Android plugin into
the package with `tools\stage-android-plugin.ps1`, build a Quest client of a
scene with a player (`Basis > Tools > Media Player > Test Scene` makes one),
and play the row's source. Read the result from the captures
([`DIAGNOSTICS.md`](DIAGNOSTICS.md)) and from `adb logcat -s basis-media`.

| Row | What it checks | How to run | Runs in |
| --- | --- | --- | --- |
| Android build lane | The whole engine graph compiles and lints clean for `aarch64-linux-android`, with the AV1 software decoder deliberately left out of it. | the gate's `android check (aarch64)` step (skips loudly without an NDK). By hand: `. .\tools\android-env.ps1` then `cargo clippy --target aarch64-linux-android -p media-ffi -p decode-mediacodec -- -D warnings` | CI, Android build |
| Engine .so | `libbasis_media.so` links only against `libmediandk`, `liblog` and the C runtime, and exports the ABI plus `UnityPluginLoad` and `JNI_OnLoad`. | `. .\tools\android-env.ps1; cargo build --target aarch64-linux-android -p media-ffi --release`, then `llvm-readelf -d`/`--dyn-syms` on `target/aarch64-linux-android/release/libbasis_media.so` | By hand |
| Playback on device | MediaCodec hardware decode presents through Vulkan into a Unity `RenderTexture` under OpenXR, with AAC audio, a mid-run seek and a natural end of stream. | a Quest build playing a fixture with a seek | Device (Quest) |
| Managed package on device | The package plays end to end on Android: Vulkan output, audio pulled at the stream rate and resampled to the device rate, and position advancing one second per second with no backwards snaps. | a Quest build playing a fixture; grade the frame capture | Device (Quest) |
| Android https | An `https://` lane connects and plays on device using the bundled root certificates, since Android offers no readable system certificate store. | a Quest build playing `https://<test-host>/vod/music_51.flac` | Device (Quest), test server |
| Diagnostics captures | The engine writes its sampled diagnostics CSV on close, and the frame capture writes one row per Unity frame; between them they show flow and display cadence after a device run. | `cargo test -p media-engine --test session diag_csv_written_on_close`. On device: `adb pull` both captures from the app's files directory after a run | CI; Device (Quest) |
| Steady-lane cadence | On a steady live stereo stream, frames hold for the ideal number of display refreshes through audio-callback jitter, with no frames skipped. | a Quest build playing `rtspt://<test-host>:8090/imaxstereo` with liveness Live; grade frame holds over a window after the join | Device (Quest), test server |
| Live lanes on device | RTSP (stereo and 5.1, over UDP and TCP), WHEP and live HLS lanes each decode, present and play audio on device. | a Quest build playing each lane, with the test server's streams up | Device (Quest), test server |

## Known gaps

What the engine does not do, and what these tests do not cover.

- The ordering that keeps the A/V offset from reading a new timeline against
  the old one after a seek is held by the code rather than by a test: nothing
  can pause the video thread at the point where the race would happen.
- A fragmented MP4 without a segment index is still opened by reading every
  fragment's header first, which is slow on a long file over HTTP. Finding
  fragments on demand from a trailing `mfra`, or as a seek needs them, is not
  built.
- A seek into a fragmented file whose audio and video fragments are cut at
  different points does not replay audio from the fragment before the one the
  picture lands in. Nothing that writes fragmented MP4 in practice cuts them
  apart.
- A hostname lookup cannot be cancelled once the operating system has started
  it; the time ceiling bounds how long a caller waits, not the lookup itself.
- A lookup queued behind the limit on lookups in flight is refused at the same
  ceiling rather than waiting indefinitely.
- The 7.1 fixtures stop at the decoder on the engine side; the speaker outputs
  and the stereo mix-down are covered by [`../TESTING.md`](../TESTING.md).
- A split pair whose two sources disagree about where their timelines start
  by more than the decoder cushion is refused rather than played.
- Raw TS files and streams cannot be seeked; HLS-TS on-demand playlists seek by
  segment.
- HLS has no encrypted playlists, byte-range segments, keyframe-only playlists
  or adaptive switching (the highest-bandwidth variant is chosen once), and
  fetches whole segments of up to 64 MiB.
- Every raw audio format seeks, none of them exactly: MP3 and Ogg Opus
  estimate, FLAC lands on a frame header, ADTS estimates from its byte rate.
- There is no VP9 software decoder, Opus is mono or stereo only, and AV1
  output above 8-bit 4:2:0 is refused.
- RTSP carries H.264 and AAC only, with no credentials and no multicast.
- WHEP carries H.264 and Opus only, does not use the server's STUN or TURN
  entries, and sends no authentication headers. Its signalling fuzz target
  needs `--features whep` and cmake on the fuzz host.
