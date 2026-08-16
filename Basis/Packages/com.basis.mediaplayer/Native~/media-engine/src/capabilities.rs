//! The engine-declared capability set (§6.11): one queryable snapshot of
//! what this build will decode and play, consumed by the resolver's
//! format selection and the managed layer. Every entry is a will-decode
//! claim for the primary route the engine would actually take — probed
//! routes (the Store VP9 extension) are checked at build time, constant
//! routes (in-box MFTs, the in-process floors) are stated outright.
//!
//! The set is a snapshot, not a live object: runtime changes (a software
//! fallback engaging) surface as diagnostics events — `DecodeFallbackHwToSw`
//! is the re-query advisory — and the consumer queries again.

use serde::Serialize;

/// Contract version (not the engine version). Bump on breaking shape
/// changes; additive fields do not bump it.
pub const CAPABILITIES_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct CapabilitySet {
    pub version: u32,
    /// "windows-x64" today; "android-quest" joins with the MediaCodec
    /// adapter.
    pub platform: String,
    pub video: Vec<VideoCap>,
    pub audio: Vec<AudioCap>,
    pub transports: Vec<TransportCap>,
    pub containers: Vec<String>,
}

/// Route the engine would actually take for the codec. Both routes may
/// appear for one codec; the resolver ranks the best offer, diagnosis
/// reads the route taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// Hardware-accelerated decode: DXVA on Windows (the two-leg probe —
    /// MFT present plus `ID3D11VideoDevice` profile/format/config —
    /// backs the claim), MediaCodec hardware on Android.
    Hardware,
    /// CPU decode: platform software MFTs and the in-process floors.
    Software,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoCap {
    pub codec: String,
    pub route: Route,
    /// 0 = unstated. Hardware routes state measured ceilings (the DXVA
    /// resolution-ladder walk on Windows, the Quest MediaCodecList
    /// figures on Android); software routes state the policy ceiling
    /// (1920/1088/60) the engine enforces.
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioCap {
    pub codec: String,
    pub max_channels: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransportCap {
    pub scheme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A software route's entry, stating the enforced ceiling so
/// resolvers rank against the same numbers the engine refuses on.
#[cfg(not(target_os = "android"))]
fn software_video(codec: &str) -> VideoCap {
    VideoCap {
        codec: codec.into(),
        route: Route::Software,
        max_width: crate::route::SOFTWARE_CAP_WIDTH,
        max_height: crate::route::SOFTWARE_CAP_HEIGHT,
        max_fps: crate::route::SOFTWARE_CAP_FPS as u32,
    }
}

fn audio(codec: &str, max_channels: u32) -> AudioCap {
    AudioCap {
        codec: codec.into(),
        max_channels,
    }
}

fn transport(scheme: &str) -> TransportCap {
    TransportCap {
        scheme: scheme.into(),
        note: None,
    }
}

/// Windows video routes (§6.7): hardware DXVA entries carry the
/// two-leg probe's measured resolution ceiling (fps unstated — DXVA has
/// no rate ceiling to measure); the CPU rungs stay listed as the
/// fallback routes, stating the software policy ceiling. Both routes may
/// appear for one codec.
#[cfg(windows)]
fn platform_video_caps() -> Vec<VideoCap> {
    use decode_mf::HwCodec;
    let mut video_caps = Vec::new();
    let hardware = [
        ("h264", HwCodec::H264),
        ("h265", HwCodec::H265),
        ("vp9", HwCodec::Vp9),
        ("av1", HwCodec::Av1),
    ];
    for (codec, hw) in hardware {
        if let Some((max_width, max_height)) = decode_mf::probe_hardware_ceiling(hw) {
            video_caps.push(VideoCap {
                codec: codec.into(),
                route: Route::Hardware,
                max_width,
                max_height,
                max_fps: 0,
            });
        }
    }
    video_caps.push(software_video("h264"));
    if decode_mf::probe_vp9() {
        video_caps.push(software_video("vp9"));
    }
    // AV1's software rung is the rav1d in-process floor (compiled in
    // unconditionally). The Store AV1 extension contributes no entry: it
    // misbehaves under sync driving and is quarantined to a runtime
    // fallback.
    video_caps.push(software_video("av1"));
    video_caps
}

/// Android video routes: every entry is the platform MediaCodec decoder,
/// probed by creation (a will-decode claim for the route the engine
/// takes). Route and ceilings come from the adapter's own probe: the
/// codec name separates hardware from the c2.android software fallbacks,
/// and the ceilings are the `MediaCodecList` figures when the JVM probe
/// is available (0 = unstated — rank conservatively).
#[cfg(target_os = "android")]
fn platform_video_caps() -> Vec<VideoCap> {
    use decode_mediacodec::VideoMime;
    let mimes = [
        ("h264", VideoMime::H264),
        ("h265", VideoMime::H265),
        ("vp8", VideoMime::Vp8),
        ("vp9", VideoMime::Vp9),
        ("av1", VideoMime::Av1),
    ];
    mimes
        .into_iter()
        .filter_map(|(codec, mime)| {
            decode_mediacodec::probe_video_decoder(mime).map(|probe| VideoCap {
                codec: codec.into(),
                route: if probe.hardware {
                    Route::Hardware
                } else {
                    Route::Software
                },
                max_width: probe.max_width,
                max_height: probe.max_height,
                max_fps: probe.max_fps,
            })
        })
        .collect()
}

/// Headless platforms: the in-process floors are the only video routes —
/// AV1 on rav1d, CPU decode, the software policy ceiling. Hardware entries
/// join with the VAAPI adapter.
#[cfg(not(any(windows, target_os = "android")))]
fn platform_video_caps() -> Vec<VideoCap> {
    vec![software_video("av1")]
}

/// Build the capability set for this process. Cheap enough to rebuild on
/// every query (one decoder enumerate + activate per probed codec);
/// callers cache, the engine does not — a re-query after a
/// capability-change diagnostic must re-probe.
pub fn capabilities() -> CapabilitySet {
    let platform = if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x64".to_string()
    } else if cfg!(all(target_os = "android", target_arch = "aarch64")) {
        "android-arm64".to_string()
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64".to_string()
    } else {
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    };

    let video_caps = platform_video_caps();

    // Audio ceilings are the adapters' real screens: AAC chan_conf 1..=6,
    // MP3 mono/stereo, Opus mapping family 0 only, claxon's 8-channel cap,
    // and the PCM adapter's 1..=8. The screens hold on Android too — the
    // demux-side AAC channel screen and the in-process Opus/FLAC/PCM floors
    // are platform-free. Headless platforms carry only the in-process
    // floors: AAC/MP3 ride platform decoders, which do not exist there, so
    // listing them would be false will-decode claims.
    #[cfg(any(windows, target_os = "android"))]
    let audio_caps = vec![
        audio("aac", 6),
        audio("mp3", 2),
        audio("opus", 2),
        audio("flac", 8),
        audio("pcm", 8),
    ];
    #[cfg(not(any(windows, target_os = "android")))]
    let audio_caps = vec![audio("opus", 2), audio("flac", 8), audio("pcm", 8)];

    // The engine's routing table. `rist` appears iff the feature was
    // compiled in; the stub's typed refusal stays the runtime backstop.
    let mut transports = vec![
        transport("file"),
        transport("http"),
        transport("https"),
        transport("rtsp"),
        transport("rtspt"),
        transport("whep"),
        transport("wheps"),
    ];
    if cfg!(feature = "rist") {
        transports.push(transport("rist"));
    }

    let containers = [
        "mp4", "ts", "m2ts", "mkv", "webm", "hls", "wav", "flac", "mp3", "adts", "ogg",
    ]
    .map(String::from)
    .to_vec();

    CapabilitySet {
        version: CAPABILITIES_VERSION,
        platform,
        video: video_caps,
        audio: audio_caps,
        transports,
        containers,
    }
}

impl CapabilitySet {
    /// The one serialisation the contract ships: the versioned JSON blob
    /// that crosses the ABI (§7's UTF-8 serialised-blob posture).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("capability set serialises")
    }
}
