# Diagnostics: what to read, and what good looks like

How to tell whether a playback run was healthy from the numbers the player
records, rather than by watching it. [`TESTING.md`](TESTING.md) says which
tests and passes to run; this file says how to read their output.

## Three surfaces

They do not overlap, and a run is graded by reading numbers off them.

| Surface | Written by | Sampled | Covers |
| --- | --- | --- | --- |
| **Engine capture** | the engine, on its own thread | every 100 ms | Everything inside the pipeline: stage counters, buffer depth and lag, decode, release, events. Turned on per session: the open descriptor's `diag_csv`, the player's **Engine capture** under Advanced, or `bm-probe play --csv`. |
| **Frame capture** | `BasisMediaPlayerDiagnostics`, main thread | one row per Unity frame | The other side of the boundary, which the engine cannot see: render cadence, how long each frame stayed on screen, the audio pull rate, what the speaker outputs consumed and how loud it was, the device's audio chain. Written under `Application.persistentDataPath`. |
| **Debug window** | the editor, live | every 250 ms | The same numbers while a run is happening, stage by stage, for finding which stage stopped. `Basis > Debug > Media Player`. |

The two captures share a time base, so read them side by side: the engine
capture says what the pipeline did, the frame capture says what the viewer
got.

## Frame capture columns

The columns are positional, and new ones go on the end, so a reader keyed on
position stays valid. This line must match `BasisMediaPlayerDiagnostics.Header()`
exactly; if it does not, one of the two is wrong and the grading below reads
the wrong column:

```
unity_time,frame,frame_dt_ms,state,position_us,duration_us,banked_ms,decoded,decoded_delta,presented,presented_delta,frames_held,video_w,video_h,has_texture,audio_pulled,audio_pulled_delta,stream_rate,stream_channels,dsp_rate,dsp_buffer,dsp_buffers,listener_paused,sync_ppm,subtitle_track,caption_len,out_bound,out_playing,out_consumed,out_consumed_delta,out_peak,out_rms,out_latency_us,av_offset_us
```

## What each column means, and its healthy range

| Column | Means | Healthy |
| --- | --- | --- |
| `state` | `BmState`: 0 idle, 1 opening, 2 buffering, 3 playing, 4 paused, 5 ended, 6 error | reaches 3 and stays; 6 never appears; an on-demand run ends at 5 |
| `position_us` | the session clock | advances at 1 s per wall second, within ±2%. **Never goes backwards outside a seek**: a backwards step is a clock snap, which is how pacing faults show up |
| `banked_ms` | how much is buffered ahead | 0 while buffering; at or above the configured depth once playing (Auto settles wherever its estimate lands). Sagging towards 0 during playback is a starving source |
| `decoded_delta` | frames decoded this Unity frame | over a window, sums to the source frame rate |
| `presented_delta` | frames presented this Unity frame | over a window, matches `decoded_delta`. Persistently below it means frames are dropped after decoding |
| `frames_held` | Unity frames the last presented frame stayed on screen (0 on frames where nothing new was presented) | the display-to-content ratio, and the same value every time: 24 fps on a 72 Hz headset is 3, 3, 3. The share of holds at that value is the judder measure: **at least 99% on a steady stream**. Isolated 2s and 4s are single-refresh jitter; clusters are a fault |
| `frame_dt_ms` | Unity's own frame time | the display interval (13.9 ms at 72 Hz, 16.7 ms at 60 Hz). If this is unstable the hold figures mean nothing: the renderer is the problem, not the player. To judge whether the player costs the render thread, count missed refreshes (frames over twice the interval) and compare the mean against a control in the same sitting; frame-time percentiles vary between sittings with vsync jitter alone |
| `audio_pulled_delta` | source frames consumed this Unity frame | over a window, **`audio_pulled` advances at `stream_rate`, whatever `dsp_rate` is**, because the pull resamples. A pull rate stuck at some ratio of the stream rate drags the clock and everything follows it |
| `stream_rate` / `stream_channels` | what the engine announced | non-zero once audio is announced; channels matching the source |
| `dsp_rate`, `dsp_buffer`, `dsp_buffers` | the device's audio chain | context rather than pass or fail. Worth recording because devices pick their own (a Quest picks 24 kHz) |
| `sync_ppm` | how far shared playback is speeding this follower up or slowing it down, in parts per million | 0 unless catching up with the owner; non-zero for long stretches means it is not converging |
| `has_texture` | the output texture exists | 1 shortly after the first frame; going back to 0 mid-playback is a lost texture |
| `listener_paused` | `AudioListener.pause` | 0. If it is 1, the audio figures mean nothing and the clock falls back to wall time |
| `subtitle_track`, `caption_len` | the selected subtitle file (-1 for captions in the video), and the current caption's length | context for caption work |
| `out_bound` | how many AudioSources the audio component bound at its last build | the number of outputs set up: 1 for a stereo mix, 8 for the surround arrangement. **0 with audio announced means nothing was listening**, which every other column still reads as healthy |
| `out_playing` | whether any bound output is playing | 1 once the format is known and the outputs start. 0 while playing means the outputs stopped under a session that did not |
| `out_consumed_delta` | output frames the first output mixed this Unity frame | over a window, `out_consumed` advances at **`dsp_rate`**, not `stream_rate`, because the outputs render into the device's audio blocks. Below it means blocks were missed, which is heard as break-up |
| `out_peak`, `out_rms` | the level of the first output's last mixed block | non-zero on anything but silence. A run where every block is 0 played nothing, however healthy the pull looked |
| `out_latency_us` | the audio component's estimate of the delay from pull to speaker | the device's buffer chain plus a block of headroom. Passed to the engine on Android as the A/V compensation; recorded everywhere so the two platforms can be compared |
| `av_offset_us` | the presented frame's time minus the audio playhead, in microseconds; `-2147483648` when there is nothing to measure | read it on the rows where `presented_delta` is above 0, which is the frame's lateness as it went up: median within a few milliseconds, and nothing later than 40 ms plus one refresh. The audio playhead moves in whole pull blocks (about 21 ms at 48 kHz), which widens the reading by up to that much. On other rows it is the age of the frame already on screen |

## Grading a run

A pass over the frame capture, which an automated run should assert rather
than someone watching:

1. `state` reached 3, never 6, and reached 5 if the source was finite.
2. `position_us` advanced at 1 s/s within ±2%, with **no** backwards steps
   outside a deliberate seek.
3. The `presented` total is within a few frames of the `decoded` total.
4. `frames_held` sat at the ideal value for at least the threshold share,
   measured over a window that **leaves out the start**: a live join
   legitimately holds frames while the clock settles, and including it hides a
   real fault behind a known one.
5. `audio_pulled` advanced at `stream_rate` within ±1%.
6. `banked_ms` never sagged to 0 while `state` was 3.
7. With audio announced: `out_bound` non-zero, `out_consumed` advanced at
   `dsp_rate` within ±2%, and `out_peak` was non-zero at some point. Steps 1 to
   6 all pass on a session that makes no sound at all; this is the step that
   catches it.

The engine capture answers the follow-up in every case: if presented lags
decoded, its drop counters say where; if the position snapped, its event log
says why.

## The smoke test runs that grading for you

`Basis > Tools > Media Player > Run Smoke Test` builds a scene around a
player, plays a source, and grades the capture against every range above.
Headless, for CI or a check before committing:

```
Unity -batchmode -projectPath <project> -logFile - \
      -executeMethod BasisMediaSmokeTest.RunBatch
```

Exit code 0 on a pass and 1 on a fail, with the measured numbers in the log
either way. Environment: `BASIS_SMOKE_URL` (defaults to the engine's A/V
fixture), `BASIS_SMOKE_SECONDS`, `BASIS_SMOKE_LIVE` (a live source is not
expected to end), `BASIS_SMOKE_STRICT_HOLDS`.

Frame holds are reported but not enforced by default, and only that range is
treated this way: an editor play session does not present on a stable display
cadence, so the share is informative there and meaningful only from a build or
a device. `BASIS_SMOKE_STRICT_HOLDS=1` makes it a failure for the runs where it
means something.

The scene the smoke test builds has a real stereo output set, not a bare
AudioSource, because the audio ranges grade what reached a speaker. A surround
set would leave six of its eight outputs silent against a stereo fixture by
design, so surround is a listening pass rather than a graded one.

The grading has no Unity types in it, and is tested against made-up captures:
a clean run, a clock running fast, a clock that steps backwards, audio pulled
at half rate, presented trailing decoded, an error state, a run that never
ends, a capture from a different build, an output set that bound nothing, an
output missing half its audio blocks, outputs that ran but mixed silence, and a
video-only scene that is not expected to have outputs. Each case checks the
verdict and the reason given, so the grader cannot pass a run for the wrong
reason.

## Keeping this accurate

The header line above is quoted exactly so a mismatch can be found by search,
and a change to what a column carries changes this file in the same commit.
