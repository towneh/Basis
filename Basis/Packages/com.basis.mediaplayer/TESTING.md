# Testing

The governing test document is [`Native~/TESTING.md`](Native~/TESTING.md). It
carries the manual and device matrix, the column contracts for the diagnostics
captures, what each metric means and the healthy band for each, so a run is
graded rather than watched.

It lives beside the engine rather than here because most of what needs
exercising is the engine, and because it has to stay in step with the code it
grades.

What this file adds is the part the engine cannot see: the managed features that
live above the ABI, several of which need two clients or a populated scene and
so have no automated row anywhere.

## The short version

| What | How |
| --- | --- |
| Engine gate: format, lint, tests, licence and supply-chain audit | `Native~/tools/ci.ps1` (Windows), `Native~/tools/ci.sh` (Linux) |
| Container conformance against ffprobe, per packet | part of the gate above |
| Fuzz seeds replayed under AddressSanitizer | part of CI; corpora in `Native~/fuzz/` |
| Headless playback of any source | `Native~/bm-probe` |
| In-editor playback, graded | `Basis > Tools > Media Player > Run Smoke Test` |
| The same, in batch | `-executeMethod BasisMediaSmokeTest.RunBatch`, exit code 0 on a pass |

The smoke test builds a scene around a real player, plays a source, and grades
the capture against the bands in `Native~/TESTING.md` rather than asking
someone to watch a quad. `BASIS_SMOKE_URL`, `BASIS_SMOKE_SECONDS`,
`BASIS_SMOKE_LIVE` and `BASIS_SMOKE_STRICT_HOLDS` steer it.

While Unity holds the project lock, batch mode is unavailable and the managed
assemblies can still be checked by compiling them standalone against
`Library/ScriptAssemblies` with the editor's own Roslyn. That proves they build.
It proves nothing below, because none of it is reachable from a compiler.

## Editor pass, 2026-08-16 — first run of this surface

Everything below that a single client can exercise was run and passed: the
prefab checks, all the panel rows bar the three needing a permission or a
resolver, and the whole session-cap table. It was the first time any of the
shared-playback-era managed code had executed.

Five defects came out of it, all fixed:

- The shipped native binaries predated the ABI bump, so the player refused to
  open a session at all. The version pin caught it; a change that did not move
  the ABI would have shipped silently.
- The host's main volume was applied twice — in the taps and again by the
  AudioListener — so media played at the square of the setting.
- The DSP rate was assumed to be 48 kHz before the first callback, which is
  wrong on Quest at 24.
- A woken player never resumed its position: the resume seek was skipped
  whenever the networking component was merely present, and both shipped
  prefabs carry it.
- Every string in Settings > Developer > Media Player was missing, so the
  section rendered with no title and could not be found. A missing key is an
  empty label, not an error — worth sweeping for whenever strings are added.

Two findings were not defects here and are recorded above rather than fixed:
entering Play Mode costs seconds and looks like a stall, and media audio steps
down in level when scene init sweeps it into the World mixer group — the second
of which is a maintainer decision, written up separately.

Still unrun: the **Admin tab** row (wants the `*` permission, and whether it
resolves offline is itself unknown), **markup injection** and **sidecar
subtitles** (both want the resolver), and everything under Shared playback.

## Shared playback

Needs **two clients in the same world**, both seeing a player that carries
`BasisMediaPlayerNetworking`. Both shipped prefabs do.

`Basis > Tools > Media Player > Test Scene > Shared Playback (two clients)` builds
the scene: one networked player on the initialization scene, captures armed, and
**no source filled in**. That last part is deliberate and is the trap this pass
walks into otherwise — every other test scene pre-fills a local fixture, and a
local path is the one thing that cannot work here. It loads on the owner and
fails on the follower, which reads as a sync defect rather than as a file the
second machine does not have.

So the source has to be something both clients can fetch. The lettered clips
carry a running timecode, which makes most of these rows readable at a glance
rather than by inference — whether the follower landed where the owner is, is
the two screens agreeing. Anything seekable works; the **Live source** row wants
a live lane instead, and the **Convergence** row wants something longer than a
minute so there is half a minute to watch after the seek.

Getting the second client up, in the order that costs least:

1. Save the scene and add it to Build Settings — a build is what the second
   client is, so this one scenario has to be saved where the others are
   deliberately throwaway.
2. Build a standalone player of it and run that alongside the editor. Both join
   the same server; the editor is the easier one to drive the panel from, so
   make it the owner for the rows that need one.
3. **Owner leaves** wants the owner's process gone, so drive that row from the
   build and close it, keeping the editor as the survivor.

A headless client can stand in as the follower for the protocol rows — load,
transport, seek, ownership — but not for anything graded by eye: convergence,
and both admin rows, are about what the panel shows.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Load | One client sets a URL from the panel; the other loads it | The second client plays the same source without being touched |
| Transport | Play, pause and stop from the owner | Each reaches the follower. Stop closes it there too |
| Seek | Owner scrubs the timeline | The follower lands near the same place, then settles |
| Convergence | Watch the follower for the half minute after a seek | It **slews** onto the owner's position rather than jumping. Repeated visible jumps mean the target is being fed but not converged, which is the engine ladder failing, not the protocol |
| Late join | Second client arrives while the first is playing | It loads, and lands at the owner's position rather than at zero |
| Ownership | Second client takes control, then drives | The first becomes a follower and stops beating its position. Neither fights the other |
| Owner leaves | Owner disconnects mid-playback | The follower keeps playing free-running rather than freezing on the last target |
| Admin only | Set `AdminOnly` with the second client holding no permission | It cannot take control, and the panel gives it no playback tab |
| Open to everyone | Set `AnyoneCanControl` | The unprivileged client gains the controls **while the panel is open**, without reopening it |
| Live source | An RTSP or HLS live lane | No position sync at all: both clients sit near the edge independently. This is deliberate |
| Stalled owner | Let the owner's network stall mid-playback | The follower holds position rather than being dragged backwards towards a frozen playhead |

Two that need the resolver, so they wait until the yt-dlp package is installed:
a page URL must reach peers **as the page URL** — each client resolves for
itself, and seeing a `googlevideo` or CDN host arrive on the far side is the
failure — and a resolver that records no page URL must leave peers with no URL
rather than an expiring one.

**Not covered by any of this:** the server does not inspect media traffic, so a
client that sends control messages it should not have is only stopped on
receipt. Testing that needs a hostile client, not a second honest one.

## The Media Players panel

Single client, but needs a scene with **more than one** player to be worth
anything.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Visibility | A scene with no players, then one | The menu entry is absent, then appears |
| Selection | Switch between players | Status, URL and the transport controls follow the selection |
| Scrubber | Drag the timeline handle | It stays where you put it while dragging, issues one seek once the handle comes to rest, and does not bounce back to the old position on a short seek |
| Timeline-less media | A live lane | The scrubber hides rather than showing a meaningless bar |
| Caption rows | Toggle captions, then switch player | The setting is the viewer's: it applies to every player, and switching selection does not move the rows |
| Subtitles | A source offering sidecar tracks | The language row appears only while captions are on, and reverting to the first row restores the in-band feed |
| Status | Watch through a load | Connecting, then Buffering, then Playing, with the resolution appearing; an error shows its code |
| Admin tab | With and without the `*` permission | Present only with it, and only on a player carrying the networking component |
| Markup injection | Play something whose title contains `</noparse>` and `<b>` | The status line shows those characters literally. Rendered markup here is a defect, and titles travel between clients |

## The session cap

Needs a scene holding **more players than the cap allows** — the default is 3,
2 on Android, and `Settings > Developer > Media Player` sets it.

`Basis > Tools > Media Player > Test Scene > Four Players (7.1)` builds one:
Basis's initialization scene, which is what spawns you onto a floor to walk on,
plus four players ten metres apart along +X with the engine's own fixture already
set. Stand at the origin and walk along +X to change which are nearest. `Four
Players (Stereo)` is the same with the stereo prefab, and the `One Player` pair
is the same arrangement with a single player for the panel and playback rows.
Every one of them arms both captures, and none saves the scene — discard it
after, or save it if the pass needs a build.

### The lettered clips

The walking rows need content that outlasts a walk, which the bundled 6 s
fixture does not. The builder looks for `basis-captest/A.mp4` … `D.mp4` under
the user's Videos folder and gives one to each player; without them it falls
back to the fixture and says so.

They ship under `Native~/fixtures/captest`, so the pass needs no setup. What
they are, in case they ever need rebuilding:

- Four 60 s clips, H.264 640x360, **keyframes every second** (so a mis-landed
  seek cannot hide behind keyframe rounding, as it can on the bundled fixture's
  2 s spacing), AAC stereo 48 kHz.
- Each carries one large letter, one background colour, its genre across the
  top, and a running timecode — so "resumed near where it would have been" is
  read off the screen rather than inferred.
- One contrasting genre of music each. Flapping is then audible without
  looking: a track cutting in and out is hard to miss.
- The music is Kevin MacLeod (incompetech.com), CC BY 4.0, attributed in
  `THIRD_PARTY_NOTICES.md`. Anything replacing it needs the same treatment.
- Encoded at CRF 30 with 64 kbps stereo audio: 3.3 MB for the set, which is
  what makes shipping them reasonable.

Both build with **both captures on** — engine capture, and a diagnostics
component per player — so a run that turns out to be interesting has the
numbers already. They are numbered per player in the four-player scene, since
both default to one fixed filename and would otherwise overwrite each other.
The per-frame capture writes as it goes; the engine's lands when each session
ends, so it appears on stop rather than during play.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Cap holds | Four players in a scene, cap 3 | Only three hold a session; the furthest reads Dormant in the panel |
| Waking | Walk to a dormant player. **Note it must have played before going dormant** — one that was dormant from the first frame has no position to return to, and starting at zero is correct there. The capture makes this unambiguous: look for a Playing -> Idle transition with a non-zero position before the wake | It opens and resumes near where it would have been, and something further away goes dormant in its place |
| Live waking | The same with a live source | It rejoins at the edge rather than at a remembered position |
| No flapping | Stand midway between two players at similar distance and move gently | Sessions do not open and close repeatedly. Repeated joins here are the hysteresis failing |
| Promotion | Select a dormant player in the panel | It starts whatever its distance, and the furthest active one gives up its slot |
| Cap lifted | Set the cap to 0 | Everything dormant comes back |
| Startup | Enter a scene where every player autoplays | The cap holds from the first seconds. Every session opening at once and settling later means new players are not eligible immediately |
| With shared playback | Let the owner's own player go dormant | Playback does not stop for the other clients. Going dormant is local |

The budgeted counts themselves — 2 concurrent 1080p sessions on a Quest, 3 or
more on desktop — are engine measurements, and `Native~/TESTING.md` carries how
they were taken.

## The shipped prefabs

Both want an editor pass after any change that touches their components:
`MediaPlayerStreaming` and `MediaPlayerMultiChannelStreaming`.

- No missing scripts on any object in either.
- Audio outputs wired to the channels they claim, and the multichannel one
  reaching all of its.
- The screen material present and showing the picture.
- The networking component present and its permission fields as intended.

Both were re-serialised on 2026-08-16, which is when the older component's
field names left `MediaPlayerStreaming` and the networking component stopped
sitting at the end of the file where it had been hand-written in.

## The Picture foldout on the output sinks

`BasisVideoPicture` carries the per-output brightness, contrast, saturation and gamma,
and both sinks expose it as an inspector field. It is a plain struct, so it serialises
only because it is marked `[System.Serializable]`; without that Unity skips the field
entirely, the SDK inspector's `PropertyField` has no `SerializedProperty` to bind to,
and the foldout comes up empty. Nothing fails at compile time — the compiler is happy
either way, and Unity's serialization analyzer only says so on a real recompile of the
assembly, so a cached editor session shows nothing.

| Row | What it proves | How to run |
| --- | --- | --- |
| The foldout is populated | on `BasisVideoMaterialOutput`, the Picture foldout shows four sliders at their default of 1 (gamma included) rather than being empty | select a player's material output in the inspector |
| The values persist | move a slider, enter and leave Play Mode, reopen the scene: the value is still there | |
| Brightness reaches the picture | on `BasisVideoDisplay` it multiplies into `RawImage.color`; the other three need a material carrying the `_Basis*` properties | |

## Known: media starts loud and drops, once, at scene init

Not a media player defect and not worth re-investigating. Basis's scene
initialisation sweeps every ungrouped `AudioSource` into the World mixer group,
and `playOnStart` begins playback before that sweep runs — so the first second
or two plays ungrouped at unity gain and then drops onto the bus.

It has been measured: the PCM leaving the tap is flat (`out_peak` 0.089,
`out_rms` 0.062) for the whole session, on both prefabs, with main and world
volume both at 100% and spatialisation ruled out by the stereo rig behaving
identically. Opening a URL after play mode settles shows no transition because
the sweep has already run, not because that path is healthier.

Which bus media belongs on is a maintainer decision and is written up
separately.

## Entering Play Mode costs seconds, and it looks like a stall

A source that opens from `playOnStart` on the first Play Mode after a domain
reload will appear to freeze and then race to catch up. It is the editor, not
the engine, and it has been measured rather than assumed: on 2026-08-16 the
first three captured frames were **6924 ms, 2764 ms and 350 ms** long, and only
**14 render events** ran while the engine presented **147** frames.

What happens is that the session opens during the hitch, the engine paces
correctly into the frame pool, the pool fills because nothing is consuming it,
decode backpressures — and then the render thread frees up, drains the pool and
the position jumps. The engine's own capture for the same run: 180 decoded, 147
presented, **zero drops**, decode finished at 5.82 s for a 6.0 s fixture, and
its 100 ms sampler never slipped past 102 ms.

Opening a URL by hand after Play Mode has settled shows none of it. Judge
startup behaviour in a standalone build, where the first frames are
milliseconds; in the editor, judge it on the second load.

## In-band user data

`BasisMediaPlayer.UserDataReceived` has a managed compile and nothing else. The
engine half has its rows (`Native~/TESTING.md`, "SEI user-data lane"); what a
person still has to run is the delivery timing, which only exists with a
playback clock underneath it.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Ordered delivery | A script subscribing and logging, against `Native~/fixtures/h264-sei-userdata-640x360-30fps.ts` over HTTP | Frame indices 0..179 arrive in order, one per video frame, each as the position crosses its PTS; x264's `dc45e9bd-…` message once at the start |
| Seek | Seek back mid-clip | Logging resumes from the landed frame with nothing from the old position in between |
| Late subscriber | Subscribe a few seconds in | The first message is the one due at the moment of subscribing, neither a replay of the opening nor anything later: what was already past is gone, what was still to come all arrives |
| Subscriber one frame late | A component that assigns its player reference after `Open` and subscribes from its own `Update` (so one tick after the player's first drain) | It receives the stream's first message. An on-demand open banks the whole pre-roll before the first frame shows, and the first drain takes a tick's worth of it; none of that may be lost to a subscriber that was a frame away |

## The admin media lock

`BasisNetworkModeration.MediaPlayerBlockedLocally` is the instance-wide moderation lock: a
client under it may not load media at all, in either direction, and admins are exempt
through the global-lock bypass. It is client-enforced, so the check in this package is the
whole gate rather than a hint to a server that will re-check.

It is checked in two places, and both are needed. `Open(string)` is the funnel every route
reaches — the split-pair overload, `OpenUserUrl`'s direct path, a resolver's `OpenResolved`,
and the internal re-open — so it catches everything including a peer's synced state.
`OpenUserUrl` refuses ahead of that as well, because a page URL leaves the method for the
resolver and returns through `OpenResolved` only after extraction has already happened; a
locked client should not have handed the URL out in the first place.

| Row | What it proves | How to run |
| --- | --- | --- |
| Locked client cannot load | with the global media lock on and no bypass permission, a URL entered locally is refused with a `Video`-tagged warning naming the entry point, and no session opens | set the lock server-side, then type a URL into the panel |
| Locked client is not driven by a peer | a second, unlocked client loading a URL does not start playback on the locked one — the synced state routes through `OpenUserUrl`, so the same gate applies inbound | two clients, one locked |
| A page URL never reaches the resolver | with the lock on, a YouTube or Twitch watch page produces the refusal and **no** resolver activity in the log | needs the yt-dlp integration installed |
| An admin is exempt | the same three rows with the `*` permission held: all of them load normally | |

None of these has run yet. They want a server that can set the lock, which the local rig
cannot do.

## The diagnostics event drain

The engine's structured events reach the Console through `DrainEvents`. Each line
carries the session's own monotonic clock, so a line can be lined up against
`BasisMediaFrames.csv` and `BasisMediaEngine.csv` without hand-aligning three
timebases:

```
[BasisMedia +12.412s] AudioTrim/AudioRing: serve trimmed 70654 frames
```

Codes and stages mirror the engine's own sets (`BmEventCode`, `BmStage`). Both only
ever grow, so a plugin newer than the managed side prints the number rather than a
name; that is a gap in the mirror, not a failure.

| Row | What it proves | How to run |
| --- | --- | --- |
| Named codes and a timebase | events read as `Code/Stage` with a session-relative timestamp, not `event 15 stage 6` | play any lane and read the Console; a `StateChange/Clock` line lands within the first second of every open |
| No stack traces in the Editor | Log-level Console lines carry no call stack, so a drain is readable during a pass. Editor only — the setting is application-wide, so a build would be taking every package's Log-level traces away to tidy this one's | any scene holding a player; the first one to wake applies it |
| An unknown code degrades to its number | a plugin ahead of the mirror still produces a readable line | temporarily delete `AudioTrim = 15` from `BmEventCode`, replay a lane that trims, confirm the line reads `15/AudioRing`, restore |
| A burst arrives whole | the drain empties the queue in the frame it ran, rather than leaving a backlog | drop `EventDrainBatch` to 4, open a lane, confirm the open's events still all appear on the same frame; restore |
| A full log says so | the drained sequence is never quietly incomplete: the engine's log has a cap, and what it refuses is reported once per step rather than every frame it stays non-zero | drop `SessionDiag::default`'s cap from 1024 to 2 in the engine, rebuild the plugin, play a lane; the Console warns `diagnostics log full: N event(s) refused, M this session`. Restore |
| A cut detail ends in `…` | a Console line that ran past the record's 116 bytes says so, rather than reading as a sentence the engine stopped writing | open a URL long enough to overrun the detail of the refusal it provokes — a bad host with a long path is easiest — and read the `Error/Source` line |

The engine's free-text diagnostics are a second channel, drained by
`BasisMediaLogDrain` and prefixed `[BasisMedia process +N.NNNs]`. **That clock counts
from the plugin's first diagnostic, not from a session's start** — the two prefixes
name different origins on purpose, and commit 8 of the logging spec is what makes them
comparable. It is pumped from every player's tick and deduplicated on the frame, so it
needs at least one player alive: a client with no media player in the scene never loads
the plugin, which is deliberate and is why this is not driven from a startup hook.

| Row | What it proves | How to run |
| --- | --- | --- |
| Transport reaches the Console | `rtsp transport: …` — the fact that ruled out packet loss on the 2026-08-25 pass — no longer needs a DBWIN reader attached to see | play `rtsp://mr.town:8090/imax51`; the line appears within the first second |
| One copy per line | the drain is process-wide, not per-player: the ring is one queue for the whole plugin, so three players must not print everything three times | put three players in a scene, open a stream on one, count the `process` lines |
| Level picks the Console severity | a refusal reads as a warning and a failure as an error, rather than everything arriving flat | open a bad URL: the `session error:` line is red. Note it uses the **unreported** error path — an engine failure is the stream's or the network's fault, not a client defect, and must not raise a crash report |
| An overrun ring says so | the tail is bounded at 512 engine-side and drops its oldest, so a stall in the drain loses the *start* of what follows, not the end | leave a session erroring in a tight loop with the editor paused, then resume |
| A burst costs frames, not one frame | neither drain takes an unbounded amount of work in a single tick. Both loop until a batch comes back short, and both stop at a ceiling — 256 events per tick, 128 log lines per frame — against native queues of 1024 and 512. Every record costs a UTF-8 decode and a Console line, so draining a full queue in one go is a visible hitch; nothing is lost by stopping short, because both drains leave what they cannot carry and the next tick takes it | drop `EventDrainPerTick` to 8 and `DrainPerFrame` to 4, provoke a burst, and confirm the lines still all arrive — over several frames rather than one. Restore |

## What still needs a person

Picture and sound quality, in a headset, on the live transports. No harness
judges judder or A/V sync the way an ear and an eye do, and the device passes
that matter most — the Quest transport matrix — need someone wearing it.

Everything in the three sections above, too. None of it has an automated row:
shared playback needs a second client, the cap needs a populated scene, and the
panel needs someone to look at it.
