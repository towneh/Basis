# TESTING — basis-media

The governing verification document for this repo. When a change touches a
row's surface, run that row and say so when claiming "verified". The parity
matrix against the C player (transports × codecs × platforms) stays in the
Basis repo's `TESTING.md` until cutover; this file covers what the new
engine can already do.

## Gate 0 — local CI (every commit)

```
.\tools\ci.ps1        # Windows (ci.sh on Linux/WSL)
```

fmt, clippy `-D warnings` (including `undocumented_unsafe_blocks`), the
full test suite, cargo-deny (licence + advisories), cargo-vet, and the
ffprobe conformance gate over `fixtures/` (skipped loudly if ffprobe is
not on PATH). Everything below assumes this is green.

## Diagnostics surfaces — what to read, and what good looks like

Three surfaces, deliberately not overlapping. Grading a run means reading
numbers off them, not watching it.

| Surface | Written by | Sampled | Covers |
| --- | --- | --- | --- |
| **Engine capture** | the engine, own thread | 100 ms | Everything inside the pipeline: stage counters, bank depth and lag, decode, release, events. Column contract pinned by spec §12.4. Turned on per session: descriptor `diag_csv`, managed `diagnosticsCsvPath`, `bm-probe play --csv`. |
| **Frame capture** | `BasisMediaPlayerDiagnostics`, main thread | one row per Unity frame | The other side of the boundary, which the engine cannot see: render cadence, how long each frame was held, audio pull rate, what the per-speaker outputs consumed and how loud it was, the device's DSP chain. Sandboxed to `persistentDataPath`. |
| **Debug window** | editor, live | 250 ms | The same numbers while it is happening, walked in pipeline order, for when the question is "which stage stopped". `Basis > Media v2 > Debug Window`. |

The two captures share a time base, so they are read side by side: the engine
capture says what the pipeline did, the frame capture says what the viewer got.

### Frame capture column contract

Positional. Additions go on the end, so a reader keyed on position stays valid.
This line must match `BasisMediaPlayerDiagnostics.Header()` exactly — if it does
not, one of the two is wrong and the grading below is measuring the wrong column:

```
unity_time,frame,frame_dt_ms,state,position_us,duration_us,banked_ms,decoded,decoded_delta,presented,presented_delta,frames_held,video_w,video_h,has_texture,audio_pulled,audio_pulled_delta,stream_rate,stream_channels,dsp_rate,dsp_buffer,dsp_buffers,listener_paused,sync_ppm,subtitle_track,caption_len,out_bound,out_playing,out_consumed,out_consumed_delta,out_peak,out_rms,out_latency_us
```

### What each metric means, and its healthy band

| Column | Means | Healthy |
| --- | --- | --- |
| `state` | `BmState`: 0 idle, 1 opening, 2 buffering, 3 playing, 4 paused, 5 ended, 6 error | reaches 3 and stays; 6 never appears; a VOD run ends at 5 |
| `position_us` | the session clock | advances at 1 s per wall second (±2%, the slew cap). **Never goes backwards outside a seek** — a backwards step is a clock snap, and snaps are the thing pacing bugs show up as |
| `banked_ms` | how much is banked ahead | 0 while buffering; at or above the configured depth once playing (Auto lanes settle wherever the estimator lands). Sagging towards 0 during playback is a starving source |
| `decoded_delta` | frames decoded this Unity frame | over a window, sums to the source frame rate |
| `presented_delta` | frames presented this Unity frame | over a window, matches `decoded_delta`. Persistently below it means frames are being dropped after decode |
| `frames_held` | Unity frames the last presented frame stayed on screen (0 on frames where nothing new was presented) | the display-to-content ratio, and *the same value every time*: 24 fps on a 72 Hz headset is 3, 3, 3. The share of holds at the ideal value is the judder metric — **≥99% on a steady lane** was the bar the frame-pacing work was held to. Isolated 2s and 4s are single-vsync jitter; clusters are a regression |
| `frame_dt_ms` | Unity's own frame time | the display interval (13.9 ms at 72 Hz, 16.7 at 60). If this is unstable the hold histogram means nothing — the renderer is the problem, not the player |
| `audio_pulled_delta` | source frames consumed this Unity frame | over a window, **`audio_pulled` must advance at `stream_rate`, whatever `dsp_rate` is** — the pull resamples. A pull rate stuck at some ratio of the stream rate drags the master clock and everything else follows |
| `stream_rate` / `stream_channels` | what the engine announced | non-zero once audio is announced; channels matching the source |
| `dsp_rate`, `dsp_buffer`, `dsp_buffers` | the device's audio chain | context for the above, not a pass/fail. Worth recording because devices pick their own (a Quest picks 24 kHz on its own) |
| `sync_ppm` | the sync ladder's wanted offset from 1x | 0 unless chasing a shared position; non-zero for long stretches means the correction is not converging |
| `has_texture` | the output texture exists | 1 shortly after the first frame; dropping back to 0 mid-playback is a lost texture |
| `listener_paused` | `AudioListener.pause` | 0. If it is 1, audio metrics are meaningless and the clock will have fallen back to wall |
| `subtitle_track`, `caption_len` | selected sidecar track (-1 = in-band), current caption length | context for caption work |
| `out_bound` | AudioSources the audio sink bound at its last build | the number of outputs the rig wires — 1 for a stereo downmix, 8 for the surround arrangement. **0 with audio announced means nothing was listening**: the ring was drained and thrown away, which every other band still reads as healthy |
| `out_playing` | any bound output is playing | 1 once the format is known and the outputs start. 0 while playing is the outputs having stopped under a session that has not |
| `out_consumed_delta` | output frames the primary output mixed this Unity frame | over a window, `out_consumed` advances at **`dsp_rate`**, not `stream_rate` — the taps render into DSP blocks, so this is the device's clock. Below it means blocks were missed, which is audible as break-up |
| `out_peak`, `out_rms` | level of the primary output's last mixed block | non-zero on anything that is not silence. A run where every block is 0 played nothing, however healthy the pull looked |
| `out_latency_us` | the sink's estimate of pull-to-speaker delay | the DSP buffer chain plus a block of headroom. Reported to the engine on Android as the A/V compensation; recorded everywhere so the two platforms can be compared |

### Grading a run mechanically

A pass over the frame capture, which is what an automated run should assert
rather than a human watching:

1. `state` reached 3, never 6, and reached 5 if the source was finite.
2. `position_us` advanced at 1 s/s within ±2%, with **zero** backwards steps
   outside a deliberate seek.
3. `presented` total within a few frames of `decoded` total.
4. `frames_held` at the ideal value for at least the threshold share, measured
   over a window that **excludes the join** — a live join legitimately holds
   frames while the clock settles, and including it hides real regressions
   behind a known one.
5. `audio_pulled` advanced at `stream_rate` within ±1%.
6. `banked_ms` never sagged to 0 while `state` was 3.
7. With audio announced: `out_bound` non-zero, `out_consumed` advanced at
   `dsp_rate` within ±2%, and `out_peak` was non-zero at some point. Steps 1-6
   all pass on a session that makes no sound at all, so this is the step that
   catches it.

The engine capture answers the follow-up in every case: if presented lags
decoded, its drop counters say where; if the position snapped, its event log
says why.

### The smoke test runs that grading for you

`Basis > Media v2 > Run Smoke Test` builds a scene around a player, plays a
source, and grades the capture against every band above. Headless, for CI or a
pre-commit check:

```
Unity -batchmode -projectPath <project> -logFile - \
      -executeMethod Basis.Media.BasisMediaSmokeTest.RunBatch
```

Exit code 0 on a pass and 1 on a fail, with the measured numbers in the log
either way. Environment: `BASIS_SMOKE_URL` (defaults to the engine's A/V
fixture), `BASIS_SMOKE_SECONDS`, `BASIS_SMOKE_LIVE` (a live source is not
expected to end), `BASIS_SMOKE_STRICT_HOLDS`.

Frame holds are reported but **not** enforced by default, and only that band is
treated this way: an editor play session does not present on a stable display
cadence, so the share is informative there and meaningful only from a build or
a device. `BASIS_SMOKE_STRICT_HOLDS=1` promotes it to a failure for the runs
where it means something.

The scene the smoke test builds carries a real stereo output set, not a bare
AudioSource, because the audio bands grade what reached a speaker and there is
nothing to measure without one. A surround rig would leave six of its eight
outputs silent against a stereo fixture by design, so that is a listening pass
rather than a graded one.

The grading itself carries no Unity types, and is exercised against synthesised
captures — a clean run, a clock running fast, a clock that steps backwards,
audio pulled at half rate, presented trailing decoded, an error state, a run
that never ends, a capture written by a different build, an output set that
bound nothing, an output missing half its DSP blocks, outputs that ran but
mixed silence, and a video-only rig that is not expected to have outputs at
all. Each case asserts the verdict *and* the reason given, so the grader cannot
pass a run for the wrong reason.

### Keeping this accurate

These numbers are only worth having if they are true. The header line above is
quoted verbatim so a mismatch is greppable, the healthy bands come from the
measurements recorded further down this file rather than from taste, and a
change to what a column carries is a change to this table in the same commit.

## Rows

| Row | What it proves | How to run |
| --- | --- | --- |
| Unit/property tests | Bank sizing table + properties, clock ladder (including the R10 master-observation filter: the synthesised Quest DSP-callback jitter trace holds due-times within 3 ms filtered vs ~7 ms of episode wander raw, snaps stay on the raw error, the filter resets across generations; default-off, so the raw ladder's rows are untouched), pool leasing, ring/playhead, demux fixtures, gate blocklist, HTTP source (in-process server) | `cargo test --workspace` (in CI) |
| Session lifecycle | pause freezes position, keyframe-clean seek settles, natural end; audio-only sessions play (audio thread starts the clock, owns position) and End only once the ring's tail is consumed; seek after Ended revives the pipeline (B94) on the MP4 and HLS-TS lanes; the R8 ordering row — a seek issued from the EOS drain tail settles without a master snap and plays the remaining tail at 1x (the parked clock never restarts from a stale pre-flush frame, stale-generation work drops instead of parking, and `read_audio` serves silence while the clock is parked) | `cargo test -p media-engine --test session` (in CI) |
| Seek matrix | keyframe-clean landing across every moov layout (faststart / trailing / fragmented); HLS VOD seek-to-segment (TS + fMP4); Matroska cue seeks land keyframe-clean (targets before the first cue fall back to a linear cluster walk); raw TS and live-HLS seeks refuse as typed Unsupported; seek-to-settled measured per lane by `bench` | `cargo test -p media-demux --test mp4_stream` + `--test ts_stream` + `--test hls` (in CI); `bm-probe bench <lane>` |
| GPU conversion pass | the §6.8 D3D11 NV12→BGRA pass agrees with the CPU reference converter (same maths, point-sampled chroma) across every stated matrix/range on synthetic sweeps and on decoded fixture frames through the full shared-texture handoff; the reference itself agrees with the integer maths the CPU path shipped with | `cargo test -p media-present --test gpu_pass` (in CI, Windows) |
| Conformance (ffprobe oracle) | demuxed AU stream = ffprobe's packets: announce, count, pts to 1 µs, per-packet MD5 (raw payloads, keyframes included), keyframe flags. Covers MP4 (three moov layouts) and MPEG-TS/m2ts (Annex-B video byte-exact; ADTS audio byte-exact in raw mode; LPCM announce + flow only — no canonical packetisation) | `cargo run -p bm-probe -- conformance fixtures` (in CI) |
| TS demuxer unit rows | fixture AU/keyframe counts, mid-GOP join waits for an SPS keyframe, 33-bit PTS unwrap, m2ts stride + LPCM announce, pinned C fuzz-crash replay | `cargo test -p media-demux --test ts_stream` (in CI) |
| Matroska demuxer rows | pinned AU/keyframe counts on the remuxed A/V fixture, stored-H.264 → Annex-B with SPS/PPS on keyframes, VP9/Opus WebM announces with frames flowing, CodecDelay subtracted from audio pts (Opus pre-skip as negative lead-in), cue seeks landing keyframe-clean at or before the target (vendored matroska-demuxer, see third_party/matroska-demuxer/PATCHES.md) | `cargo test -p media-demux --test mkv_stream` (in CI) |
| Raw audio demuxer rows | sniffed routing for FLAC/Ogg/MP3/ADTS heads (+ ID3v2 → MP3); pinned AU counts against ffprobe's packet view on the sine fixtures; exact sample-derived pts; the Xing/Info frame consumed as metadata; ADTS header strip + 2-byte ASC reconstruction with the 1..=6 channel screen; Ogg Opus pre-skip as negative pts; seeks refuse as typed Unsupported on all four; the 7.1 lane (B69): 8-channel FLAC demuxes and announces 8 channels, ADTS with channel_configuration 7 refuses typed at open | `cargo test -p media-demux --test raw_audio` (in CI) |
| Software decode adapters | claxon FLAC decodes the demuxed fixture to exactly 6 s of PCM with frame-accurate pts, and the 7.1 fixture to eight-channel PCM (claxon's cap is 8); libopus Opus decodes all packets with pre-skip before the origin; surround Opus (mapping family ≠ 0) and broken headers refuse typed; rav1d decodes the AV1 WebM fixture completely with monotonic pts | `cargo test -p decode-sw` (in CI) |
| MF adapter contracts | H.264 + AAC through the real in-box MFTs (priming pts, output ranking); MP3 through the in-box decoder on the same shared sync driver; VP9/AV1 through the Store-extension MFTs found by probe (rows skip loudly if an extension is absent — that absence is the diagnostic the engine reports) | `cargo test -p decode-mf` (in CI, Windows) |
| Multichannel interleave order | the PCM ring's multichannel interleave is WAV/channel-mask order (FL FR C LFE BL BR) — the order every decoder route emits (MF's PCM convention, the Android AAC decoder's FDK default, FLAC's stored order) and the order the managed stereo downmix keys its ITU-coefficient matrix on: a 5.1 channel-marker fixture (one distinct sine per speaker) plays through the full session and each interleave slot carries its WAV-order tone | `cargo test -p media-engine --test session multichannel_interleave` (in CI, Windows); the fixture is `fixtures/sine-48k-51.m4a` |
| PTS-annotated ring serve (R15) | the media timeline, not the sample count, masters the clock: the ring carries chunk pts markers, the playhead interpolates them, and the serve trims a head running >300 ms late against the session clock in bounded ≤1024-frame steps (narrated via the rate-limited AudioTrim event). Pinned: the ms-quantised passthrough pattern (1024-sample chunks, ~1010 samples of pts) keeps the playhead on the pts timeline; a chronically late head trims bounded and every pushed frame is served or trimmed; a completely full ring on an honest timeline (the VOD startup-burst shape) never trims | `cargo test -p media-engine --lib audio` (in CI); live repro: `bm-probe play rtspt://mr.town:8090/imax51 --duration 75` — master honest (the only snap is the loop-wrap splice), trims ≈ the lane's ~1.6% structural surplus, stereo twin zero trims |
| A/V output-latency compensation (R11) | the managed sink's reported output latency shifts the audio master back so video paces to the *audible* position: the playhead subtracts the stored figure exactly; a mid-play 100 ms report engages the slew rung (never the snap) and the clock settles onto the compensated master within the dead band; the ABI setter clamps to 0..=500 ms; default 0 leaves every existing row byte-identical (the managed side sends it on Android only) | `cargo test -p media-engine --lib audio` + `--test session audio_latency` (in CI); on device: the fixture package pass + the rtspt stereo lane stay green, the sync itself is a listening judgement |
| Render-event frame selection (R12) | presentation due-ness is decided in the Unity render event at display cadence, with one vsync of lookahead against a lock-free clock mirror, so the selection and display quantisers are the same clock: a synthesised 24 fps grid selected by 72 Hz events presents every frame exactly once at the ideal 3-event hold across a full phase sweep, with ±1.5 ms event jitter never producing the late/early pair signature; a parked clock (startup, seeks, pause) selects nothing; the interval EMA tracks cadence and rejects hitches; the 500 ms consumer-liveness window hands selection between the render event and the video thread's tick-paced fallback (headless sessions never see an event, so every headless row runs the unchanged fallback path) | `cargo test -p media-engine --lib present` (in CI); integrated: the Unity host autotest (the render-event mode on Windows); on device: the Steady-lane cadence row's hold-interval histogram |
| Capability contract | the §6.11 engine-declared set: the JSON wire shape pinned byte-exact (versioned contract, field renames are breaking), the built set matching what this build actually routes (software rungs: H.264 + rav1d AV1 constant, VP9 present exactly when the Store-extension probe finds a decoder, every software entry stating the enforced R20 ceiling 1920/1088/60; hardware entries present exactly where the two-leg DXVA probe passes, with the resolution-ladder ceiling; the audio adapters' real channel screens, `rist` transport present iff the feature compiled in, no LPCM entry while no adapter exists), and `bm_capabilities`' size/fill calling convention over the ABI (short buffer untouched, length still returned) | `cargo test -p media-engine --test capabilities` + `cargo test -p media-ffi --test capabilities` (in CI, Windows); by hand: `cargo run -p bm-probe -- caps` prints the blob (`--compact` for the exact ABI bytes; add `--features rist` to see the rist row) |
| MKV/WebM playback | EBML-sniffed routing; H.264+AAC in MKV plays end to end; VP9/Opus WebM plays both tracks; AV1/Opus WebM plays both; H.265+AAC in MKV plays through the hardware route (hvcC parsed, VPS/SPS/PPS converted to Annex-B — HEVC has no software rung, so machines without the DXVA profile refuse it typed) | `cargo run -p bm-probe -- play fixtures/mkv/h264-aac.mkv --duration 8`, `…/mkv/vp9-opus.webm`, `…/mkv/av1-opus.webm`, `…/mkv/h265-aac.mkv`; mr.town `/vod/` codec-batch lanes |
| Windows hardware decode (R19) | the DXVA route: sync MFTs bound to a D3D11 device through the DXGI device manager, decoder-allocated NV12 texture-array slices flowing as opaque frames into the conversion pass (one GPU subresource copy, no CPU touch), presenter sharing the decode device. Conformance: hardware output byte-matches the software route over the visible frame for H.264 (including after a flush/restart), VP9 and AV1-vs-rav1d — the coded pad rows (360→368) are unspecified content and excluded; HEVC (no software oracle) pins count/dims/monotonic-pts. Ported C contracts pinned: sample released on every path (RAII payload), per-iteration `GetOutputStreamInfo`, subresource index honoured (the present-slice array test drives slice 1 before slice 0), aperture re-read at stream change, sizeless-HEVC refusal before the MFT is configured, AV1 config OBUs riding the first accepted AU, post-reset output floor | `cargo test -p decode-mf --test dxva_decode` + `cargo test -p media-present --test gpu_pass present_slice` (in CI; rows skip loudly where the GPU lacks a profile); integrated: every Windows session/smoke row now rides the hardware route where the GPU has one |
| Forced hardware fallback | with `BASIS_MEDIA_DISABLE_HW_DECODE` set, every hardware probe reports absent: the default preference lands on the software rung with a `DecodeFallbackHwToSw` diagnostic and plays to a natural end; `hardware_only` instead refuses typed (CodecRefused — video mutes, audio owns Ended). The same env var is the field escape hatch for broken drivers. The runtime DXGI-backing signal (an output sample without GPU backing = the MFT silently fell back to CPU) reroutes mid-stream through the same ladder | `cargo test -p media-engine --test hw_fallback` (in CI; its own binary — the env var is process-wide) |
| Decode preference | descriptor `decode_preference` (`hardware_with_fallback` default / `hardware_only` / `software_only`) → route ladder; a rung the platform lacks refuses typed (Android has no software rung; headless has no hardware rung); managed `BasisMediaPlayer.DecodePreference` static (never a serialised inspector field) appends it at open. `software_only` is the §11 CPU A/B lever | the hw_fallback rows above cover both non-default rungs; by hand: `bm-probe` lanes stay green either way once bm-probe grows a flag (engine-side the descriptor field is the surface) |
| Software-route cap (R20) | software decode routes accept up to 1080p60 coded pixel rate (1920×1088×60; dimensions-only gate ≤1920×1088 while no demuxer states fps) and refuse above it in the CodecRefused posture before any decoder builds — on the direct `software_only` route and the fallback rung alike; software capability entries state the same ceiling the engine enforces. Tightens on field evidence (a one-constant change in `media-engine/src/route.rs`) | `cargo test -p media-engine --lib route` (in CI: the pixel-rate/dims policy rows + the over-cap refusal through the real Windows ladder) |
| Audio-only file lanes | each raw audio container plays through the full engine to a natural End at its stored duration: FLAC and Opus in-process, MP3 and AAC(ADTS) through MF; audio thread owns the clock and position | `cargo run -p bm-probe -- play fixtures/sine-48k-stereo.{flac,mp3,aac,opus} --duration 9` |
| Codec batch (mr.town /vod/) | the full breadth over real HTTPS: FLAC 16/24-bit, 5.1, embedded-art and variable-blocksize; MP3 CBR/VBR/ID3; Ogg Opus; VP9 WebM ×4 layouts + VP9-in-MP4; AV1 WebM, fragmented MP4 and 1080p60 AV1+AAC (rav1d holds 60 fps) — all play at 1x with zero pool drops | bring `tls443` up, `bm-probe play https://mr.town/vod/<lane>` per the endpoints doc, down after |
| Headless playback, file | full pipeline at 1x: decode/present counts, zero pool drops, audio at hardware cadence | `cargo run -p media-engine --example smoke -- fixtures/h264-aac-640x360-30fps.mp4 5` |
| Headless playback, TS file | the same through the ported TS demuxer (container sniffed, PIDs from the PMT) | `cargo run -p bm-probe -- play fixtures/h264-aac-640x360-30fps.ts --duration 5` |
| Headless playback, HTTP | the same through media-io (ranges, pinned connect) | serve `fixtures/` locally, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/h264-aac-640x360-30fps.mp4 --duration 5 --allow-local` |
| Headless playback, HTTP-TS live | the async I/O domain: sequential streaming source, per-read stall detection, Bank in lag mode (liveness stated via `--live`, never inferred) | `python tools/live-ts-server.py <file.ts> <duration_s> <port>`, then `cargo run -p bm-probe -- play http://127.0.0.1:<port>/live --live --allow-local --duration 12` |
| HLS scheduler unit rows | playlist parse + refusals (EXT-X-KEY, BYTERANGE, I-frame-only), EXT-X-MAP carry-forward, live window advance/refresh cadence (B21), window fall-out jump + discontinuity, stated-splice TS rebuild with trailing-PES flush, master variant choice, live join point, VOD seek-to-segment — all over a virtual fetcher, no sleeps | `cargo test -p media-demux --test hls` (in CI) |
| HLS VOD playback | playlist-sniffed routing (file or HTTP), TS chaining through one demuxer / per-segment fMP4 with absolute tfdt timestamps, full pipeline to Ended with zero pool drops | `cargo run -p bm-probe -- play fixtures/hls/ts/index.m3u8 --duration 8` and `…/hls/fmp4/index.m3u8`; seek via `bench` on the same lanes |
| HLS live | live playlist (no ENDLIST) ⇒ Bank lag mode from the playlist's own statement; join three segments back; window slides in real time; loop-point EXT-X-DISCONTINUITY splices play through; ENDLIST ends the session | `python tools/live-hls-server.py fixtures/hls/ts <duration_s> <port>`, then `bm-probe play http://127.0.0.1:<port>/live.m3u8 --allow-local --duration 25` |
| HLS over real HTTPS | the VOD lanes against a real origin (206 ranges on segments, 5.1 audio) and a public multi-variant master playlist | mr.town `https://mr.town/vod/hls_imax/index.m3u8` + `…/hls_fmp4/index.m3u8` (bring `tls443` up first, down after); `https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8` for the master-playlist path |
| Live-source unit rows | sequential stream through the router, head-cache re-reads, stall as a typed error, cancellable connect, redirect re-vetting, gate | `cargo test -p media-io --test live_source` (in CI) |
| Impairment, CI lane | the worst phase-0 jitter profile (ts-rtt300-loss005) through the full engine at 3 s depth over a 1x-paced fixture; pass = session alive, presentation flowing, measured stall within the sizing model's residual | `bm-probe impair fixtures/h264-aac-320x180-30s.ts --profile ts-rtt300-loss005 --duration 25 --depth-ms 3000` (in CI, bounded to 25 s) |
| Impairment, full profiles | every phase-0 capture full-length, VOD-file or live-URL lane (`--profile` × `--depth-ms` per the sizing table; the 0.5%-loss profile is the throughput regime — informational, not a gate, §3.4). URL lanes gate on survival + flow (the model's instant-recovery assumption doesn't hold over a real WAN); the deterministic file lane gates on the model | `bm-probe impair <file.ts\|live-url> --profile <name> [--depth-ms N] [--csv out.csv]` (release gate, by hand). VRCDN's 24/7 channel `https://stream.vrcdn.live/live/vrcdn.live.ts` is the authoritative live lane |
| Reconnect/resilience | a dropped live connection (and live EOF — indistinguishable from a dropped connection-close body) rebuilds the transport with jittered backoff, keeps the Bank and generation, rejoins mid-GOP, and narrates itself as Reconnect events; exhausted attempts end the session (Ended for EOF, Error for I/O loss) | `cargo test -p media-engine --test reconnect` (in CI); by hand: play a feeder with `--live` and restart it mid-run |
| RTSP lanes | `rtsp://` negotiates UDP first (media-rtp reorder/jitter/RTCP-RR layer under retina's signalling) and falls back to TCP-interleaved when SETUP fails or no datagram arrives within 5 s; `rtspt://` pins TCP (verify no UDP SETUP is attempted). Rows on both transports: stereo, 5.1 (mediamtx fragments its large AAC AUs — reassembly exercised, over reordered UDP delivery too), the adversarial ~10 s-GOP mid-GOP join, publisher-restart reconnect (RTCP BYE ends the session as source loss; failed re-opens consume the attempt budget; rejoin renegotiates UDP), and the forced-blackhole fallback (block the server's outbound RTP/RTCP, e.g. `iptables -I OUTPUT -p udp --sport 8000:8001 -j DROP` on the box, and confirm the TransportFallback event + TCP playback); RTCP-SR-based A/V alignment with a bounded join-skew fallback | mr.town `rtsp://mr.town:8090/{imaxstereo,imax51,imaxsilent,imaxslowjoin}` + the same paths via `rtspt://` (bring `mediamtx` + `mtx-pub-*` up first, down after; restart a publisher mid-run for the reconnect row); authoritative: `rtsp://stream.vrcdn.live/live/vrcdn` |
| CEA-608 caption lane | in-band captions (ATSC A/53 SEI in the H.264 AUs) decode through the media-bitstream SEI walker + 608 field-1 (CC1) state machine on the demux thread and surface as cues on arrival with their due PTS (managed side displays at position): the authored fixture's scripted sequence — pop-on, special + extended characters, EDM clear, two-row roll-up with CR — arrives complete with 2 s video-timeline spacing; unit rows cover the decoder's modes, doubling dedup, channel-2 rejection, the backwards-PTS epoch reset (clear cue emitted) and hostile SEI length prefixes; seeks reset the decoder and clear the display | `cargo test -p media-bitstream` + `cargo test -p media-engine --test session caption` (in CI); by hand: `bm-probe play fixtures/h264-608-640x360-30fps.ts --duration 10` prints the cue timeline; regenerate the fixture with `python tools/gen-caption-fixture.py` (gated on ffmpeg's own subcc decoder as oracle) |
| WHEP lanes | `whep://` / `wheps://` (§6.13, the sub-second lane): hand-rolled signalling over gate-vetted pinned HTTP — both the 201+answer and 406+counter-offer/PATCH flows, ICE gathered fully before the POST (host candidate rides in the offer, so PATCH-refusing servers work; the direct flow never PATCHes), `Link rel="ice-server"` parsed and surfaced (not used for gathering — a check-initiating receive-only client needs no srflx and TURN is out of scope), DELETE fired on teardown; str0m (sans-IO, RTP mode, wincrypto on Windows) runs ICE/DTLS/SRTP/RTCP on our socket; decrypted RTP goes through media-rtp's reorder, retina's H.264 depacketizer / RFC 7587 Opus, and the RTSP lanes' shared aligner/emit path (SR-based A/V alignment included); every media-path address str0m wants to reach is gate-checked at the transmit boundary (§9.3 — a blocked candidate's connectivity check is never sent); Bank rides at the decoder-cushion floor (§6.14 shallow posture, explicit depth still wins); publisher restart ⇒ feed stall ⇒ full re-signalling via the engine reconnect path. Codecs: H.264 + Opus only, offered as such (WebRTC carries neither B-frames nor AAC — a B-framed publisher's video track is closed by mediamtx server-side) | `cargo test -p media-whep` (in CI: signalling flows, PATCH discipline, redirect re-vet, Link parse, blocked-candidate row, feed stall); by hand: local mediamtx + `ffmpeg … -bf 0 … -c:a libopus -f rtsp rtsp://127.0.0.1:8554/test`, then `bm-probe play whep://127.0.0.1:8889/test/whep --allow-local`; mr.town: bring `mediamtx` + the transient `whepav` publisher up per the endpoints doc, `bm-probe play whep://mr.town:8091/whepav/whep`, down after |
| RIST lanes | `rist://` (Main profile, caller) through librist behind the `rist` cargo feature: librist owns sockets/ARQ/jitter/PSK-AES and serves recovered TS into the ordinary live-TS pipeline; the host is resolved + gate-vetted and librist pinned to the vetted literal; plain and AES-128 (`?secret=…&aes-type=128`) both play 5.1 at 16 Mbps with zero pool drops; a wrong secret fails typed at the container sniff (librist delivers undecryptable bytes, nothing plays); a `rist`-less build refuses `rist://` with a typed "not built" error. Requires the staged librist static (`tools/build-librist.ps1`, pinned v0.2.11) — CI's rist rows skip loudly when it is absent | local loopback: `ffmpeg -re -stream_loop -1 -i fixtures/h264-aac-640x360-30fps.ts -c copy -f mpegts "rist://@127.0.0.1:11968"`, then `cargo run -p bm-probe --features rist -- play rist://127.0.0.1:11968 --duration 12 --allow-local` (AES: add `?secret=…&aes-type=128` on both sides); mr.town: bring `rist5000`/`rist5001` up (not in the preflight — verify with `ss -unlp` on the box), `bm-probe play rist://88.208.227.151:5000 --duration 20` + `…:5001?secret=<key>&aes-type=128`, down after |
| Bench lane | the §11 budgets measured mechanically: startup-to-first-frame and seek-to-settled per lane, aggregated over runs (with the startup burst: ~133 ms TTFF / ~56 ms seek on the A/V MP4 fixture, vs ~880/~890 ms without) | `bm-probe bench <src\|url> [--runs N] [--seek-to-ms N] [--live]` |
| Startup burst (VOD) | the Bank releases the leading `startup_burst` window (default 2 s) unpaced at every anchor — startup and post-seek — then returns to the 1x + lead schedule; steady-state pacing properties hold with the burst excluded | `cargo test -p media-bank` (in CI) |
| Priming join (live) | live lanes overlap decoder priming with the startup hold: release runs ahead of 1x during the hold (bounded by `startup_burst` beyond the 1x line from the first arrival — a moving cap, so release can never wedge), presentation stays gated until the hold target has arrived (explicit depths hold to the full configured depth, cushion included, so the join delivers the depth the user asked for; Auto lanes hold to the estimator's lag only — join fast, grow on evidence; target-zero lanes lift immediately, preserving the §6.14 sub-second posture), and the 1x schedule anchors presentation-relative when the first frame reaches the viewer — the decoder's standing in-flight depth becomes the cushion and the lag lands on target; the debt bound and decay run unchanged after the anchor; `startup_burst: 0` restores the strict hold-then-1x startup | `cargo test -p media-bank --test priming` (in CI); integrated: the impairment rows below |
| Per-track release | the release thread never blocks on a full decode channel: the target's messages park in order, the whole track gates in the Bank (`pop_due_gated` — Eos a barrier, cursor advances only in-order so banked/lag grade from the laggard, `pop_due` = the empty gate byte-for-byte), and the other track keeps releasing past it. Audio-leading live joins (RTSP: audio flows from SETUP, video waits ≤ ~10 s for an IDR) bank the audio upstream instead of wedging release; the audio thread sheds exactly the pre-join span once the presentation origin is known (one `AudioShed` diag event reports it), keeps everything at/after the join even against a full ring (a deep explicit hold's primed audio survives: the CI impair row's `audio_ring_drops` is 0), and stands the shed down after the first ring write so HLS wrap splices never re-shed | `cargo test -p media-bank --test gated` (in CI); integrated: the CI impair row's `audio_ring_drops` column + bench (not play — joins must be measured, GOP-phase luck hides wedges) on the mediamtx rtsp/rtspt stereo + slowjoin lanes |
| Audio-leading start (R1) | opt-in per source (`audioLeadingStart` → descriptor `"audio_leading"` → `OpenRequest`), live lanes only, default off: the session starts audible at the first banked PCM (the audio-only clock-start path), video appears at its keyframe against the running clock; the pre-join shed is disabled (the first banked audio is the join); the picture can trail the sound by the video decoder's input depth — the documented trade for sources where the audio is the content | `bm-probe bench <rtsp-lane> --live --audio-lead` (startup then measures time-to-first-audio; ~2.07 s on the mediamtx slowjoin lane vs ~9 s video-gated, 2026-08-14) |
| Sync soft target (§8.4) | shared-playback receivers feed the owner's extrapolated position via `bm_session_set_sync_target` (negative clears) and the engine runs the ladder — dead band 150 ms (no action), then a ±2% slew, then a seek only past 2 s (the last rung, never the first); the wanted slew surfaces as the snapshot's `sync_rate_ppm`, which the managed audio pull applies through its resampler on audio-master lanes (a consumer that ignores it degrades to the seek rung); wall-master (video-only) lanes are slewed engine-side (`MediaClock::slew_wall`); live sessions ignore targets (§8.5); the target extrapolates at 1x between calls, so one call per received heartbeat converges | `cargo test -p media-clock` (slew_wall rows) + `cargo test -p media-engine --test session sync_target` + the `sync.rs` ladder units (in CI); `SyncSlew`/`SyncSeek` diag events carry the error and applied rate |
| Divergence bound (§8.5) | live lanes state `max_divergence_ms` in the descriptor (managed `maxDivergenceMs`); it lands as a ceiling on the Bank's lag cap, which also clamps Auto's depth growth — an explicit depth beyond it fails typed through the Bank's own validation; live position is never chased peer-to-peer | covered by the Bank's config validation rows; by hand: open a live lane with `"max_divergence_ms"` set and confirm Auto depth stays inside it in the capture CSV |
| Linux headless lane | the engine builds and runs headless on `linux-x64` (B7's CI half): software floors route (AV1 on rav1d without its non-PIC asm, FLAC, Opus), platform codecs refuse typed (H.264/H.265/AAC/MP3 — no VAAPI adapter yet), the sink consumes due frames (position/EOS/counters flow with no present target), and the capability blob claims only what the build decodes; the full test suite, conformance gate and the software-decode play row all pass on the GPU-less VPS | on a Linux host: `cargo fmt --check && cargo clippy --workspace --all-targets --examples -- -D warnings && cargo test --workspace`, then `bm-probe caps --compact` (expect the linux-x64 shape), `bm-probe play fixtures/mkv/av1-opus.webm --duration 8`, `bm-probe conformance fixtures` (needs ffprobe); `tools/ci.sh` runs the lot and skips the impair row loudly where H.264 decode is absent (debug-build rav1d without asm decodes below real time — the play row grades pipeline health, not rate) |
| Split sources | `audio_url` alongside `url`: a video-only source played against a separate audio-only one, which is how adaptive ladders serve every rung above their muxed fallback. Two demux threads feed the one Bank, so the buffering model, the clock and the release schedule stay the session's. The audio leg's track ids are namespaced (both demuxers number from zero and the Bank routes on the id alone), each leg contributes only its own kind of track, the video leg owns seek and the audio leg follows it to the same landing on the same generation, and the session's Eos is banked only once both legs are exhausted — re-offered on every idle tick rather than decided on one edge, so the handshake cannot be lost. Legs are held within `SPLIT_LEAD_CAP_US` (100 ms) of each other by dts: the Bank releases in arrival order on a dts schedule, so a leg that reads ahead puts not-yet-due events at the head and, if it outruns the Bank's read-ahead depth, fills it and locks the other leg out entirely — a deadlock, since the clock waits on the video leg. On-demand HTTP(S) and files only; a split request on a live transport, an HLS playlist or a caller-supplied source refuses typed | `cargo test -p media-engine --test split_source` (in CI: both legs to a natural end, seek taking both legs, per-leg track filtering with the same muxed file on both legs, the live-transport refusal). Integrated, and a CI row: `bm-probe play fixtures/split/h264-640x360-30fps-video.mp4 --audio-url fixtures/split/aac-48k-stereo-audio.m4a --duration 9` — expect 180/180 presented, 0 pool drops, ~287.7k audio frames, natural Ended. **Soak it**: this lane's failure mode is a timing-dependent wedge that instrumentation hides, so judge changes here by ~20 repeat runs, not one |
| Capture recorder | the diagnostics timeline as CSV (stable column contract) | `bm-probe play <src> --csv out.csv`; feed to the basis-buffer-analysis tooling |
| Headless audio lane | decoded PCM as raw interleaved f32 | `bm-probe play <src> --audio-out out.f32`; inspect with `ffplay -f f32le -ar 48000 -ch_layout stereo out.f32` |
| Unity host autotest | ABI v2 + managed component v2 end to end: presents via render event, audio on the Unity audio thread, mid-run seek | see "Unity host autotest" below |
| Fuzz | demuxers never panic/overread on hostile bytes; the HLS playlist/scheduler surface never panics on hostile playlists; the caption SEI walker + 608 decoder never panic on hostile AU bytes | `cargo +nightly fuzz run mp4_stream` / `ts_stream` / `hls_playlist` / `mkv_stream` / `flac_stream` / `mp3_stream` / `adts_stream` / `ogg_stream` / `caption_scan` (not on the MSVC host: the VPS fuzz lane — ship via `git archive HEAD \| ssh … tar -x`, nightly + cargo-fuzz installed there — or a local Linux Docker container). `fuzz/corpus/ts_stream/` seeds include the C player's four pinned crash inputs; replay with `-- -runs=0`, campaign with `-- -fork=8 -max_total_time=600` |

## Bench baseline (2026-08-14)

The §11 budget comparison against the C player measures from these
numbers (medians of 3 via `bm-probe bench`, Windows dev host; raw
per-run spread from the same sweep is reproducible per the Bench lane
row above):

| Lane | TTFF | Seek-to-settled |
| --- | --- | --- |
| MP4 faststart (file) | 134 ms | 30 ms |
| MP4 trailing moov (file) | 135 ms | 28 ms |
| MP4 fragmented (file) | 133 ms | 27 ms |
| MPEG-TS (file) | 131 ms | refuses by design |
| MKV (file) | 130 ms | 35 ms |
| HLS-TS VOD (local files) | 124 ms | 23 ms |
| HLS-fMP4 VOD (local files) | 134 ms | 28 ms |
| HLS live (local windowed server) | 195 ms (was 1.15 s pre-priming) | — |
| MP4 ranged (mr.town HTTPS) | 327 ms | 35 ms |
| HLS-TS VOD (mr.town HTTPS) | 350 ms | 19 ms |
| HLS-fMP4 VOD (mr.town HTTPS) | 308 ms | 15 ms |
| RTSP mediamtx stereo (live) | 4.39 s | — |
| RTSP mediamtx slowjoin (adversarial ~10 s GOP) | 8.08 s | — |
| RTSP VRCDN (live) | 2.02 s | — |

RTSP-over-UDP joins (2026-08-14, same method — `rtsp://` negotiated
onto UDP; same-day `rtspt://` comparisons in brackets):

| Lane | TTFF |
| --- | --- |
| RTSP-UDP mediamtx stereo (live) | 4.00 s [TCP same day: 4.06 s] |
| RTSP-UDP mediamtx 5.1 (live) | 6.25 s |
| RTSP-UDP mediamtx slowjoin (adversarial ~10 s GOP) | 7.64 s |
| RTSP-UDP VRCDN (live) | 3.96 s [TCP same day: degraded/variable — one 14 s join, one 30 s timeout; the 2.02 s baseline row was a healthier day] |
| HTTP-TS VRCDN (live) | 1.97 s | — |

RIST joins (2026-08-14, same method — mr.town broadcasters, 16 Mbps
x264 + AAC 5.1, 2 s GOP, WAN):

| Lane | TTFF |
| --- | --- |
| RIST plain (mr.town :5000, live) | 1.19 s |
| RIST AES-128 (mr.town :5001, live) | 1.23 s |

WHEP joins (2026-08-14, same method — mediamtx `whepav` path, H.264
`-bf 0` + Opus stereo, 2 s GOP; join = signalling + ICE/DTLS + keyframe
wait, so the GOP dominates the spread):

| Lane | TTFF |
| --- | --- |
| WHEP (mr.town :8091, WAN) | 2.57 s median / 1.06 s min / 3.02 s max |
| WHEP (local mediamtx) | ~1.0–2.2 s (a many-interface host offers many ICE candidate pairs and nomination occasionally settles on a dead one, stalling that session; the reconnect path recovers. Single-candidate WAN topologies did not show it) |

Decode-breadth lanes (2026-08-14, same method; audio-only TTFF is
time-to-first-audible-audio):

| Lane | TTFF | Seek-to-settled |
| --- | --- | --- |
| VP9+Opus WebM (file) | 126 ms | 33 ms |
| AV1+Opus WebM (file, rav1d) | 123 ms | 673 ms (decoder refills its frame pipeline) |
| FLAC / MP3 / ADTS / Ogg Opus (file, audio-only) | 22–29 ms | refuse by design (no seek tables built) |
| VP9 WebM 1080p (mr.town HTTPS) | 307 ms | refuses (MKV) |
| AV1 WebM 1080p (mr.town HTTPS, rav1d) | 399 ms | refuses (MKV) |
| AV1+AAC MP4 1080p60 (mr.town HTTPS, rav1d) | 328 ms | 36 ms |
| AV1 fragmented MP4 (mr.town HTTPS) | 1.5 s | 37 ms |
| FLAC / MP3 / Ogg Opus (mr.town HTTPS, audio-only) | 148–162 ms | refuse |

Reading: the WebM/MP4 codec lanes match the container-independent
~130 ms local / ~300–400 ms remote pattern. The fragmented-MP4 outlier
is open cost (the box walk visits every moof, spread across the file —
each a cache-block fetch over HTTPS). tos_vp9.mp4 seeks settle
inconsistently (its muxing marks samples sync that are not — the landed
"keyframe" can sit mid-GOP); MKV seek stays refused pending the
matroska-demuxer fix or our own cue walker.

Reading: VOD TTFF is container-independent (~130 ms locally; network
lanes add ~one fetch round trip), seeks land in 15–35 ms everywhere
they are supported, and the two VRCDN transports join within 50 ms of
each other. The mediamtx joins carry their lanes' GOP structure (2 s
and ~10 s respectively) plus the live hold — the adversarial lane's
join cost passes through undamped, as it is designed to.

## R19 hardware-route measurements (2026-08-15, desk box, NVIDIA)

Release `bm-probe bench` on the A/V MP4 fixture, 5 runs: **hardware
route TTFF median 154 ms (min 53, max 244 — device + DXVA MFT setup
varies run to run), seek 15 ms; software route (via
`BASIS_MEDIA_DISABLE_HW_DECODE`) TTFF 126 ms, seek 5 ms.** The ~30 ms
median TTFF cost is the decode-device/manager creation; the payoff is
the CPU row below.

Per-stream CPU A/B (1080p30 H.264+AAC 20 s generated lane,
`bm-probe play --duration 20`, process CPU time): **hardware 0.75 s
(≈3.7% of one core), software 5.45 s (≈27%)** — a ~7× reduction; both
routes ~597/594 decoded/presented, 0 pool drops. The remaining §11
Windows rows (vs the C player on the same lane, VRAM per session with
the decoder pool now GPU-side) still need the C-player A/B pass.

## Live-join baseline after the priming join (2026-08-14)

Same-day before/after, medians of 3 via `bm-probe bench --live`
(before = pre-priming build, same host, lanes up for both passes):

| Lane | Before | After | Reading |
| --- | --- | --- | --- |
| HLS live (local windowed server) | 1154 ms | 195 ms | backlog-burst lanes are where priming wins: the 3-segment join primes the decoder at line rate instead of 1x |
| RTSP-UDP mediamtx stereo | 3725 ms | 2689 ms | the SR-aligner's ~2 s join burst now primes unmetered |
| rtspt mediamtx stereo | 3946 ms | 2082 ms | same mechanism |
| RTSP-UDP mediamtx slowjoin | 5652 ms (4.4–9.3 s) | 8648 ms (8.2–10.5 s) | undamped by design: the join cost is the lane's content-dependent IDR spacing (~10 s GOP with scene cuts), and both spreads sit inside it |
| rtspt mediamtx slowjoin | 10330 ms (8.8–10.5 s) | 5289 ms (2.1–5.7 s) | same caveat — IDR-phase luck dominates run-to-run |
| WHEP (mr.town, WAN) | 2648 ms | 1067 ms | GOP-dominated both sides; audio-first runs measure ~1.07 s on both builds; one 11.7 s ICE-retry outlier seen after |
| RIST plain / AES-128 | 1170 / 1236 ms | 1161 / 1186 ms | parity — arrival-bound 1x lanes have nothing to prime ahead of |
| VRCDN http-ts | 410 ms | 471 ms | parity (day noise; the 1.97 s baseline row was a slower day) |
| VRCDN rtsp | 3688 ms | 1977 ms | aligner-burst priming, as the mediamtx lanes |

Priming cannot beat arrival physics: a 1x push lane with no server-side
backlog joins no faster than its keyframe wait plus the decoder's
first-output input depth at 1x. The wins come from lanes where a join
burst exists (HLS segments, the RTSP SR-aligner's flush) that the old
1x-metered startup released in real time.

The per-track release + pre-join shed re-verified this table (2026-08-14,
same lanes, medians of 3): HLS live local 143 ms, rtspt stereo 2094 ms,
RTSP-UDP stereo 3172 ms (2.3–5.6 s spread), rtspt slowjoin 2948 ms and
UDP slowjoin 10531 ms (both inside the lane's ~10 s IDR spacing), WHEP
1717 ms (GOP-dominated), RIST plain/AES 1224/1278 ms, VRCDN http-ts
245 ms. VRCDN rtsp was degraded that day on both builds (the pre-change
build measured 11.2 s median on the same lane in the same hour; the new
build's healthy runs sat at 1.9–2.3 s over TCP) — no regression
attributable to the change. Publisher-restart rejoined on attempt 3
(924/924 presented, 0 drops); CI impair row + all full-length phase-0
profiles passed with margins unchanged and `audio_ring_drops` 0 on the
CI row (41 before the shed rework).

## Android (M5, in progress)

The MediaCodec adapter + Vulkan present path. Rows join here as they
become real; the parity matrix on Quest is the M5 artefact.

| Row | What it proves | How to run |
| --- | --- | --- |
| Android build lane | the whole engine graph (media-ffi cdylib, decode-mediacodec, the Vulkan present module, decode-sw's Opus/FLAC floors cross-compiled via cmake+NDK) compiles clean for `aarch64-linux-android` with clippy `-D warnings`; the AV1 software floor is deliberately absent from this graph (rav1d's published crate cannot build its arm64 asm, and the present path has no CPU-frame upload) | in CI (`android check (aarch64)` row; skips loudly without an NDK). By hand: `. .\tools\android-env.ps1` then `cargo clippy --target aarch64-linux-android -p media-ffi -p decode-mediacodec -- -D warnings` |
| Engine .so | `libbasis_media.so` links against exactly `libmediandk`/`liblog` (+ libc/m/dl) and exports the ABI plus `UnityPluginLoad`/`JNI_OnLoad` | `. .\tools\android-env.ps1; cargo build --target aarch64-linux-android -p media-ffi --release`, then `llvm-readelf -d`/`--dyn-syms` on `target/aarch64-linux-android/release/libbasis_media.so` (the M0 DT_NEEDED lesson) |
| On-device pass (Quest) | the M5 vertical slice on hardware: MediaCodec async decode (hardware AVC) → AImageReader → AHardwareBuffer → Vulkan import on Unity's device → compute convert into a Unity RenderTexture under OpenXR; audio via MediaCodec AAC through the ring; capability blob with hardware routes + MediaCodecList ceilings; mid-run seek (flush/restart in async mode); natural Ended | stage the `.so` into the package (`tools\stage-android-plugin.ps1`; the spike host loads it from there), then `UnityScratch\BasisMediaM0Quest\run-media-pass.ps1` — builds `bmmedia.apk`, deploys via adb, soaks ~25 s, prints the `BM_MEDIA_VERDICT` line (decode/present/ended/audio each PASS). **PASSED 2026-08-14 on Quest Pro** (repeat runs: ~300 decoded / ~275 presented, 0 errors, Playing at ~45 ms, seek + natural Ended; picture verified correct-side-up in stereo under URP; video decoder resolves to OMX.qcom.video.decoder.avc, audio to c2.android.aac.decoder). Known open item, root-caused: the harness's audio consumer pulls at ~half the hardware cadence, dragging the audio-master clock (~0.7 s snap-backs every ~1.5 s — play/freeze cycling); with the pull muted (`bm-no-audio` marker in the app's files dir, no rebuild needed) the engine paces exactly 1 s/s with zero snaps, so the engine is exonerated — the fix is the managed package's Android audio path (the row below). The OMX decoder also never delivers an EOS-flagged output (the bounded drain-timeout fallback ends the stream) |
| Managed package pass (Quest) | `com.basis.mediaplayer`'s Android arm end to end: the component owns the Vulkan graphics contract (linear RGBA32 RenderTexture with `enableRandomWrite`, render events via `RenderPipelineManager.endCameraRendering` — URP ignores camera command buffers) and the DSP-rate-aware audio pull (the ring is consumed at the *stream* rate and linearly resampled to the device DSP rate; a raw 1:1 pull consumes at dspRate/streamRate of real time and drags the audio-master clock — the raw harness's half-cadence class). The verdict grades decode/present/ended plus **audio** (pulled frames per wall second ≈ stream rate) and **pacing** (position advances 1 s/s with zero backwards snaps after the seek settles, sampled at 4 Hz) | `UnityScratch\BasisMediaM0Quest\run-package-pass.ps1` — the host project references the package as a `file:` dependency and the `.so` ships inside the package (`tools\stage-android-plugin.ps1` after any repo change); builds `bmpackage.apk`, deploys via adb, soaks ~35 s, prints `BM_PKG_VERDICT decode/present/ended/audio/pacing`. A/B without rebuild: `bm-no-audio` marker in the app's files dir removes the AudioSource (wall clock master); `bm-url.txt` (line 1 = URL, later lines `live`/`audio-lead`) drives any lane — URL runs skip the seek and the Ended gate; `bm-hold` delays the open 15 s (a no-session `dumpsys meminfo` baseline window for the per-session A/B); `bm-two` opens a second session on the same source beside the first (per-second `s2` lines + a `BM_PKG_S2` counter line in the log). Unattended runs (no wearer): `adb shell am broadcast -a com.oculus.vrpowermanager.prox_close` fakes the proximity sensor so the app renders headset-on-desk (`…automation_disable` restores it) — note the eleventh session's ideal-hold percentages were measured worn; desk runs grade ~90–95% ideal with zero skips, so compare like with like. **Steady state PASSED 2026-08-15 on Quest Pro, in-headset confirmed** (smooth playback, continuous tone; no-seek run: position exactly 1 s/s, 175/175 presented, audio pulled at the stream rate to the content-bound total, natural Ended, zero snaps; boot log confirmed the device DSP rate is **24000 Hz** — the raw harness's half-cadence consumption was exactly dspRate/streamRate). **The full verdict including the seek is green since the R8 fix (2026-08-15, same day)**: the seek settles with a 28 ms presentation gap (was ~5 s — the OMX drain blocking the flush, a stale-frame clock restart, and the ring free-running through the settle), lands at the keyframe, 1 s/s after, natural Ended with the tail intact. The audio-led slowjoin lane still fails the pacing gate on its single join-window snap — the documented audio-led join shape (R5/R1 trade), not a defect; the gate is not join-aware on URL runs |
| Android https (R16) | TLS trust anchors on Android come from the bundled webpki-roots CCADB set (no readable OS CA store there — the first device https attempt failed at connect with error 103; every other platform keeps the OS store): an `https://` lane connects and plays on device | **PENDING first device run** (code landed 2026-08-15): rebuild + `tools\stage-android-plugin.ps1`, `bm-url.txt` = `https://mr.town/vod/music_51.flac` (bring `tls443` up), `run-package-pass.ps1 -SkipBuild` |
| Diagnostics captures | two layers, both CSV: (1) engine-side — `diag_csv` in the descriptor (managed `diagnosticsCsvPath`) makes an engine-owned thread sample the §12.4 capture recorder at 100 ms and write it on close, same pinned column contract as `bm-probe play --csv` (`cargo test -p media-engine --test session diag_csv_written_on_close` pins the surface); (2) display-side — the Quest package harness writes `bm-frames.csv`, one row per Unity frame (wall_dt, state, position, banked, decoded/presented, audio pulled — the C player's judder-relevant columns). The engine CSV sees flow/starvation; only the frame CSV sees display cadence (hold intervals) — the 2026-08-15 judder analysis needed both | on device: run any package pass, then `adb pull .../files/bm-diag.csv` + `bm-frames.csv`; scratch analysis scripts from the session live outside the repo |
| Steady-lane cadence (Quest, R10 + R12) | the master-observation filter holds presentation on the ideal 3-vsync hold through Quest's DSP-callback jitter episodes: post-join windows on the rtspt stereo lane grade 97.4–100% ideal holds with **zero newest-wins skips** across repeat soaks (pre-fix: ~74% with ~2 skips/s during episodes). One caveat when reading the histogram: the mediamtx stereo lane's re-encoder drops the odd frame (~one 83 ms pts gap per 20 s, ffprobe-verified) which displays as a correct 6-vsync hold. The residual 1–5 isolated one-vsync-late/early hold pairs per ~25 s (presentation-handoff quantisation) are R12's target: frame selection now runs in the render event itself with a vsync of lookahead, so those pair events should collapse to isolated occurrences minutes apart — **the on-device re-grade of this row on an R12 build is pending** (headless + autotest verified; the histogram is the device verdict) | bring the stereo lane up, `bm-url.txt` = `rtspt://mr.town:8090/imaxstereo` + `live`, `run-package-pass.ps1` (full build — the staged `.so` must be the R12 build), pull `bm-frames.csv`, grade hold intervals over a post-join window (scratch script; rewrite from the column contract) |
| §11 budgets on device (first measurements, 2026-08-15, Quest Pro, 640×360 A/V fixture) | startup → first frame: decode and present both inside the first 100 ms sampler bucket of the engine capture (budget: ≤ C player −20%; the C-on-Quest baseline still needs its own run). Seek → settled: **28 ms presentation gap** (last pre-seek present to first post-seek present in the frame capture; was ~5 s pre-R8, and matches Windows' ~26 ms). Binary size: **inside the reset budget** — the §11 row is ≤12 MB per platform (reset from 8 MB, R13, 2026-08-15): `libbasis_media.so` 9.2 MB (was 15.0; fat LTO + codegen-units 1 + staging strip-all) / `basis_media.dll` 10.7 MB (was 11.8; MSVC keeps debuginfo in the .pdb, so the dll is essentially all code). The remainder is the feature set's weight — the `cargo bloat` audit shows a long tail (std 1.5 MiB, rav1d 0.7 MiB, the WHEP stack ≈1.2 MiB including its unused SCTP, rustls 0.4 MiB), with the str0m SCTP feature-gate upstream PR as the named first move if size ever binds. Per-session memory (2026-08-15, twelfth session, `bm-hold` no-session baseline A/B): app no-session baseline ~310 MB PSS / EGL ~27 MB; one 640×360 session ≈ +21 MB PSS; one 1080p30 session ≈ +70–77 MB PSS with **EGL +53 MB** (RGBA RT + the 10-image AImageReader pool — inside the ≤80 MiB decoded-side row, first crude measurement); two 1080p30 sessions EGL +100 MB (≈50 MB/session, linear). Two simultaneous sessions (`bm-two`): **both full rate at 1080p30** — session 1 all gates PASS (900/896 @30 s, ratio 1.000, audio at rate), session 2 900/895 at 1 s/s with full audio; app CPU 37% of one core for one 1080p session, 44% for two (hardware decode keeps it flat). Unmeasured rows (per-stream CPU vs the C player, main/render-thread cost, C-on-Quest baselines) need C-player A/B runs | fixture package pass + mid-soak `adb shell top` / `dumpsys meminfo`; TTFF/seek from `bm-diag.csv` / `bm-frames.csv`; the 1080p lane is a generated fixture (ffmpeg testsrc2 1080p30 H.264 high + AAC, 60 s, timecode burn-in) pushed to the app files dir as `bm-1080p.mp4` and driven via `bm-url.txt` |
| Live lanes on device (Quest) | the M5 live matrix over the package pass's `bm-url.txt` lane driver — **all run 2026-08-15 on Quest Pro, decode/present/audio green on every lane**: RTSP stereo (`rtsp://mr.town:8090/imaxstereo`, 655/651, ratio 1.000, in-headset smooth) · RTSP 5.1 (`…/imax51` UDP + `rtspt://` TCP, audio at rate through the managed stereo downmix — the downmix itself is listening-verified on the clean FLAC 5.1 VOD lane; the imax51 lane additionally pops and degrades over a soak on ANY build because its `-c copy` passthrough carries millisecond-quantised timestamps whose pts timeline runs slower than the sample count, saturating the PcmRing — reproduces headless on Windows, kept as the R15 repro lane; per-speaker mapping stays the managed multichannel work) · WHEP (`whep://mr.town:8091/whepav/whep`, **first Android WHEP run** — join <2 s, ratio 1.000, Bank grinding to the sub-second floor) · HLS live (local windowed server via `adb reverse tcp:8899`, wrap splices handled; the pacing gate's "snaps" are the documented splice timeline restarts, expected FAIL on this lane). Visual A/B: occasional macroblock pixellation on the UDP-carried lanes (RTSP-UDP, WHEP) heals at the next keyframe and vanishes on the TCP twins — transport loss, not the present path (R9 files WHEP NACK/PLI); no FOREIGN-barrier-class artefacts seen; the OMX drain-timeout stayed bounded throughout | bring the mr.town lanes up per the remote-servers notes (+ the transient `whepav` publisher), preflight, then per lane: write `bm-url.txt`, `run-package-pass.ps1 -SkipBuild` |

Known Android gap: https lanes fail at connect (error 103) — the TLS
stack's native-roots loader finds no CA store on Android, so device runs
use plain-http/rtsp/whep lanes until the webpki-roots fix lands (R16 in
the engine backlog). First observed 2026-08-15 on the first device https
attempt; every earlier device lane was rtsp/rist/whep/http/local.

The Vulkan graphics contract (normative, mirrors the D3D11 one): a
**linear** (sRGB off) RGBA32 RenderTexture with `enableRandomWrite`,
display-sized, registered via `bm_session_set_output_texture`; the plugin
preloaded so the Vulkan-init interception registers before graphics init
(the package's committed `.meta` sets `isPreloaded`); render events issued
per frame as on D3D11. `BasisMediaPlayer` implements the contract on
Android — the raw harness stays as the A/B reference consumer.

## Unity host autotest

The disposable host project (`C:\Users\Matt\Documents\UnityScratch\BasisMediaM0Host`,
Unity 6000.5.8f1, forced D3D11) references the `com.basis.mediaplayer`
package as a local file dependency and drives it in batch play mode
(commands from the workspace root, `Native~`):

```
cargo build --release -p media-ffi
copy target\release\basis_media.dll ..\Runtime\Plugins\x86_64\
set BASIS_SPIKE_AUTOTEST=1
Unity.exe -batchmode -projectPath <host> -executeMethod SpikeSetup.AutoPlayTest -logFile Logs\autotest.log
```

The dll ships committed inside the package `Runtime/Plugins/`
(rebuild+copy after native changes), so every referencing project loads
the same binary.

Exit 0 = pass (Playing — or Ended at the fixture's full position, since
the startup/seek burst finishes the 6 s fixture inside the test window —
plus >60 presents, >1 s of audio pulled, seek exercised).

`BASIS_SPIKE_DSP_RATE=44100` additionally forces the device DSP rate
below the fixture's 48 kHz, so the run exercises the component's
stream→DSP resample path (the path Android devices hit for real) on
Windows; the same pass bar applies.

## Interactive pass in Basis (the M2 artefact proper)

The Basis checkout references the package as a local file dependency in
`Packages/manifest.json`. In the Basis editor: open any scene, run
**Basis → Media v2 → Create Test Player** (spawns a screen quad + player
with the repo's A/V fixture prefilled), enter play mode, and drive it from
the inspector (state readout, play/pause/seek). Swap the URL for the
HTTP(S) rows. Run by hand; report state transitions, A/V sync, seek
behaviour and teardown on scene exit.

## The C player as differential oracle (M2 →)

Both demuxers are diffed against the *same* ffprobe oracle semantics:
this repo via `bm-probe conformance`, the C player via
`Basis/tools/media-conformance` (`basis_demux_dump` + `demux_gate.py`).
Running both over the same fixture set is the differential lane:

```
# C side (Git Bash, needs cc + ffmpeg):
cd <Basis>/tools/media-conformance && ./build.sh && python demux_gate.py <fixtures-dir>
# Rust side:
cargo run -p bm-probe -- conformance <fixtures-dir>
```

Divergence from ffprobe on either side fails its gate; passing both on one
fixture set is byte-level agreement on every payload ffprobe hashes. The
`gen_fixtures.sh` matrix (faststart / trailing-moov / fragmented and the
codec spread) is the richer shared corpus as formats land here.

## Known gaps (M2/M3)

- The *VOD* HTTP source (ranged/blocking) still tears down by waiting out
  at most one request timeout; live sources cancel immediately via the
  session token. Migrating the ranged source onto the async domain is
  deferred until something needs it.
- The range-less VOD fallback (sequential 200 on the blocking client)
  remains untimed; a range-less server that stalls holds that one request
  open. Live sources don't use this path.
- The 7.1 fixtures (B69: `sine-48k-71.flac` decodes 8ch through claxon;
  `sine-48k-71.aac` pins the ADTS channel-screen refusal) stop at the
  decoder: the PcmRing carries the 8-channel interleave and the managed
  side's `OnAudioFilterRead` adapter downmixes it to stereo (ITU
  coefficients over the WAV-order interleave; 4ch/7ch layouts fall back
  to the front pair), but no per-speaker mapping/splitter exists yet —
  that is the managed multichannel work, a dedicated session.
- Raw TS file/stream seek still returns Unsupported (HLS-TS VOD seeks by
  segment index — that landed with HLS); LPCM audio demuxes but has no
  decode adapter yet (announce is refused as a diagnostic event; video
  plays on).
- HLS carries no EXT-X-KEY (encrypted), EXT-X-BYTERANGE, or I-frame-only
  playlists (typed refusals), no ABR (the master playlist's
  highest-bandwidth variant is chosen once), and fetches whole segments
  (bounded 64 MiB) rather than streaming them; live position reporting
  inherits the segment timeline's own offsets.
- Raw FLAC/MP3/ADTS/Ogg-Opus demuxers refuse seeks (SEEKTABLE / Xing
  TOC / granule bisection all unbuilt).
- Decode breadth gaps: no VP9 software floor (libvpx build infra on
  MSVC is unresolved — VP9 is platform-MFT-only, refused where the
  extension is absent); Opus is mono/stereo only (no multistream
  decoder — surround Opus refuses typed); AV1 output above 8-bit 4:2:0
  refuses (no P010 path); LPCM still announces without an adapter. AV1
  hardware decode is future async/D3D11VA work — the Store AV1
  extension blocks inside ProcessInput under sync driving, so rav1d is
  the primary AV1 route.
- RTSP `rtsp://` negotiates UDP (media-rtp reorder/RTCP-RR under
  retina's signalling; retina's own UDP path is unused — no reorder, no
  receiver reports, and real servers kill RR-less sessions) with
  TCP-interleaved fallback; `rtspt://` pins TCP. Sessions carry no
  credentials; the host's resolved addresses are vetted before the
  client dials, and the SETUP response's server-controlled UDP `source`
  address is vetted before any datagram is sent to it. UDP-side
  multicast is not offered. H.264 + AAC only; video dts = pts (RTP
  carries presentation time; these lanes do not reorder B-frames).
- WHEP is H.264 + Opus only (deliberately offered as such — codecs the
  decode factories would refuse are cleaner refused at negotiation);
  `Link rel="ice-server"` entries are parsed, vetted-surfaced and
  reported but not used for STUN/TURN gathering (a receive-only client
  that initiates every connectivity check needs no srflx candidate;
  TURN-only topologies fail as ICE-disconnected). Signalling carries no
  authentication headers yet. The `whep_signal` fuzz target is
  feature-gated (`--features whep`) because str0m's Linux crypto
  backend compiles aws-lc-sys (needs cmake on the fuzz host).
- TS fixtures derive from the MP4 fixture
  (`ffmpeg -i fixtures/h264-aac-640x360-30fps.mp4 -c copy -f mpegts …`);
  the m2ts LPCM fixture is synthesised
  (`… -c:v libx264 -c:a pcm_bluray -f mpegts -mpegts_m2ts_mode 1 …`).
