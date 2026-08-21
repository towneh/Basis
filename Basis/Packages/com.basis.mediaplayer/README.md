# Basis Media Player

Live and on-demand video — and audio-only media — for Basis. A Rust engine
demuxes and decodes, using the operating system's hardware codecs where they
exist, and presents **zero-copy** into a Unity texture. No transcode server, no
`UnityEngine.Video.MediaPlayer`.

- **Windows (PC / VR)** — Media Foundation for H.264, HEVC, VP9 and AAC/MP3,
  through a D3D11VA hardware decoder with a software rung behind it. AV1
  decodes in software. NV12 to BGRA happens in a D3D11 pixel shader into a
  texture Unity samples.
- **Android (Quest)** — MediaCodec decodes; frames arrive as
  `AHardwareBuffer`s, imported into Vulkan and converted by a compute pass into
  a Unity `RenderTexture`.
- **Linux** — builds and runs, software decode only. No hardware path yet.

The demux and protocol layer is the same code on every platform, and it is
`#![forbid(unsafe_code)]`. What has to be unsafe — the platform decoder FFI and
the GPU interop — is a small surface behind a typed boundary.

## Supported URLs

| Scheme | Use | Example |
|---|---|---|
| `rtsp://` | PC/VR low latency — UDP first, TCP-interleaved fallback | `rtsp://stream.vrcdn.live/live/vrcdn` |
| `rtspt://` | TCP-interleaved pinned, for hosts or networks where UDP never works | `rtspt://stream.vrcdn.live/live/vrcdn` |
| `rist://` | RIST live ingest (UDP, loss recovery, optional AES) | `rist://stream.example:5000?secret=KEY&aes-type=128` |
| `whep://` / `wheps://` | WHEP — WebRTC receive, sub-second join | `whep://stream.example:8889/live/whep` |
| `https://….mp4` | MP4 over HTTPS — fragmented (live), or progressive VOD (faststart or trailing moov, seekable) | `https://stream.vrcdn.live/live/vrcdn.live.mp4` |
| `https://….ts` | MPEG-TS over HTTPS | `https://stream.vrcdn.live/live/vrcdn.live.ts` |
| `https://….m3u8` | HLS, VOD or live, TS or fragmented-MP4 segments | `https://stream.example/live/index.m3u8` |
| `https://….webm` | WebM — VP9 or AV1 video, Opus audio; Cues-indexed files seek | `https://stream.example/vod/clip.webm` |
| `https://….mkv` | Matroska, same codecs | `https://stream.example/vod/clip.mkv` |
| `https://….flac` `.mp3` `.aac` `.opus` `.wav` | Audio-only, direct | `https://stream.example/audio/track.flac` |
| `file://` or an absolute path | Local file | `C:\media\clip.mp4` |

Containers are chosen by sniffing the bytes, not the extension, so an
extensionless CDN URL routes correctly.

**Codecs.** Video: H.264 everywhere, HEVC on Windows, VP9 and VP8 where the
platform decoder has them, AV1 in software on desktop. Audio: AAC, MP3, FLAC,
Opus and integer PCM, up to 7.1 channels. PCM covers both RIFF/WAVE files and
the LPCM carried in Blu-ray-style MPEG-TS.

Some of these are refusals rather than degradations, which is deliberate — a
typed error beats a black screen:

- **HEVC over MPEG-TS is refused outright.** TS carries no dimensions ahead of
  decode and the platform HEVC decoder faults on sizeless input.
- **VP9 and VP8 are platform-decoder-or-refusal** on every platform. There is
  no bundled software floor for them and none planned.
- **AV1 refuses on Quest Pro**, which has no AV1 decoder of any kind.

`BasisMediaPlayer.EngineCapabilities` reports what this machine actually
probed — containers, transports, codecs, and for each video codec whether the
route is hardware or software and up to what resolution and frame rate.

## Live vs on-demand

A live source is presented at the live edge; an on-demand one is paced to real
time, so a file that arrives faster than it plays doesn't fast-forward. The
player works out which from the source itself, and almost always gets it right:
a source that states a length and serves byte ranges is on-demand, and anything
else is a live edge. Range support is judged on an actual `206` answer rather
than an advertised header, so a server that honours ranges without saying so is
still recognised.

Most sources never reach that test. `rtsp://`, `rtspt://`, `whep://` and
`rist://` are live by transport, an HLS playlist settles it with
`EXT-X-ENDLIST`, and a resolver states what it extracted. What is left is a
plain HTTP URL that is not a playlist.

The `liveness` field overrides the answer, and lives under **Advanced** in the
inspector because it should not normally be touched:

| `liveness` | Behaviour |
|---|---|
| `Auto` (default) | Worked out from the source |
| `Live` | Force live-edge buffering, no read-ahead |
| `Vod` | Force real-time pacing with read-ahead |

Set one only to overrule a server whose headers mislead. The misread that does
happen is one-directional — an on-demand file served with neither a length nor
ranges reads as live — and the engine records a diagnostic event when it
decides that way, so it shows up in a capture rather than as a mystery.

The jitter buffer is a viewer setting rather than an authored one, because what
it trades off is that viewer's own connection against how soon they see a frame,
which a world cannot know. `0` is Auto, which sizes itself from the
delivery-delay distribution it observes and is right for almost everyone.

It is set in two places. Settings > Developer > Media Player carries the default
every player follows; the Media Players panel, My Settings, under Advanced tunes
the player you have selected and pins it there. Per player because one scene can
hold both a source next door, where the point is the latency a shallow buffer
buys, and one from the far side of the world that only plays smoothly with depth
behind it. Either way the change re-opens the affected sessions at the position
they are at, so it can be tuned while watching, and nobody else's playback moves. Buffering, pacing and clock drift
are modelled rather than tuned by feel, and the model is held to recorded
delivery-gap captures from impaired live streams, committed as fixtures and run
as tests.

`audioLeadingStart` is for live sources where the audio is the content and the
picture can trail: playback starts on the first audio rather than waiting for a
video keyframe, which on a long-GOP lane is the difference between two seconds
and ten.

## Seeking

`Seek(double seconds)` requests an absolute position. Sources that report a
duration are seekable; a duration is necessary but not sufficient, since a
transport that cannot reposition still refuses. Playback resumes from the
keyframe at or before the target, so it lands at or shortly before where you
asked. Seeking a live source is refused.

How exactly it lands depends on what the container offers. MP4, Matroska and
HLS have an index and land on a keyframe. WAV has no keyframes and no index it
needs — the offset is arithmetic — so it lands on the exact frame. MP3 and Ogg
Opus have neither, so they estimate: MP3 through its Xing table where the file
has one and a constant-bitrate guess where it does not, Ogg by bisecting on
granule positions. Both report the landing at the request and resume near it,
which is what every player does with these formats. Raw FLAC and raw ADTS
refuse outright rather than guess.

```csharp
if (player.DurationSeconds > 0)
    player.Seek(30);
```

## Split sources

Adaptive ladders serve high-resolution video and audio as separate streams,
because that is how every rung above the muxed fallback is published. Call the
two-argument `Open` and both play on one clock:

```csharp
player.Open(videoOnlyUrl, audioOnlyUrl);
```

A null second argument is an ordinary muxed source, and drops any pair left
over from a previous open. There is no authored field for the audio half: a
resolver works the pair out, and a hand-typed one would only ever be right for
a source someone had split themselves.

## Page URLs and the resolver

The player opens **stream** URLs. It does not itself turn a **page** URL — a
YouTube or Twitch watch page — into a stream. That is a separate, optional
resolver package which registers itself on `BasisMediaUrlRouter`; the player
has no reference to it.

`OpenUserUrl(url)` is the entry point that steers: a directly-playable URL
opens straight through, anything else is offered to the registered resolvers in
priority order until one takes ownership. With none installed every URL opens
directly, which is the same behaviour as having no integration at all — so
stream URLs keep working and page URLs stop resolving.

`playOnStart` uses `OpenUserUrl`, so an authored page URL resolves rather than
failing to open.

### Writing a resolver

Implement `IBasisVideoResolver` and register it at startup:

```csharp
internal sealed class MyResolver : IBasisVideoResolver
{
    public int Priority => 0; // higher runs first; ties run in registration order

    // Cheap and side-effect-free. Decline directly-playable URLs so the player
    // opens them itself.
    public bool CanResolve(string url) => !BasisMediaUrlRouter.IsDirectlyPlayable(url);

    // Take ownership: resolve, then open. May be async; return true as soon as
    // you have taken it, not when it finishes.
    public bool TryResolve(BasisMediaPlayer player, string url)
    {
        // … player.OpenResolved(new BasisResolvedMedia { … }) …
        return true;
    }
}

internal static class MyResolverInstaller
{
    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)]
    private static void Install() => BasisMediaUrlRouter.Register(new MyResolver());
}
```

`BasisResolvedMedia` carries what a resolver knows and the player cannot work
out: the stream URL and its audio leg, the liveness, subtitle tracks, and
display metadata (title, uploader, thumbnail, duration).

- **Async resolves must guard against stale loads.** Capture
  `player.LoadGeneration` before starting and drop the result if it no longer
  matches when the work finishes; otherwise a slow resolve of an earlier URL
  overwrites a newer load.
- **Main thread only.** The resolver list is unsynchronised.
- **Routing, never trust.** A resolver decides *how* a URL loads, not whether
  it is allowed.

## Usage

```csharp
var player = gameObject.AddComponent<BasisMediaPlayer>();
gameObject.AddComponent<BasisVideoMaterialOutput>().TargetRenderer = quadRenderer;
player.OpenUserUrl("rtsp://stream.vrcdn.live/live/vrcdn");
```

Or drop `Prefabs/MediaPlayerStreaming` into a scene and fill in the player's
`URL`. `Prefabs/MediaPlayerMultiChannelStreaming` is the same thing with eight
positioned speakers instead of one stereo output.

### One source, two platforms

A feed is often published differently per platform — RTSP is lowest latency on
desktop, and Quest wants MPEG-TS over HTTPS from the same source. Set
`androidUrl` and an Android build uses it instead of `url`:

```
url         rtsp://stream.vrcdn.live/live/vrcdn
androidUrl  https://stream.vrcdn.live/live/vrcdn.live.ts
```

Empty means the same URL everywhere, so there is no mode to get wrong. The
editor always takes `url`, whatever the build target is set to, so entering
play mode never silently exercises the other one. `ResolvedUrl` reports which
applies.

`Basis > Tools > Media Player > Insert Player (existing scene)` drops the shipped
prefab into the scene you are working in, stereo or multi-channel, exactly as it
ships — set its URL and it is wired. The `Test Scene` menu beside it is the other
job: it replaces the open scene with a pass-ready one and arms the captures.

**A player with no audio component beside it is silent, deliberately.** The
player produces one interleaved PCM stream and nothing else; de-interleaving,
per-speaker routing, downmixing, device-rate conversion and spatialisation all
belong to the audio components, which is what makes per-channel positioning
possible at all.

### Video output

Frames reach the world through one of two sinks:

- **`BasisVideoMaterialOutput`** — binds the frame to one or more `Renderer`
  material properties (`_BaseMap` on URP, `_MainTex` on legacy, per
  `TexturePropertyName`). `TargetRenderer` plus every entry in
  `AdditionalTargets` is driven from the same texture, so one player can feed
  several screens.
- **`BasisVideoDisplay`** — binds it to a uGUI `RawImage`, optionally driving
  an `AspectRatioFitter` from the reported `VideoSize`.

Aspect, stereo-eye selection and flips are applied as a **UV scale and offset**
on the sampled texture, composed once and written to the material's texture ST
(or the `RawImage`'s `uvRect`). The `Equirect360`, `VR180` and `Fisheye`
projections are the exception: they can't be expressed that way, so they enable
a shader keyword instead.

On the material path nothing touches the mesh, so a screen placed at a known
size in a world keeps that size whatever plays on it.

#### The screen shader

`Basis/Media Player Video` is URP/Unlit with one change: **UVs outside `[0,1]`
render opaque black** rather than being resolved by the sampler. That single
branch is what makes letterboxing possible. `FitInside` fits the whole source
inside the screen by scaling the *bar* axis above 1, which pushes the sampled
UV outside the texture over the bar region, and a UV transform has no other way
to produce a bar.

The frame texture is `Clamp`-wrapped, so on any other material that region
resolves to the outermost row or column of video pixels smeared across the bar:
a streak of edge colour that shifts with the content, rather than a black bar.
The video itself stays correctly proportioned either way, so it is the bars
that give it away.

The player resolves this shader **by name**, not by asset path or GUID, so
nothing breaks if the package folder moves.

#### Aspect

`AspectMode` compares the source's aspect against the **display aspect** — the
shape of the surface being drawn on, not the shape of the video:

| `AspectMode` | Behaviour | Safe on any material? |
|---|---|---|
| `Original` (default) | Sample untransformed; the mesh or `RectTransform` stretches the frame to its own shape | yes |
| `Stretch` | Same as `Original` | yes |
| `FitInside` | Letterbox or pillarbox — whole source visible, bars on the remaining axis | **no** — needs the shader above |
| `FitOutside` | Crop to fill — no bars, edges of the source lost | yes |
| `PixelPerfect` | Crop to fill, insetting on the opposite axis to `FitOutside` | yes |

`DisplayAspectOverride` supplies the display aspect directly. Left at 0 it is
derived from the target: the renderer's **local** bounds on the material path,
the `RectTransform`'s rect on the UI path.

> **Known gap.** Local bounds are mesh-local and exclude transform scale, so a
> 1×1 quad scaled to (16, 9, 1) still reports 1:1, and `FitInside` letterboxes
> into a square in the middle of a wide screen. Set `DisplayAspectOverride` on
> any screen that isn't uniformly scaled. The aspect is also recomputed only
> when the frame texture changes, so a screen resized at runtime keeps the fit
> it was given.

#### Projection and orientation

`ProjectionMode` describes how the source frame is laid out. `SideBySideLR`/`RL`
and `OverUnderTB`/`BT` select one half of a stereo frame through the same UV
transform, with `StereoEye` picking which. `Equirect360`, `VR180` and `Fisheye`
enable a `BASIS_PROJ_*` keyword instead. **No bundled shader implements those
keywords**, so on the stock material those three render the source flat, as
mono.

Whether a backend publishes frames top-left origin can depend on the GPU rather
than the content, so the player reports `OutputFrameIsTopLeftOrigin` and both
sinks fold the correction in automatically. Leave `FlipVertically` off for
normal content; it is there for a source genuinely encoded upside-down.

#### Using your own shader

Anything bound as the screen material needs to expose the texture property
named in `TexturePropertyName` and transform the sampled UV by that property's
ST — `TRANSFORM_TEX(input.uv, _BaseMap)` — since that is where aspect, stereo
eye and flips arrive. That covers every aspect mode except `FitInside`, which
also needs UVs outside `[0,1]` rendered black before sampling.

`Picture` (Brightness, Contrast, Saturation, Gamma) is published as
`_BasisBrightness` / `_BasisContrast` / `_BasisSaturation` / `_BasisGamma`,
through a `MaterialPropertyBlock` on the material path and onto
`RawImage.material` on the UI path. A shader that doesn't declare them ignores
them, which includes the bundled one.

### Audio

Audio routes through a `BasisMediaPlayerAudio` on the player's GameObject. List
the `AudioSource`s in `Outputs`, each carrying a `BasisMediaAudioChannel`
selecting what it plays: a single decoded channel, or a stereo downmix of the
whole stream. One output set to `Stereo` is the stereo case; one output per
channel lets a 5.1 or 7.1 mix be positioned speaker by speaker.

Each output carries a `BasisMediaPlayerAudioTap` writing the decoded stream
into that source's DSP block. Unity applies filters in component order, so a
Low Pass, High Pass or Reverb filter has to sit **below** the tap on the same
GameObject; anything above it is handed silence. The inspector flags an output
whose filters are above its tap, with a button that fixes the order.

Each source's own `Volume` and `Mute` fold into that tap's gain, so they behave
as they would for a clip and stay per-output — on a surround rig you can trim
one speaker without touching the rest. `BasisMediaPlayerAudio`'s `VolumeGain`
and `Mute` are the player-wide pair, and the client's main volume scales the
lot; all three multiply.

Two AudioSource controls behave differently from a clip. `Pitch` does nothing,
since pitching the stream would pull audio off the video. Spatialisation needs
the spatialiser to run *after* the tap, so **Spatialize Post Effects** stays
ticked and **Bypass Effects** unticked; with either the wrong way round the
spatialiser processes silence and the tap overwrites the result.

Per-source analysers — AudioLink and anything else built on
`AudioSource.GetOutputData` — can't see audio a script generates, so they read
silence from a tap-driven output. `BasisMediaAudioChannel.AnalysisFeed`
switches that output to a streaming `AudioClip` written once a frame, which
those APIs can read back. It costs that output a small delay, so set it on the
analyser's own `AudioSource` rather than on a speaker you listen to.

**Channel ceilings depend on the codec.** FLAC carries a full 7.1. AAC caps at
5.1. Opus is mono or stereo — the multi-stream layouts are refused rather than
half-decoded. A multichannel source played through a single stereo output is
downmixed with ITU BS.775 coefficients, with headroom against clipping.

### Captions and subtitles

In-band CEA-608 captions are parsed by the engine and handed over as timed
cues; the player holds each one until playback reaches it, and
`BasisMediaCaptionOverlay` draws them. Whether captions are shown, and the two
opacities, belong to the viewer rather than the world: they live in
`BasisMediaSettings`, persist client-side, and are set from the Media Players
panel.

Sidecar subtitles arrive at the same overlay through the same caption state.
`SetSubtitleTracks` supplies them (a resolver usually does this) and
`SelectSubtitleTrack(index)` picks one; selecting a track suppresses the in-band
feed and reverting to -1 brings it straight back. Sidecar fetches go out over
`UnityWebRequest`, so they are checked against the client's URL security before
the request is made — the engine's own vetting doesn't cover them.

### In-band user data

A video stream can carry application data inside the picture itself, as SEI
`user_data_unregistered` messages (H.264 and H.265 payload type 5): a lighting
relay stamping DMX snapshots frame by frame, say, so the data stays locked to
the picture through any CDN that remuxes rather than transcodes. The engine
surfaces every such message with the 16-byte UUID that opens it and the bytes
that follow, unparsed, and `BasisMediaPlayer.UserDataReceived` raises each one
when playback reaches its timestamp:

```csharp
static readonly Guid Mine = Guid.Parse("b1f0a7d4-9c3e-4a52-8f61-2d7c5e0b93a8");

player.UserDataReceived += (ptsUs, uuid, payload) =>
{
    if (uuid != Mine) return;      // x264 stamps its own build string this way
    Decode(payload);               // borrowed for the call; copy what outlives it
};
```

Every UUID is delivered and the consumer filters, so the player carries no
particular application's identity. Messages arrive in timestamp order; a seek
drops whatever was queued from the old position, and a loop drops what the
previous pass left behind. Messages are held until due whether or not anyone is
subscribed, so a subscriber that attaches mid-session receives everything still
to come and nothing already past. The engine holds up to 64 KiB
per message and refuses larger ones, and a stream carrying more than this lane
can hold loses oldest first.

Writing a consumer, in short:

- The delegate is `UserDataHandler(long ptsUs, Guid uuid, ReadOnlySpan<byte> payload)`.
  It runs on the main thread inside the player's tick, once per message, in
  timestamp order. Keep the handler short; anything slow delays the frame.
- `payload` is borrowed for the call. Copy out what you keep; do not hold the span.
- Filter on `uuid` first. The encoder's own messages arrive through the same event.
  A UUID you hold as the 16 wire bytes converts with
  `BasisMediaPlayer.GuidFromRfc4122(ReadOnlySpan<byte>)`, whose text form matches the
  RFC string (`new Guid(byte[])` would not: it reads the first three fields
  little-endian).
- Subscribe whenever suits you. Messages are held until due whether or not anyone is
  listening, so subscribing a frame after `Open` loses nothing that is still to come.
- Unsubscribe in `OnDisable`, and compare the player reference by
  `ReferenceEquals` when doing so: a destroyed player compares equal to `null` the
  Unity way while the managed object still holds your delegate.
- Treat the bytes as untrusted. Whatever format they carry should verify itself
  (a length, a CRC) before you act on it; the player checks nothing past the UUID.

SEI rides inside the video elementary stream, so it survives a pipeline that
copies that stream through unchanged (the usual remux), and is dropped by one
that re-encodes it, unless the transcoder goes out of its way to carry it
across. A remux that runs bitstream filters over the video can also strip or
add SEI. When the lane goes quiet on a path that worked elsewhere, that is the
first thing to check.

### Choosing an audio track

A container can carry several audio tracks: one per language on a film, or one
per capture device on a screen recording. Where it does, `AudioTracks` lists
them in container order with whatever the container states — an ISO 639
language, and for Matroska a track name — and `SelectAudioTrack(index)` plays
one. The Media Players panel shows the list as a dropdown.

A source with a single audio track reports an empty list rather than a list of
one, so a picker can simply hide itself. Neither a language nor a name is
guaranteed: a recording usually states neither, so label rows by position as
well, or three unnamed stereo tracks all read the same.

Switching re-opens the session at the current position rather than swapping
decoders underneath a running one, so expect a short re-buffer. MP4 and
Matroska enumerate today.

### Shared playback

Add `BasisMediaPlayerNetworking` beside the player and every client in the
world watches the same thing. Both shipped prefabs already carry it.

One client owns the player at a time and drives it; the rest follow. The owner
broadcasts the URL, the transport commands and its playhead, and a client that
joins late is sent the current state so it lands where everyone else is. Who
may take control is authored on the component:

| Field | Effect |
| --- | --- |
| `AdminOnly` | Only clients holding `basis.mediaplayer.control` or `*` may take control. Overrides the other two |
| `AllowAnyoneToTakeControl` | Any client may take ownership. On by default |
| `AnyoneCanControl` | Clients holding no control permission also get the playback controls in the menu |
| `PositionHeartbeatSeconds` | How often the owner broadcasts its playhead. 0 turns the heartbeat off |

Followers do not jump to the owner's position. The playhead is handed to the
engine as a target, which converges through a dead band, then a rate slew of a
couple of percent, and only seeks when it is more than a couple of seconds out,
so ordinary drift is corrected without anything audible. Live sources are not
position-synced at all — there is no shared timeline to land on, and how far
behind the live edge a viewer may sit is bounded by `maxDivergenceMs` instead.

A page URL is shared as the page URL, never as the stream a resolver produced
from it: those are per-client and expire, so each client resolves for itself.

### The Media Players panel

`Media Players` in the main menu lists every player in the scene and appears
only when there is one. Pick a player and it offers the URL and transport
controls with a scrubber, the viewer's own settings (volume, captions and their
opacity, subtitle track), an audio-track dropdown when the source carries more
than one, an admin tab carrying the permission flags above for clients holding
`*`, and a debug readout behind the Advanced toggle. The
playback tab is shown only to clients that may actually drive the player, and
appears and disappears as that changes.

### How many play at once

A world's media-player count stopped being the world author's decision once
props could carry one: anyone can spawn more, and every open session costs the
viewer bandwidth, memory and decode work whether they are looking at it or not.

`Settings > Developer > Media Player` carries the cap. It defaults to the
per-platform session counts the engine is budgeted and measured for — 2 on
Android, 3 elsewhere — and 0 lifts it entirely.

Beyond the cap the furthest players go dormant. Dormant is a closed session
rather than a paused one, since a paused session still holds its buffer, frame
pool and decoder; the URL and position are remembered, so waking one costs an
ordinary join and lands where it would have been. Live players rejoin at the
edge. Nearest wins, with a margin and a settling delay so that walking between
screens does not open and close sessions the whole way, and selecting a player
in the Media Players panel takes a slot for it whatever the distance.

A player under shared playback is not returned to its remembered position when
it wakes: the owner's position is the truth, and it arrives on the next
heartbeat.

Everything counts against the cap, audio-only sessions included, even though
they cost far less than a video one. Splitting them out wants their measured
cost first.

### Settings and diagnostics

`Settings > Developer > Media Player` carries the decode-route preference
(hardware with software fallback, hardware only, or software only) and a
readout of what the engine probed on this machine. It is a per-user client
setting, deliberately not a serialised inspector field: it describes the
machine, not the world, and a serialised field would drift into prefabs.

`BasisMediaPlayerDiagnostics` records a per-frame capture of what the engine
cannot see from the inside — presentation cadence, pull rates, position
against wall clock. `Basis > Debug > Media Player` shows the same live.
`Native~/TESTING.md` documents the column contracts and the healthy band for
each figure.

## Building the engine

The engine is a Rust workspace under `Native~/`, which Unity ignores because of
the trailing tilde. The committed binaries in `Runtime/Plugins/` are what
ships; rebuild them only when the engine changes.

```sh
cd Native~
cargo build --release -p media-ffi --features rist
```

`tools/ci.ps1` (Windows) and `tools/ci.sh` (Linux) run the full gate:
formatting, clippy at `-D warnings`, the test suite, the ffprobe conformance
oracle, the impairment rows, and the licence and supply-chain audits. The
`media-engine-release` workflow builds the shipping binaries for each platform,
so a release can be reproduced without a particular dev box.

Building on Windows needs NASM on PATH (rav1d's x86 assembly) and, for the
`rist` feature, meson and ninja for `tools/build-librist.ps1`. Android builds
through the NDK that ships with Unity — see `tools/android-env.ps1` — and its
`rist` feature needs `tools/build-librist-android.sh`, which cross-builds
librist against that same NDK.

## Known limits

- **No RTMP.** The C player had a minimal RTMP client; this engine does not.
  Use RTSP, HLS or MPEG-TS over HTTPS.
- **Raw FLAC and raw ADTS do not seek.** Neither carries an index, and neither
  has the table MP3 and Ogg estimate from. The same codecs seek normally inside
  MP4 or Matroska.
- **WebM/Matroska seek** needs a Cues index and a range-capable host.
- **HLS** picks the highest-bandwidth rendition and stays there; there is no
  adaptive switching. Encrypted playlists (`EXT-X-KEY`), byte-range segments
  and I-frame-only playlists are refused.
- **WHEP** reports packet loss but does not request retransmission or a
  keyframe, so a lossy path recovers only at the next natural keyframe.
- **Shared playback is not enforced by the server.** Media travels on the
  general scene relay, which the server does not inspect, so the check that a
  control message came from the owner happens on receipt rather than in
  transit. There is no retention either: a player whose owner has left gives a
  late joiner nothing until someone takes control.
- **Projection modes** `Equirect360`, `VR180` and `Fisheye` set a shader
  keyword no bundled shader implements, so they render flat.
- **`Picture`** needs a shader declaring the `_Basis*` floats, which the
  bundled one doesn't.

## Not yet ported from the C player

Listed for anyone coming from the C player. Some of these were never working
there either, and some are deliberate:

- **Bitrate selection.** Choosing a rung of an adaptive ladder by hand. Note
  that the C player did not do this either: the managed side was there, but the
  engine entry points it called were never exported, so the control was always
  hidden.
- **The playlist component.** Parked rather than pending. The C player's was an
  example script, and it did not network the playlist itself — only the URL it
  landed on — so on a shared player every client raced to advance at the end of
  an entry. Nobody has asked for it. Sequencing sources is a handful of lines
  against `Ended` and `OpenUserUrl`, and a world that wants it can own the
  policy rather than inherit a half-shared one.
- **Host trust.** A client asked to load a URL by whoever controls the player
  does so without asking its viewer. Address-level security still applies —
  private and loopback addresses are refused — so this is an arbitrary *public*
  URL, not a way into a local network. The C player did not gate this either;
  it is on the list because shared playback makes it worth fixing rather than
  because something was lost.
- **Windows ARM64 binaries.** The C player shipped one, though it linked a stub
  decoder that never produced a frame, so nothing played there either.

Audio track selection was on this list and is now available — see
**Choosing an audio track** above.
