using System;
using System.Buffers;
using System.Globalization;
using System.Text;
using UnityEngine;
using UnityEngine.Rendering;

/// <summary>
/// Playback component over the basis_media engine (ABI v2; D3D11 on
/// Windows, Vulkan on Android).
///
/// Poll-driven: one snapshot per frame, one render event per frame once
/// the video texture exists, and the decoded audio ring offered as an
/// <see cref="IBasisPcmSource"/>.
///
/// It draws nothing on its own. The video texture reaches the world through
/// an output sink — <see cref="BasisVideoMaterialOutput"/> for a renderer,
/// <see cref="BasisVideoDisplay"/> for a uGUI RawImage — which own aspect,
/// projection, stereo eye and the frame-origin correction.
///
/// It makes no sound on its own. Audio belongs to
/// <see cref="BasisMediaPlayerAudio"/> on the same GameObject, which
/// broadcasts this ring to one AudioSource per speaker, so each channel can
/// be positioned, spatialised and filtered independently. Without one, the
/// session decodes audio that nothing consumes.
/// </summary>
// Ahead of every default-order MonoBehaviour, so anything that reads the poll's
// snapshot without being one of the registered consumers below — a component in
// another package, a test harness's own Update — still sees this frame's values
// rather than the previous frame's.
[DefaultExecutionOrder(-100)]
[AddComponentMenu("Basis/Basis Media Player")]
public class BasisMediaPlayer : MonoBehaviour, IBasisPcmSource
{
    [Tooltip("http(s) URL or absolute file path.")]
    public string url;

    // The audio-only half of a split pair, set by Open(url, audioUrl) and
    // by the resolver — never authored. Adaptive ladders serve every rung
    // above their muxed fallback as a video-only stream plus this one, so
    // it only ever means something a resolver has just worked out. Not
    // serialized and not public: there is no version of "type an audio URL
    // next to the video URL" that is a thing to ask of anyone, and an
    // authored value would take effect with nothing on screen to show it.
    // What is actually open is on ActiveAudioStreamUrl.
    string audioUrl;

    [Tooltip("Publish this source differently per platform, rather than using " +
             "one URL everywhere. RTSP is lowest latency on desktop where a " +
             "headset wants MPEG-TS over HTTPS from the same feed, which is the " +
             "case this exists for. Authoring only: what actually decides at " +
             "runtime is whether the Android URL is set.")]
    public bool perPlatformUrls;

    [Tooltip("Used instead of the URL above on Android builds. Empty means the " +
             "same URL everywhere. This is for a source published differently " +
             "per platform — RTSP is lowest latency on desktop, where Quest " +
             "wants MPEG-TS over HTTPS from the same feed. The editor always " +
             "takes the URL above, whatever the build target is set to.")]
    public string androidUrl;

    public bool playOnStart = true;

    [Tooltip("Override what the player works out for itself. Auto reads it from the " +
             "source and is almost always right; set Live or Vod only to overrule a " +
             "server whose headers mislead. RTSP, WHEP and RIST are always live, and " +
             "an HLS playlist decides for itself, whatever is set here.")]
    public BmLiveness liveness = BmLiveness.Auto;

    [Tooltip("Shared playback, live sources: the furthest behind the live edge this " +
             "viewer may sit, milliseconds (a ceiling on automatic buffer growth). " +
             "Live position is never hard-synced between viewers — this bound is the " +
             "world author's instrument. 0 = the engine default.")]
    public int maxDivergenceMs;

    [Tooltip("Permit sources on private/loopback addresses (local test rigs only).")]
    public bool allowLocalAddresses;

    [Tooltip("Have the ENGINE capture its own side of the session: bank depth, " +
             "decode and present counts and clock, sampled every 100 ms on its " +
             "own thread. It is held in memory and written as one file when the " +
             "session ends, so a session killed rather than closed leaves no " +
             "capture. Not the BasisMediaPlayerDiagnostics component, which " +
             "records the other side of the boundary — render cadence, frame " +
             "hold times and audio pull, which the engine cannot observe. Run " +
             "both to see a session from each side.")]
    public bool engineCapture;

    [Tooltip("Filename for the engine capture, under Application.persistentDataPath " +
             "beside the frame capture. Empty uses BasisMediaEngine.csv.")]
    public string engineCaptureFileName = "";

    [Tooltip("Append each session's capture to that file instead of replacing it. " +
             "A player that opens and closes repeatedly — one going dormant and " +
             "waking under the session cap, or a source re-opened to switch audio " +
             "track — otherwise leaves only the last session behind. The header is " +
             "written once, when the file is created.")]
    public bool engineCaptureAppend;

    ulong _handle;
    bool _open;
    bool _abiChecked;
    Texture _texture;
    Texture2D _artwork;
    bool _artworkRead;
    /// Whether _texture is the cover art rather than a decode target. The
    /// engine presents into a video texture every frame; art is a still it
    /// never touches, so the render event must not be issued for it.
    bool _textureIsArtwork;
    CommandBuffer _commandBuffer;
    long _audioFramesPulled;
    int _engineChannels;
    int _engineSampleRate;
    int _syncRatePpm;
    int _avOffsetUs = int.MinValue;
    BasisMediaPlayerAudio _audio;
    BasisMediaDriverTick _frameTick;
#if UNITY_ANDROID && !UNITY_EDITOR
    bool _renderHooked;
    int _lastRenderEventFrame = -1;
    long _sentAudioLatencyUs = -1;
#endif
    // Ordered by TickStage on insert, so the tick runs them in the order they
    // read each other rather than in registration order.
    readonly System.Collections.Generic.List<IBasisMediaTickConsumer> _consumers = new();
    readonly System.Collections.Generic.Queue<(long ptsUs, string text)> _captionQueue = new();
    // Pending messages in timestamp order, delivered from the front. The
    // engine hands them over in decode order, and under B-frames that runs
    // up to a reorder depth ahead of presentation, so a FIFO would hold a
    // B-frame's message behind the P-frame's that was decoded before it.
    readonly System.Collections.Generic.List<(long ptsUs, Guid uuid, byte[] buffer, int length)> _userDataPending = new();
    // The largest timestamp queued, so a backwards jump is measured against
    // the far end of the backlog. long.MinValue while nothing is queued.
    long _userDataMaxPtsUs = long.MinValue;
    // The entries due this tick, lifted out of the pending list before any
    // handler runs, so a handler that seeks (and so clears the list) meets
    // a consistent one.
    readonly System.Collections.Generic.List<(long ptsUs, Guid uuid, byte[] buffer, int length)> _userDataDue = new();
    // Bumped by every clear, so a delivery loop can tell that a handler
    // abandoned the timeline under it and stop handing out the rest.
    int _userDataTimeline;
    // Sized to the engine's per-message ceiling, so nothing it holds is
    // ever too large to cross; allocated on the first drain and kept for
    // the component's life.
    byte[] _userDataBytes;
    const int UserDataBytesCapacity = 64 * 1024;
    const int UserDataRecordsPerDrain = 64;
    readonly BasisSidecarSubtitleEngine _subtitles = new();
    readonly System.Collections.Generic.List<BasisSubtitleTrack> _subtitleTracks = new();

    public BmState State { get; private set; } = BmState.Idle;
    public int ErrorCode { get; private set; }
    public double PositionSeconds { get; private set; }
    public double DurationSeconds { get; private set; }
    public long BankedMilliseconds { get; private set; }
    public ulong FramesDecoded { get; private set; }
    public ulong FramesPresented { get; private set; }
    public Texture Texture => _texture;

    /// <summary>Cover art the container carried, decoded, or null. Audio-only
    /// sources with art drive it onto the output texture, so a screen shows
    /// the sleeve rather than black.</summary>
    public Texture2D Artwork => _artwork;

    /// <summary>The same texture as <see cref="Texture"/>, under the name
    /// <see cref="OutputTextureChanged"/> is already spelled with. Sandbox
    /// permission grants name a member, so this is the name a world script or
    /// a DMX bridge asks for.</summary>
    public Texture OutputTexture => _texture;

    public long AudioFramesPulled => System.Threading.Interlocked.Read(ref _audioFramesPulled);

    /// <summary>Where the engine capture was written for the current session,
    /// or null when <see cref="engineCapture"/> is off or the filename was
    /// refused. Set at open.</summary>
    public string EngineCapturePath { get; private set; }

    /// <summary>The audio sink on this GameObject, or null when there is
    /// none and the session plays silently.</summary>
    public BasisMediaPlayerAudio AudioComponent => _audio;

    /// <summary>The decoded frame size (zero until the engine announces
    /// dimensions).</summary>
    public Vector2Int VideoSize { get; private set; }

    /// <summary>Whether row 0 of <see cref="Texture"/> is the top of the
    /// picture, so a sink sampling with v=0 at the bottom has to flip.
    /// D3D11 hands over a top-left-origin texture; the Android convert
    /// pass already writes rows in Unity's Vulkan sampling
    /// orientation.</summary>
    public bool OutputFrameIsTopLeftOrigin
    {
        get
        {
            // Cover art is decoded by Unity, which writes row 0 at the
            // bottom as it does for any Texture2D. Only the engine's own
            // frames follow the platform's convention, so a sink told
            // otherwise flips a picture that was already the right way up.
            if (_textureIsArtwork) return false;
#if UNITY_ANDROID && !UNITY_EDITOR
            return false;
#else
            return true;
#endif
        }
    }

    /// <summary>Raised when the output texture is created or dropped
    /// (null on close). Output sinks bind on this rather than polling.
    /// </summary>
    public event Action<Texture> OutputTextureChanged;

    /// <summary>Raised once when the session reaches the end of the
    /// stream.</summary>
    public event Action Ended;

    /// <summary>The stream's audio sample rate (Hz; 0 until announced).
    /// The pull consumes the engine ring at this rate regardless of the
    /// device DSP rate.</summary>
    public int AudioSampleRate => System.Threading.Volatile.Read(ref _engineSampleRate);

    /// <summary>The stream's channel count (0 until announced).</summary>
    public int AudioChannels => System.Threading.Volatile.Read(ref _engineChannels);

    /// <summary>The engine-declared capability set (§6.11) — what this
    /// basis_media build will decode and play. Queried once and cached;
    /// null when the plugin is unavailable or the ABI mismatched. See
    /// <see cref="BasisMediaCapabilities"/> for the raw JSON and
    /// re-query.</summary>
    public static BmCapabilitySet EngineCapabilities => BasisMediaCapabilities.Set;

    /// <summary>Decode-route preference applied to every descriptor this
    /// component builds at open (takes effect on the next open).
    /// Deliberately a static, not a serialised inspector field: it is the
    /// user's machine setting, persisted client-side by the settings UI,
    /// never world content — and never subject to prefab-serialisation
    /// drift.</summary>
    public static BmDecodePreference DecodePreference = BmDecodePreference.HardwareWithFallback;

    /// <summary>Jitter buffer depth for players that have not been tuned
    /// individually, in milliseconds; 0 = Auto, which sizes itself from the
    /// delivery delays it observes and is right for almost everyone.
    ///
    /// A static for the same reason as <see cref="DecodePreference"/>, and a
    /// stronger one: what this trades off is the viewer's own connection
    /// against how soon they see a frame, and a world author cannot see that
    /// connection. Audio-leading start and the divergence bound stay authored
    /// — one is a property of the content, the other of the shared
    /// experience.</summary>
    public static int DefaultBufferDepthMs;

    /// <summary>This player's own depth, when the viewer has tuned it away
    /// from the default; null while it follows <see cref="DefaultBufferDepthMs"/>.
    ///
    /// Per player because one scene can hold both: a source next door, where
    /// the point is the latency a shallow buffer buys, and one from the far
    /// side of the world that only plays smoothly with depth behind it. A
    /// single figure cannot serve those two at once.
    ///
    /// Not serialised and never synced: it describes this viewer's route to
    /// that source, which no other client shares and no world can know.
    /// Read when a session opens, so <see cref="ReopenAtPosition"/> is what
    /// makes a change take effect on one already running.</summary>
    /// <remarks><see cref="NonSerializedAttribute"/> states what the comment above
    /// already says. Unity cannot serialise a nullable either way, so the attribute
    /// changes no behaviour — it stops the serialization analyzer reporting a field
    /// that is skipped on purpose, and keeps a real one from hiding among those.</remarks>
    [NonSerialized] public int? BufferDepthOverrideMs;

    /// <summary>The depth this player actually opens with.</summary>
    public int EffectiveBufferDepthMs => BufferDepthOverrideMs ?? DefaultBufferDepthMs;

    /// <summary>Raised when one of this player's caption overrides moves, so a
    /// presenter showing it can re-read. Style follows the viewer, so it is not
    /// part of the session and nothing here is synced.</summary>
    public event Action CaptionPreferencesChanged;

    bool? _captionsEnabledOverride;
    float? _captionTextOpacityOverride;
    float? _captionBackgroundOpacityOverride;

    /// <summary>Whether captions are drawn for this player, when the viewer
    /// has decided it for this one; null while it follows the stored
    /// preference. Per player because a scene can hold one source worth
    /// reading and another that is only ever background.</summary>
    public bool? CaptionsEnabledOverride
    {
        get => _captionsEnabledOverride;
        set { if (_captionsEnabledOverride != value) { _captionsEnabledOverride = value; CaptionPreferencesChanged?.Invoke(); } }
    }

    /// <summary>Caption text opacity for this player, or null to follow the
    /// stored preference.</summary>
    public float? CaptionTextOpacityOverride
    {
        get => _captionTextOpacityOverride;
        set { if (_captionTextOpacityOverride != value) { _captionTextOpacityOverride = value; CaptionPreferencesChanged?.Invoke(); } }
    }

    /// <summary>Caption background opacity for this player, or null to follow
    /// the stored preference.</summary>
    public float? CaptionBackgroundOpacityOverride
    {
        get => _captionBackgroundOpacityOverride;
        set { if (_captionBackgroundOpacityOverride != value) { _captionBackgroundOpacityOverride = value; CaptionPreferencesChanged?.Invoke(); } }
    }

    /// <summary>Whether captions should be drawn for this player.</summary>
    public bool CaptionsEnabledEffective =>
        _captionsEnabledOverride ?? BasisMediaSettings.CaptionsEnabled.RawValue;

    /// <summary>Caption text opacity for this player, 0..1.</summary>
    public float CaptionTextOpacityEffective =>
        _captionTextOpacityOverride ?? BasisMediaSettings.CaptionTextOpacity.RawValue;

    /// <summary>Caption background opacity for this player, 0..1.</summary>
    public float CaptionBackgroundOpacityEffective =>
        _captionBackgroundOpacityOverride ?? BasisMediaSettings.CaptionBackgroundOpacity.RawValue;

    /// <summary>The in-band CEA-608 caption currently due at the playback
    /// position (empty = none). Rows are joined with '\n'.</summary>
    public string CurrentCaption { get; private set; } = "";

    /// <summary>Raised when <see cref="CurrentCaption"/> changes (an empty
    /// string is a clear). Raised regardless of whether the viewer has
    /// captions switched on, so a display can stay primed while hidden.
    /// </summary>
    public event Action<string> CaptionChanged;

    /// <summary>
    /// One SEI user-data message due at the playback position. The engine
    /// hands these over unparsed: <paramref name="payload"/> is whatever
    /// followed the 16-byte UUID inside the `user_data_unregistered`
    /// message, and the handler decides what it means. It is borrowed for
    /// the call — copy what outlives it.
    /// </summary>
    public delegate void UserDataHandler(long ptsUs, Guid uuid, ReadOnlySpan<byte> payload);

    /// <summary>
    /// Raised for each SEI user_data_unregistered message (H.264 and H.265
    /// payload type 5) once playback reaches its timestamp, in timestamp
    /// order (messages with equal timestamps keep their stream order).
    /// Every UUID arrives, the encoder's own included (x264 stamps its build
    /// string on keyframes this way), so a consumer filters on the UUID it
    /// expects. A seek drops whatever was queued from the old position.
    /// Messages are held until due whether or not anyone is subscribed, so
    /// a subscriber attaching mid-session receives everything still to come
    /// and nothing already past.
    /// </summary>
    public event UserDataHandler UserDataReceived;

    /// <summary>Out-of-band subtitle tracks offered for this source. The
    /// media carries none of these — a resolver or other enrichment source
    /// supplies them through <see cref="SetSubtitleTracks"/>.</summary>
    public System.Collections.Generic.IReadOnlyList<BasisSubtitleTrack> SubtitleTracks => _subtitleTracks;

    /// <summary>Index into <see cref="SubtitleTracks"/>, or -1 for the
    /// default: no sidecar track, with in-band captions flowing as usual.
    /// While a sidecar track is selected, in-band cues are suppressed.
    /// Client-side only, like the viewer's caption preferences in
    /// <see cref="BasisMediaSettings"/>.</summary>
    public int SelectedSubtitleTrackIndex { get; private set; } = -1;

    /// <summary>Selection changed, including the automatic revert to -1
    /// when a fetch fails or the session closes.</summary>
    public event Action<int> SubtitleTrackChanged;

    readonly System.Collections.Generic.List<BasisAudioTrack> _audioTracks =
        new System.Collections.Generic.List<BasisAudioTrack>();
    int _audioTrackIndex;
    bool _audioTracksRead;
    // Set while SelectAudioTrack is re-opening, so the position restore
    // and the enumeration refresh know this is a track switch rather than
    // a fresh load.
    double _reopenResumeAt = -1d;

    /// <summary>The audio tracks this source offers, in container order.
    /// Empty when there is nothing to choose between — one track, or a
    /// container that does not enumerate them — so a picker can simply
    /// hide itself on an empty list.</summary>
    public System.Collections.Generic.IReadOnlyList<BasisAudioTrack> AudioTracks => _audioTracks;

    /// <summary>Which of <see cref="AudioTracks"/> is playing.</summary>
    public int SelectedAudioTrackIndex => _audioTrackIndex;

    /// <summary>Fires when the offered tracks or the selection change:
    /// on open once the engine has read the container, and after a
    /// successful switch.</summary>
    public event Action<int> AudioTrackChanged;

    /// <summary>Play a different audio track. The engine binds its audio
    /// track when the container is opened, so this re-opens the session
    /// and returns to the current position rather than switching in
    /// place — a short re-buffer, in exchange for no new class of race
    /// against a live Bank. Ignored if the index is already selected or
    /// out of range.</summary>
    public void SelectAudioTrack(int index)
    {
        if (index < 0 || index >= _audioTracks.Count || index == _audioTrackIndex)
            return;
        if (string.IsNullOrEmpty(ActiveStreamUrl))
            return;

        _audioTrackIndex = index;
        ReopenAtPosition();
        AudioTrackChanged?.Invoke(_audioTrackIndex);
    }

    /// <summary>Re-open the current source where it is, so a setting the
    /// engine only reads at open takes effect now. Costs a short re-buffer;
    /// a live source rejoins the edge because it has nothing to return to.
    /// Re-opens the stream the session is already on rather than routing
    /// through the resolver again, and does not go through the networking
    /// component — this is one viewer's own session, and nobody else's
    /// playback should move because of it.</summary>
    public void ReopenAtPosition()
    {
        // ActiveStreamUrl outlives a close, so the state check is what stops
        // a setting change starting a player that was stopped or had ended.
        if (string.IsNullOrEmpty(ActiveStreamUrl))
            return;
        if (State != BmState.Opening && State != BmState.Buffering
            && State != BmState.Playing && State != BmState.Paused)
            return;

        // Remembered across the re-open; a live source has nothing to
        // return to, so it simply rejoins the edge.
        _reopenResumeAt = DurationSeconds > 0d ? PositionSeconds : -1d;
        string streamUrl = ActiveStreamUrl;
        string streamAudioUrl = ActiveAudioStreamUrl;
        bool wasPlaying = State == BmState.Playing || State == BmState.Buffering;
        Open(streamUrl, streamAudioUrl);
        if (wasPlaying) Play();
    }

    /// <summary>Replaces the offered tracks and drops any selection. The
    /// tracks belong to a source, so set them per open.</summary>
    public void SetSubtitleTracks(System.Collections.Generic.IReadOnlyList<BasisSubtitleTrack> tracks)
    {
        SelectSubtitleTrack(-1);
        _subtitleTracks.Clear();
        if (tracks == null)
            return;
        for (int i = 0; i < tracks.Count; i++)
        {
            if (tracks[i] != null && !string.IsNullOrEmpty(tracks[i].Url))
                _subtitleTracks.Add(tracks[i]);
        }
    }

    /// <summary>Selects a sidecar track by <see cref="SubtitleTracks"/>
    /// index, or -1 to return to in-band captions. The track is fetched
    /// once, and the URL is checked against the client's URL security
    /// first; on failure the
    /// selection reverts to -1.</summary>
    public void SelectSubtitleTrack(int index)
    {
        if (index < 0 || index >= _subtitleTracks.Count)
            index = -1;
        if (index == SelectedSubtitleTrackIndex)
            return;

        SelectedSubtitleTrackIndex = index;
        // Whatever is on screen belongs to the previous selection; the
        // in-band feed repaints at its next cue change.
        ClearCaptionDisplay();
        if (index < 0)
        {
            _subtitles.Clear();
            SubtitleTrackChanged?.Invoke(-1);
            return;
        }
        SubtitleTrackChanged?.Invoke(index);
        _ = LoadSubtitleTrackAsync(_subtitleTracks[index], index);
    }

    async System.Threading.Tasks.Task LoadSubtitleTrackAsync(BasisSubtitleTrack track, int index)
    {
        bool loaded = await _subtitles.LoadTrackAsync(track);
        // A later selection (or a close) already superseded this fetch.
        if (SelectedSubtitleTrackIndex != index)
            return;
        if (loaded)
        {
            // The cue covering the current position has never been
            // reported, so let it through on the next tick.
            _subtitles.ResetCueTracking();
            return;
        }
        BasisDebug.LogWarning($"[BasisMedia] subtitle track {index} failed to load; reverting to in-band captions.", BasisDebug.LogTag.Video);
        SelectedSubtitleTrackIndex = -1;
        _subtitles.Clear();
        SubtitleTrackChanged?.Invoke(-1);
    }

    void ClearUserData()
    {
        foreach (var pending in _userDataPending)
            ArrayPool<byte>.Shared.Return(pending.buffer);
        _userDataPending.Clear();
        _userDataMaxPtsUs = long.MinValue;
        _userDataTimeline++;
    }

    void ClearCaptionDisplay()
    {
        _captionQueue.Clear();
        _subtitles.ResetCueTracking();
        SetCaption("");
    }

    void SetCaption(string text)
    {
        text ??= "";
        if (CurrentCaption == text)
            return;
        CurrentCaption = text;
        CaptionChanged?.Invoke(text);
    }

    /// Unity stamps a full call stack onto every Log-level Console line,
    /// which buries the event drain under a dozen frames of its own
    /// plumbing. The setting is application-wide — Unity offers no
    /// narrower one — so it is applied once, and only once a player
    /// exists to need it.
    static bool _logStackTracesTrimmed;

    void Awake()
    {
        if (!_logStackTracesTrimmed)
        {
            _logStackTracesTrimmed = true;
            Application.SetStackTraceLogType(LogType.Log, StackTraceLogType.None);
        }

        _frameTick = new BasisMediaDriverTick(Tick);
        // The sink lives beside the player, as it does in the authored
        // prefabs. It is optional: a player with none is a decoder with no
        // speakers wired to it.
        TryGetComponent(out _audio);
        BasisMediaPlayerRegistry.Add(this);
    }

    void OnEnable() => _frameTick.Arm();

    void OnDisable() => _frameTick.Disarm();

    /// <summary>The authored URL for the platform this build runs on:
    /// <see cref="androidUrl"/> on an Android player when it is set, and
    /// <see cref="url"/> otherwise. The editor always reports
    /// <see cref="url"/>, so entering play mode with the Android build target
    /// selected does not silently exercise the other one.</summary>
    public string ResolvedUrl
    {
        get
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            if (!string.IsNullOrEmpty(androidUrl)) return androidUrl.Trim();
#endif
            return url?.Trim();
        }
    }

    void Start()
    {
        // Through the router, so an authored page URL resolves rather
        // than failing to open.
        string authored = ResolvedUrl;
        if (playOnStart && !string.IsNullOrEmpty(authored))
            OpenUserUrl(authored);
    }

    /// <summary>Open a split pair: a video-only source and the
    /// audio-only one that belongs with it. Pass null for the second
    /// argument to open an ordinary muxed source and drop any pair
    /// left over from a previous open.</summary>
    public void Open(string sourceUrl, string sourceAudioUrl)
    {
        audioUrl = sourceAudioUrl;
        Open(sourceUrl);
    }

    /// <summary>
    /// Open whatever the user actually typed or a world author authored,
    /// steering page URLs (a YouTube or Twitch watch page) through any
    /// installed resolver. A directly-playable URL opens straight
    /// through, and with no resolver installed every URL does — the same
    /// behaviour as having no integration at all.
    /// </summary>
    public void OpenUserUrl(string sourceUrl)
    {
        // Refused up front as well as at Open, so a locked client never hands a page
        // URL to the resolver: that route leaves this method and comes back through
        // OpenResolved later, by which time the extraction has already happened.
        if (BasisNetworkModeration.MediaPlayerBlockedLocally)
        {
            BasisDebug.LogWarning(
                "BasisMediaPlayer.OpenUserUrl blocked: media players are locked by an admin.",
                BasisDebug.LogTag.Video);
            return;
        }
        sourceUrl = BasisMediaUrlRouter.NormalizeUrl(sourceUrl);
        if (string.IsNullOrEmpty(sourceUrl))
            return;
        // The resolver owns the load once it claims the URL: it opens the
        // player itself, asynchronously, when extraction finishes.
        if (!BasisMediaUrlRouter.IsDirectlyPlayable(sourceUrl)
            && BasisMediaUrlRouter.TryResolveAndLoad(this, sourceUrl))
            return;
        Open(sourceUrl, null);
    }

    /// <summary>
    /// Open what a resolver produced: the stream or pair of streams, the
    /// liveness it already knows, and the subtitle tracks and display
    /// metadata it picked up on the way.
    /// </summary>
    public void OpenResolved(BasisResolvedMedia media)
    {
        if (media == null || string.IsNullOrEmpty(media.Url))
            return;
        liveness = media.Liveness;
        // The URL fields keep what a person actually asked for. What a
        // resolver extracted is issued per client and carries an expiry, so
        // writing it back would replace a page URL that keeps working with a
        // stream URL that stops, and would overwrite an authored field on the
        // way. See ActiveStreamUrl for what is really open.
        string askedFor = !string.IsNullOrEmpty(media.SourceUrl) ? media.SourceUrl : url;
        string askedForAudio = audioUrl;
        // All of this lands after Open, which clears what the previous
        // source left behind.
        Open(media.Url, media.AudioUrl);
        url = askedFor;
        audioUrl = askedForAudio;
        Media = media;
        SetSubtitleTracks(media.SubtitleTracks);
        MediaChanged?.Invoke(media);
    }

    /// <summary>The stream the engine was actually handed, which for a
    /// resolved page URL is the extracted stream rather than the page. Not
    /// serialised, and not shareable: these carry an expiry and are issued per
    /// client. Null until something has been opened.</summary>
    public string ActiveStreamUrl { get; private set; }

    /// <summary>The audio leg the engine was handed, when the source is a
    /// split pair. Empty otherwise.</summary>
    public string ActiveAudioStreamUrl { get; private set; }

    /// <summary>What is playing, as far as anyone could tell: a
    /// resolver's answer when one handled the load, otherwise null.
    /// </summary>
    public BasisResolvedMedia Media { get; private set; }

    /// <summary>Raised when <see cref="Media"/> changes.</summary>
    public event Action<BasisResolvedMedia> MediaChanged;

    /// <summary>Bumped by every open. A resolver captures it before its
    /// extraction and drops the result if it changed, so a slow resolve
    /// cannot overwrite a load the user started after it.</summary>
    public int LoadGeneration { get; private set; }

    /// <summary>Report a load that failed before the engine ever saw it —
    /// a resolver that could not extract a page URL. The engine's own
    /// failures arrive through the snapshot instead.</summary>
    public void ReportLoadError(Exception error)
    {
        BasisDebug.LogError($"[BasisMedia] load failed: {error?.GetType().Name ?? "unknown"}", BasisDebug.LogTag.Video);
        State = BmState.Error;
    }

    /// <summary>Open (or re-open) a source. The engine opens asynchronously;
    /// watch <see cref="State"/>. Keeps whichever split-pair audio source
    /// the last <see cref="Open(string, string)"/> set, so re-opening a
    /// resolved pair re-opens it as a pair; pass null there to drop it.
    /// </summary>
    public void Open(string sourceUrl)
    {
        // The load funnel: the split-pair overload, OpenUserUrl's direct path, a
        // resolver's OpenResolved and the re-open all land here, so the moderation
        // lock is enforced here rather than at each of them.
        if (BasisNetworkModeration.MediaPlayerBlockedLocally)
        {
            BasisDebug.LogWarning(
                "BasisMediaPlayer.Open blocked: media players are locked by an admin.",
                BasisDebug.LogTag.Video);
            return;
        }
        LoadGeneration++;
        // The track list belongs to a source. A switch re-opens the same
        // one and keeps its choice; anything else starts over, because a
        // remembered index means nothing against different content.
        _audioTracksRead = false;
        _audioTracks.Clear();
        if (_reopenResumeAt < 0d) _audioTrackIndex = 0;
        // Whatever a resolver told us about the last source does not
        // describe this one. OpenResolved fills it back in, and raises
        // the change; a plain open just has nothing to say.
        Media = null;
        Close();
        if (!_abiChecked)
        {
            uint abi = BasisMediaNative.bm_abi_version();
            if (abi != BasisMediaNative.AbiVersion)
            {
                BasisDebug.LogError($"[BasisMedia] basis_media ABI v{abi}, this package needs v{BasisMediaNative.AbiVersion}; refusing.", BasisDebug.LogTag.Video);
                State = BmState.Error;
                return;
            }
            _abiChecked = true;
        }
#if UNITY_ANDROID && !UNITY_EDITOR
        if (SystemInfo.graphicsDeviceType != GraphicsDeviceType.Vulkan)
        {
            BasisDebug.LogError($"[BasisMedia] needs Vulkan on Android, running on {SystemInfo.graphicsDeviceType}", BasisDebug.LogTag.Video);
            State = BmState.Error;
            return;
        }
#else
        if (SystemInfo.graphicsDeviceType != GraphicsDeviceType.Direct3D11)
        {
            BasisDebug.LogError($"[BasisMedia] needs D3D11, running on {SystemInfo.graphicsDeviceType}", BasisDebug.LogTag.Video);
            State = BmState.Error;
            return;
        }
#endif

        url = sourceUrl;
        ActiveStreamUrl = sourceUrl;
        ActiveAudioStreamUrl = audioUrl;
        byte[] descriptor = Encoding.UTF8.GetBytes(BuildDescriptor(sourceUrl));
        int rc = BasisMediaNative.bm_session_open(descriptor, (UIntPtr)descriptor.Length, out _handle);
        if (rc != 0)
        {
            BasisDebug.LogError($"[BasisMedia] bm_session_open failed: {rc}", BasisDebug.LogTag.Video);
            State = BmState.Error;
            return;
        }
        _open = true;
        State = BmState.Opening;
        System.Threading.Volatile.Write(ref _engineChannels, 0);
        System.Threading.Volatile.Write(ref _engineSampleRate, 0);
        System.Threading.Volatile.Write(ref _syncRatePpm, 0);
        System.Threading.Volatile.Write(ref _avOffsetUs, int.MinValue);
        // Re-resolved here as well as in Awake, so a rig that adds the sink
        // after the player still gets sound.
        if (_audio == null) TryGetComponent(out _audio);
        if (_audio != null) _audio.NativePcmSource = this;
#if UNITY_ANDROID && !UNITY_EDITOR
        _sentAudioLatencyUs = -1;
#endif
        _commandBuffer ??= new CommandBuffer { name = "BasisMedia present" };
    }

    static void AppendJsonString(StringBuilder json, string value)
    {
        json.Append('"');
        foreach (char c in value)
        {
            switch (c)
            {
                case '"': json.Append("\\\""); break;
                case '\\': json.Append("\\\\"); break;
                default:
                    if (c < ' ') json.Append($"\\u{(int)c:x4}");
                    else json.Append(c);
                    break;
            }
        }
        json.Append('"');
    }

    string BuildDescriptor(string sourceUrl)
    {
        var json = new StringBuilder(sourceUrl.Length + 96);
        json.Append("{\"url\":");
        AppendJsonString(json, sourceUrl);
        if (!string.IsNullOrEmpty(audioUrl))
        {
            json.Append(",\"audio_url\":");
            AppendJsonString(json, audioUrl);
        }
        if (allowLocalAddresses)
            json.Append(",\"allow_local_addresses\":true");
        int depthMs = EffectiveBufferDepthMs;
        if (depthMs > 0)
            json.Append($",\"buffer_depth_ms\":{depthMs}");
        if (liveness == BmLiveness.Live)
            json.Append(",\"liveness\":\"live\"");
        else if (liveness == BmLiveness.Vod)
            json.Append(",\"liveness\":\"vod\"");
        if (_audioTrackIndex > 0)
            json.Append($",\"audio_track\":{_audioTrackIndex}");
        if (maxDivergenceMs > 0)
            json.Append($",\"max_divergence_ms\":{maxDivergenceMs}");
        if (DecodePreference == BmDecodePreference.HardwareOnly)
            json.Append(",\"decode_preference\":\"hardware_only\"");
        else if (DecodePreference == BmDecodePreference.SoftwareOnly)
            json.Append(",\"decode_preference\":\"software_only\"");
        if (engineCapture)
        {
            string path = BasisMediaCapturePath.Resolve(
                engineCaptureFileName, "BasisMediaEngine.csv", out string refusal);
            if (path == null)
            {
                BasisDebug.LogWarning($"[BasisMedia] engine capture refused: {refusal}", BasisDebug.LogTag.Video);
            }
            else
            {
                EngineCapturePath = path;
                json.Append(",\"diag_csv\":");
                AppendJsonString(json, path);
                if (engineCaptureAppend)
                    json.Append(",\"diag_csv_append\":true");
            }
        }
        json.Append('}');
        return json.ToString();
    }

    public void Play()
    {
        if (_open) BasisMediaNative.bm_session_play(_handle);
    }

    public void Pause()
    {
        if (_open) BasisMediaNative.bm_session_pause(_handle);
    }

    /// <summary>Raised when a seek is issued, carrying the requested position in
    /// seconds. Shared playback broadcasts on this, so a seek from a world script
    /// reaches the other viewers the same way one from the menu does.</summary>
    public event Action<double> Seeked;

    public void Seek(double seconds)
    {
        if (!_open)
            return;
        BasisMediaNative.bm_session_seek(_handle, (long)(seconds * 1_000_000.0));
        Seeked?.Invoke(seconds);
        // Audio still in the sink's window belongs to the timeline being
        // left behind; playing it out would be heard against the landed
        // picture.
        _audio?.ResetSyncAnchor();
        // Queued in-band cues are stamped against the old timeline, so
        // they would either flush in a burst or sit undue past a backwards
        // seek. The engine emits its own clear at the landed position.
        ClearCaptionDisplay();
        ClearUserData();
    }

    /// <summary>
    /// Feed the shared-playback owner's position (seconds) as a soft
    /// sync target. The engine corrects with dead band → gentle rate
    /// slew → seek only past a large threshold, and extrapolates the
    /// target at 1x between calls — one call per received heartbeat
    /// is enough. Live sources ignore targets (divergence is bounded
    /// by <see cref="maxDivergenceMs"/> instead).
    /// </summary>
    public void SetSyncTarget(double seconds)
    {
        if (_open) BasisMediaNative.bm_session_set_sync_target(_handle, (long)(seconds * 1_000_000.0));
    }

    /// <summary>Stop chasing a sync target (local user took control,
    /// or the owner left).</summary>
    public void ClearSyncTarget()
    {
        if (_open) BasisMediaNative.bm_session_set_sync_target(_handle, -1);
    }

    /// <summary>The sync ladder's current rate offset from 1x, ppm
    /// (0 = none). Applied by the audio pull automatically; exposed
    /// for diagnostics/UI.</summary>
    public int SyncRatePpm => System.Threading.Volatile.Read(ref _syncRatePpm);

    /// <summary>Presented video pts minus the audio playhead, microseconds,
    /// as the engine measures it (both read on one engine tick, so it is a
    /// difference rather than two samples taken a frame apart).
    /// <c>int.MinValue</c> until audio and a presented frame both exist.
    /// Diagnostics only.</summary>
    public int AvOffsetUs => System.Threading.Volatile.Read(ref _avOffsetUs);

    public void Close()
    {
        // Before the handle goes: the sink pulls on the audio thread, and
        // dropping the source is what stops it.
        if (_audio != null && ReferenceEquals(_audio.NativePcmSource, this))
            _audio.NativePcmSource = null;
        if (_open)
        {
            BasisMediaNative.bm_session_close(_handle);
            _open = false;
        }
        State = BmState.Idle;
        // The per-session engine readings describe a session that no longer
        // exists. Left alone they survive into the next one: the open path
        // clears them only after `bm_session_open` succeeds, so a close, or an
        // open that fails or is refused before that point, leaves the previous
        // session's values readable — and the diagnostics recorder writes them
        // into its capture for an idle player, which is a poisoned column
        // rather than a cosmetic wart. Clearing them here covers the open path
        // too, since `Open` closes first; the one path that deliberately does
        // not reach here is a moderation-blocked open, which returns before
        // `Close` and leaves a still-playing session's readings alone.
        System.Threading.Volatile.Write(ref _engineChannels, 0);
        System.Threading.Volatile.Write(ref _engineSampleRate, 0);
        System.Threading.Volatile.Write(ref _syncRatePpm, 0);
        System.Threading.Volatile.Write(ref _avOffsetUs, int.MinValue);
#if UNITY_ANDROID && !UNITY_EDITOR
        if (_renderHooked)
        {
            RenderPipelineManager.endCameraRendering -= OnEndCameraRendering;
            _renderHooked = false;
        }
#endif
        bool hadTexture = _texture != null;
        SetOutputTexture(null);
        _textureIsArtwork = false;
        _artworkRead = false;
        if (_artwork != null)
        {
            Destroy(_artwork);
            _artwork = null;
        }
        VideoSize = Vector2Int.zero;
        _subtitles.Clear();
        if (SelectedSubtitleTrackIndex != -1)
        {
            SelectedSubtitleTrackIndex = -1;
            SubtitleTrackChanged?.Invoke(-1);
        }
        ClearCaptionDisplay();
        ClearUserData();
        if (hadTexture)
            OutputTextureChanged?.Invoke(null);
    }

    /// <summary>
    /// Install <paramref name="texture"/> as the output and dispose of whatever
    /// it replaces.
    ///
    /// On Vulkan the plugin holds an image view over the texture it was
    /// registered with and can only destroy it from a later render event, so
    /// the image has to stay alive past the call that ends the registration —
    /// a close or a replacement alike — and the retirement queue is what holds
    /// it. Elsewhere the plugin owns nothing that outlives the session, so the
    /// texture is destroyed here; Unity releases the graphics resource through
    /// the render command queue, which orders it after the events already
    /// issued for it.
    ///
    /// The replacement case cannot arise today, since the texture is only
    /// created where there is none. Routing it through here anyway is what
    /// covers a future resolution change by construction rather than by memory.
    /// </summary>
    void SetOutputTexture(Texture texture)
    {
        Texture previous = _texture;
        _texture = texture;
        // The cover art has an owner already: Close destroys it by name, and it
        // is the one texture here the session never drew into.
        if (previous == null || ReferenceEquals(previous, texture)
            || ReferenceEquals(previous, _artwork))
            return;
#if UNITY_ANDROID && !UNITY_EDITOR
        BasisMediaTextureRetirement.Retire(previous);
#else
        Destroy(previous);
#endif
    }

    void Update() => _frameTick.RunFromUpdate();

    /// <summary>
    /// The frame's work for this player and for everything wired to it: the
    /// engine poll first, then the components that read what it wrote, in the
    /// order they depend on each other. Left to their own Update methods they
    /// have no order between them, so a consumer could describe the previous
    /// frame's snapshot instead of this one's.
    /// </summary>
    void Tick()
    {
        PollSession();
        // Count is re-read rather than cached: a consumer is free to disable
        // itself from inside its own tick, which drops it from this list.
        for (int i = 0; i < _consumers.Count; i++)
        {
            // One consumer throwing used to cost only its own Update; from
            // here it would take the rest of the tick with it.
            try
            {
                _consumers[i].MediaTick();
            }
            catch (Exception e)
            {
                BasisDebug.LogErrorOnce(
                    $"[BasisMedia] {_consumers[i].GetType().Name} tick failed: {e}",
                    BasisDebug.LogTag.Video);
            }
        }
    }

    /// <summary>Register for the ordered tick. From the consumer's OnEnable;
    /// it is dropped again from OnDisable.</summary>
    internal void AddTickConsumer(IBasisMediaTickConsumer consumer)
    {
        if (consumer == null || _consumers.Contains(consumer))
            return;
        int at = _consumers.Count;
        while (at > 0 && _consumers[at - 1].TickStage > consumer.TickStage)
            at--;
        _consumers.Insert(at, consumer);
    }

    internal void RemoveTickConsumer(IBasisMediaTickConsumer consumer) => _consumers.Remove(consumer);

    void PollSession()
    {
        if (!_open)
            return;

        if (BasisMediaNative.bm_session_poll(_handle, out var snapshot) != 0)
            return;
        BmState previousState = State;
        State = (BmState)snapshot.State;
        ErrorCode = snapshot.ErrorCode;
        if (snapshot.Width > 0)
            VideoSize = new Vector2Int((int)snapshot.Width, (int)snapshot.Height);
        PositionSeconds = snapshot.PositionUs / 1e6;
        DurationSeconds = snapshot.DurationUs / 1e6;
        BankedMilliseconds = snapshot.BankedMs;
        FramesDecoded = snapshot.FramesDecoded;
        FramesPresented = snapshot.FramesPresented;
        System.Threading.Volatile.Write(ref _engineChannels, (int)snapshot.AudioChannels);
        System.Threading.Volatile.Write(ref _engineSampleRate, (int)snapshot.AudioSampleRate);
        System.Threading.Volatile.Write(ref _syncRatePpm, snapshot.SyncRatePpm);
        System.Threading.Volatile.Write(ref _avOffsetUs, snapshot.AvOffsetUs);
        if (_audio != null && snapshot.AudioSampleRate > 0 && snapshot.AudioChannels > 0)
            _audio.SetExpectedFormat((int)snapshot.AudioSampleRate, (int)snapshot.AudioChannels);

#if UNITY_ANDROID && !UNITY_EDITOR
        // A/V output-latency compensation: the engine masters the clock
        // on the pull playhead, but audible audio leaves the speaker one
        // DSP output chain later. Report the sink's estimate so video
        // paces to the audible position. Android only — the desktop
        // offset is inside the sync noise floor. With no sink there is no
        // audio master to compensate.
        long latencyUs = _audio != null ? _audio.EstimatedOutputLatencyUs : 0;
        if (latencyUs != _sentAudioLatencyUs)
        {
            BasisMediaNative.bm_session_set_audio_latency(_handle, latencyUs);
            _sentAudioLatencyUs = latencyUs;
        }
#endif

        DrainEvents();
        DrainCaptions(snapshot.PositionUs);
        DrainUserData(snapshot.PositionUs);
        RefreshAudioTracks();
        RefreshArtwork(snapshot);

        // A track switch re-opened the session; return to where playback
        // was once the new one can accept a seek.
        if (_reopenResumeAt >= 0d && DurationSeconds > 0d
            && (State == BmState.Playing || State == BmState.Buffering))
        {
            double resume = _reopenResumeAt;
            _reopenResumeAt = -1d;
            Seek(resume);
        }

        if (State == BmState.Ended && previousState != BmState.Ended)
            Ended?.Invoke();

        if (State == BmState.Error)
        {
            string why = string.IsNullOrEmpty(_lastErrorDetail) ? "no detail reported" : _lastErrorDetail;
            BasisDebug.LogError(
                $"[BasisMedia] session error {snapshot.ErrorCode} " +
                $"({(BmErrorCategory)snapshot.ErrorCategory}): {why} [{url}]",
                BasisDebug.LogTag.Video);
            // Nothing ticks this session again, so what is queued would hold
            // its pooled buffers until the next open.
            ClearUserData();
            _open = false;
            return;
        }

        if (_texture == null && snapshot.Width > 0)
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            // Vulkan graphics contract (normative in the ABI header):
            // linear RGBA32 RenderTexture with enableRandomWrite at
            // display size; the engine's convert pass writes rows in
            // Unity's Vulkan sampling orientation, so no material flip.
            var target = new RenderTexture((int)snapshot.Width, (int)snapshot.Height, 0,
                RenderTextureFormat.ARGB32, RenderTextureReadWrite.Linear)
            {
                enableRandomWrite = true,
            };
            target.Create();
            SetOutputTexture(target);
            BasisMediaNative.bm_session_set_output_texture(_handle, target.GetNativeTexturePtr());
#else
            var texture = new Texture2D((int)snapshot.Width, (int)snapshot.Height, TextureFormat.BGRA32, false);
            SetOutputTexture(texture);
            BasisMediaNative.bm_session_set_output_texture(_handle, texture.GetNativeTexturePtr());
#endif
            OutputTextureChanged?.Invoke(_texture);
        }

        if (_texture != null && !_textureIsArtwork)
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            // Camera.AddCommandBuffer is silently ignored under URP;
            // the render event rides the pipeline's per-camera callback
            // instead, once per frame after the first camera finishes.
            if (GraphicsSettings.currentRenderPipeline != null)
            {
                if (!_renderHooked)
                {
                    _commandBuffer.Clear();
                    _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(),
                        BasisMediaNative.RenderEventPresent, (IntPtr)_handle);
                    RenderPipelineManager.endCameraRendering += OnEndCameraRendering;
                    _renderHooked = true;
                }
            }
            else
            {
                _commandBuffer.Clear();
                _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(),
                    BasisMediaNative.RenderEventPresent, (IntPtr)_handle);
                Graphics.ExecuteCommandBuffer(_commandBuffer);
            }
#else
            _commandBuffer.Clear();
            _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(),
                BasisMediaNative.RenderEventPresent, (IntPtr)_handle);
            Graphics.ExecuteCommandBuffer(_commandBuffer);
#endif
        }
    }

#if UNITY_ANDROID && !UNITY_EDITOR
    void OnEndCameraRendering(ScriptableRenderContext context, Camera camera)
    {
        if (!_open || _texture == null || !isActiveAndEnabled)
            return;
        if (_lastRenderEventFrame == Time.frameCount)
            return;
        _lastRenderEventFrame = Time.frameCount;
        Graphics.ExecuteCommandBuffer(_commandBuffer);
    }
#endif

    /// Detail of the most recent failure, held so the error the session
    /// reports can say what went wrong rather than only which code it was.
    /// The engine's `Error` event carries it; the snapshot carries only a
    /// number.
    private string _lastErrorDetail;

    unsafe void DrainEvents()
    {
        var events = stackalloc BmEvent[8];
        int count = BasisMediaNative.bm_session_drain_events(_handle, events, 8);
        for (int i = 0; i < count; i++)
        {
            string detail = Encoding.UTF8.GetString(events[i].Detail, (int)events[i].DetailLen);
            if (events[i].Code == (uint)BmEventCode.Error) _lastErrorDetail = detail;
            // WallUs is the session's own monotonic clock, so a line can be
            // lined up against either diagnostics CSV without hand-aligning.
            string at = (events[i].WallUs / 1_000_000.0).ToString("F3", CultureInfo.InvariantCulture);
            BasisDebug.Log(
                $"[BasisMedia +{at}s] {(BmEventCode)events[i].Code}/{(BmStage)events[i].Stage}: {detail}",
                BasisDebug.LogTag.Video);
        }
    }

    // Cues arrive ahead of presentation stamped with their due PTS; hold
    // them until the playback position reaches each one, so captions stay
    // in lockstep with the frame they belong to regardless of the decode
    // buffer's lead.
    // The engine learns the track list when it opens the container, which
    // is after the session handle exists — so this polls until the list
    // arrives, then stops. Cheap: one integer call per frame until the
    // container is parsed, none afterwards.
    /// <summary>Fetch the container's cover art once the session has opened.
    /// The engine hands over the stored JPEG/PNG bytes; Unity decodes them,
    /// which is why no image parser lives in native code.</summary>
    unsafe void RefreshArtwork(BmSnapshot snapshot)
    {
        if (_artworkRead) return;
        int len = BasisMediaNative.bm_session_artwork_len(_handle);
        if (len <= 0)
        {
            // 0 is "no art"; only settle once the container has been read,
            // since the answer is not known while still opening.
            if (State != BmState.Opening) _artworkRead = true;
            return;
        }
        _artworkRead = true;

        var data = new byte[len];
        var mime = new byte[64];
        int got;
        fixed (byte* d = data)
        fixed (byte* m = mime)
        {
            got = BasisMediaNative.bm_session_get_artwork(_handle, d, (uint)data.Length, m, (uint)mime.Length);
        }
        if (got <= 0)
        {
            BasisDebug.LogWarning($"[BasisMedia] cover art present but unreadable ({got})", BasisDebug.LogTag.Video);
            return;
        }

        var texture = new Texture2D(2, 2, TextureFormat.RGBA32, false);
        if (!texture.LoadImage(data))
        {
            // A container can state any MIME it likes; Unity decodes PNG
            // and JPEG, and anything else is simply not shown.
            int end = System.Array.IndexOf(mime, (byte)0);
            string stated = Encoding.UTF8.GetString(mime, 0, end < 0 ? mime.Length : end);
            BasisDebug.Log($"[BasisMedia] cover art in an undecodable format ({stated})", BasisDebug.LogTag.Video);
            Destroy(texture);
            return;
        }
        _artwork = texture;

        // A source with pictures of its own owns the output; art fills it
        // only when there is no video to show.
        if (snapshot.Width == 0 && _texture == null)
        {
            SetOutputTexture(_artwork);
            _textureIsArtwork = true;
            VideoSize = new Vector2Int(_artwork.width, _artwork.height);
            OutputTextureChanged?.Invoke(_texture);
        }
    }

    unsafe void RefreshAudioTracks()
    {
        if (_audioTracksRead) return;
        int count = BasisMediaNative.bm_session_audio_track_count(_handle);
        if (count <= 0)
        {
            // Nothing to offer yet, or nothing to offer at all; settle
            // once the session is past opening.
            if (State != BmState.Opening) _audioTracksRead = true;
            return;
        }

        var records = stackalloc BmAudioTrack[16];
        int got = BasisMediaNative.bm_session_get_audio_tracks(_handle, records, 16);
        if (got <= 0) return;

        _audioTracks.Clear();
        for (int i = 0; i < got; i++)
        {
            _audioTracks.Add(new BasisAudioTrack
            {
                Index = i,
                TrackId = (int)records[i].TrackId,
                SampleRate = (int)records[i].SampleRate,
                ChannelCount = (int)records[i].Channels,
                Language = records[i].LanguageLen > 0
                    ? Encoding.UTF8.GetString(records[i].Language, (int)records[i].LanguageLen)
                    : null,
                Label = records[i].LabelLen > 0
                    ? Encoding.UTF8.GetString(records[i].Label, (int)records[i].LabelLen)
                    : null,
            });
        }
        // A remembered index that this source cannot honour: the engine
        // already fell back to the first track, so agree with it.
        if (_audioTrackIndex >= _audioTracks.Count) _audioTrackIndex = 0;
        _audioTracksRead = true;
        AudioTrackChanged?.Invoke(_audioTrackIndex);
    }

    unsafe void DrainCaptions(long positionUs)
    {
        var cues = stackalloc BmCaption[8];
        int count = BasisMediaNative.bm_session_drain_captions(_handle, cues, 8);
        for (int i = 0; i < count; i++)
        {
            string text = cues[i].TextLen > 0
                ? Encoding.UTF8.GetString(cues[i].Text, (int)cues[i].TextLen)
                : "";
            _captionQueue.Enqueue((cues[i].PtsUs, text));
        }

        // A selected sidecar track replaces the in-band feed. The engine
        // ring is still drained above so it can't back up behind the
        // selection; the cues themselves are dropped.
        if (SelectedSubtitleTrackIndex >= 0)
        {
            _captionQueue.Clear();
            if (_subtitles.TryGetCueChange(positionUs, out BasisCaptionCue cue))
                SetCaption(cue.Text);
            return;
        }

        string due = null;
        while (_captionQueue.Count > 0 && _captionQueue.Peek().ptsUs <= positionUs)
            due = _captionQueue.Dequeue().text;
        if (due != null)
            SetCaption(due);
    }

    // The engine ring is drained every tick and everything drained is held
    // until the playback position reaches it, subscriber or not. An
    // on-demand open banks seconds of media before the first frame shows,
    // so what the engine hands over here is mostly still in the future; a
    // subscriber that attaches a frame after Open (a component whose
    // Update runs after this one's, say) must see all of it. Without a
    // subscriber a message is dropped only as it falls due.
    unsafe void DrainUserData(long positionUs)
    {
        var handler = UserDataReceived;
        _userDataBytes ??= new byte[UserDataBytesCapacity];

        var records = stackalloc BmUserData[UserDataRecordsPerDrain];
        int count;
        fixed (byte* bytes = _userDataBytes)
        {
            count = BasisMediaNative.bm_session_drain_user_data(
                _handle, records, UserDataRecordsPerDrain, bytes, UserDataBytesCapacity);
        }
        for (int i = 0; i < count; i++)
        {
            int length = (int)records[i].Len;
            long ptsUs = records[i].PtsUs;
            // A backwards jump past the engine's 1 s reordering slack is a
            // new timeline (a loop, or a discontinuity the engine did not
            // flag); what is queued from the old one would sit undue in
            // front of it. Within the slack it is B-frame reordering, and
            // the ordered insert below puts it where it belongs.
            if (_userDataPending.Count > 0
                && _userDataMaxPtsUs != long.MinValue
                && ptsUs < _userDataMaxPtsUs - 1_000_000)
                ClearUserData();
            byte[] buffer = ArrayPool<byte>.Shared.Rent(Math.Max(length, 1));
            Buffer.BlockCopy(_userDataBytes, (int)records[i].Offset, buffer, 0, length);
            // After every entry with the same or an earlier timestamp, so
            // equal timestamps keep their stream order. Walks from the
            // back: the common case lands on the end.
            int at = _userDataPending.Count;
            while (at > 0 && _userDataPending[at - 1].ptsUs > ptsUs)
                at--;
            _userDataPending.Insert(at, (ptsUs, GuidFromRfc4122(records[i].Uuid), buffer, length));
            if (ptsUs > _userDataMaxPtsUs)
                _userDataMaxPtsUs = ptsUs;
        }

        // One removal for the whole due run: a catch-up tick after the
        // open-time burst can have hundreds due, and shifting the list
        // once per entry would make that quadratic.
        int due = 0;
        while (due < _userDataPending.Count && _userDataPending[due].ptsUs <= positionUs)
            due++;
        if (due == 0)
            return;
        _userDataDue.Clear();
        for (int i = 0; i < due; i++)
            _userDataDue.Add(_userDataPending[i]);
        _userDataPending.RemoveRange(0, due);
        int timeline = _userDataTimeline;
        for (int i = 0; i < _userDataDue.Count; i++)
        {
            var (ptsUs, uuid, buffer, length) = _userDataDue[i];
            // A handler that seeks or closes has left this timeline; what
            // is still due belongs to it and is not delivered. With no
            // handler at all, due messages are simply let go.
            if (handler == null || _userDataTimeline != timeline)
            {
                ArrayPool<byte>.Shared.Return(buffer);
                continue;
            }
            try
            {
                handler?.Invoke(ptsUs, uuid, new ReadOnlySpan<byte>(buffer, 0, length));
            }
            catch (Exception e)
            {
                BasisDebug.LogErrorOnce($"[BasisMedia] user data handler failed: {e}", BasisDebug.LogTag.Video);
            }
            finally
            {
                ArrayPool<byte>.Shared.Return(buffer);
            }
        }
        _userDataDue.Clear();
    }

    /// <summary>
    /// A UUID as the wire carries it (RFC 4122, big-endian fields) as a
    /// <see cref="Guid"/> whose text form matches — so
    /// <c>Guid.Parse("b1f0a7d4-...")</c> compares equal to the UUID an
    /// encoder wrote as those bytes. <c>new Guid(byte[])</c> would not: it
    /// reads the first three fields little-endian. Exactly 16 bytes.
    /// </summary>
    public static unsafe Guid GuidFromRfc4122(ReadOnlySpan<byte> uuid)
    {
        if (uuid.Length != 16)
            throw new ArgumentException("a UUID is 16 bytes", nameof(uuid));
        fixed (byte* p = uuid)
            return GuidFromRfc4122(p);
    }

    // The pointer form the drain uses on the record's fixed buffer; the
    // caller vouches for 16 readable bytes.
    static unsafe Guid GuidFromRfc4122(byte* uuid)
    {
        int a = uuid[0] << 24 | uuid[1] << 16 | uuid[2] << 8 | uuid[3];
        short b = (short)(uuid[4] << 8 | uuid[5]);
        short c = (short)(uuid[6] << 8 | uuid[7]);
        return new Guid(a, b, c, uuid[8], uuid[9], uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15]);
    }

    // ---- IBasisPcmSource: the decoded ring, offered to the audio stack ----
    //
    // Everything above the ring - de-interleaving, per-speaker routing,
    // downmixing, device rate conversion, spatialisation - belongs to
    // BasisMediaPlayerAudio and its per-output taps. The engine's side of the
    // boundary is one interleaved stream at the stream's own rate.

    /// <summary>The stream's audio format. False until the engine has
    /// announced one, which is what keeps the sink silent rather than
    /// building outputs against a guess.</summary>
    public bool TryGetPcmFormat(out int sampleRate, out int channels)
    {
        sampleRate = System.Threading.Volatile.Read(ref _engineSampleRate);
        channels = System.Threading.Volatile.Read(ref _engineChannels);
        return sampleRate > 0 && channels > 0;
    }

    /// <summary>The shared-playback rate trim, handed to the consumer
    /// because consuming faster or slower is how the audio master moves
    /// towards the owner's position.</summary>
    public int PullRateOffsetPpm => System.Threading.Volatile.Read(ref _syncRatePpm);

    /// <summary>
    /// Audio thread. Fills <paramref name="buffer"/> with interleaved floats
    /// at the stream rate and returns how many it wrote, in whole frames. The
    /// engine zero-fills what it could not serve, so an underrun reads as
    /// silence rather than repeated samples, and it takes no media-path lock.
    /// </summary>
    public unsafe int ReadPcm(float[] buffer)
    {
        if (!_open || buffer == null || buffer.Length == 0)
            return 0;
        int channels = System.Threading.Volatile.Read(ref _engineChannels);
        if (channels <= 0)
            return 0;
        // The engine serves whole frames; asking for a partial one would
        // leave the caller's de-interleave carrying a remainder forever.
        int frames = buffer.Length / channels;
        if (frames <= 0)
            return 0;
        fixed (float* p = buffer)
        {
            int pulled = BasisMediaNative.bm_session_read_audio(_handle, p, (uint)(frames * channels));
            if (pulled <= 0)
                return 0;
            System.Threading.Interlocked.Add(ref _audioFramesPulled, pulled);
            return pulled * channels;
        }
    }

    void OnDestroy()
    {
        BasisMediaPlayerRegistry.Remove(this);
        Close();
    }
}
