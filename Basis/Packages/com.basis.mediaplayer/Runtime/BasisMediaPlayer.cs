using System;
using System.Text;
using UnityEngine;
using UnityEngine.Rendering;

namespace Basis.Media
{
    /// <summary>
    /// Playback component over the basis_media engine (ABI v2; D3D11 on
    /// Windows, Vulkan on Android).
    ///
    /// Poll-driven: one snapshot per frame, one render event per frame once
    /// the video texture exists, and the decoded audio ring offered as an
    /// <see cref="IBasisPcmSource"/>. Assigns the video texture to
    /// <see cref="targetRenderer"/>'s main texture when set.
    ///
    /// It makes no sound on its own. Audio belongs to
    /// <see cref="BasisMediaPlayerAudio"/> on the same GameObject, which
    /// broadcasts this ring to one AudioSource per speaker, so each channel can
    /// be positioned, spatialised and filtered independently. Without one, the
    /// session decodes audio that nothing consumes.
    /// </summary>
    [AddComponentMenu("Basis/Basis Media Player")]
    public class BasisMediaPlayer : MonoBehaviour, IBasisPcmSource
    {
        [Tooltip("http(s) URL or absolute file path.")]
        public string url;

        [Tooltip("A separate audio-only source played against the URL above, " +
                 "which is then treated as video-only. This is how adaptive " +
                 "ladders (YouTube and the like) serve every rung above their " +
                 "muxed fallback, so a resolver fills this in. On-demand " +
                 "http(s) and files only — live transports carry both tracks " +
                 "in one stream and refuse it.")]
        public string audioUrl;

        public bool playOnStart = true;

        [Tooltip("Buffer depth in milliseconds; 0 = Auto (self-sizing).")]
        public int bufferDepthMs;

        [Tooltip("What the source is. Stated by the caller (the resolver knows); " +
                 "the engine never infers it. Unknown behaves as VOD.")]
        public BmLiveness liveness = BmLiveness.Unknown;

        [Tooltip("Audio-leading start for live sources where the audio is the content " +
                 "(DJ/club streams): sound starts at the first banked audio instead of " +
                 "waiting for a video keyframe. The picture appears at its keyframe and " +
                 "can trail the sound by the decoder's pipeline depth. Ignored on VOD.")]
        public bool audioLeadingStart;

        [Tooltip("Shared playback, live sources: the furthest behind the live edge this " +
                 "viewer may sit, milliseconds (a ceiling on automatic buffer growth). " +
                 "Live position is never hard-synced between viewers — this bound is the " +
                 "world author's instrument. 0 = the engine default.")]
        public int maxDivergenceMs;

        [Tooltip("Permit sources on private/loopback addresses (local test rigs only).")]
        public bool allowLocalAddresses;

        [Tooltip("Optional renderer whose material main texture receives the video.")]
        public Renderer targetRenderer;

        [Header("Captions")]
        [Tooltip("Show in-band closed captions (CEA-608) when the stream carries them. " +
                 "Client-side only — does not affect playback or sync. A " +
                 "BasisMediaCaptionOverlay (or your own UI) draws the cues; this " +
                 "toggles their visibility.")]
        [SerializeField] bool captionsEnabled;

        [Tooltip("Caption text opacity (0..1). Client-side; applied by the caption overlay.")]
        [Range(0f, 1f)] [SerializeField] float captionTextOpacity = 1f;

        [Tooltip("Caption background opacity (0..1). Client-side; applied by the caption overlay.")]
        [Range(0f, 1f)] [SerializeField] float captionBackgroundOpacity = 0.5f;

        [Tooltip("Diagnostics: absolute path the engine writes its capture CSV to on " +
                 "close (100 ms stage-counter samples). Empty = off.")]
        public string diagnosticsCsvPath;

        ulong _handle;
        bool _open;
        bool _abiChecked;
        Texture _texture;
        CommandBuffer _commandBuffer;
        long _audioFramesPulled;
        int _engineChannels;
        int _engineSampleRate;
        int _syncRatePpm;
        BasisMediaPlayerAudio _audio;
#if UNITY_ANDROID && !UNITY_EDITOR
        bool _renderHooked;
        int _lastRenderEventFrame = -1;
        long _sentAudioLatencyUs = -1;
#endif
        readonly System.Collections.Generic.Queue<(long ptsUs, string text)> _captionQueue = new();
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
        public long AudioFramesPulled => System.Threading.Interlocked.Read(ref _audioFramesPulled);

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
        public bool OutputFrameIsTopLeftOrigin =>
#if UNITY_ANDROID && !UNITY_EDITOR
            false;
#else
            true;
#endif

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

        /// <summary>The in-band CEA-608 caption currently due at the playback
        /// position (empty = none). Rows are joined with '\n'.</summary>
        public string CurrentCaption { get; private set; } = "";

        /// <summary>Raised when <see cref="CurrentCaption"/> changes (an empty
        /// string is a clear). Raised regardless of
        /// <see cref="CaptionsEnabled"/>, so a display can stay primed while
        /// hidden.</summary>
        public event Action<string> CaptionChanged;

        /// <summary>Whether closed captions should be shown. A per-viewer
        /// preference: it changes nothing about playback or sync, only whether
        /// an overlay draws the cues the engine is already producing.</summary>
        public bool CaptionsEnabled
        {
            get => captionsEnabled;
            set
            {
                if (captionsEnabled == value)
                    return;
                captionsEnabled = value;
                CaptionsEnabledChanged?.Invoke(value);
            }
        }

        /// <summary>Caption text opacity, 0..1. Client-side.</summary>
        public float CaptionTextOpacity
        {
            get => captionTextOpacity;
            set
            {
                value = Mathf.Clamp01(value);
                if (captionTextOpacity == value)
                    return;
                captionTextOpacity = value;
                CaptionStyleChanged?.Invoke();
            }
        }

        /// <summary>Caption background opacity, 0..1. Client-side.</summary>
        public float CaptionBackgroundOpacity
        {
            get => captionBackgroundOpacity;
            set
            {
                value = Mathf.Clamp01(value);
                if (captionBackgroundOpacity == value)
                    return;
                captionBackgroundOpacity = value;
                CaptionStyleChanged?.Invoke();
            }
        }

        /// <summary>Raised when <see cref="CaptionsEnabled"/> is toggled.</summary>
        public event Action<bool> CaptionsEnabledChanged;

        /// <summary>Raised when caption text or background opacity changes.</summary>
        public event Action CaptionStyleChanged;

        /// <summary>Out-of-band subtitle tracks offered for this source. The
        /// media carries none of these — a resolver or other enrichment source
        /// supplies them through <see cref="SetSubtitleTracks"/>.</summary>
        public System.Collections.Generic.IReadOnlyList<BasisSubtitleTrack> SubtitleTracks => _subtitleTracks;

        /// <summary>Index into <see cref="SubtitleTracks"/>, or -1 for the
        /// default: no sidecar track, with in-band captions flowing as usual.
        /// While a sidecar track is selected, in-band cues are suppressed.
        /// Client-side only, like <see cref="CaptionsEnabled"/>.</summary>
        public int SelectedSubtitleTrackIndex { get; private set; } = -1;

        /// <summary>Selection changed, including the automatic revert to -1
        /// when a fetch fails or the session closes.</summary>
        public event Action<int> SubtitleTrackChanged;

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
            Debug.LogWarning($"[BasisMedia] subtitle track {index} failed to load; reverting to in-band captions.");
            SelectedSubtitleTrackIndex = -1;
            _subtitles.Clear();
            SubtitleTrackChanged?.Invoke(-1);
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

        void Awake()
        {
            // The sink lives beside the player, as it does in the authored
            // prefabs. It is optional: a player with none is a decoder with no
            // speakers wired to it.
            TryGetComponent(out _audio);
            BasisMediaPlayerRegistry.Add(this);
        }

        void Start()
        {
            // Through the router, so an authored page URL resolves rather
            // than failing to open.
            if (playOnStart && !string.IsNullOrEmpty(url))
                OpenUserUrl(url);
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
            // All of this lands after Open, which clears what the previous
            // source left behind.
            Open(media.Url, media.AudioUrl);
            Media = media;
            SetSubtitleTracks(media.SubtitleTracks);
            MediaChanged?.Invoke(media);
        }

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
            Debug.LogError($"[BasisMedia] load failed: {error?.GetType().Name ?? "unknown"}");
            State = BmState.Error;
        }

        /// <summary>Open (or re-open) a source. The engine opens asynchronously;
        /// watch <see cref="State"/>. Uses whatever <see cref="audioUrl"/>
        /// currently holds, so an authored pair opens as a pair.</summary>
        public void Open(string sourceUrl)
        {
            LoadGeneration++;
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
                    Debug.LogError($"[BasisMedia] basis_media ABI v{abi}, this package needs v{BasisMediaNative.AbiVersion}; refusing.");
                    State = BmState.Error;
                    return;
                }
                _abiChecked = true;
            }
#if UNITY_ANDROID && !UNITY_EDITOR
            if (SystemInfo.graphicsDeviceType != GraphicsDeviceType.Vulkan)
            {
                Debug.LogError($"[BasisMedia] needs Vulkan on Android, running on {SystemInfo.graphicsDeviceType}");
                State = BmState.Error;
                return;
            }
#else
            if (SystemInfo.graphicsDeviceType != GraphicsDeviceType.Direct3D11)
            {
                Debug.LogError($"[BasisMedia] needs D3D11, running on {SystemInfo.graphicsDeviceType}");
                State = BmState.Error;
                return;
            }
#endif

            url = sourceUrl;
            byte[] descriptor = Encoding.UTF8.GetBytes(BuildDescriptor(sourceUrl));
            int rc = BasisMediaNative.bm_session_open(descriptor, (UIntPtr)descriptor.Length, out _handle);
            if (rc != 0)
            {
                Debug.LogError($"[BasisMedia] bm_session_open failed: {rc}");
                State = BmState.Error;
                return;
            }
            _open = true;
            State = BmState.Opening;
            System.Threading.Volatile.Write(ref _engineChannels, 0);
            System.Threading.Volatile.Write(ref _engineSampleRate, 0);
            System.Threading.Volatile.Write(ref _syncRatePpm, 0);
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
            if (bufferDepthMs > 0)
                json.Append($",\"buffer_depth_ms\":{bufferDepthMs}");
            if (liveness == BmLiveness.Live)
                json.Append(",\"liveness\":\"live\"");
            else if (liveness == BmLiveness.Vod)
                json.Append(",\"liveness\":\"vod\"");
            if (audioLeadingStart)
                json.Append(",\"audio_leading\":true");
            if (maxDivergenceMs > 0)
                json.Append($",\"max_divergence_ms\":{maxDivergenceMs}");
            if (DecodePreference == BmDecodePreference.HardwareOnly)
                json.Append(",\"decode_preference\":\"hardware_only\"");
            else if (DecodePreference == BmDecodePreference.SoftwareOnly)
                json.Append(",\"decode_preference\":\"software_only\"");
            if (!string.IsNullOrEmpty(diagnosticsCsvPath))
            {
                json.Append(",\"diag_csv\":");
                AppendJsonString(json, diagnosticsCsvPath);
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

        public void Seek(double seconds)
        {
            if (!_open)
                return;
            BasisMediaNative.bm_session_seek(_handle, (long)(seconds * 1_000_000.0));
            // Audio still in the sink's window belongs to the timeline being
            // left behind; playing it out would be heard against the landed
            // picture.
            _audio?.ResetSyncAnchor();
            // Queued in-band cues are stamped against the old timeline, so
            // they would either flush in a burst or sit undue past a backwards
            // seek. The engine emits its own clear at the landed position.
            ClearCaptionDisplay();
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
#if UNITY_ANDROID && !UNITY_EDITOR
            if (_renderHooked)
            {
                RenderPipelineManager.endCameraRendering -= OnEndCameraRendering;
                _renderHooked = false;
            }
            if (_texture is RenderTexture renderTexture)
                renderTexture.Release();
#endif
            bool hadTexture = _texture != null;
            _texture = null;
            VideoSize = Vector2Int.zero;
            _subtitles.Clear();
            if (SelectedSubtitleTrackIndex != -1)
            {
                SelectedSubtitleTrackIndex = -1;
                SubtitleTrackChanged?.Invoke(-1);
            }
            ClearCaptionDisplay();
            if (hadTexture)
                OutputTextureChanged?.Invoke(null);
        }

        void Update()
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

            if (State == BmState.Ended && previousState != BmState.Ended)
                Ended?.Invoke();

            if (State == BmState.Error)
            {
                Debug.LogError($"[BasisMedia] session error {snapshot.ErrorCode} (category {snapshot.ErrorCategory})");
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
                _texture = target;
                BasisMediaNative.bm_session_set_output_texture(_handle, target.GetNativeTexturePtr());
                if (targetRenderer != null)
                {
                    var material = targetRenderer.material;
                    material.mainTexture = _texture;
                    material.mainTextureScale = Vector2.one;
                    material.mainTextureOffset = Vector2.zero;
                }
#else
                var texture = new Texture2D((int)snapshot.Width, (int)snapshot.Height, TextureFormat.BGRA32, false);
                _texture = texture;
                BasisMediaNative.bm_session_set_output_texture(_handle, texture.GetNativeTexturePtr());
                if (targetRenderer != null)
                {
                    var material = targetRenderer.material;
                    material.mainTexture = _texture;
                    // D3D11 row 0 is the top of the image; quad UVs put v=0
                    // at the bottom.
                    material.mainTextureScale = new Vector2(1f, -1f);
                    material.mainTextureOffset = new Vector2(0f, 1f);
                }
#endif
                OutputTextureChanged?.Invoke(_texture);
            }

            if (_texture != null)
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
                        _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(), 1, (IntPtr)_handle);
                        RenderPipelineManager.endCameraRendering += OnEndCameraRendering;
                        _renderHooked = true;
                    }
                }
                else
                {
                    _commandBuffer.Clear();
                    _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(), 1, (IntPtr)_handle);
                    Graphics.ExecuteCommandBuffer(_commandBuffer);
                }
#else
                _commandBuffer.Clear();
                _commandBuffer.IssuePluginEventAndData(BasisMediaNative.bm_render_event_func(), 1, (IntPtr)_handle);
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

        unsafe void DrainEvents()
        {
            var events = stackalloc BmEvent[8];
            int count = BasisMediaNative.bm_session_drain_events(_handle, events, 8);
            for (int i = 0; i < count; i++)
            {
                string detail = Encoding.UTF8.GetString(events[i].Detail, (int)events[i].DetailLen);
                Debug.Log($"[BasisMedia] event {events[i].Code} stage {events[i].Stage}: {detail}");
            }
        }

        // Cues arrive ahead of presentation stamped with their due PTS; hold
        // them until the playback position reaches each one, so captions stay
        // in lockstep with the frame they belong to regardless of the decode
        // buffer's lead.
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
}
