# Testing the player in Unity

What this package's managed side needs checking, and how. The engine
underneath has its own guide at [`Native~/TESTING.md`](Native~/TESTING.md):
how to run its tests, how they are laid out, how to add one, and what each row
checks. [`Native~/DIAGNOSTICS.md`](Native~/DIAGNOSTICS.md) says how to read a
run's captures, so a run is graded rather than watched.

This file covers what the engine cannot see: the managed features above the
plugin boundary, several of which need two clients or a populated scene and so
have no automated test anywhere.

## The short version

| What | How |
| --- | --- |
| Engine gate: format, lint, tests, licence and supply-chain audit | `Native~/tools/ci.ps1` (Windows), `Native~/tools/ci.sh` (Linux) |
| Container conformance against ffprobe, per packet | part of the gate above |
| Fuzz targets | `Native~/fuzz/` (see its README) |
| Headless playback of any source | `Native~/bm-probe` |
| In-editor playback, graded | `Basis > Tools > Media Player > Run Smoke Test` |
| The same, in batch | `-executeMethod BasisMediaSmokeTest.RunBatch`, exit code 0 on a pass |

The smoke test builds a scene around a real player, plays a source, and grades
the capture against the ranges in `Native~/DIAGNOSTICS.md`. `BASIS_SMOKE_URL`,
`BASIS_SMOKE_SECONDS`, `BASIS_SMOKE_LIVE` and `BASIS_SMOKE_STRICT_HOLDS` steer
it.

While Unity holds the project open, batch mode is unavailable, but the managed
assemblies can still be checked by compiling them on their own against
`Library/ScriptAssemblies` with the editor's Roslyn compiler. That proves they
build. It proves nothing below, because none of it is reachable from a
compiler.

## Shared playback

Needs **two clients in the same world**, both seeing a player that carries
`BasisMediaPlayerNetworking`. Both shipped prefabs do.

`Basis > Tools > Media Player > Test Scene > Shared Playback (two clients)` builds
the scene: one networked player on the initialization scene, captures armed, and
**no source filled in**. That is deliberate. Every other test scene fills in a
local file, and a local file is the one thing that cannot work here: it loads
on the owner and fails on the follower, which looks like a sync fault rather
than a file the second machine does not have.

So the source has to be something both clients can fetch. The lettered clips
(below) carry a running timecode, which makes most of these rows readable at a
glance: whether the follower landed where the owner is, is whether the two
screens agree. Anything seekable works; the **Live source** row wants a live
stream instead, and the **Convergence** row wants something longer than a
minute, so there is half a minute to watch after the seek.

Getting the second client up, in the order that costs least:

1. Save the scene and add it to Build Settings. A build is what the second
   client is, so this one scene has to be saved where the others are
   throwaway.
2. Build a standalone player of it and run that alongside the editor. Both join
   the same server; the editor is the easier one to drive the panel from, so
   make it the owner for the rows that need one.
3. **Owner leaves** wants the owner's process gone, so drive that row from the
   build and close it, keeping the editor as the one that stays.

Give the build its own capture filenames before building. The editor and a
standalone build share `Application.persistentDataPath`, so with the default
names each overwrites the other's captures.

A headless client can stand in as the follower for the protocol rows (load,
transport, seek, ownership), but not for anything judged by eye: convergence
and both admin rows are about what the panel shows.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Load | One client sets a URL from the panel; the other loads it | The second client plays the same source without being touched |
| Transport | Play, pause and stop from the owner | Each reaches the follower. Stop closes it there too |
| Seek | Owner scrubs the timeline, once playing and once paused | The follower lands on the same place, to within a frame or two, then settles. Scrubbed while paused, both clients show the new frame and stay paused, and Play then resumes both from it |
| Convergence | Watch the follower for the half minute after a seek or a late join | It **slews** onto the owner's position rather than jumping. Repeated visible jumps mean the target is being fed but not converged, which is the engine's correction failing, not the network messages |
| Late join | Second client arrives while the first is playing | It loads, and lands at the owner's position rather than at zero |
| Late join, owner paused | Owner pauses mid-video, then a second client leaves and rejoins. Run it with a direct `.mp4` and with a page URL, and once more with the owner pausing while the rejoining client is still loading | It lands paused on the owner's own frame and seek bar position, not on the keyframe before it, not playing and not on the opening frame, and no sound is heard while it lands. The panel reads Paused, and the owner pressing Play starts it from there. The mid-load variant runs the other way too: owner paused when the client starts loading, then presses Play before it is up, and the client comes up playing at the owner's position |
| Ownership | Second client takes control, then drives | The first becomes a follower and stops sending its position. Neither fights the other |
| Owner leaves | Owner disconnects mid-playback | The follower keeps playing on its own rather than freezing on the last position it was given |
| Admin only | Set `AdminOnly` with the second client holding no permission | It cannot take control, and the panel gives it no playback tab |
| Open to everyone | Set `AnyoneCanControl` | The unprivileged client gains the controls **while the panel is open**, without reopening it |
| Live source | An RTSP or HLS live stream | No position sync at all: both clients sit near the live edge independently. This is deliberate |
| Stalled owner | Let the owner's network stall mid-playback | The follower holds position rather than being dragged backwards towards a frozen playhead |
| Resync everyone | Drift a follower off, then the owner presses **Resync Everyone** | Every client, the owner included, lands on the owner's current position and stays converged. No approval prompt |
| Resync keeps the owner's place | Owner presses **Resync Everyone** while playing. Run it on a direct `.mp4` and on a page URL | The owner keeps playing from where it was, **not** from zero, on both. A page URL is the harder case: it is resolved before it opens, and the session it replaces plays on for those seconds. A jump to zero means the owner's own saved position is not being applied |
| Resync while stopped | Owner presses **Resync Everyone** with nothing playing (session closed, a URL still held) | Nothing happens. It must **not** close the room and reopen playing alone |
| Resync a page URL | Owner presses **Resync Everyone** on a YouTube or Twitch source, well into it | Each client re-resolves the page URL for itself and lands together **at the owner's position**. Landing together at zero is a failure, and so is a seek in place a few seconds before the reload, which is the owner's position being spent on the session about to be replaced. The shared URL stays the page URL, never a peer's expiring stream URL |
| Join mid-resync | A client joins, or asks for state, while a resync reopen is still in flight | It lands at the owner's real position, not near zero |
| Local resync | A follower presses **Local Resync** | Only that client reopens its own stream; the owner and other followers are undisturbed |
| A video hours long | Owner loads a fragmented video of two hours or more, seeks well into it, and a second client joins there. Run it twice: on a direct URL, which takes the resolver out of it (make the file as the engine guide's "A fragmented MP4 hours long" row describes), and on `https://www.youtube.com/watch?v=aNS5o3VJ0-A` (11 h, H.264 the only codec it lists, so the resolver cannot pick AV1) | It opens in the time an ordinary video takes rather than minutes, the seek lands, and the joiner lands beside the owner. On the page URL, record the date it last worked, since a public link can go. Past about six hours in, expect the stream URL to expire and playback to stop; that is a separate problem, not this row's |

The resync rows need a little setup the transport rows do not. *Resync keeps
the owner's place* wants a seekable source well into playback, where a jump to
zero is obvious. *Resync while stopped* wants the owner to have played and
then stopped: a URL still held, nothing playing. *Join mid-resync* wants the
second client to arrive during the reopen; on a page URL that window is the
seconds of resolving, which makes it easy to hit. None of the owner's messages
(the settle broadcast, the position heartbeat, a late-join answer or a
state-request answer) may carry a reopening session's position near zero. A
follower that briefly snaps to the start on any resync is a fault even if it
recovers.

Two rows need the resolver, so they wait until the yt-dlp package is
installed: a page URL must reach other clients **as the page URL** (each client
resolves for itself, and seeing a `googlevideo` or CDN host arrive on the far
side is the failure), and a resolver that records no page URL must leave other
clients with no URL rather than an expiring one.

## The Media Players panel

Single client, but needs a scene with **more than one** player to be worth
anything.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Visibility | A scene with no players, then one | The menu entry is absent, then appears |
| Selection | Switch between players | Status, URL and the playback controls follow the selection |
| Scrubber | Drag the timeline handle | It stays where you put it while dragging, issues one seek once the handle comes to rest, and does not bounce back to the old position on a short seek. Dragged while paused, the picture changes to the new position and playback stays paused. The frame shown is the one at the position the handle was dropped on, not the keyframe before it: on a clip with sparse keyframes the bar must not settle back by seconds |
| A device too slow for the video | Set `BASIS_MEDIA_SLOW_VIDEO_DECODE_MS=80` in the player's environment before it starts (the Editor counts), then play an on-demand source of half a minute or more that has sound, and one carrying captions or SEI user data. Watch for a minute | The sound never drops out, changes pitch or skips, and the timeline moves at real time. The picture moves for a moment, holds, and rejoins, and whenever it moves it matches the sound: lips and beats are never out of step. Captions and user data keep arriving on time while the picture is held. The Console shows `LateVideoSkip` warnings, a few seconds apart at most. With the variable unset the same sources play as normal, and the render thread costs the same either way: no more missed refreshes than a control run (see `Native~/DIAGNOSTICS.md`). Unset it afterwards |
| Long pause | Pause an on-demand source for a minute, then press Play. Run it on a video and on an audio-only file | It resumes from where it stopped, with no error. A ranged source is read at playback pace and not at all while paused, so nothing may treat a slow or idle read as a dead link |
| Timeline-less media | A live stream | The scrubber hides rather than showing a meaningless bar |
| Caption rows | Toggle captions, then switch player | The setting is the viewer's: it applies to every player, and switching selection does not move the rows |
| Subtitles | A source offering subtitle files | The language row appears only while captions are on, and choosing the first row restores the captions carried in the video |
| Status | Watch through a load | Connecting, then Buffering, then Playing, with the resolution appearing; an error shows its code |
| Admin tab | With and without the `*` permission | Present only with it, and only on a player carrying the networking component |
| Markup injection | Play something whose title contains `</noparse>` and `<b>` | The status line shows those characters literally. Rendered markup here is a fault, and titles travel between clients |

## The session cap

Needs a scene holding **more players than the cap allows**. The default is 3,
2 on Android, and `Settings > Developer > Media Player` sets it.

`Basis > Tools > Media Player > Test Scene > Four Players (7.1)` builds one:
Basis's initialization scene, which puts you on a floor to walk on, plus four
players ten metres apart along +X with the engine's own fixture already set.
Stand at the origin and walk along +X to change which are nearest. `Four
Players (Stereo)` is the same with the stereo prefab, and the `One Player` pair
is the same arrangement with a single player, for the panel and playback rows.
Every one of them arms both captures, and none saves the scene: discard it
afterwards, or save it if the pass needs a build.

### The lettered clips

The walking rows need content that outlasts a walk, which the 6 s fixture does
not. The builder looks for `basis-captest/A.mp4` to `D.mp4` under the user's
Videos folder and gives one to each player; without them it falls back to the
fixture and says so.

They ship under `Native~/fixtures/captest` and need no setup. What they are,
in case they need rebuilding:

- Four 60 s clips, H.264 640x360, **keyframes every second** (so a seek that
  lands wrongly cannot hide behind keyframe rounding), AAC stereo 48 kHz.
- Each carries one large letter, one background colour, its genre across the
  top, and a running timecode: "resumed near where it would have been" is read
  off the screen rather than inferred.
- Each has a different genre of music, which makes a player switching on and
  off audible without looking.
- The music is Kevin MacLeod (incompetech.com), CC BY 4.0, attributed in
  `THIRD_PARTY_NOTICES.md`. Anything replacing it needs the same treatment.
- Encoded at CRF 30 with 64 kbps stereo audio: 3.3 MB for the set.

The test scenes record **both captures**, the engine's and one frame capture
per player, and a run that turns out to be interesting already has its numbers.
They are numbered per player in the four-player scene, since both default to
one fixed filename. The frame capture writes as it goes; the engine's lands
when each session ends, on stop rather than during play.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Cap holds | Four players in a scene, cap 3 | Only three hold a session; the furthest reads Dormant in the panel |
| Waking | Walk to a dormant player. **It must have played before going dormant**: one that was dormant from the first frame has no position to return to, and starting at zero is correct there. The capture shows it plainly: look for a Playing to Idle transition with a non-zero position before the wake | It opens and resumes near where it would have been, and something further away goes dormant in its place |
| Live waking | The same with a live source | It rejoins at the live edge rather than at a remembered position |
| No flapping | Stand midway between two players at similar distance and move gently | Sessions do not open and close repeatedly. Repeated opens here mean the margin between them is not working |
| Promotion | Select a dormant player in the panel | It starts whatever its distance, and the furthest active one gives up its slot |
| Cap lifted | Set the cap to 0 | Everything dormant comes back |
| Startup | Enter a scene where every player autoplays | The cap holds from the first seconds. Every session opening at once and settling later means new players are not held back from the start |
| With shared playback | Let the owner's own player go dormant | Playback does not stop for the other clients. Going dormant is local |

## The shipped prefabs

Both want an editor pass after any change that touches their components:
`MediaPlayerStreaming` and `MediaPlayerMultiChannelStreaming`.

- No missing scripts on any object in either.
- Audio outputs wired to the channels they claim, and the multichannel one
  reaching all of its.
- The screen material present and showing the picture.
- The networking component present and its permission fields as intended.

## The Picture foldout on the output components

`BasisVideoPicture` holds each output's brightness, contrast, saturation and
gamma, and both output components show it in the inspector. It is a plain
struct and serialises only because it is marked `[System.Serializable]`;
without that the foldout comes up empty, and nothing reports it at compile
time.

| Row | What it proves | How to run |
| --- | --- | --- |
| The foldout is populated | On `BasisVideoMaterialOutput`, the Picture foldout shows four sliders at their default of 1 (gamma included) rather than being empty | Select a player's material output in the inspector |
| The values persist | Move a slider, enter and leave Play Mode, reopen the scene: the value is still there | |
| Brightness reaches the picture | On `BasisVideoDisplay` it multiplies into `RawImage.color`; the other three need a material carrying the `_Basis*` properties | |

## Known behaviour worth recognising

**Media starts loud and drops, once, at scene start.** Basis's scene
initialisation moves every `AudioSource` with no mixer group into the World
group, and **Play On Start** begins playback before that happens, so the first
second or two plays at full level and then drops onto the group. The sound the
player produces is level throughout. Opening a URL after the scene has settled
shows no step.

**The first Play Mode after a domain reload looks like a stall.** A source
opened by **Play On Start** freezes and then races to catch up. The editor's
first frames take seconds while the engine keeps decoding, so frames pile up
and are shown late all at once; the engine's capture for such a run shows no
drops. Judge startup in a standalone build, or on the second load in the
editor.

## In-band user data

`BasisMediaPlayer.UserDataReceived` is covered in the engine guide up to the
point where messages are handed over ("SEI user-data lane"); what a person has
to check is the delivery timing, which only exists with a playback clock
underneath it.

| Row | What it proves | What good looks like |
| --- | --- | --- |
| Ordered delivery | A script subscribing and logging, against `Native~/fixtures/h264-sei-userdata-640x360-30fps.ts` over HTTP | Frame indices 0 to 179 arrive in order, one per video frame, each as the position crosses its timestamp; x264's `dc45e9bd-…` message once at the start |
| Seek | Seek back mid-clip | Logging resumes from the landed frame with nothing from the old position in between |
| Late subscriber | Subscribe a few seconds in | The first message is the one due at the moment of subscribing, neither a replay of the opening nor anything later |
| Subscriber one frame late | A component that assigns its player reference after `Open` and subscribes from its own `Update` (one tick after the player's first drain) | It receives the stream's first message. An on-demand open buffers the whole start before the first frame shows, and none of it may be lost to a subscriber that was a frame away |

## The admin media lock

`BasisNetworkModeration.MediaPlayerBlockedLocally` is the instance-wide
moderation lock: a client under it may not load media at all, in either
direction, and admins are exempt. It is enforced on the client: the check in
this package is the whole gate.

It is checked in two places, and both are needed. `Open(string)` is where every
route ends up (the two-source overload, `OpenUserUrl`'s direct path, a
resolver's `OpenResolved`, and the internal reopen), and it catches everything,
including another client's synced state. `OpenUserUrl` refuses before that as
well, because a page URL leaves the method for the resolver and comes back
through `OpenResolved` only after extraction has happened; a locked client
should not hand the URL out in the first place.

| Row | What it proves | How to run |
| --- | --- | --- |
| Locked client cannot load | With the lock on and no bypass permission, a URL entered locally is refused with a `Video`-tagged warning naming the entry point, and no session opens | Set the lock on the server, then type a URL into the panel |
| Locked client is not driven by a peer | A second, unlocked client loading a URL does not start playback on the locked one: the synced state goes through `OpenUserUrl`, so the same check applies | Two clients, one locked |
| A page URL never reaches the resolver | With the lock on, a YouTube or Twitch watch page produces the refusal and **no** resolver activity in the log | Needs the yt-dlp integration installed |
| An admin is exempt | The same three rows with the `*` permission held: all of them load normally | |

None of these has run yet. They need a server that can set the lock.

## The diagnostics event drain

The engine's structured events reach the Console through `DrainEvents`. Each
line carries the session's own clock, so it can be lined up against
`BasisMediaFrames.csv` and `BasisMediaEngine.csv` without converting between
time bases:

```
[BasisMedia +12.412s] AudioTrim/AudioRing: serve trimmed 70654 frames
```

Codes and stages mirror the engine's own sets (`BmEventCode`, `BmStage`).
Both only ever grow. A plugin newer than the managed side prints the number
rather than a name, which is a gap in the mirror, not a failure.

| Row | What it proves | How to run |
| --- | --- | --- |
| Named codes and a time base | Events read as `Code/Stage` with a session-relative timestamp, not `event 15 stage 6` | Play any source and read the Console; a `StateChange/Clock` line lands within the first second of every open |
| No stack traces in the Editor | Log-level Console lines carry no call stack, so a drain is readable during a pass. Editor only: the setting is application-wide, and a build would lose every package's Log-level traces to tidy this one's | Any scene holding a player; the first one to wake applies it |
| An unknown code falls back to its number | A plugin ahead of the mirror still produces a readable line | Temporarily delete `AudioTrim = 15` from `BmEventCode`, replay a source that trims, confirm the line reads `15/AudioRing`, restore |
| A burst arrives whole | The drain empties the queue in the frame it ran, rather than leaving a backlog | Drop `EventDrainBatch` to 4, open a source, confirm the open's events still all appear on the same frame; restore |
| A full log says so | The drained sequence is never quietly incomplete: the engine's log has a cap, and what it refuses is reported once per step rather than every frame it stays non-zero | Drop `SessionDiag::default`'s cap from 1024 to 2 in the engine, rebuild the plugin, play a source; the Console warns `diagnostics log full: N event(s) refused, M this session`. Restore |
| A cut detail ends in `…` | A Console line that ran past the record's 116 bytes says so, rather than reading as a sentence the engine stopped writing | Open a URL long enough to overrun the detail of the refusal it provokes (a bad host with a long path is easiest) and read the `Error/Source` line |

The engine's free-text diagnostics are a second channel, drained by
`BasisMediaLogDrain` and prefixed `[BasisMedia process +N.NNNs]`. **That clock
counts from the plugin's first diagnostic, not from a session's start**, so the
two prefixes are not comparable. It is pumped from every player's update and
deduplicated per frame, so it needs at least one player alive: a client with no
media player in the scene never loads the plugin, which is deliberate.

| Row | What it proves | How to run |
| --- | --- | --- |
| Transport reaches the Console | The `rtsp transport: …` line, saying whether RTSP negotiated UDP or TCP, appears in the Console with nothing else attached | Play `rtsp://<test-host>:8090/imax51`; the line appears within the first second |
| One copy per line | The drain is process-wide, not per player, so three players must not print everything three times | Put three players in a scene, open a stream on one, count the `process` lines |
| Level picks the Console severity | A refusal reads as a warning and a failure as an error, rather than everything arriving flat | Open a bad URL: the `session error:` line is red. It uses the unreported error path, since an engine failure is the stream's or the network's fault, not a client fault, and must not raise a crash report |
| An overrun ring says so | The queue holds 512 lines engine-side and drops its oldest, so a stall in the drain loses the start of what follows, not the end | Leave a session erroring in a tight loop with the editor paused, then resume |
| A burst costs frames, not one frame | Neither drain does an unbounded amount of work in one frame: both stop at a ceiling (256 events per update, 128 log lines per frame) against native queues of 1024 and 512, and leave the rest for the next frame | Drop `EventDrainPerTick` to 8 and `DrainPerFrame` to 4, provoke a burst, and confirm the lines still all arrive, over several frames rather than one. Restore |

`<test-host>` is the test server described in the engine guide.

## What still needs a person

Picture and sound quality, in a headset, on the live transports. No harness
judges judder or A/V sync the way an ear and an eye do, and the device passes
that matter most need someone wearing the headset.

Everything in the sections above, too. None of it has an automated test:
shared playback needs a second client, the cap needs a populated scene, and
the panel needs someone to look at it.
