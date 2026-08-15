using Basis.Scripts.Settings;

namespace Basis.Media
{
    /// <summary>
    /// Client-persisted settings for the media engine, owned by this package
    /// rather than the framework's BasisSettingsDefaults. Values live in the
    /// user's settings file and are never world content.
    /// </summary>
    public static class BasisMediaSettings
    {
        /// <summary>Stored value of <see cref="DecodePreference"/> for
        /// <see cref="BmDecodePreference.HardwareWithFallback"/>.</summary>
        public const string DecodeHardwareWithFallback = "hardware_fallback";

        /// <summary>Stored value of <see cref="DecodePreference"/> for
        /// <see cref="BmDecodePreference.HardwareOnly"/>.</summary>
        public const string DecodeHardwareOnly = "hardware_only";

        /// <summary>Stored value of <see cref="DecodePreference"/> for
        /// <see cref="BmDecodePreference.SoftwareOnly"/>.</summary>
        public const string DecodeSoftwareOnly = "software_only";

        static BasisMediaSettings()
        {
            BasisSettingsBindingPostLoad.Register(typeof(BasisMediaSettings));
            DecodePreference.OnChanged += ApplyDecodePreference;
        }

        /// <summary>Which decode route the engine may take. Stored as one of the
        /// Decode* constants; applied to <see cref="BasisMediaPlayer.DecodePreference"/>,
        /// which every session reads when it opens.</summary>
        public static BasisSettingsBinding<string> DecodePreference =
            new("mediaplayerdecodepreference", new BasisPlatformDefault<string>(DecodeHardwareWithFallback));

        /// <summary>Touches the binding so the static constructor runs, then pushes
        /// the stored decode preference into the engine.</summary>
        public static void EnsureLoaded()
        {
            ApplyDecodePreference(DecodePreference.RawValue);
        }

        /// <summary>Maps a stored value onto the engine enum. Unrecognised values
        /// (a hand-edited settings file, or a key written by a later build) resolve
        /// to the default route rather than refusing to play.</summary>
        public static BmDecodePreference ToEnginePreference(string stored) => stored switch
        {
            DecodeHardwareOnly => BmDecodePreference.HardwareOnly,
            DecodeSoftwareOnly => BmDecodePreference.SoftwareOnly,
            _ => BmDecodePreference.HardwareWithFallback,
        };

        static void ApplyDecodePreference(string stored)
        {
            BasisMediaPlayer.DecodePreference = ToEnginePreference(stored);
        }
    }
}
