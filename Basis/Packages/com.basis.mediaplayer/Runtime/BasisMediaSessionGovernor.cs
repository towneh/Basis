using System.Collections.Generic;
using Basis.Scripts.BasisSdk.Players;
using UnityEngine;

/// <summary>
/// Caps how many media players decode at once on this machine.
///
/// A world's media-player count stopped being a world author's decision once
/// props could carry one: anyone can spawn more, and every open session costs
/// this viewer stream bandwidth, memory and decode work whether or not they are
/// looking at it. A handful of autoplaying prop players will exhaust a
/// standalone headset's link and frame budget on their own.
///
/// The nearest few players stay running and the rest go dormant. Dormant means
/// the session is closed rather than paused — a paused session still holds its
/// buffer, frame pool and decoder — with the URL and playback position
/// remembered, so re-activating costs an ordinary join and lands where the
/// player would have been.
///
/// The cap is the viewer's setting, never the world's, for the same reason the
/// decode route is: it describes this machine.
/// </summary>
public static class BasisMediaSessionGovernor
{
    /// <summary>How near a dormant player must be, as a fraction of the
    /// distance to the active player it would displace, before it takes its
    /// place. Without a margin the two swap back and forth every time the
    /// viewer moves between them.</summary>
    const float SwapMargin = 0.8f;

    /// <summary>How long a player stays where it was put after being activated
    /// or made dormant. Stops a walk past a row of screens from opening and
    /// closing sessions the whole way along.</summary>
    const float DwellSeconds = 5f;

    /// <summary>How long an explicit promotion outranks distance.</summary>
    const float PromotionSeconds = 60f;

    const float EvaluateIntervalSeconds = 1f;

    class Entry
    {
        public BasisMediaPlayer Player;
        public bool Dormant;
        public string Url;
        public double PositionSeconds;
        public bool Live;
        public float SettledAt;
        public float PromotedUntil;
        public bool AwaitingResume;
        public float ResumeStartedAt;
    }

    static readonly Dictionary<BasisMediaPlayer, Entry> entries = new();
    static readonly List<Entry> ranked = new();
    static float nextEvaluate;

    /// <summary>Players this governor has closed to stay inside the cap.</summary>
    public static int DormantCount
    {
        get
        {
            int count = 0;
            foreach (Entry entry in entries.Values)
            {
                if (entry.Dormant) count++;
            }

            return count;
        }
    }

    /// <summary>The cap in force on this machine. 0 means no cap.</summary>
    public static int MaxActive => Mathf.Max(0, BasisMediaSettings.MaxActivePlayers.RawValue);

    /// <summary>Whether this player is dormant because of the cap, rather than
    /// simply not playing.</summary>
    public static bool IsDormant(BasisMediaPlayer player) =>
        player != null && entries.TryGetValue(player, out Entry entry) && entry.Dormant;

    /// <summary>
    /// Put a player at the front of the queue: it is activated now, and the
    /// furthest active player gives up its slot. This is what selecting a
    /// dormant player in the menu, or a world script deliberately starting one,
    /// should call.
    /// </summary>
    public static void Promote(BasisMediaPlayer player)
    {
        if (player == null) return;
        Entry entry = GetOrCreate(player);
        entry.PromotedUntil = Time.unscaledTime + PromotionSeconds;
        Evaluate();
    }

    static Driver driver;

    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.AfterSceneLoad)]
    static void Install()
    {
        // Statics outlive a domain reload with reload disabled in the editor, so
        // a second play session would otherwise inherit the previous one's view
        // of which players were dormant, and stack up a second driver.
        entries.Clear();
        ranked.Clear();
        nextEvaluate = 0f;
        if (driver != null) return;

        var host = new GameObject(nameof(BasisMediaSessionGovernor)) { hideFlags = HideFlags.HideAndDontSave };
        Object.DontDestroyOnLoad(host);
        driver = host.AddComponent<Driver>();
    }

    sealed class Driver : MonoBehaviour
    {
        BasisMediaDriverTick frameTick;

        void Awake() => frameTick = new BasisMediaDriverTick(Tick);

        void OnEnable() => frameTick.Arm();

        void OnDisable() => frameTick.Disarm();

        void Update() => frameTick.RunFromUpdate();
    }

    static void Tick()
    {
        ResumeWhereReady();
        if (Time.unscaledTime < nextEvaluate) return;
        nextEvaluate = Time.unscaledTime + EvaluateIntervalSeconds;
        Evaluate();
    }

    static Entry GetOrCreate(BasisMediaPlayer player)
    {
        if (!entries.TryGetValue(player, out Entry entry))
        {
            // Eligible immediately. The dwell exists to stop a player that just
            // moved from moving straight back, and a player we have never seen
            // has not moved — dating it from now would let every session in a
            // scene open at once and stay open for the first few seconds, which
            // is the moment the cap is most needed.
            entry = new Entry { Player = player, SettledAt = float.NegativeInfinity };
            entries[player] = entry;
        }

        return entry;
    }

    static void Evaluate()
    {
        int cap = MaxActive;
        Prune();
        if (cap <= 0)
        {
            // No cap: bring back anything this governor closed, and stop.
            foreach (Entry entry in entries.Values)
            {
                if (entry.Dormant) Activate(entry);
            }

            return;
        }

        ranked.Clear();
        IReadOnlyList<BasisMediaPlayer> players = BasisMediaPlayerRegistry.Players;
        for (int i = 0; i < players.Count; i++)
        {
            BasisMediaPlayer player = players[i];
            if (player == null) continue;
            Entry entry = GetOrCreate(player);

            // A player nobody has started does not hold a session and so costs
            // nothing to leave alone. Only players that are running, or that we
            // stopped, are competing for a slot.
            if (!entry.Dormant && !HoldsSession(player)) continue;

            // Someone started a dormant player behind our back. That is an
            // intent to watch it, so treat it as a promotion rather than
            // closing it again on the next pass.
            if (entry.Dormant && HoldsSession(player))
            {
                entry.Dormant = false;
                entry.PromotedUntil = Time.unscaledTime + PromotionSeconds;
                entry.SettledAt = Time.unscaledTime;
            }

            ranked.Add(entry);
        }

        if (ranked.Count == 0) return;

        Vector3 listener = ListenerPosition();
        ranked.Sort((a, b) => CompareForSlot(a, b, listener));

        float now = Time.unscaledTime;
        for (int i = 0; i < ranked.Count; i++)
        {
            Entry entry = ranked[i];
            bool wantsActive = i < cap;
            if (wantsActive == !entry.Dormant) continue;

            // Hold a player where it is for a moment after it moves, so walking
            // between screens does not open and close sessions the whole way.
            if (now - entry.SettledAt < DwellSeconds) continue;

            if (wantsActive)
            {
                // Only displace the sitting player when meaningfully nearer, or
                // the pair swaps back and forth around equal distance.
                Entry displaced = FirstActiveAtOrAfter(cap);
                if (displaced != null && !IsPromoted(entry, now) &&
                    DistanceTo(entry, listener) > DistanceTo(displaced, listener) * SwapMargin)
                {
                    continue;
                }

                Activate(entry);
            }
            else
            {
                Demote(entry);
            }
        }
    }

    /// <summary>The nearest player that is still running despite having ranked
    /// outside the cap. That is the one a nearer dormant player displaces, so it
    /// is what the swap margin is measured against.</summary>
    static Entry FirstActiveAtOrAfter(int index)
    {
        for (int i = Mathf.Max(0, index); i < ranked.Count; i++)
        {
            if (!ranked[i].Dormant) return ranked[i];
        }

        return null;
    }

    static bool IsPromoted(Entry entry, float now) => now < entry.PromotedUntil;

    static int CompareForSlot(Entry a, Entry b, Vector3 listener)
    {
        float now = Time.unscaledTime;
        bool pa = IsPromoted(a, now), pb = IsPromoted(b, now);
        if (pa != pb) return pa ? -1 : 1;
        return DistanceTo(a, listener).CompareTo(DistanceTo(b, listener));
    }

    /// <summary>True distance rather than the squared form, because the swap
    /// margin is expressed as a fraction of one and squaring would silently
    /// change what that fraction means. A handful of players once a second does
    /// not care about the square root.</summary>
    static float DistanceTo(Entry entry, Vector3 listener)
    {
        if (entry.Player == null) return float.MaxValue;
        return Vector3.Distance(entry.Player.transform.position, listener);
    }

    /// <summary>Where the viewer is. Falls back to the rendering camera, and
    /// then to the origin, so a headless or camera-less run still ranks
    /// deterministically rather than throwing.</summary>
    static Vector3 ListenerPosition()
    {
        BasisLocalPlayer local = BasisLocalPlayer.Instance;
        if (local != null) return local.transform.position;
        Camera camera = Camera.main;
        return camera != null ? camera.transform.position : Vector3.zero;
    }

    /// <summary>Whether the player is holding engine resources: anything but
    /// closed. A finished or failed session has already released them.</summary>
    static bool HoldsSession(BasisMediaPlayer player)
    {
        switch (player.State)
        {
            case BmState.Opening:
            case BmState.Buffering:
            case BmState.Playing:
            case BmState.Paused:
                return true;
            default:
                return false;
        }
    }

    static void Demote(Entry entry)
    {
        BasisMediaPlayer player = entry.Player;
        if (player == null) return;

        entry.Url = ShareableUrl(player);
        entry.Live = player.liveness == BmLiveness.Live || player.DurationSeconds <= 0d;
        entry.PositionSeconds = entry.Live ? 0d : player.PositionSeconds;
        entry.Dormant = true;
        entry.AwaitingResume = false;
        entry.SettledAt = Time.unscaledTime;
        player.Close();
    }

    static void Activate(Entry entry)
    {
        BasisMediaPlayer player = entry.Player;
        entry.Dormant = false;
        entry.SettledAt = Time.unscaledTime;
        if (player == null || string.IsNullOrEmpty(entry.Url)) return;

        player.OpenUserUrl(entry.Url);
        // Live sources rejoin at the edge; there is nothing to return to.
        if (entry.Live || entry.PositionSeconds <= 0d) return;
        entry.AwaitingResume = true;
        entry.ResumeStartedAt = Time.unscaledTime;
    }

    /// <summary>
    /// Puts a reactivated player back where it was, once its session is running
    /// far enough to accept a seek. A shared-playback player is left alone: its
    /// owner's position is the truth, and it arrives on the next heartbeat.
    /// </summary>
    static void ResumeWhereReady()
    {
        foreach (Entry entry in entries.Values)
        {
            if (!entry.AwaitingResume) continue;
            BasisMediaPlayer player = entry.Player;
            if (player == null) { entry.AwaitingResume = false; continue; }

            BmState state = player.State;
            if (state == BmState.Opening || state == BmState.Buffering) continue;
            entry.AwaitingResume = false;
            if (state != BmState.Playing && state != BmState.Paused) continue;
            // Under shared playback the owner's position is the truth and
            // arrives on the next heartbeat, so seeking locally would fight
            // it. Carrying the component is not the same as being in a
            // session, though: offline, or before a NetworkID is assigned,
            // there is no owner to defer to and nothing else will place this
            // player. HasNetworkID is the same test the component applies to
            // itself before it treats anything as networked.
            if (player.TryGetComponent(out BasisMediaPlayerNetworking networking)
                && networking.HasNetworkID)
            {
                continue;
            }
            if (player.DurationSeconds <= 0d) continue;

            player.Seek(entry.PositionSeconds + (Time.unscaledTime - entry.ResumeStartedAt));
        }
    }

    /// <summary>What to reopen with: the page URL a resolver recorded, since the
    /// stream it extracted is issued per client and will have expired by the
    /// time this player wakes up.</summary>
    static string ShareableUrl(BasisMediaPlayer player)
    {
        BasisResolvedMedia media = player.Media;
        if (media != null) return media.SourceUrl ?? string.Empty;
        return player.url ?? string.Empty;
    }

    static void Prune()
    {
        if (entries.Count == 0) return;
        List<BasisMediaPlayer> gone = null;
        foreach (KeyValuePair<BasisMediaPlayer, Entry> pair in entries)
        {
            if (pair.Key == null) (gone ??= new List<BasisMediaPlayer>()).Add(pair.Key);
        }

        if (gone == null) return;
        for (int i = 0; i < gone.Count; i++) entries.Remove(gone[i]);
    }
}
