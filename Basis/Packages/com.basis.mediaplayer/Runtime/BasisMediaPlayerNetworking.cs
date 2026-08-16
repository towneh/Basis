using System;
using System.Text;
using System.Threading.Tasks;
using Basis;
using Basis.Network.Core;
using Basis.Scripts.Networking;
using Basis.Scripts.Networking.NetworkedAvatar;
using UnityEngine;

/// <summary>
/// Shared playback for a <see cref="BasisMediaPlayer"/>: one owner drives the
/// URL, the transport commands and the playhead, and every other client
/// follows.
///
/// Convergence is the engine's job, not this component's. The owner broadcasts
/// its position on a heartbeat and receivers hand it to
/// <see cref="BasisMediaPlayer.SetSyncTarget"/>, which corrects through a dead
/// band, then a bounded rate slew, then a seek as the last resort, and
/// extrapolates the target at 1x between beats. Live sources take no target at
/// all — they have no shared timeline to land on, and divergence is bounded by
/// the player's own maxDivergenceMs instead.
/// </summary>
[DisallowMultipleComponent]
[RequireComponent(typeof(BasisMediaPlayer))]
public sealed class BasisMediaPlayerNetworking : BasisNetworkBehaviour, IBasisMediaTickConsumer
{
    public enum SyncedPlaybackState : byte
    {
        Stopped = 0,
        Playing = 1,
        Paused = 2,
    }

    private enum MessageId : byte
    {
        FullState = 1,
        Play = 2,
        Pause = 3,
        Stop = 4,
        Seek = 5,
        RequestState = 6,
        Settings = 7,
        Position = 8,
    }

    [Flags]
    private enum SettingsFlags : byte
    {
        None = 0,
        AdminOnly = 1 << 0,
        AllowAnyoneToTakeControl = 1 << 1,
        AnyoneCanControl = 1 << 2,
    }

    // Matches the panel's Perm_Control constant; admins (perm "*") also satisfy it.
    public const string PermControl = "basis.mediaplayer.control";
    public const string PermAdmin = "*";

    [Header("Permissions")]
    [Tooltip("If true, only clients with the basis.mediaplayer.control or * permission may take ownership and control playback. Overrides AllowAnyoneToTakeControl.")]
    public bool AdminOnly = false;

    [Tooltip("If true, any client may take ownership and control playback. If false, only the current owner can call SetUrl/Play/Stop/Pause/Resume/Seek. Ignored when AdminOnly is true.")]
    public bool AllowAnyoneToTakeControl = true;

    [Tooltip("If true, clients WITHOUT the basis.mediaplayer.control (or *) permission may also load URLs and drive playback on this player, and the menu shows them the playback controls. Ignored when AdminOnly is true.")]
    public bool AnyoneCanControl = false;

    [Header("Sync")]
    [Tooltip("While playing seekable media, the owner broadcasts its position every this many seconds and receivers feed it to the engine's sync ladder. 0 disables the heartbeat, which leaves receivers free-running between transport commands.")]
    [Min(0f)] public float PositionHeartbeatSeconds = 3f;

    [Tooltip("Verbose log lines for join/leave sync, sync targets, rejected control attempts.")]
    public bool VerboseLogging = false;

    private static readonly Encoding UrlEncoding = new UTF8Encoding(false, false);
    // FullState payload after the 1-byte MessageId: [state:1][positionTicks:8][loadNonce:2][settingsFlags:1][urlLen:2] then url bytes.
    // positionTicks is 0 when the source is live (no seekable timeline); receivers treat 0 as "no position".
    // loadNonce bumps per SetUrl so re-loading the same URL is applied as a fresh load, not a no-op.
    private const int SettingsBlockSize = 1;
    private const int FullStateNonceOffset = 1 + 1 + 8;
    private const int FullStateSettingsOffset = FullStateNonceOffset + 2;
    private const int FullStateUrlLenOffset = FullStateSettingsOffset + SettingsBlockSize;
    private const int FullStateHeaderSize = FullStateUrlLenOffset + 2;
    private const int SettingsPayloadSize = 1 + SettingsBlockSize;
    private const int SeekPayloadSize = 1 + 8;
    // Position carries the owner's wall clock alongside the playhead, so a receiver
    // can tell a stalled owner (wall advancing, playhead frozen) from a paused one
    // and stop feeding the ladder a target it would drag itself backwards towards.
    private const int PositionPayloadSize = 1 + 8 + 8;

    // Cached single-byte command payloads; SendCustomNetworkEvent does not retain references.
    private static readonly byte[] PlayBytes = { (byte)MessageId.Play };
    private static readonly byte[] PauseBytes = { (byte)MessageId.Pause };
    private static readonly byte[] StopBytes = { (byte)MessageId.Stop };
    private static readonly byte[] RequestStateBytes = { (byte)MessageId.RequestState };

    private BasisMediaPlayer mediaPlayer;
    private string currentSyncedUrl = string.Empty;

    /// <summary>The URL shared with peers for the current source — the input/page URL, not the per-client resolved stream.</summary>
    public string SyncedUrl => currentSyncedUrl;
    private bool sendOnNetworkReady;
    private bool sendOnNetworkReadyFreshLoad;
    private bool applyingRemoteCommand;
    private bool eventsHooked;
    private ushort loadNonce;
    private ushort lastAppliedLoadNonce;
    private bool syncedUrlFromSetUrl;
    private float heartbeatTimer;

    // Local playback state, sampled each frame: this player reports a state enum
    // rather than raising started/paused events, so transitions are detected here
    // and broadcast from the same place the C component's event handlers did.
    private BmState lastObservedState = BmState.Idle;
    private int lastObservedLoadGeneration;
    private bool announcedThisLoad;

    // Owner state stashed while a remote URL loads locally (resolution and the
    // engine's own open are both asynchronous): applied once the session is
    // running, so a late joiner lands at the owner's position instead of at zero.
    private bool pendingRemoteApply;
    private SyncedPlaybackState pendingRemoteState;
    private long pendingRemotePositionTicks;
    private float pendingRemoteStashedAt;

    // Last heartbeat seen from the owner, to spot a stalled playhead.
    private long lastOwnerPositionTicks = -1;
    private long lastOwnerWallTicks = -1;
    private bool syncTargetActive;

    // Main-thread scratch — Unity callbacks are serial so these don't need locking.
    private readonly ushort[] singleRecipient = new ushort[1];
    private readonly byte[] seekScratch = new byte[SeekPayloadSize];
    private readonly byte[] positionScratch = new byte[PositionPayloadSize];
    private readonly byte[] settingsScratch = new byte[SettingsPayloadSize];
    private byte[] fullStateScratch = Array.Empty<byte>();
    private byte[] cachedUrlBytes = Array.Empty<byte>();
    private string cachedUrlBytesSource;

    public BasisMediaPlayer MediaPlayer => mediaPlayer;

    public bool CanLocallyControl
    {
        get
        {
            if (!HasNetworkID)
            {
                return true;
            }

            if (IsOwnedLocallyOnClient)
            {
                return true;
            }

            if (IsLocalAdmin())
            {
                return true;
            }

            if (AdminOnly)
            {
                return false;
            }

            return AllowAnyoneToTakeControl || AnyoneCanControl;
        }
    }

    /// <summary>True when this player's controls are open to clients that hold no control permission.</summary>
    public bool ControlOpenToEveryone => AnyoneCanControl && !AdminOnly;

    public static bool IsLocalAdmin()
    {
        var perms = BasisNetworkManagement.LocalPermissions;
        return perms != null && (perms.Contains(PermAdmin) || perms.Contains(PermControl));
    }

    public void Awake()
    {
        TryGetComponent(out mediaPlayer);
    }

    private void OnEnable()
    {
        if (mediaPlayer == null)
        {
            TryGetComponent(out mediaPlayer);
        }

        HookPlayerEvents();
        if (mediaPlayer != null)
        {
            mediaPlayer.AddTickConsumer(this);
        }
    }

    private void OnDisable()
    {
        if (mediaPlayer != null)
        {
            mediaPlayer.RemoveTickConsumer(this);
        }

        UnhookPlayerEvents();
        ClearSyncTarget();
    }

    BasisMediaTickStage IBasisMediaTickConsumer.TickStage => BasisMediaTickStage.Networking;

    // Runs from the player's tick, after the poll: what is observed and
    // broadcast below is this frame's state and position.
    void IBasisMediaTickConsumer.MediaTick()
    {
        if (mediaPlayer == null)
        {
            return;
        }

        ObserveLocalPlayback();
        ApplyPendingRemoteStateWhenReady();
        BroadcastHeartbeat();
    }

    // Owner position heartbeat: a small latest-wins ping (Sequenced, like the
    // framework's other position streams) so receivers keep a fresh target for
    // the engine ladder. Only while playing seekable media — live sources have
    // no timeline to correct against.
    private void BroadcastHeartbeat()
    {
        if (PositionHeartbeatSeconds <= 0f) return;
        if (!HasNetworkID || !IsOwnedLocallyOnClient) return;
        if (GetLocalState() != SyncedPlaybackState.Playing) return;
        if (mediaPlayer.DurationSeconds <= 0d) return;
        heartbeatTimer += Time.deltaTime;
        if (heartbeatTimer < PositionHeartbeatSeconds) return;
        heartbeatTimer = 0f;
        positionScratch[0] = (byte)MessageId.Position;
        WriteLong(positionScratch, 1, PositionTicks());
        WriteLong(positionScratch, 9, DateTime.UtcNow.Ticks);
        SendCustomNetworkEvent(positionScratch, DeliveryMethod.Sequenced);
    }

    public override void OnNetworkReady()
    {
        if (sendOnNetworkReady)
        {
            sendOnNetworkReady = false;
            bool freshLoad = sendOnNetworkReadyFreshLoad;
            sendOnNetworkReadyFreshLoad = false;
            BroadcastFullState(freshLoad);
        }
    }

    public override void OnPlayerJoined(BasisNetworkPlayer player)
    {
        if (player == null)
        {
            return;
        }

        if (!IsOwnedLocallyOnClient)
        {
            return;
        }

        var local = BasisNetworkPlayer.LocalPlayer;
        if (local != null && player.playerId == local.playerId)
        {
            return;
        }

        singleRecipient[0] = player.playerId;
        SendFullStateTo(singleRecipient);
        if (VerboseLogging)
        {
            BasisDebug.Log($"{nameof(BasisMediaPlayerNetworking)} sent late-join state to player {player.playerId}.", BasisDebug.LogTag.Video);
        }
    }

    public override void OnOwnershipTransfer(BasisNetworkPlayer newOwner)
    {
        if (IsOwnedLocallyOnClient)
        {
            // We drive from here, so stop chasing the position we were handed.
            ClearSyncTarget();
            BroadcastFullState();
            return;
        }

        if (!HasNetworkID)
        {
            return;
        }

        ushort owner = CurrentOwnerId;
        if (owner == 0)
        {
            return;
        }

        var local = BasisNetworkPlayer.LocalPlayer;
        if (local != null && owner == local.playerId)
        {
            return;
        }

        singleRecipient[0] = owner;
        SendCustomNetworkEvent(RequestStateBytes, DeliveryMethod.ReliableOrdered, singleRecipient);
        if (VerboseLogging)
        {
            BasisDebug.Log($"{nameof(BasisMediaPlayerNetworking)} requested state from new owner {owner}.", BasisDebug.LogTag.Video);
        }
    }

    /// <summary>Nobody owns this player any more. Keep playing free-running rather
    /// than freezing on the departed owner's last target.</summary>
    public override void OnServerOwnershipDestroyed()
    {
        ClearSyncTarget();
    }

    public async Task SetUrl(string url)
    {
        if (string.IsNullOrEmpty(url))
        {
            return;
        }

        if (!await AcquireControlAsync())
        {
            return;
        }

        currentSyncedUrl = url;
        loadNonce++;
        syncedUrlFromSetUrl = true;

        // FullState is the only message carrying a URL, so it goes out up front rather than
        // waiting for the session to come up — peers that never see a broadcast never learn
        // what to load. It also hides resolution latency: a page URL costs each client
        // seconds of yt-dlp work, and announcing immediately lets peers resolve in parallel
        // with us. Opening a session starts it playing, the later ready broadcast settles
        // state, and the position heartbeat keeps everyone converged.
        BroadcastFullState(freshLoad: true);

        ClearSyncTarget();
        mediaPlayer.OpenUserUrl(url);
    }

    public async Task Play()
    {
        if (!await AcquireControlAsync())
        {
            return;
        }

        StartOrResumeLocal();
    }

    public async Task Stop()
    {
        if (!await AcquireControlAsync())
        {
            return;
        }

        mediaPlayer.Close();
        // Closing the session raises no event, so we broadcast directly.
        SendOwnerSimple(MessageId.Stop);
    }

    public async Task Pause()
    {
        if (!await AcquireControlAsync())
        {
            return;
        }

        mediaPlayer.Pause();
    }

    public async Task Resume()
    {
        if (!await AcquireControlAsync())
        {
            return;
        }

        StartOrResumeLocal();
    }

    public async Task Seek(TimeSpan position)
    {
        if (!await AcquireControlAsync())
        {
            return;
        }

        mediaPlayer.Seek(position.TotalSeconds);
    }

    public async Task SetAdminOnly(bool value)
    {
        if (AdminOnly == value)
        {
            return;
        }

        if (!await AcquireControlAsync())
        {
            return;
        }

        AdminOnly = value;
        BroadcastSettings();
    }

    public async Task SetAllowAnyoneToTakeControl(bool value)
    {
        if (AllowAnyoneToTakeControl == value)
        {
            return;
        }

        if (!await AcquireControlAsync())
        {
            return;
        }

        AllowAnyoneToTakeControl = value;
        BroadcastSettings();
    }

    public async Task SetAnyoneCanControl(bool value)
    {
        if (AnyoneCanControl == value)
        {
            return;
        }

        if (!await AcquireControlAsync())
        {
            return;
        }

        AnyoneCanControl = value;
        BroadcastSettings();
    }

    private async Task<bool> AcquireControlAsync()
    {
        if (!HasNetworkID)
        {
            return true;
        }

        if (IsOwnedLocallyOnClient)
        {
            return true;
        }

        if (!IsLocalAdmin())
        {
            if (AdminOnly)
            {
                if (VerboseLogging)
                {
                    BasisDebug.LogWarning($"{nameof(BasisMediaPlayerNetworking)} control rejected: AdminOnly is on and this client lacks the {PermControl} (or {PermAdmin}) permission.", BasisDebug.LogTag.Video);
                }

                return false;
            }

            if (!AllowAnyoneToTakeControl && !AnyoneCanControl)
            {
                if (VerboseLogging)
                {
                    BasisDebug.LogWarning($"{nameof(BasisMediaPlayerNetworking)} control rejected: AllowAnyoneToTakeControl and AnyoneCanControl are both false and this client is not the owner.", BasisDebug.LogTag.Video);
                }

                return false;
            }
        }

        var result = await TakeOwnershipAsync();
        if (!result.Success && VerboseLogging)
        {
            BasisDebug.LogWarning($"{nameof(BasisMediaPlayerNetworking)} ownership request was denied by the server.", BasisDebug.LogTag.Video);
        }

        if (result.Success)
        {
            ClearSyncTarget();
        }

        return result.Success;
    }

    public override void OnNetworkMessage(ushort senderId, byte[] buffer, DeliveryMethod deliveryMethod)
    {
        if (buffer == null || buffer.Length < 1)
        {
            return;
        }

        var id = (MessageId)buffer[0];

        switch (id)
        {
            case MessageId.RequestState:
                if (IsOwnedLocallyOnClient)
                {
                    singleRecipient[0] = senderId;
                    SendFullStateTo(singleRecipient);
                }

                return;

            case MessageId.FullState:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                if (!TryDeserializeFullState(buffer, out string url, out var state, out long fullPos, out ushort fullNonce))
                {
                    return;
                }

                ApplyRemoteFullState(url, state, fullPos, fullNonce);
                return;

            case MessageId.Play:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                ApplyRemotePlay();
                return;

            case MessageId.Pause:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                ApplyRemotePause();
                return;

            case MessageId.Stop:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                ApplyRemoteStop();
                return;

            case MessageId.Seek:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                if (buffer.Length < SeekPayloadSize)
                {
                    return;
                }

                ApplyRemoteSeek(ReadLong(buffer, 1));
                return;

            case MessageId.Settings:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                if (buffer.Length < SettingsPayloadSize)
                {
                    return;
                }

                ApplyRemoteSettings(buffer, 1);
                return;

            case MessageId.Position:
                if (IsOwnedLocallyOnClient)
                {
                    return;
                }

                if (buffer.Length < PositionPayloadSize)
                {
                    return;
                }

                // Drift-only: state changes ride FullState and the transport
                // commands; the heartbeat never starts or pauses playback.
                if (GetLocalState() != SyncedPlaybackState.Playing)
                {
                    return;
                }

                ApplyOwnerPosition(ReadLong(buffer, 1), ReadLong(buffer, 9));
                return;
        }
    }

    private void ApplyRemotePlay()
    {
        applyingRemoteCommand = true;
        try
        {
            StartOrResumeLocal();
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    private void ApplyRemotePause()
    {
        applyingRemoteCommand = true;
        try
        {
            mediaPlayer.Pause();
            ClearSyncTarget();
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    private void ApplyRemoteStop()
    {
        applyingRemoteCommand = true;
        try
        {
            ClearSyncTarget();
            pendingRemoteApply = false;
            mediaPlayer.Close();
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    private void ApplyRemoteSeek(long ticks)
    {
        if (ticks < 0)
        {
            return;
        }

        applyingRemoteCommand = true;
        try
        {
            mediaPlayer.Seek(TimeSpan.FromTicks(ticks).TotalSeconds);
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    private void ApplyRemoteFullState(string url, SyncedPlaybackState state, long positionTicks, ushort remoteLoadNonce)
    {
        // Reload when the URL changes OR the owner issued a fresh load of the same URL
        // (loadNonce bumps per SetUrl). Without the nonce, re-loading the same URL on the
        // owner would be a no-op here and the two clients would drift apart.
        bool loadChanged = !string.IsNullOrEmpty(url) &&
            (url != currentSyncedUrl || remoteLoadNonce != lastAppliedLoadNonce);
        applyingRemoteCommand = true;
        pendingRemoteApply = false; /* superseded by whatever this state says */
        try
        {
            if (loadChanged)
            {
                currentSyncedUrl = url;
                lastAppliedLoadNonce = remoteLoadNonce;
                ClearSyncTarget();

                if (state == SyncedPlaybackState.Stopped)
                {
                    mediaPlayer.Close();
                    return;
                }

                // A page URL (YouTube/Twitch/…) is resolved per-client: resolved CDN URLs
                // are per-client and expiring, so they can't be shared. Route it through
                // the router so this client resolves the page URL itself. Both that and
                // the engine's own open are asynchronous, and the session starts playing
                // as soon as it is up, so the owner's position/pause snapshot is stashed
                // and applied once playback is actually running (aged by the elapsed
                // time), after which the heartbeat refines it.
                pendingRemoteState = state;
                pendingRemotePositionTicks = positionTicks;
                pendingRemoteStashedAt = Time.realtimeSinceStartup;
                pendingRemoteApply = true;
                mediaPlayer.OpenUserUrl(url);
                return;
            }

            switch (state)
            {
                case SyncedPlaybackState.Stopped:
                    ClearSyncTarget();
                    mediaPlayer.Close();
                    break;

                case SyncedPlaybackState.Playing:
                    StartOrResumeLocal();
                    ApplyOwnerPosition(positionTicks, 0);
                    break;

                case SyncedPlaybackState.Paused:
                    mediaPlayer.Pause();
                    ClearSyncTarget();
                    if (positionTicks > 0)
                    {
                        mediaPlayer.Seek(TimeSpan.FromTicks(positionTicks).TotalSeconds);
                    }

                    break;
            }
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    // The stashed owner state lands once the local session is actually running.
    private void ApplyPendingRemoteStateWhenReady()
    {
        if (!pendingRemoteApply || IsOwnedLocallyOnClient)
        {
            return;
        }

        BmState state = mediaPlayer.State;
        if (state == BmState.Idle || state == BmState.Opening || state == BmState.Buffering)
        {
            return;
        }

        pendingRemoteApply = false;
        if (state == BmState.Error)
        {
            return;
        }

        applyingRemoteCommand = true;
        try
        {
            if (pendingRemotePositionTicks > 0 && mediaPlayer.DurationSeconds > 0d)
            {
                // The owner's snapshot aged while this client resolved and opened;
                // advance it by the elapsed time when the owner was playing. The
                // heartbeat corrects the residual.
                long ticks = pendingRemotePositionTicks;
                if (pendingRemoteState == SyncedPlaybackState.Playing)
                {
                    ticks += (long)((Time.realtimeSinceStartup - pendingRemoteStashedAt) * TimeSpan.TicksPerSecond);
                }

                mediaPlayer.Seek(TimeSpan.FromTicks(ticks).TotalSeconds);
            }

            // Opening a session starts it playing, so only the paused case needs acting on.
            if (pendingRemoteState == SyncedPlaybackState.Paused)
            {
                mediaPlayer.Pause();
            }
        }
        finally
        {
            applyingRemoteCommand = false;
        }
    }

    /// <summary>
    /// Hand the owner's playhead to the engine's sync ladder. Nothing is corrected
    /// here: the engine converges through a dead band, a bounded rate slew and
    /// finally a seek, and extrapolates the target at 1x between beats.
    /// </summary>
    private void ApplyOwnerPosition(long positionTicks, long ownerWallTicks)
    {
        if (positionTicks <= 0 || mediaPlayer.DurationSeconds <= 0d)
        {
            return;
        }

        // An owner whose wall clock is advancing while its playhead is not has
        // stalled (buffering, or wedged). Chasing a frozen target would drag this
        // client backwards, so hold position until it moves again.
        if (ownerWallTicks > 0 && lastOwnerWallTicks > 0 &&
            ownerWallTicks > lastOwnerWallTicks && positionTicks == lastOwnerPositionTicks)
        {
            lastOwnerWallTicks = ownerWallTicks;
            ClearSyncTarget();
            return;
        }

        lastOwnerPositionTicks = positionTicks;
        if (ownerWallTicks > 0)
        {
            lastOwnerWallTicks = ownerWallTicks;
        }

        double seconds = TimeSpan.FromTicks(positionTicks).TotalSeconds;
        mediaPlayer.SetSyncTarget(seconds);
        syncTargetActive = true;
        if (VerboseLogging)
        {
            BasisDebug.Log($"{nameof(BasisMediaPlayerNetworking)} sync target {seconds:F2}s (local {mediaPlayer.PositionSeconds:F2}s).", BasisDebug.LogTag.Video);
        }
    }

    private void ClearSyncTarget()
    {
        lastOwnerPositionTicks = -1;
        lastOwnerWallTicks = -1;
        if (!syncTargetActive || mediaPlayer == null)
        {
            return;
        }

        syncTargetActive = false;
        mediaPlayer.ClearSyncTarget();
    }

    /// <summary>Start playing, whichever state the session is in: resume a paused
    /// one, and re-open the synced URL when there is no session left to resume.</summary>
    private void StartOrResumeLocal()
    {
        switch (mediaPlayer.State)
        {
            case BmState.Opening:
            case BmState.Buffering:
            case BmState.Playing:
                return;
            case BmState.Paused:
                mediaPlayer.Play();
                return;
            default:
                string url = GetActiveUrl();
                if (!string.IsNullOrEmpty(url))
                {
                    mediaPlayer.OpenUserUrl(url);
                }

                return;
        }
    }

    private void HookPlayerEvents()
    {
        if (eventsHooked || mediaPlayer == null)
        {
            return;
        }

        mediaPlayer.Seeked += HandleLocalSeeked;
        lastObservedState = mediaPlayer.State;
        lastObservedLoadGeneration = mediaPlayer.LoadGeneration;
        // Ended is deliberately not acted on: end-of-stream is per-client. Every peer
        // plays the same source and reaches its own end; broadcasting a stop on the
        // owner's end would cut off any client still behind its playhead (a late
        // joiner, by its join latency). Deliberate stops broadcast from Stop() directly.
        eventsHooked = true;
    }

    private void UnhookPlayerEvents()
    {
        if (!eventsHooked || mediaPlayer == null)
        {
            return;
        }

        mediaPlayer.Seeked -= HandleLocalSeeked;
        eventsHooked = false;
    }

    /// <summary>
    /// Watches the player's own state for the transitions the C component took
    /// from started/paused/ready events, and broadcasts them when we are the owner.
    /// </summary>
    private void ObserveLocalPlayback()
    {
        BmState state = mediaPlayer.State;
        int generation = mediaPlayer.LoadGeneration;
        BmState previous = lastObservedState;
        lastObservedState = state;

        if (generation != lastObservedLoadGeneration)
        {
            lastObservedLoadGeneration = generation;
            announcedThisLoad = false;
        }

        if (applyingRemoteCommand || !IsOwnedLocallyOnClient)
        {
            return;
        }

        // First frame this load actually reached playback: settle peers on the real
        // URL, state and position, the way the C component's ready handler did.
        if (!announcedThisLoad && (state == BmState.Playing || state == BmState.Paused))
        {
            announcedThisLoad = true;
            AdoptActiveUrlIfUnset();
            // The queued load has arrived, so a fresh-load broadcast still waiting on a
            // network ID is superseded: from here the player's own state and position
            // are the truth.
            sendOnNetworkReadyFreshLoad = false;
            BroadcastFullState();
            return;
        }

        if (state == previous)
        {
            return;
        }

        if (state == BmState.Paused)
        {
            SendOwnerSimple(MessageId.Pause);
        }
        else if (state == BmState.Playing && previous == BmState.Paused)
        {
            SendOwnerSimple(MessageId.Play);
        }
    }

    private void AdoptActiveUrlIfUnset()
    {
        // currentSyncedUrl is the URL we share. When SetUrl drove this load it's the
        // input/page URL peers must resolve themselves — keep it (overwriting with the
        // resolved CDN URL would broadcast a per-client/expiring URL that works for no
        // one else). When the load bypassed SetUrl (a world script opening the player
        // directly), adopt what it opened so we don't keep broadcasting a stale URL.
        if (!syncedUrlFromSetUrl)
        {
            currentSyncedUrl = ResolveShareableUrl();
        }

        syncedUrlFromSetUrl = false;
    }

    private void HandleLocalSeeked(double seconds)
    {
        if (applyingRemoteCommand || !IsOwnedLocallyOnClient || !HasNetworkID)
        {
            return;
        }

        seekScratch[0] = (byte)MessageId.Seek;
        WriteLong(seekScratch, 1, TimeSpan.FromSeconds(seconds).Ticks);
        SendCustomNetworkEvent(seekScratch, DeliveryMethod.ReliableOrdered);
    }

    private void SendOwnerSimple(MessageId id)
    {
        if (!HasNetworkID)
        {
            sendOnNetworkReady = true;
            return;
        }

        byte[] payload = id switch
        {
            MessageId.Play => PlayBytes,
            MessageId.Pause => PauseBytes,
            MessageId.Stop => StopBytes,
            MessageId.RequestState => RequestStateBytes,
            _ => new byte[] { (byte)id },
        };
        SendCustomNetworkEvent(payload, DeliveryMethod.ReliableOrdered);
    }

    private void BroadcastFullState(bool freshLoad = false)
    {
        if (!HasNetworkID)
        {
            sendOnNetworkReady = true;
            // A queued fresh load outranks a queued ordinary broadcast: the deferred send
            // still has to describe the pending load, not the source being replaced.
            sendOnNetworkReadyFreshLoad |= freshLoad;
            return;
        }

        SendCustomNetworkEvent(SerializeFullState(freshLoad), DeliveryMethod.ReliableOrdered);
    }

    private void SendFullStateTo(ushort[] recipients)
    {
        if (!HasNetworkID)
        {
            return;
        }

        SendCustomNetworkEvent(SerializeFullState(), DeliveryMethod.ReliableOrdered, recipients);
    }

    private SyncedPlaybackState GetLocalState()
    {
        switch (mediaPlayer.State)
        {
            case BmState.Paused:
                return SyncedPlaybackState.Paused;
            case BmState.Opening:
            case BmState.Buffering:
            case BmState.Playing:
                return SyncedPlaybackState.Playing;
            default:
                return SyncedPlaybackState.Stopped;
        }
    }

    private long PositionTicks() =>
        mediaPlayer.DurationSeconds > 0d
            ? TimeSpan.FromSeconds(mediaPlayer.PositionSeconds).Ticks
            : 0L;

    /// <summary>The URL peers can act on: what the world or the menu asked for,
    /// never the per-client, expiring stream a resolver produced.</summary>
    private string ResolveShareableUrl()
    {
        BasisResolvedMedia media = mediaPlayer.Media;
        if (media == null)
        {
            // Nothing resolved this load, so what the player was opened with is
            // the URL itself.
            return mediaPlayer.url ?? string.Empty;
        }

        // A resolver handled it. Its page URL is the only shareable one; the
        // player's own URL is now the stream that was extracted, which is issued
        // per client and expires. Sharing nothing beats sharing that.
        return media.SourceUrl ?? string.Empty;
    }

    private string GetActiveUrl() =>
        !string.IsNullOrEmpty(currentSyncedUrl) ? currentSyncedUrl : ResolveShareableUrl();

    // freshLoad describes the load we are about to start rather than the source still
    // loaded: the player has not swapped over yet, so its state and position still belong
    // to the outgoing media and would otherwise be applied as the new source's start
    // position on peers.
    private byte[] SerializeFullState(bool freshLoad = false)
    {
        string url = GetActiveUrl();
        bool urlChanged = !string.Equals(cachedUrlBytesSource, url, StringComparison.Ordinal);
        if (urlChanged)
        {
            cachedUrlBytes = string.IsNullOrEmpty(url) ? Array.Empty<byte>() : UrlEncoding.GetBytes(url);
            if (cachedUrlBytes.Length > ushort.MaxValue)
            {
                BasisDebug.LogError($"{nameof(BasisMediaPlayerNetworking)} URL exceeds {ushort.MaxValue} bytes; truncating.", BasisDebug.LogTag.Video);
                Array.Resize(ref cachedUrlBytes, ushort.MaxValue);
            }

            cachedUrlBytesSource = url;
        }

        byte[] urlBytes = cachedUrlBytes;
        int totalSize = FullStateHeaderSize + urlBytes.Length;
        bool sizeChanged = fullStateScratch.Length != totalSize;
        if (sizeChanged)
        {
            fullStateScratch = new byte[totalSize];
            fullStateScratch[0] = (byte)MessageId.FullState;
            WriteUShort(fullStateScratch, FullStateUrlLenOffset, (ushort)urlBytes.Length);
        }

        if ((urlChanged || sizeChanged) && urlBytes.Length > 0)
        {
            Buffer.BlockCopy(urlBytes, 0, fullStateScratch, FullStateHeaderSize, urlBytes.Length);
        }

        // Opening a session starts it playing, so a load we are announcing ahead of
        // time is always announced as playing.
        fullStateScratch[1] = (byte)(freshLoad ? SyncedPlaybackState.Playing : GetLocalState());
        WriteLong(fullStateScratch, 2, freshLoad ? 0L : PositionTicks());
        WriteUShort(fullStateScratch, FullStateNonceOffset, loadNonce);
        WriteSettingsBlock(fullStateScratch, FullStateSettingsOffset);
        return fullStateScratch;
    }

    private byte[] SerializeSettings()
    {
        settingsScratch[0] = (byte)MessageId.Settings;
        WriteSettingsBlock(settingsScratch, 1);
        return settingsScratch;
    }

    private void WriteSettingsBlock(byte[] buf, int offset)
    {
        SettingsFlags flags = SettingsFlags.None;
        if (AdminOnly)
        {
            flags |= SettingsFlags.AdminOnly;
        }

        if (AllowAnyoneToTakeControl)
        {
            flags |= SettingsFlags.AllowAnyoneToTakeControl;
        }

        if (AnyoneCanControl)
        {
            flags |= SettingsFlags.AnyoneCanControl;
        }

        buf[offset] = (byte)flags;
    }

    private void ReadSettingsBlock(byte[] buf, int offset)
    {
        var flags = (SettingsFlags)buf[offset];
        AdminOnly = (flags & SettingsFlags.AdminOnly) != 0;
        AllowAnyoneToTakeControl = (flags & SettingsFlags.AllowAnyoneToTakeControl) != 0;
        AnyoneCanControl = (flags & SettingsFlags.AnyoneCanControl) != 0;
    }

    private void ApplyRemoteSettings(byte[] buffer, int offset)
    {
        ReadSettingsBlock(buffer, offset);
        if (VerboseLogging)
        {
            BasisDebug.Log($"{nameof(BasisMediaPlayerNetworking)} applied remote settings: AdminOnly={AdminOnly}, AllowAnyoneToTakeControl={AllowAnyoneToTakeControl}, AnyoneCanControl={AnyoneCanControl}.", BasisDebug.LogTag.Video);
        }
    }

    private void BroadcastSettings()
    {
        if (!HasNetworkID)
        {
            sendOnNetworkReady = true;
            return;
        }

        SendCustomNetworkEvent(SerializeSettings(), DeliveryMethod.ReliableOrdered);
    }

    private bool TryDeserializeFullState(byte[] buffer, out string url, out SyncedPlaybackState state, out long positionTicks, out ushort remoteLoadNonce)
    {
        url = string.Empty;
        state = SyncedPlaybackState.Stopped;
        positionTicks = 0;
        remoteLoadNonce = 0;
        if (buffer == null || buffer.Length < FullStateHeaderSize)
        {
            return false;
        }

        byte stateByte = buffer[1];
        if (stateByte > (byte)SyncedPlaybackState.Paused)
        {
            return false;
        }

        state = (SyncedPlaybackState)stateByte;
        positionTicks = ReadLong(buffer, 2);
        remoteLoadNonce = ReadUShort(buffer, FullStateNonceOffset);
        ReadSettingsBlock(buffer, FullStateSettingsOffset);
        ushort urlLen = ReadUShort(buffer, FullStateUrlLenOffset);
        if (buffer.Length < FullStateHeaderSize + urlLen)
        {
            return false;
        }

        if (urlLen > 0)
        {
            url = UrlEncoding.GetString(buffer, FullStateHeaderSize, urlLen);
        }

        return true;
    }

    private static void WriteLong(byte[] buf, int offset, long value)
    {
        for (int i = 0; i < 8; i++)
        {
            buf[offset + i] = (byte)(value >> (i * 8));
        }
    }

    private static long ReadLong(byte[] buf, int offset)
    {
        long v = 0;
        for (int i = 0; i < 8; i++)
        {
            v |= (long)buf[offset + i] << (i * 8);
        }

        return v;
    }

    private static void WriteUShort(byte[] buf, int offset, ushort value)
    {
        buf[offset] = (byte)(value & 0xFF);
        buf[offset + 1] = (byte)((value >> 8) & 0xFF);
    }

    private static ushort ReadUShort(byte[] buf, int offset)
    {
        return (ushort)(buf[offset] | (buf[offset + 1] << 8));
    }
}
