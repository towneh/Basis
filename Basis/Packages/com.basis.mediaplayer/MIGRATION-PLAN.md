# Media player migration plan

How `com.basis.mediaplayer` (Rust engine) replaces `com.basis.mediaplayer`
(C engine), what is already built, and what has to be true before the swap.

The C package is gone from this project and this one has taken its component
GUIDs and assembly names, so a prefab or scene built against it binds here
instead of showing missing scripts. The parity gates at the bottom of this
document are not all met; what is still missing is listed against each stage.

## Why replace it

The C engine is about 18k lines of hand-written C and C++, of which roughly
9k is the portable protocol and container layer. That layer parses
attacker-controlled bytes off the network in a language with no bounds
checking, and it is the part the Rust rewrite puts in
`#![forbid(unsafe_code)]` crates. What stays unsafe by necessity (platform
decoder FFI, GPU interop) is a small surface behind a typed boundary.

The rewrite also consolidates behaviour that grew in place in the C player:
one buffering model instead of separate VOD and live branches, one clock
instead of per-path pacing, and a demuxer stack that is the same code on
every platform.

## Package layout

```
com.basis.mediaplayer/
  package.json
  Runtime/
    Basis.Media.asmdef              the component family and its Basis glue
    BasisMediaPlayer.cs             the MonoBehaviour
    BasisMediaNative.cs             ABI v2 P/Invoke surface
    BasisMediaCapabilities.cs       capability blob query + cache
    BasisMediaPlayerRegistry.cs     every live player, for panels and governors
    Audio/                          PCM seam, splitter, per-output taps, channel outputs
    Rendering/                      output sinks, caption overlay, output maths
    Subtitles/                      sidecar track model, json3 parser, cue lookup
    Resolver/                       URL router, resolver seam, resolved-media model
    Diagnostics/                    per-frame capture, for what the engine cannot see
    BasisMediaSettings.cs           client-persisted decode preference
    SettingsProviderMedia.cs        Settings > Developer section
    Plugins/
      x86_64/basis_media.dll
      Android/arm64-v8a/libbasis_media.so
  Editor/                           inspectors + scene-setup menu
    StyleSheets/                    UXML layouts and the shared bvp- stylesheet
  Localization/Languages/en.json
  Native~/                          the Rust workspace (not imported by Unity)
```

One runtime assembly, referencing the framework directly, as the C package's
does. The URL check and the main volume read call the framework where they
need it rather than through installable seams, so there is no arrangement in
which the package compiles but a check is missing.

Unity ignores `Native~` because of the trailing tilde, so the Rust workspace,
its fixtures and its `third_party` vendoring cost nothing at import time.

### Name collision

Both packages export a `BasisMediaPlayer` MonoBehaviour. The C one is in the
global namespace; the v2 one is in `Basis.Media`. That is what lets them
coexist. This package references nothing of the C package, so its own code
never sees both, and the collision is only a hazard for a third assembly that
references both and names a type unqualified — the bare name binds to whichever
of the two its enclosing namespace reaches first. Alias both there.

It is not only the player. Every type this package grows with a name the C
package also uses — the registry, the output sinks, the caption overlay —
shadows its native counterpart the same way. The compiler catches most of it,
but not all: a `GetComponentInChildren<T>` bound to the wrong `T` compiles and
then finds nothing.

## Stage 1 — engine (done)

The Rust engine is feature-complete against the C player's decode and
transport surface, and then some. What it does today:

| Area | Support |
| --- | --- |
| Containers | MP4 (faststart, trailing moov, fragmented), MPEG-TS and m2ts, Matroska/WebM, HLS (VOD and live, TS and fMP4), raw FLAC/MP3/ADTS/Ogg-Opus |
| Transports | HTTP(S) ranged and live, RTSP over TCP-interleaved and UDP with fallback, RIST (Main profile, plain and PSK-AES), WHEP, local files |
| Video codecs | H.264, HEVC, VP9, AV1 |
| Audio codecs | AAC, MP3, FLAC, Opus, up to 7.1 channels |
| Other | CEA-608 captions, seek across every seekable container, live reconnect with backoff, runtime capability reporting, split video-only + audio-only sources played as one session |

Platform notes that matter for parity, because they are refusals rather than
degradations:

- **HEVC is Windows only.** HEVC over MPEG-TS is refused outright: TS carries
  no dimensions ahead of decode, and the platform HEVC decoder faults on
  sizeless input. The C player takes the same posture.
- **VP9 and VP8 are platform-decoder-or-refusal** on every platform. There is
  no bundled software floor for them and none planned.
- **AV1 decodes in software** through a pure-Rust decoder on desktop. Quest Pro
  has no AV1 decoder of any kind, so AV1 refuses there.
- **Hardware decode on Windows** goes through D3D11VA with a software fallback
  rung. Measured against software decode on the same 1080p30 H.264 lane, it
  costs about a seventh of the CPU.
- **Android** decodes through MediaCodec and presents through a Vulkan compute
  pass into a Unity render texture. There is no software rung there.
- **Linux** builds and runs headless. It has no hardware decode path yet.

Buffering, pacing and sync are modelled rather than tuned by feel. The bank's
sizing behaviour is held to recorded delivery-gap distributions from impaired
live captures, committed as fixtures and run as tests.

### Verification already in place

- `Native~/tools/ci.ps1` and `ci.sh`: formatting, clippy at `-D warnings`, the
  test suite, licence and advisory gates, supply-chain audit.
- Byte-exact conformance against ffprobe for demuxed packets, including
  keyframes, on every container fixture.
- Fuzz targets for every parser that touches the network, with seed corpora
  replayed under AddressSanitizer per commit and longer nightly campaigns.
- An impairment harness that replays measured network gap profiles through the
  full engine and grades the result against the buffering model.
- Unity batch-mode playback autotests on Windows, and on-device passes on
  Quest Pro covering the live transports, seek, and memory and CPU budgets.

GitHub Actions runs the Windows, Linux, Android and fuzz-replay jobs on every
push touching the engine. The C player's pipeline is untouched and its path
filters are disjoint, so neither package's changes trigger the other's build.

## Stage 2 — coexistence in Basis (done)

Both packages resolve, compile and load side by side. Nothing needed changing
in the C player to get there:

- Assembly names differ (`Basis.Media` against `BasisMediaPlayer`).
- Namespaces differ, as above.
- Native plugin filenames differ (`basis_media` against `basis_media_native`),
  so the importers do not collide.
- Menu paths differ.

The v2 engine is reachable two ways, both developer-only:

**In the editor.** `Basis > Tools > Media Player > Create Test Player` drops a
screen, a v2 player and a stereo output into the open scene, pre-filled with a repo fixture
path when one is present. `Create Test Player (Surround)` gives it the eight
positioned speakers instead, which is what listening to a 5.1 or 7.1 source
needs.

**At runtime.** `Settings > Developer > Media Player` carries the decode-route
preference and a readout of what the engine probed on this machine, which is
what says whether "hardware only" can play anything here.

The decode preference is a per-user client setting, stored in the user's
settings file and applied to the engine as a static. It is deliberately not a
serialised inspector field: it describes the machine, not the world, and a
serialised field would drift into prefabs.

## Stage 3 — in-Basis verification (in progress)

1. **Editor smoke.** Create Test Player, enter play mode, confirm picture and
   sound. First time this has been runnable inside the Basis project itself.
2. **In-world A/B.** Judge this engine against the C player on the same source
   by eye and ear: picture, A/V sync, judder, join latency, seek. The two
   engines no longer coexist in one project, so the comparison runs against a
   build of the C player from the development fork.
3. **Parity measurements** against the C player in the same world, per platform:
   CPU per stream, GPU memory per session, time to first frame, seek settle,
   memory footprint. Desktop numbers exist for the v2 engine in isolation;
   the same-world comparison and the Quest baselines for the C player do not.

   These are read off the diagnostics surfaces rather than judged by eye, and
   `Native~/TESTING.md` carries the column contracts, what each metric means and
   the healthy band for each — so a run is graded mechanically and an automated
   pass can assert the same things a person would look for.
4. **The transport matrix** through the Basis UI rather than the headless
   harness, across editor, Windows standalone and Quest.

## Stage 4 — managed parity (the real work)

The engine is ahead of the C engine. The **managed layer is well behind**, and
this is the bulk of what remains. The C package ships a component family the
v2 package has not grown yet:

| C package feature | v2 status |
| --- | --- |
| Multi-channel audio output across per-channel `AudioSource`s | Ported |
| Audio tap, per-output volume, filter ordering, AudioLink analysis feed | Ported |
| Caption overlay rendering | Ported |
| Sidecar subtitles (JSON3 parser, subtitle track selection) | Ported |
| Networked playback, ownership and permissions | Not started |
| Player registry and the in-world Media Players panel | Registry ported; panel not started |
| URL resolver seam and the yt-dlp integration | Ported; needs a run with the yt-dlp package installed |
| Host trust and URL security gating | The subtitle fetch is checked; host trust not started |
| Material and uGUI output sinks, aspect and projection modes | Ported |
| Diagnostics component and editor debug window | Ported |
| Bitrate and audio track selection | Not started |
| Streaming example component | Ported |
| Playlist example component | Not started |

### Networked playback, and what it takes to land

The largest gap, and the one the shipped prefabs feel: drop either of them into
a world today and every client plays independently. No shared playhead, no
ownership, no permission gating on who may change the URL.

The engine half is already built and tested. A session can be given a target
position and converges on it through a dead band, then a bounded rate slew,
then a seek as the last resort. What is missing is the managed protocol above
it and the ownership model.

Two things are staged for whoever picks it up, so the prefabs rebind rather
than needing rebuilding:

- `Editor/StyleSheets/MediaPlayerNetworkingSDK.uxml` is already here, so the
  inspector has its layout waiting.
- The C components' script GUIDs are free and should be adopted by their
  replacements, the way every other ported component adopted one:
  `9313d54e9f39acb40861404700a309d4` for the component and
  `1674ca56f182ca646b6ee08b3e5ad8be` for its inspector. A prefab or scene in
  someone's world that carried the old component then binds to the new one.

Two of these are not straight ports, because the engine already does the work
the C managed layer had to do itself:

- **Captions.** The engine parses CEA-608 and hands over timed cues, and the
  player holds each one until playback reaches it, so the overlay that draws
  them is a presenter and nothing more. Sidecar subtitles arrive at the same
  overlay through the same caption state: selecting a track suppresses the
  in-band feed, and reverting to -1 brings it straight back.
- **A/V sync.** The engine owns the clock, so the managed layer does not need
  its own sync component. The networked-playback work still needs a
  shared-position protocol, and the engine side of that is built: a session
  can be given a target position and will converge on it through a dead band,
  a bounded rate slew, and a seek as the last resort.

### Audio: the same components, on the same AudioSources

The managed audio model is kept rather than redesigned, so a world built against
the current player finds everything where it was. `BasisMediaPlayerAudio` holds
the output list; each output is an `AudioSource` carrying a
`BasisMediaAudioChannel` naming the channel it plays and a
`BasisMediaPlayerAudioTap` generating that channel into its DSP blocks. A
`BasisMultiChannelPcmSplitter` de-interleaves once and broadcasts, so two
outputs can play the same channel from different places. The 7.1 arrangement —
eight GameObjects named "Channel 1 - Front Left" through "Channel 8 - Side
Right" — is the contract, and `BasisMediaAudioRig` builds both it and the
one-output stereo case in code, so the scene-setup menu items produce the
arrangement the prefabs author rather than inventing one.

What the swap changes:

- **The player has no audio output of its own.** It implements
  `IBasisPcmSource` and nothing else: one interleaved ring at the stream's rate.
  Everything above that — de-interleaving, per-speaker routing, downmixing,
  device rate conversion, spatialisation — belongs to the audio components,
  which is what makes per-channel positioning and per-output filters possible at
  all. A player with no audio component beside it is silent, deliberately.
- **The ring stays interleaved.** Spec §6.9 describes it as planar, but the
  managed model §6.9 promises to keep consumes interleaved and de-interleaves in
  the splitter. Keeping interleaved *is* keeping that model, and it costs no
  engine or ABI change; a planar ring would mean converting on both sides of the
  boundary to arrive at the same place.
- **The sink folds in the main volume.** Audio generated in
  `OnAudioFilterRead` reaches the listener without passing the stage the main
  volume slider drives, so the sink reads the framework's figure itself, once
  per frame on the main thread.
- **The sync trim reaches the pull.** Shared playback converges by consuming the
  ring slightly fast or slow, so the engine's wanted rate offset travels through
  the source seam and lands on the splitter's per-reader cursor, which carries
  its own sub-sample remainder. A trim therefore slews the pull without
  restarting the interpolation. The C player has no equivalent; this is the one
  place the port gains behaviour rather than matching it.
- **Output latency is reported to the engine on Android only**, on the measured
  grounds that the desktop offset sits inside the sync noise floor. The sink
  computes and exposes the figure on every platform, so the debug window and the
  frame capture can show it either way.

The prefabs are still to rebuild. Most of what the current ones carry beyond
audio — the caption overlay, networking — is still to port, so they are built
once rather than grown; the audio half of that rebuild is now expressible.

Also outstanding on the Basis side:

- **Concurrent session governor.** Prop-spawned media players make the number
  of live sessions per client unbounded, and each 1080p session costs real
  bandwidth and memory. A client-side cap on active sessions, with dormant
  players holding a remembered URL and position rather than an open session,
  plus nearest-N activation with hysteresis.
- **Resolver integration.** The existing resolver seam is typed against the C
  player. Making it engine-agnostic is the smaller half; the yt-dlp
  integration is the consumer that has to keep working.

  The engine prerequisite is now in place. A resolver's interesting output is a
  *split* source — a video-only stream and the audio-only one that belongs with
  it — because that is how adaptive ladders serve every rung above their muxed
  fallback. Wiring the resolver up before the engine could open a pair would
  have capped YouTube on-demand at the progressive rung, which is a visible
  regression against the C player rather than a missing feature. `audio_url`
  carries the second leg through the descriptor, and the component exposes it
  as `audioUrl` plus a two-argument `Open`.

  Both halves have since landed. `Runtime/Resolver/` holds the router and the
  resolved-media model; the yt-dlp package grew a core assembly with the format
  ladder in it, knowing about neither player, and a thin adapter per engine.
  The v2 adapter is a separate assembly because an assembly's references
  resolve whenever its define constraints are met, so one assembly naming both
  players would break for anyone who has installed only one.

  **Not yet run.** The yt-dlp runtime package is not installed in this project,
  so every one of those assemblies is compiled out here and none of this has
  executed. It is verified by compiling each assembly standalone against the
  yt-dlp assembly, which is not the same thing as a resolve. First real run
  wants: a YouTube VOD above 360p (the split pair), a Twitch or live YouTube
  stream (the muxed HLS path), and a video with captions (the subtitle tracks
  now have a producer).

### Editor UI: the target is the same navigation experience

Whoever has been using the C player should not have to relearn anything. The
goal for every ported inspector is the same visual language and the same way
around: the same cards, headers, grouping and spacing, and the same order and
shape of sections, so a user's route through a component is unchanged.

Field lists will differ, and that is expected rather than a failure to match.
The two engines expose different things. v2 surfaces bank depth in
milliseconds, a sync rate in ppm, a decode-route preference and a runtime
capability set, none of which the C player has a concept of; some C player
fields have no v2 counterpart. Where the data allows, mirror the C layout.
Where it does not, keep the same visual treatment and let the content differ.

This is cheap to hold to, because the appearance is data rather than code.
Every C inspector is UIElements, not IMGUI: one shared stylesheet
(`MediaPlayerSDK.uss`, `bvp-` prefixed classes) carries the whole look, each
component has its own UXML for layout, and the C# is thin binding glue that
clones the tree, attaches the sheet and wires fields by name. The UXML uses
only stock `<engine>` and `<editor>` controls — no custom control from either
package — and the stylesheet has no `@import` and no `url()`. It is portable
as-is.

The approach:

- **Copy the stylesheets into this package** under `Editor/StyleSheets/`,
  keeping the filenames and class names identical. Do not reference the C
  package's copies: permanent code here must not depend on the package it
  replaces, or the swap breaks it.
- **Resolve the asset path from the assembly, not a string constant.** The C
  inspectors hardcode `Packages/com.basis.mediaplayer/Editor/StyleSheets/...`,
  which a package rename breaks. Ask the package manager instead, and the same
  code is correct before and after the cutover rename:

  ```csharp
  static string PackagePath => UnityEditor.PackageManager.PackageInfo
      .FindForAssembly(typeof(BasisMediaPlayerEditor).Assembly).assetPath;
  ```

- **Port per component**, landing the runtime component, its UXML and its
  inspector together, so each one is finished rather than half-done.

Holding the filenames identical means there is no migration step for the UI at
all. Delete the C package, rename this one, and the assets sit at exactly the
paths the old ones did.

Coexistence is safe meanwhile: custom editors resolve by target type so the two
sets never compete, a stylesheet attaches to the visual tree that asks for it
rather than globally, and the copies carry their own GUIDs.

Duplicating the stylesheets across both packages during the coexistence window
is deliberate. Hoisting them into a shared package would mean editing the
shipping C player to point at new paths — risk on working code — to avoid a
drift that shows up as a visible difference in appearance rather than a silent
bug, over a window that ends at cutover.

### Shared assets: reuse rather than fork

The stylesheets are copied because appearance drift between two installed
inspectors would be visible and the window ends at cutover. Assets with nothing
to do with the engine swap get the opposite treatment: reuse them.

The screen shader is the case in point. `Basis/Media Player Video` is URP's
Unlit with one added branch — out-of-range UVs paint black, which is what makes
a letterboxed source letterbox instead of smearing its edge texel. Nothing in it
knows which engine fills the texture. So v2 resolves it by shader name rather
than by package path or GUID, which leaves no asmdef reference, no meta
dependency and nothing to fix up when the folder is renamed. The screen material
is reused the same way.

URL security is the same argument with more at stake. The engine vets its own
media legs, but a sidecar subtitle fetch goes out over `UnityWebRequest` and
never reaches it, so it needs a check of its own — and a second copy of
address-classification logic is exactly the thing that drifts out of step with
the original. The subtitle fetch calls the framework's `BasisUrlSecurity`
directly, which every other download in the client already answers to: the
literal address check first because it needs no network, then the DNS check
that closes the name-that-resolves-to-a-private-address hole.

The same goes for the streaming prefabs. The v2 equivalents are a
reimplementation — the components are different, so the prefab has to be rebuilt
rather than copied — but they keep the assets the current ones use: the same
screen material, the same AudioSource configuration and spatialisation, the same
caption canvas and mesh. What changes is which components are wired into the
hierarchy, and nothing else.

They get built once the component set is complete rather than grown alongside
it, since some of what the current prefab carries — the caption overlay,
networking — is still to port. The audio hierarchy is settled: same output
GameObjects, same components on each, same speaker positions, and
`BasisMediaAudioRig` already assembles both arrangements in code.

## Stage 5 — engine backlog

Not parity blockers on their own, but worth clearing before the swap:

- WHEP loss recovery. Packet loss is reported but never acted on; no
  retransmission or keyframe requests are sent.
- HLS fMP4 seek settling more slowly than the other containers.
- Frame-pacing re-grade on device after the vsync-locked selection change.
- Android HTTPS trust anchors verified on hardware.
- Hardware decode on Linux.
- Field probes of the Windows hardware path on AMD and Intel GPUs, and
  confirmation that software rasterisers refuse rather than claim support.

## Stage 6 — cutover mechanics

Once the gates pass, in this order:

1. **Ship binaries for every platform the C player covers.** Today that means
   adding Windows ARM64 and Linux x64 to the committed plugins; the C package
   ships both and v2 ships neither.
2. **Rename the package** back to `com.basis.mediaplayer`: folder,
   `package.json` name and display name, and path references. The
   `Basis.Media` namespace, the assembly names and every `.meta` GUID stay
   exactly as they are, so scenes and prefabs referencing the v2 components
   survive the rename. This is the one step where getting it wrong is
   expensive.
3. **Migrate scenes and prefabs** from the C components to the v2 ones.
4. **Delete the C package**, its native tree, and its CI pipeline.
5. **Move the decode preference to a user-facing settings section.** It sits
   under Developer today because that is the only extension point the
   framework exposes; a real section needs a hook adding.
6. **Drop the "v2" wording from everything a user sees.** It exists only to
   tell the two engines apart while both are installed, and it is the easiest
   thing to leave behind. At least: the `Basis/Media v2/...` menu paths, the
   debug window's menu entry, the component's `AddComponentMenu` label, the
   Developer section title, and the `settings.developer.mediav2.*`
   localisation keys and their values. Grep for `v2` and `mediav2` across the
   package and the localisation files rather than working from this list.

Steps 2 and 6 are the two that leave a mess if half-done. Nothing about the
editor UI needs migrating: the stylesheets and UXML are already at the
filenames the C package used, so step 2's rename lands them on the old paths,
and the inspectors resolve their own package path rather than a constant.

## Gates

The swap does not happen until all of these hold.

**Correctness**

- The transport and codec matrix passes in the editor, in a Windows standalone
  build and on a Quest, driven through the Basis UI.
- Every managed feature in the Stage 4 table is either ported or has a written,
  accepted reason for dropping it.
- No regression against the C player on any stream that plays today. Streams
  the C player cannot play are a bonus, not a substitute.

**Performance**

- CPU per stream at or below the C player on the same source, same machine,
  on Windows and on Quest.
- GPU memory per session at or below the C player's.
- Time to first frame and seek settle at or below the C player's on every
  transport, live joins included.
- The budgeted number of simultaneous sessions per platform holds, measured
  rather than asserted.

**Operational**

- CI green across Windows, Linux, Android and fuzz replay.
- Nightly fuzz campaigns clean for a sustained run against the shipping tree.
- Supply-chain and licence gates green, with notices present for every
  vendored dependency.
- Shipping binaries built by the release pipeline rather than a developer box.

## Verification map

| What | Where |
| --- | --- |
| Engine test suite, lint, licence and supply-chain gates | `Native~/tools/ci.ps1`, `Native~/tools/ci.sh` |
| Manual and device test matrix | `Native~/TESTING.md` |
| Fuzz targets, seeds and campaign notes | `Native~/fuzz/` |
| Buffering model fixtures | `Native~/media-testkit/fixtures/phase0/` |
| Headless harness player | `Native~/bm-probe` |
| Device codec dumps | `Native~/docs/probes/` |
