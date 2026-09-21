# Basis Media Player

Live and on-demand video, and audio-only media, for Basis. Put a player in a
world, give it a URL, and everyone in the world watches the same thing.

It plays live streams (RTSP, WHEP, RIST, HLS and MPEG-TS over HTTPS) and
on-demand files (MP4, WebM, Matroska, HLS and the common audio formats), using
the hardware video decoders of the machine it runs on. Hardware-decoded frames
go straight into a Unity texture without a copy through the CPU. It needs
neither a transcode server nor Unity's own `VideoPlayer`.

## Requirements

- Unity 6000.0 or later, with the Basis packages this one depends on
  (`com.basis.common`, `com.basis.eventdriver`, `com.basis.framework`,
  `com.basis.sdk` and `com.basis.settings`).
- The player's native plugin is committed for every platform below, so nothing
  needs building to use it.

| Platform | Graphics API | Video | Audio |
| --- | --- | --- | --- |
| Windows x64 (PC and PC VR) | Direct3D 11 | H.264, HEVC, VP9 and AV1 through the GPU's hardware decoder where it has one; H.264, VP9 and AV1 fall back to software | AAC, MP3, FLAC, Opus, PCM |
| Android arm64 (Quest) | Vulkan | H.264, HEVC, VP8, VP9 and AV1, whichever the device's hardware decoders support; there is no software fallback | AAC, MP3, FLAC, Opus, PCM |

The graphics API is a hard requirement. On any other (Direct3D 12 on Windows,
OpenGL ES on Android) the player logs `needs D3D11` or `needs Vulkan` and
plays nothing.

A Linux plugin ships too, and the engine is built and tested on Linux, but the
player does not open a session there: Linux has no Direct3D 11.

## Getting started

### With the prefab

`Basis > Tools > Media Player > Insert Player (existing scene)` adds a ready
player to the open scene, in a **Stereo** or a **Multi-Channel** version. The
same prefabs are in `Prefabs/`: `MediaPlayerStreaming` and
`MediaPlayerMultiChannelStreaming`, the second with eight positioned speakers
instead of one stereo output. Each carries everything a player needs: the
screen, audio, captions and shared playback.

Select the player, type a URL into **URL**, and enter play mode. The screen
shows the video and its sound plays from the player.

### The player's inspector

| Field | What it does |
| --- | --- |
| **URL** | What to play: a stream or file URL, or a page URL such as a YouTube link if a resolver is installed (see [Page URLs and resolvers](#page-urls-and-resolvers)) |
| **Per-Platform URLs** | Shows a second URL for Android builds (see below) |
| **Play On Start** | Opens the URL when the scene starts. On by default |
| **Max Divergence (ms)** | Live streams only: how far behind the live edge a viewer may fall on a poor connection. 0 uses the default |
| **Advanced > Liveness** | Overrides whether a source is treated as live or on-demand. Leave it on Auto (see [Live and on-demand](#live-and-on-demand)) |
| **Advanced > Engine capture** | Writes the engine's own diagnostics to a file (see [When it does not play](#when-it-does-not-play)) |
| **Advanced > Allow Local Addresses** | Lets this player open sources on private and loopback addresses, for a test server on your own machine or network |

### One source, two platforms

A feed is often published differently per platform: RTSP has the lowest
latency on PC, and Quest wants MPEG-TS over HTTPS from the same source. Tick
**Per-Platform URLs** and fill in **Android URL**, and an Android build uses it
instead of **URL**:

```
URL          rtsp://stream.vrcdn.live/live/vrcdn
Android URL  https://stream.vrcdn.live/live/vrcdn.live.ts
```

An empty Android URL means the same URL everywhere. The editor always plays
**URL**, whatever the build target, so play mode never quietly uses the other
one. `ResolvedUrl` reports which URL applies.

### From code

```csharp
var player = gameObject.AddComponent<BasisMediaPlayer>();
gameObject.AddComponent<BasisVideoMaterialOutput>().TargetRenderer = quadRenderer;
player.OpenUserUrl("rtsp://stream.vrcdn.live/live/vrcdn");
```

This gives picture only. A player makes no sound until it has a
`BasisMediaPlayerAudio` beside it (see [Audio](#audio)); the prefabs are the
complete setup.

### Checking it works

`Basis > Tools > Media Player > Run Smoke Test` plays a test file end to end
and reports a pass or a fail against the expected playback figures. It needs a
checkout that includes the engine's test files, or a source named in
`BASIS_SMOKE_URL`.

`Basis > Tools > Media Player > Test Scene` replaces the open scene with a
ready-made one (one player, four players, or two clients sharing playback)
with diagnostics already recording.

## Supported URLs

| Scheme | Use | Example |
|---|---|---|
| `rtsp://` | Low-latency live on PC: UDP first, TCP if UDP fails | `rtsp://stream.vrcdn.live/live/vrcdn` |
| `rtspt://` | RTSP over TCP only, for networks where UDP never works | `rtspt://stream.vrcdn.live/live/vrcdn` |
| `rist://` | RIST live ingest (UDP, loss recovery, optional AES) | `rist://stream.example:5000?secret=KEY&aes-type=128` |
| `whep://` / `wheps://` | WHEP: WebRTC receive, sub-second join | `whep://stream.example:8889/live/whep` |
| `https://….mp4` | MP4 over HTTPS: fragmented (live), or an on-demand file, seekable | `https://stream.vrcdn.live/live/vrcdn.live.mp4` |
| `https://….ts` | MPEG-TS over HTTPS | `https://stream.vrcdn.live/live/vrcdn.live.ts` |
| `https://….m3u8` | HLS, on-demand or live | `https://stream.example/live/index.m3u8` |
| `https://….webm` | WebM: VP9 or AV1 video, Opus audio | `https://stream.example/vod/clip.webm` |
| `https://….mkv` | Matroska, same codecs | `https://stream.example/vod/clip.mkv` |
| `https://….flac` `.mp3` `.aac` `.opus` `.wav` | Audio only | `https://stream.example/audio/track.flac` |
| `file://` or an absolute path | Local file | `C:\media\clip.mp4` |

The format is worked out from the bytes, not the extension, so a URL with no
extension plays too.

Private and loopback addresses (`localhost`, `192.168.…`, `10.…` and the like)
are refused, so a URL handed to a player can never reach into a viewer's own
network. **Allow Local Addresses** in the inspector lifts that for one player,
for testing against a server on your own machine.

Audio goes up to 7.1 channels. PCM covers WAV files and the LPCM carried in
Blu-ray-style MPEG-TS. Some combinations are refused rather than half-played,
so you get an error that says what is wrong instead of a black screen:

- HEVC inside MPEG-TS is refused. TS carries no picture size ahead of
  decoding, and the platform HEVC decoder fails without one.
- VP8 and VP9 play only where the platform has a decoder for them, and VP8 has
  none on Windows.
- AV1 is refused on Quest Pro, which has no AV1 decoder.

`BasisMediaPlayer.EngineCapabilities` reports what the current machine
supports: formats, transports, codecs, and for each video codec whether it
decodes in hardware or software and up to what resolution and frame rate.

## Video output

Frames reach the world through one of two components:

- **`BasisVideoMaterialOutput`** puts the video on one or more renderers'
  materials (`_BaseMap` on URP, `_MainTex` otherwise, or whatever
  `TexturePropertyName` names). `TargetRenderer` and every entry in
  `AdditionalTargets` show the same video, so one player can feed several
  screens.
- **`BasisVideoDisplay`** puts it on a uGUI `RawImage`, and can drive an
  `AspectRatioFitter` from the video's size.

Aspect, choosing one eye of a stereo video, and flips are applied to the
texture coordinates, not the mesh, so a screen placed at a set size in a world
keeps that size whatever plays on it.

### The screen shader

`Basis/Media Player Video` is URP Unlit with one change: texture coordinates
outside `[0,1]` draw black. That is what makes letterboxing work. `FitInside`
fits the whole video inside the screen by stretching the coordinates past the
edge of the texture where the bars go.

On any other material, that region repeats the outermost row or column of
video pixels across the bar instead: a streak of edge colour that changes with
the picture. The video itself keeps its proportions either way, so the bars
are what give it away.

The player finds this shader by name, so nothing breaks if the package folder
moves.

### Aspect

`AspectMode` compares the video's shape with the shape of the surface it is
drawn on.

| `AspectMode` | Behaviour | Works on any material? |
|---|---|---|
| `Original` (default) | Drawn untransformed; the mesh or `RectTransform` stretches it to its own shape | yes |
| `Stretch` | Same as `Original` | yes |
| `FitInside` | Letterbox or pillarbox: the whole video visible, bars on the other axis | **no**, it needs the shader above |
| `FitOutside` | Crop to fill: no bars, the edges of the video lost | yes |
| `PixelPerfect` | Crop to fill, cropping the opposite axis to `FitOutside` | yes |

`DisplayAspectOverride` sets the surface's shape directly. Left at 0, it is
taken from the target: the renderer's mesh bounds, or the `RectTransform`'s
rect.

Mesh bounds leave out the transform's scale, so a 1×1 quad scaled to
(16, 9, 1) still reads as square and `FitInside` letterboxes into a square in
the middle of a wide screen. Set `DisplayAspectOverride` on any screen that is
not uniformly scaled. The shape is also read again only when the video's
texture changes, so a screen resized while playing keeps the fit it had.

### Projection and orientation

`ProjectionMode` says how the video frame is laid out. `SideBySideLR`/`RL` and
`OverUnderTB`/`BT` show one half of a stereo frame, with `StereoEye` picking
which. `Equirect360`, `VR180` and `Fisheye` switch a `BASIS_PROJ_*` shader
keyword instead, and **no bundled shader implements those keywords**, so on
the standard screen those three show the frame flat.

Some GPUs deliver frames upside down relative to others; both output
components correct for that automatically. Leave `FlipVertically` off for
normal video. It is for a source that was genuinely encoded upside down.

### Using your own shader

A custom screen material has to expose the texture property named in
`TexturePropertyName` and apply that property's tiling and offset to its
texture coordinates (`TRANSFORM_TEX(input.uv, _BaseMap)`), because that is how
aspect, stereo eye and flips arrive. That covers every aspect mode except
`FitInside`, which also needs coordinates outside `[0,1]` drawn black.

`Picture` (Brightness, Contrast, Saturation, Gamma) is sent to the material as
`_BasisBrightness`, `_BasisContrast`, `_BasisSaturation` and `_BasisGamma`. A
shader that does not declare them ignores them, and the bundled one does not,
so `Picture` needs your own shader.

## Audio

Sound plays through a `BasisMediaPlayerAudio` on the player's GameObject. List
the `AudioSource`s it plays through in `Outputs`, each with a
`BasisMediaAudioChannel` saying what it plays: one channel of the source, or a
stereo mix of all of it. One output set to `Stereo` is ordinary stereo; one
output per channel places a 5.1 or 7.1 mix speaker by speaker.

The player itself produces the decoded sound and nothing else. Splitting it
into channels, mixing down, matching the output device's sample rate and
spatialisation all happen in the audio components, which is what lets each
channel be placed on its own.

Each output has a `BasisMediaPlayerAudioTap` that writes the sound into that
`AudioSource`. Unity runs audio filters in component order, so a Low Pass,
High Pass or Reverb filter has to sit **below** the tap on the same
GameObject; anything above it receives silence. The inspector flags an output
whose filters are above its tap, with a button that fixes the order.

Each `AudioSource`'s own `Volume` and `Mute` apply to its output, as they would
for a clip, so on a surround setup you can turn down one speaker without
touching the rest. `BasisMediaPlayerAudio`'s `VolumeGain` and `Mute` apply to
the whole player, and the viewer's main volume scales everything; all three
multiply.

Two `AudioSource` controls behave differently from a clip. `Pitch` does
nothing, because changing the pitch of the sound would pull it away from the
picture. Spatialisation has to run after the tap, so **Spatialize Post
Effects** stays ticked and **Bypass Effects** unticked; the wrong way round,
the spatialiser processes silence and the tap overwrites the result.

Audio analysers that read an `AudioSource` (AudioLink, and anything else built
on `AudioSource.GetOutputData`) cannot see sound a script writes, so they read
silence from a normal output. `BasisMediaAudioChannel.AnalysisFeed` makes that
output play through a streaming `AudioClip` instead, which they can read. It
adds a small delay to that output, so set it on the analyser's own
`AudioSource` rather than on a speaker you listen to.

How many channels a source can carry depends on its codec. FLAC carries a full
7.1, AAC up to 5.1, and Opus mono or stereo (surround Opus is refused rather
than half-decoded). A surround source played through a single stereo output is
mixed down with the standard ITU BS.775 coefficients, with headroom against
clipping.

### Choosing an audio track

A file can carry several audio tracks: one per language on a film, or one per
microphone on a screen recording. Where it does, `AudioTracks` lists them in
the file's order with whatever the file says about them (a language code, and
for Matroska a track name), and `SelectAudioTrack(index)` plays one. The Media
Players panel shows the list as a dropdown.

A source with only one audio track reports an empty list, so a picker can hide
itself. Neither a language nor a name is guaranteed: a recording usually has
neither, so label entries by position as well, or three unnamed stereo tracks
all read the same.

Switching track reopens the source at the current position, so expect a short
pause while it buffers. MP4 and Matroska list their tracks.

## Captions and subtitles

Captions carried inside the video (CEA-608) are read by the player and shown
by `BasisMediaCaptionOverlay` as playback reaches each one. Whether captions
are shown, and their text and background opacity, are the viewer's own
choices: they are saved on the viewer's machine and set from the Media
Players panel.

Separate subtitle files appear through the same overlay. `SetSubtitleTracks`
supplies them (usually a resolver does this) and `SelectSubtitleTrack(index)`
picks one; selecting a track hides the captions carried in the video, and
selecting -1 brings them back. Subtitle files are downloaded with
`UnityWebRequest`, and each request is checked against the client's URL
security before it is made.

## Shared playback

Add `BasisMediaPlayerNetworking` beside the player and every client in the
world watches the same thing. Both prefabs already have it.

One client owns the player at a time and controls it; the rest follow. The
owner sends the URL, its play, pause, seek and stop, and its position, and a
client that joins late is sent the current state so it lands where everyone
else is. Who may take control is set on the component:

| Field | Effect |
| --- | --- |
| `AdminOnly` | Only clients holding `basis.mediaplayer.control` or `*` may take control. Overrides the other two |
| `AllowAnyoneToTakeControl` | Any client may take control. On by default |
| `AnyoneCanControl` | Clients with no control permission also get the playback controls in the menu |
| `PositionHeartbeatSeconds` | How often the owner sends its position. 0 turns it off |

A page URL is shared as the page URL, never as the stream a resolver turned it
into. Resolved streams belong to one client and expire, so each client
resolves the page for itself.

### What viewers experience with a video

The owner's play, pause, seek and stop reach everyone, and a seek made while
paused shows everyone the new frame without starting anyone playing. Reaching
the end is each client's own: nobody is held back for anyone else.

While playing, the owner sends its position every `PositionHeartbeatSeconds`
(3 by default), and each follower corrects towards it in three steps:

| How far out | What the follower does |
| --- | --- |
| Up to 150 ms | Nothing |
| 150 ms to 2 s | Plays up to 2% fast or slow until it is back within 150 ms. Nothing jumps, but while it lasts the follower's sound is slightly faster or slower and a little higher or lower in pitch, by up to a third of a semitone |
| More than 2 s | Jumps straight to the owner's position |

Followers stay within 150 ms of the owner, and usually within a frame or two.
A pause stops each viewer where it is, as close to the owner as it was while
playing.

### What viewers experience with a live stream

Position is not synced on a live stream, which has no shared timeline. Each
viewer plays from the live edge at the depth their own connection needs. Two
viewers can end up a second or more apart, and nothing pulls them together. A
live stream cannot be paused or scrubbed by anyone; changing the URL and
stopping still reach everyone. **Max Divergence (ms)** sets how far behind the
live edge a viewer is allowed to fall on a poor connection: past it, playback
stutters rather than falling further behind.

### Joining late

A client that arrives while something is playing opens the same source and
lands at the owner's position; if the owner is paused, it lands paused on the
owner's frame and starts when the owner does. It usually lands a fraction of a
second behind, the time it took to open, and closes the gap over the next half
minute at the 2% rate above. A page URL has to be resolved on the joiner
first, which can take a few seconds; the joiner then jumps to where the owner
has got to.

### Getting back in step by hand

Two buttons in the Media Players panel restart playback in the right place:

| Button | Where | What it does |
| --- | --- | --- |
| **Resync Everyone** | The playback tab, for anyone who may control the player | Takes control and reloads the source for everyone, you included, at your current position and play or pause state. No confirmation |
| **Local Resync** | My Settings, for anyone | Reloads only your own playback, landing wherever the owner is. With no owner present, another viewer answers instead; if nobody answers within three seconds, it reloads where you already are |

### When the owner leaves or stalls

If the owner leaves, followers keep playing on their own rather than freezing,
and anyone still in the world tells a newcomer what is playing. If the owner's
own playback stalls, followers carry on instead of being pulled back to a
frozen position, and follow again once the owner moves.

### What each viewer keeps to themselves

Volume, captions and their language and opacity, the audio track, the buffer
depth, and the hardware or software decode preference are all per viewer:
changing them never moves anyone else's playback. So is how far behind the
live edge each viewer sits.

## Live and on-demand

A live source plays at the live edge. An on-demand one plays at real speed, so
a file that downloads faster than it plays does not fast-forward. The player
works out which a source is, and almost always gets it right: a source that
states its length and lets the player download any part of it (byte ranges) is
on-demand, and anything else is live. It tests whether ranges really work
rather than trusting what the server advertises.

Most sources are decided before that test. RTSP, WHEP and RIST are always
live, and an HLS playlist says whether it has an end. What is left is a plain
HTTPS URL that is not a playlist.

**Liveness**, under **Advanced** in the inspector, overrides the answer:

| Liveness | Behaviour |
|---|---|
| Auto (default) | Worked out from the source |
| Live | Treat as live: play at the edge, no reading ahead |
| Vod | Treat as on-demand: real-speed playback, reading ahead |

Set it only to overrule a server that misreports. The one mistake the player
does make goes one way: an on-demand file served with neither a length nor
byte ranges plays as live. The player notes that decision in its diagnostics,
so it shows up in a capture rather than as a mystery.

Live playback starts with the sound. A viewer joining a live stream hears it
straight away rather than waiting for the next full video frame, which on a
stream with keyframes far apart is the difference between two seconds and ten,
and the picture follows shortly after, in step. Within a player, the sound
sets the time: it is never sped up, slowed down or cut to let the picture
catch up, and the picture does all the adjusting. On-demand sources start
picture and sound together. Keeping several viewers in step with each other is
the one case where the sound is adjusted, described under
[Shared playback](#shared-playback).

## Seeking

`Seek(double seconds)` jumps to a position. A source that reports a duration
can be seeked, as long as its server allows jumping around in it; a live source
cannot.

```csharp
if (player.DurationSeconds > 0)
    player.Seek(30);
```

A video seek shows the frame you asked for. Decoding has to start at a
keyframe, so the player starts at the one before the target and decodes
forward without showing anything until it reaches the target. If that keyframe
is more than 12 seconds back, it shows the keyframe instead, so a file with
very long gaps between keyframes does not sit buffering for long. How exactly
each format finds its place is in [Format notes](#format-notes).

## Viewer settings

These belong to the person watching, not to the world, because they depend on
that viewer's connection and machine. They are set in two places:
`Settings > Developer > Media Player` holds the defaults every player uses,
and the Media Players panel adjusts the player you have selected.

### Buffer depth

How much the player buffers ahead trades smoothness on a poor connection for
how soon a viewer sees a frame. 0 is Auto, which sizes the buffer from how
late packets actually arrive on the viewer's connection and suits almost
everyone. Set per player under **My Settings > Advanced** in the Media Players
panel, because one world can hold a source next door, where a shallow buffer's
low latency is the point, and one from the far side of the world that only
plays smoothly with more behind it. A change reopens the player at its current
position, so it can be tuned while watching, and nobody else's playback moves.

### How many play at once

Anyone can bring more players into a world on props, and every open player
costs the viewer bandwidth, memory and decoding whether they are looking at it
or not. So the viewer sets the limit: `Settings > Developer > Media Player`
carries a cap, 2 on Android and 3 elsewhere by default, and 0 lifts it.

Beyond the cap, the furthest players go dormant. A dormant player is closed,
not paused, since a paused one still holds its buffer and decoder; its URL and
position are remembered, so waking it costs an ordinary open and lands where it
would have been. Live players rejoin at the edge. The nearest players win, with
a margin and a short delay so walking between screens does not open and close
players all the way, and selecting a player in the Media Players panel keeps it
awake whatever the distance. Under shared playback a waking player goes to the
owner's position rather than its remembered one. Audio-only players count
against the cap like any other.

### Decode route

`Settings > Developer > Media Player` also sets how video is decoded: hardware
with a software fallback (the default), hardware only, or software only. It
shows what the current machine supports too. It is a setting on the viewer's
machine rather than a field on the player, because it describes the machine,
not the world.

### The Media Players panel

**Media Players** in the main menu lists every player in the scene, and
appears only when there is one. Pick a player to get:

- the URL and playback controls with a seek bar, shown only to clients that may
  control that player;
- **My Settings**: your own volume, captions and their opacity, subtitle
  track, audio track, **Local Resync**, and buffer depth under Advanced;
- an admin tab with the permission fields from
  [Shared playback](#shared-playback), for clients holding `*`;
- a live readout of the player's state under Advanced.

## When it does not play

1. **Read the Console.** A player that fails logs one error naming the reason,
   in the form `[BasisMedia] session error <code> (<category>): <reason> [<url>]`,
   and its progress along the way as `[BasisMedia +<seconds>s] …` lines.
   Reasons include a codec this machine cannot decode, a private or loopback
   address (see [Supported URLs](#supported-urls)), and a server that refuses
   the request.
2. **Check what the machine supports.** `Settings > Developer > Media Player`
   shows it, and `BasisMediaPlayer.EngineCapabilities` returns it in code.
3. **Watch it live.** `Basis > Debug > Media Player` shows one player while it
   runs, stage by stage (source, decoding, buffering, presentation, clock,
   audio, captions), with the rate each is running at and the error if there
   is one, so you can see which stage stopped.
4. **Record a session.** Add `BasisMediaPlayerDiagnostics` beside the player
   and it writes one row per frame of what the player saw (frame timing, sound
   delivered, position against real time) to `BasisMediaFrames.csv` in the
   app's persistent data folder.
   **Engine capture** under **Advanced** on the player writes the engine's own
   side. `Native~/DIAGNOSTICS.md` says what each column means and what healthy
   values look like.

## Extending the player

### Page URLs and resolvers

The player plays **stream** URLs. It does not itself turn a **page** URL (a
YouTube or Twitch watch page) into a stream: a resolver package does that, and
registers itself with `BasisMediaUrlRouter`. The player does not depend on any
resolver.

`OpenUserUrl(url)` is the method to call with a URL a person typed: a URL the
player can play directly opens straight away, and anything else is offered to
the installed resolvers in priority order until one takes it. With no resolver
installed, every URL opens directly, so stream URLs still work and page URLs do
not. **Play On Start** uses `OpenUserUrl`, so a page URL set in the inspector
resolves.

### Writing a resolver

Implement `IBasisVideoResolver` and register it at startup:

```csharp
internal sealed class MyResolver : IBasisVideoResolver
{
    public int Priority => 0; // higher runs first; ties run in registration order

    // Cheap and side-effect-free. Decline directly playable URLs so the player
    // opens them itself.
    public bool CanResolve(string url) => !BasisMediaUrlRouter.IsDirectlyPlayable(url);

    // Take the URL: resolve, then open. May be async; return true as soon as
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
out: the stream URL and, where the audio is separate, the audio URL; whether it
is live; subtitle tracks; and display details (title, uploader, thumbnail,
duration).

- An async resolve must guard against being overtaken. Read
  `player.LoadGeneration` before starting and drop the result if it has changed
  by the time the work finishes; otherwise a slow resolve of an earlier URL
  replaces a newer one.
- Register and resolve on the main thread only.
- A resolver decides *how* a URL plays, not whether it is allowed to. The
  address rules in [Supported URLs](#supported-urls) still apply to whatever it
  opens.

### Separate video and audio streams

Video sites serve their higher qualities as separate video and audio streams.
Pass both to the two-argument `Open` and they play together:

```csharp
player.Open(videoOnlyUrl, audioOnlyUrl);
```

A null second argument is an ordinary single source, and clears any pair left
from an earlier open. The inspector has no field for the audio half: a
resolver supplies the pair.

### In-band user data

A video stream can carry application data inside the picture itself, as SEI
`user_data_unregistered` messages (payload type 5 in H.264 and H.265). A
lighting desk could stamp a DMX snapshot on every frame this way, and the data
stays locked to the picture through any server that repackages the stream
without re-encoding it. The player hands over every such message as the
16-byte UUID that begins it and the bytes that follow, untouched, and
`BasisMediaPlayer.UserDataReceived` raises each one when playback reaches its
timestamp:

```csharp
static readonly Guid Mine = Guid.Parse("b1f0a7d4-9c3e-4a52-8f61-2d7c5e0b93a8");

player.UserDataReceived += (ptsUs, uuid, payload) =>
{
    if (uuid != Mine) return;      // x264 stamps its own build string this way
    Decode(payload);               // borrowed for the call; copy what outlives it
};
```

Every UUID is delivered and the subscriber filters, so the player carries no
knowledge of any particular application. Messages arrive in timestamp order; a
seek drops whatever was waiting from the old position, and a loop drops what
the previous pass left. The player holds up to 64 KiB per message and refuses
larger ones, and a stream carrying more than it can hold loses the oldest
first.

Writing a subscriber:

- The delegate is `UserDataHandler(long ptsUs, Guid uuid, ReadOnlySpan<byte> payload)`.
  It runs on the main thread inside the player's update, once per message, in
  timestamp order. Keep it short; anything slow delays the frame.
- `payload` is only valid during the call. Copy out what you keep.
- Filter on `uuid` first. The encoder's own messages arrive through the same
  event. A UUID you hold as 16 raw bytes converts with
  `BasisMediaPlayer.GuidFromRfc4122(ReadOnlySpan<byte>)`, whose text form
  matches the standard string (`new Guid(byte[])` would not: it reads the first
  three fields in the opposite byte order).
- Subscribe whenever suits you. Messages are held until due whether or not
  anyone is listening, so subscribing after `Open` loses nothing still to come.
- Unsubscribe in `OnDisable`, and compare the player with `ReferenceEquals`
  when doing so: a destroyed player equals `null` the Unity way while the object
  still holds your delegate.
- Treat the bytes as untrusted. Whatever format they carry should check itself
  (a length, a CRC) before you act on it; the player checks nothing past the
  UUID.

SEI travels inside the video, so it survives a pipeline that copies the video
through unchanged, and is lost in one that re-encodes it unless the encoder
deliberately carries it across. A repackaging step that filters the video can
also remove or add SEI. When messages stop arriving on a route that worked
elsewhere, check that first.

## Format notes

How a seek finds its place depends on what the format offers:

- **MP4, Matroska and HLS** have an index to look the keyframe up in.
  Matroska without one (a Cues index) still seeks, less precisely; see
  [Known limits](#known-limits).
- **WAV** has no keyframes and needs no index, since the position is
  arithmetic, so it lands on the exact sample.
- **MP3 and Ogg Opus** have neither, so they estimate: MP3 from its Xing table
  where the file has one and from its bitrate where it does not, Ogg by
  bisecting on its granule positions. Both report the requested position and
  resume near it.
- **FLAC** reads its own frame headers back, so it reports exactly where
  playback resumes: through its SEEKTABLE where the file has one, and by
  bisecting over frame headers where it does not.
- **Raw AAC (ADTS)** states neither a length nor a frame count, so it
  estimates from the byte rate of its first frames and rounds down to a frame.

## Known limits

- **No RTMP.** Use RTSP, HLS or MPEG-TS over HTTPS.
- **WebM/Matroska without a Cues index** still seeks, but not to the exact
  frame: the sound resumes at the target straight away, and the picture holds
  until the next keyframe after it.
- **HLS** plays the highest-quality version a playlist offers and stays on it;
  it does not switch with the connection, and a version cannot be chosen by
  hand. Encrypted playlists (`EXT-X-KEY`), byte-range segments and
  keyframe-only playlists are refused.
- **WHEP** asks for lost packets to be resent where the server supports it,
  but never asks for a fresh keyframe, so loss that resending cannot cover
  waits for the next one.
- **Shared playback is not enforced by the server.** Its messages travel on the
  general scene relay, which the server does not inspect. The server keeps no
  media state either. A late joiner is told what is playing by the owner or,
  once the owner has left, by anyone still in the world; when everyone leaves,
  the state goes with them.
- **360°, VR180 and fisheye video show flat** with the bundled shader (see
  [Projection and orientation](#projection-and-orientation)).
- **`Picture` needs your own shader** (see
  [Using your own shader](#using-your-own-shader)).
- **Linux** does not play (see [Requirements](#requirements)).

## Building the engine

The player is a Rust engine under `Native~/`, which Unity ignores because of
the trailing tilde. The committed binaries in `Runtime/Plugins/` are what
ships; rebuild them only when the engine changes.

```sh
cd Native~
cargo build --release -p media-ffi --features rist
```

`tools/ci.ps1` (Windows) and `tools/ci.sh` (Linux) run the full check:
formatting, clippy at `-D warnings`, the test suite, a comparison against
ffprobe, the network-impairment tests, and the licence and supply-chain
audits. The `media-engine-release` workflow builds the shipping binaries for
each platform, so a release can be reproduced on any machine.

Building on Windows needs NASM on PATH (for rav1d's assembly) and, for the
`rist` feature, meson and ninja for `tools/build-librist.ps1`. Android builds
with the NDK that ships with Unity (see `tools/android-env.ps1`), and its
`rist` feature needs `tools/build-librist-android.sh`, which builds librist
with that same NDK.

### How the engine is put together

The engine reads the container, decodes and presents, using the platform's
decoders:

- **Windows:** Media Foundation, through a D3D11 hardware decoder with a
  software decoder behind it (rav1d for AV1). Frames are converted from NV12 to
  BGRA by a D3D11 pixel shader into the texture Unity samples.
- **Android:** MediaCodec. Frames arrive as `AHardwareBuffer`s, are imported
  into Vulkan and converted by a compute pass into a Unity `RenderTexture`.
- **Everywhere:** FLAC, Opus and PCM decode in software, and on Linux so does
  AV1, which is the only video the engine decodes there.

Reading containers and network protocols is the same code on every platform,
and it is `#![forbid(unsafe_code)]`. What has to be unsafe (the platform
decoder calls and the GPU interop) is a small surface behind a typed boundary.

Buffering, pacing and clock drift follow a model rather than hand-tuned
values, and the model is held to recorded delivery captures from impaired live
streams, which are committed as test fixtures.
