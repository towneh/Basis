using System;
using System.Runtime.InteropServices;

/// <summary>
/// ABI v2 bindings for basis_media. Probe <see cref="bm_abi_version"/>
/// first and refuse a mismatch loudly; every other export takes the
/// generational session handle.
/// </summary>
public static class BasisMediaNative
{
    public const uint AbiVersion = 4;
    const string Dll = "basis_media";

    [DllImport(Dll)] public static extern uint bm_abi_version();

    /// <summary>
    /// Engine capability set (§6.11): writes one UTF-8 JSON blob and
    /// returns its byte length. Call with (null, 0) to size, allocate,
    /// call again; when the buffer is too small nothing is written and
    /// the required length still returns. Negative = error. Engine-level,
    /// not per-session. See <see cref="BasisMediaCapabilities"/>.
    /// </summary>
    [DllImport(Dll)]
    public static extern unsafe int bm_capabilities(byte* buffer, UIntPtr capacity);

    [DllImport(Dll)]
    public static extern int bm_session_open(byte[] descriptorUtf8, UIntPtr descriptorLen, out ulong handle);

    [DllImport(Dll)] public static extern int bm_session_close(ulong handle);
    [DllImport(Dll)] public static extern int bm_session_poll(ulong handle, out BmSnapshot snapshot);
    [DllImport(Dll)] public static extern int bm_session_play(ulong handle);
    [DllImport(Dll)] public static extern int bm_session_pause(ulong handle);
    [DllImport(Dll)] public static extern int bm_session_seek(ulong handle, long positionUs);

    [DllImport(Dll)]
    public static extern unsafe int bm_session_read_audio(ulong handle, float* buffer, uint maxSamples);

    /// <summary>
    /// Report the audio sink's estimated output latency (µs) — the chain
    /// between the pull and the speaker. The engine shifts the audio
    /// master clock back by it so video paces to the audible position.
    /// Send when the estimate changes; clamped engine-side to 0..500 ms.
    /// </summary>
    [DllImport(Dll)] public static extern int bm_session_set_audio_latency(ulong handle, long latencyUs);

    /// <summary>
    /// Feed the owner's extrapolated position as a shared-playback
    /// soft sync target (µs); negative clears it. The engine runs
    /// dead band → 2% slew → seek-last; the wanted slew comes back as
    /// the snapshot's SyncRatePpm, which the audio pull applies
    /// through its resampler. Live sessions ignore targets.
    /// </summary>
    [DllImport(Dll)] public static extern int bm_session_set_sync_target(ulong handle, long positionUs);

    [DllImport(Dll)]
    public static extern unsafe int bm_session_drain_events(ulong handle, BmEvent* events, uint capacity);

    [DllImport(Dll)]
    public static extern unsafe int bm_session_drain_captions(ulong handle, BmCaption* captions, uint capacity);

    /// <summary>
    /// Drain pending SEI user-data messages: records into
    /// <paramref name="records"/> (up to <paramref name="capacity"/>),
    /// their payloads packed into <paramref name="bytes"/> (up to
    /// <paramref name="bytesCapacity"/>), each record's Offset/Len locating
    /// its payload. Returns the record count. Only whole messages that fit
    /// are taken; the rest wait. A message whose payload alone exceeds
    /// bytesCapacity is dropped, so size bytes to at least the engine's
    /// 64 KiB per-message ceiling to see everything.
    /// </summary>
    [DllImport(Dll)]
    public static extern unsafe int bm_session_drain_user_data(ulong handle, BmUserData* records, uint capacity, byte* bytes, uint bytesCapacity);

    /// <summary>
    /// How many audio tracks the source offers instead of the bound one.
    /// 0 = nothing to choose between (one track, or a container that does
    /// not enumerate them). Stable for the session's life: switching track
    /// re-opens rather than switching in place.
    /// </summary>
    [DllImport(Dll)]
    public static extern int bm_session_audio_track_count(ulong handle);

    [DllImport(Dll)]
    public static extern unsafe int bm_session_get_audio_tracks(ulong handle, BmAudioTrack* tracks, uint capacity);

    /// <summary>
    /// Byte length of the container's embedded cover art, 0 where it
    /// carries none. Call before <see cref="bm_session_get_artwork"/> to
    /// size the buffer.
    /// </summary>
    [DllImport(Dll)]
    public static extern int bm_session_artwork_len(ulong handle);

    /// <summary>
    /// Copy the cover art and its MIME type out. The bytes are the
    /// container's own — JPEG or PNG as stored — so the caller decodes
    /// them; nothing in the engine parses an image. A buffer shorter than
    /// the reported length is refused rather than half-filled.
    /// </summary>
    [DllImport(Dll)]
    public static extern unsafe int bm_session_get_artwork(ulong handle, byte* data, uint capacity, byte* mime, uint mimeCapacity);

    /// <summary>
    /// Register the Unity output texture (from GetNativeTexturePtr).
    /// D3D11: a BGRA32 Texture2D. Vulkan/Android: a linear RGBA32
    /// RenderTexture with enableRandomWrite, created before this call.
    /// </summary>
    [DllImport(Dll)] public static extern int bm_session_set_output_texture(ulong handle, IntPtr texture);
    [DllImport(Dll)] public static extern IntPtr bm_render_event_func();

    /// <summary>The per-session present pass: the event data is the session
    /// handle (BM_EVENT_PRESENT).</summary>
    internal const int RenderEventPresent = 1;

    /// <summary>Destroy Vulkan objects retired by a closed session, with no
    /// session looked up and no event data (BM_EVENT_COLLECT). Android only;
    /// every other platform finds no session and does nothing.</summary>
    internal const int RenderEventCollect = 2;
}

/// <summary>What kind of source this is. Every transport bar bare
/// http(s) settles this itself — RTSP/WHEP/RIST are always live, an HLS
/// playlist says so, a resolver states it — so this is an override for
/// the one case left: a plain HTTP URL that is not a playlist.</summary>
public enum BmLiveness
{
    /// <summary>Let the player work it out from the source's own answer:
    /// finite and rangeable is on-demand, anything else is a live edge.
    /// The default, and right for everything except a lying server.</summary>
    Auto = 0,
    /// <summary>Force on-demand: read ahead and seek.</summary>
    Vod = 1,
    /// <summary>Force live: lag the edge, never read ahead.</summary>
    Live = 2,
}

/// <summary>Decode-route preference (engine §6.7): a per-user machine
/// setting, applied to every session the client opens — never a
/// world-author control. A rung the platform does not have is a typed
/// refusal (Quest has no software rung for H.264/HEVC/VP9).</summary>
public enum BmDecodePreference
{
    /// <summary>Hardware first; the software path engages with a
    /// DecodeFallbackHwToSw diagnostic. The default.</summary>
    HardwareWithFallback = 0,
    /// <summary>Hardware or typed refusal.</summary>
    HardwareOnly = 1,
    /// <summary>Software only — also the CPU A/B measurement lever and
    /// a driver-workaround escape hatch.</summary>
    SoftwareOnly = 2,
}

public enum BmState : uint
{
    Idle = 0,
    Opening = 1,
    Buffering = 2,
    Playing = 3,
    Paused = 4,
    Ended = 5,
    Error = 6,
}

/// <summary>Which stage owns a failure. The snapshot's error code is this
/// category times 100 plus a per-category sub-code, so the category names
/// the half of the number a reader can act on.</summary>
public enum BmErrorCategory : uint
{
    None = 0,
    Io = 1,
    Demux = 2,
    Decode = 3,
    Present = 4,
    Config = 5,
    Internal = 6,
}

[StructLayout(LayoutKind.Sequential)]
public struct BmSnapshot
{
    public uint AbiVersion;
    public uint State;
    public uint Width;
    public uint Height;
    public long PositionUs;
    public long DurationUs;
    public ulong FramesDecoded;
    public ulong FramesPresented;
    public int ErrorCode;
    public uint ErrorCategory;
    public long BankedMs;
    public uint BankHolding;
    public uint AudioSampleRate;
    public uint AudioChannels;
    public uint Reserved;
    /// <summary>
    /// Sync ladder's wanted rate offset from 1x, ppm. The audio pull
    /// must consume source frames at (1 + ppm/1e6) × stream rate
    /// while non-zero, or corrections degrade to seeks.
    /// </summary>
    public int SyncRatePpm;
    public uint Reserved2;
}

[StructLayout(LayoutKind.Sequential)]
public unsafe struct BmEvent
{
    public long WallUs;
    public uint Code;
    public uint Stage;
    public uint DetailLen;
    public fixed byte Detail[116];
}

/// <summary>
/// One in-band CEA-608 caption cue: the full displayed text as of
/// PtsUs (UTF-8, rows joined with '\n'; TextLen 0 = display cleared).
/// Cues arrive ahead of presentation — display when the session
/// position reaches PtsUs.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct BmCaption
{
    public long PtsUs;
    public uint TextLen;
    public fixed byte Text[256];
    /// <summary>
    /// Names the four bytes PtsUs's alignment adds after Text. Not part of
    /// the contract — always 0. The struct is 272 bytes either way.
    /// </summary>
    public uint Reserved;
}

/// <summary>
/// One SEI user_data_unregistered message, surfaced with its UUID and left
/// unparsed. The payload lands in the caller's byte buffer at Offset for
/// Len; this record only points at it. Messages arrive ahead of
/// presentation — act on one when the session position reaches PtsUs.
/// 32 bytes, no padding.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct BmUserData
{
    public long PtsUs;
    public fixed byte Uuid[16];
    public uint Offset;
    public uint Len;
}

/// <summary>
/// One selectable audio track. Language is the container's ISO 639 code
/// and Label its track name; either can be absent — a recording that puts
/// a microphone on its own track usually states neither — so a picker has
/// to be able to tell rows apart by position alone.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct BmAudioTrack
{
    public uint TrackId;
    public uint SampleRate;
    public uint Channels;
    public uint LanguageLen;
    public fixed byte Language[16];
    public uint LabelLen;
    public fixed byte Label[64];
}
