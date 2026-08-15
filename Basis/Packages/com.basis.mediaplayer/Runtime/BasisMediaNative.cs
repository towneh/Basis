using System;
using System.Runtime.InteropServices;

namespace Basis.Media
{
    /// <summary>
    /// ABI v2 bindings for basis_media. Probe <see cref="bm_abi_version"/>
    /// first and refuse a mismatch loudly; every other export takes the
    /// generational session handle.
    /// </summary>
    public static class BasisMediaNative
    {
        public const uint AbiVersion = 2;
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
        /// Register the Unity output texture (from GetNativeTexturePtr).
        /// D3D11: a BGRA32 Texture2D. Vulkan/Android: a linear RGBA32
        /// RenderTexture with enableRandomWrite, created before this call.
        /// </summary>
        [DllImport(Dll)] public static extern int bm_session_set_output_texture(ulong handle, IntPtr texture);
        [DllImport(Dll)] public static extern IntPtr bm_render_event_func();
    }

    public enum BmLiveness
    {
        Unknown = 0,
        Vod = 1,
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
    }
}
