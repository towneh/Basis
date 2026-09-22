# Testing the player in Unity

Manual checks for the managed side: shared playback, the panel, the session
cap and the prefabs. None has an automated test. The engine's tests are in
[`Native~/TESTING.md`](Native~/TESTING.md), and
[`Native~/DIAGNOSTICS.md`](Native~/DIAGNOSTICS.md) explains the captures.

| What | How |
| --- | --- |
| Engine gate: format, lint, tests, audits, ffprobe conformance | `Native~/tools/ci.ps1` (Windows), `Native~/tools/ci.sh` (Linux); each step is described in [`Native~/TESTING.md`](Native~/TESTING.md#running-the-tests) |
| Headless playback | `Native~/bm-probe` |
| Graded playback in the editor | `Basis > Tools > Media Player > Run Smoke Test` |
| The same in batch | `-executeMethod BasisMediaSmokeTest.RunBatch` (exit code 0 on a pass; steer it with `BASIS_SMOKE_URL`, `BASIS_SMOKE_SECONDS`, `BASIS_SMOKE_LIVE`, `BASIS_SMOKE_STRICT_HOLDS`) |

With the project open in Unity, the managed assemblies can be compiled on their
own against `Library/ScriptAssemblies` with the editor's Roslyn compiler. That
shows they build, nothing more.

## Shared playback

Needs two clients in the same world. `Basis > Tools > Media Player > Test
Scene > Shared Playback (two clients)` builds a scene with one networked player
and no source set: use a URL both clients can fetch, not a local file. The
lettered clips (see [Session cap](#session-cap)) show a running timecode for
comparing the two screens.

For the second client, save the scene, add it to Build Settings, and run a
standalone build alongside the editor. Drive the panel from the editor as
owner, except for **Owner leaves**, where the build is the owner and is closed.
Give the build its own capture filenames first; both write to the same
`persistentDataPath`.

| Row | Do | Expect |
| --- | --- | --- |
| Load | One client sets a URL | The other plays it |
| Transport | Play, pause and stop from the owner | Each reaches the follower |
| Seek | Owner scrubs, playing and then paused | The follower lands within a frame or two. Paused, both show the new frame and stay paused |
| Convergence | Watch the follower for half a minute after a seek or late join | It slews onto the owner without visible jumps |
| Late join | A client joins mid-playback | It lands at the owner's position |
| Late join, owner paused | Owner paused; a client rejoins. Try a direct `.mp4`, a page URL, and the owner pausing (or resuming) while the client loads | It lands paused on the owner's frame, silently, and plays when the owner does. If the owner resumed during loading, it comes up playing at the owner's position |
| Ownership | The second client takes control | The first becomes a follower; neither fights |
| Owner leaves | Close the owner mid-playback | The follower keeps playing |
| Admin only | `AdminOnly` on, second client without permission | It cannot take control and has no playback tab |
| Open to everyone | `AnyoneCanControl` on | The other client gains the controls without reopening the panel |
| Live source | A live stream | No position sync; each client at its own live edge |
| Stalled owner | Stall the owner's network | The follower holds position |
| Resync everyone | Drift a follower, owner presses **Resync Everyone** | All clients land on the owner's position. No prompt |
| Resync keeps the owner's place | **Resync Everyone** while playing, on a direct `.mp4` and a page URL | The owner carries on from where it was, not from zero |
| Resync while stopped | **Resync Everyone** after stopping, URL still held | Nothing happens |
| Resync a page URL | **Resync Everyone** well into a YouTube or Twitch source | All clients land together at the owner's position, not zero, with no seek before the reload. The shared URL stays the page URL |
| Join mid-resync | Join during a resync reopen | Lands at the owner's position, not near zero |
| Local resync | A follower presses **Local Resync** | Only that client reopens |
| Inspector buttons | The same two presses from the networking component's inspector, in Play Mode | Same as from the panel |
| Untrusted URL | The owner sets a URL on a host outside `BasisTrustedUrls` | The follower loads it with no prompt |
| A video hours long | Owner seeks deep into a fragmented video of two hours or more, then a client joins. Try a direct URL (see the engine guide's "A fragmented MP4 hours long") and `https://www.youtube.com/watch?v=aNS5o3VJ0-A` (11 h, H.264) | Opens as fast as a short video; the joiner lands beside the owner |

No follower may snap to the start during any resync, even briefly. With the
yt-dlp package installed, also check that page URLs reach other clients as page
URLs, never as `googlevideo` or other CDN URLs.

## Media Players panel

One client, in a scene with more than one player.

| Row | Do | Expect |
| --- | --- | --- |
| Visibility | A scene with no players, then one | The menu entry appears with the first player |
| Selection | Switch players | Status, URL and controls follow |
| Scrubber | Drag the seek bar, playing and paused | One seek when the handle stops, no bounce back, and the frame at the dropped position (not the keyframe before it). Paused stays paused |
| Slow decoder | Start the player with `BASIS_MEDIA_SLOW_VIDEO_DECODE_MS=80` in its environment and play an on-demand source with sound, and one with captions or SEI data | Sound unbroken at real time. The picture moves, holds and rejoins, in step whenever it moves. Captions and user data stay on time. `LateVideoSkip` in the Console. Unset the variable afterwards |
| Long pause | Pause a minute, then play, on a video and an audio-only file | Resumes with no error |
| Live stream | Open one | The seek bar hides |
| Captions | Toggle captions, switch player | The setting applies to every player |
| Subtitles | A source with subtitle files | The language row shows only with captions on |
| Status | Watch a load | Connecting, Buffering, Playing, with the resolution; errors show a code |
| Admin tab | With and without `*` | Shown only with it, on a networked player |
| Markup injection | A title containing `</noparse>` and `<b>` | Shown literally |

## Session cap

`Basis > Tools > Media Player > Test Scene > Four Players (7.1)` (or `(Stereo)`)
builds four players ten metres apart along +X. Walk along +X to change which
are nearest. The cap is set in `Settings > Developer > Media Player`.

The players use the lettered clips in `Native~/fixtures/captest`: four 60 s
H.264 clips with a keyframe every second, each showing a letter, a colour and a
running timecode, and playing a different genre of music. The music is Kevin
MacLeod (incompetech.com), CC BY 4.0, attributed in `THIRD_PARTY_NOTICES.md`.

| Row | Do | Expect |
| --- | --- | --- |
| Cap holds | Four players, cap 3 | The furthest reads Dormant |
| Waking | Walk to a dormant player that had been playing | It resumes near where it would have been; a further one goes dormant |
| Live waking | The same with a live source | It rejoins at the live edge |
| No flapping | Stand between two players and move gently | Sessions do not repeatedly open and close |
| Promotion | Select a dormant player in the panel | It starts; the furthest active one goes dormant |
| Cap lifted | Set the cap to 0 | Every player wakes |
| Startup | A scene where every player autoplays | The cap holds from the start |
| With shared playback | The owner's player goes dormant | Other clients keep playing |

## Prefabs and components

After changing their components, check `MediaPlayerStreaming` and
`MediaPlayerMultiChannelStreaming`: no missing scripts, outputs wired to their
channels (all eight on the multi-channel one), the screen showing the picture,
and the networking component's permission fields as intended.

On `BasisVideoMaterialOutput`, the Picture foldout shows four sliders at 1, and
a moved slider keeps its value after Play Mode. `BasisVideoPicture` must stay
`[System.Serializable]` or the foldout is empty.

The first Play Mode after a script reload stalls for a few seconds; judge
startup in a build or on the second load.

## SEI user data

Against `Native~/fixtures/h264-sei-userdata-640x360-30fps.ts` over HTTP, with a
script logging `UserDataReceived`:

| Row | Do | Expect |
| --- | --- | --- |
| Ordered delivery | Play | Frame indices 0 to 179 in order, each as playback reaches it, and x264's `dc45e9bd-…` once at the start |
| Seek | Seek back | Logging resumes from the landed frame |
| Late subscriber | Subscribe a few seconds in | Starts with the message then due |
| Subscriber one frame late | Subscribe from `Update` after `Open` | Receives the first message |

## URL consent

A URL on a host outside `BasisTrustedUrls` prompts before it opens. Pick a host
that is not on the built-in list (`BasisTrustedUrls.GetBuiltIn()`), and clear
the user list (`ClearAll()`) between rows that remember. One client unless
stated.

| Row | Do | Expect |
| --- | --- | --- |
| Authored URL | A scene player with an untrusted URL and **Play On Start**, enter Play Mode | The prompt shows; nothing opens until **Accept**. **Decline** leaves the player idle |
| Panel URL | Type the same URL into the Media Players panel | Opens with no prompt |
| Declined, then Play | **Decline** the authored prompt, then press the panel's **Play** | Prompts again |
| Re-open | Let a source end, or **Local Resync** with no networking, then **Play** | Re-opens with no prompt |
| Remember | Accept with Remember set to the URL, then the host, then the domain, re-entering Play Mode each time | The next entry does not prompt; `GetUserAdded()` shows the pattern |
| Play while pending | Press the panel's **Play** while the prompt is up, then **Accept** | Plays as soon as the source is up |
| Page URL | An untrusted YouTube page URL, with the yt-dlp package installed | One prompt; the extracted stream opens with no second one |
| Split pair | A script calls `Open(video, audio)` with the audio leg on an untrusted host | Prompts for the audio URL as well; nothing opens until both are accepted |
| World script sets the room's URL | A script calls `SetUrl` on the networking component with an untrusted URL, two clients | The owner is asked first; **Decline** sends nothing, and the follower loads only after **Accept** |
| Prop | A prop whose script calls `OpenUserUrl` on its player | Prompts, then plays |
| Prop bypass | The same prop calling `OpenResolved` or `SetSubtitleTracks` | Refused by the sandbox |
| Avatar auto-start | An avatar carrying a `BasisMediaPlayer` with an authored URL and **Play On Start** | Nothing opens and nothing prompts; **Play On Start** is off on the loaded copy |

Closing the menu does not answer the prompt. It moves to the notification list,
where it can be brought back up or dismissed; a dismissal is a decline.

## Admin media lock

`BasisNetworkModeration.MediaPlayerBlockedLocally` stops a client loading any
media. The client enforces it, in `Open(string)` and in `OpenUserUrl`. None of
these rows has been run; they need a server that can set the lock.

| Row | Do | Expect |
| --- | --- | --- |
| Locked client | Lock on, no bypass permission, enter a URL | Refused with a `Video` warning; no session |
| Driven by a peer | An unlocked client loads a URL | The locked client does not play |
| Page URL | Enter a YouTube URL while locked | Refused, with no resolver activity |
| Admin | The same with `*` | All load normally |

## Console diagnostics

Engine events appear as `[BasisMedia +12.412s] AudioTrim/AudioRing: …`, stamped
with the session clock used in the captures. Free-text engine lines appear as
`[BasisMedia process +N.NNNs] …`, timed from the plugin's first line. A code
newer than the managed side prints as a number.

| Row | Do | Expect |
| --- | --- | --- |
| Named codes | Play any source | `Code/Stage` lines; `StateChange/Clock` within a second of opening |
| No stack traces | Any player, in the Editor | Log-level lines have no call stack (builds keep them) |
| Unknown code | Delete `AudioTrim = 15` from `BmEventCode`, play a source that trims | The line reads `15/AudioRing`. Restore |
| Burst | Set `EventDrainBatch` to 4, open a source | The open's events arrive in one frame. Restore |
| Full log | Set `SessionDiag::default`'s cap to 2, rebuild, play | `diagnostics log full: N event(s) refused, M this session`. Restore |
| Cut detail | Open a very long bad URL | The `Error/Source` line ends in `…` |
| Transport line | Play any RTSP stream (e.g. `rtsp://stream.vrcdn.live/live/vrcdn`) | `rtsp transport: …` within a second |
| One copy | Three players, a stream on one | Each `process` line once |
| Severity | Open a bad URL | `session error:` in red, with no crash report |
| Overrun | Error in a loop with the editor paused, then resume | The oldest lines are dropped |
| Frame budget | Set `EventDrainPerTick` to 8 and `DrainPerFrame` to 4, provoke a burst | Every line arrives, over several frames. Restore |

## Direct3D 11 and 12

Start the Editor or a build once with `-force-d3d11` and once with
`-force-d3d12`, and run each row on both. The log names the API in use: the
project's `Logs/Editor.log` for the Editor, `Player.log` for a build, or the
file given with `-logFile <path>`.

| Row | Do | Expect |
| --- | --- | --- |
| Playback | Play a video with sound, seek forwards and back, pause, resume | Picture and sound throughout, seeks land on their target, the pause holds its frame, and no `consumer open failed` line |
| Render-thread cost | With `BasisMediaPlayerDiagnostics` recording, leave a minute of playback untouched, then a minute with the player idle | Mean frame time within 0.1 ms of the idle minute, and no more than three extra frames over 33 ms. If either misses, run both minutes again; a second miss is a regression |

## Still needs a person

Picture and sound quality in a headset on the live transports, and everything
above.
