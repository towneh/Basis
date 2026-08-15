//! Platform decoder routing (§6.7): one factory pair per platform behind
//! the same signatures, so the pipeline threads stay platform-free. A
//! refusal is typed — the caller turns it into a CodecRefused diagnostic,
//! mutes the track and plays on; a software fallback engaging is reported,
//! never silent.
//!
//! The route ladder honours the user's decode preference (§6.7):
//! hardware-with-fallback (default) / hardware-only / software-only. A
//! rung the platform does not have is a typed refusal. Software routes
//! additionally enforce the performance cap: content over 1080p60
//! coded pixel rate refuses in the CodecRefused posture rather than
//! melting a CPU the platform gave no hardware path for.

use crate::DecodePreference;
use media_decode::{AudioDecoder, VideoDecoder};

/// The software-route cap: coded pixel rate ≤ 1920 × 1088 × 60
/// (~125 Mpx/s — real 1080p with macroblock padding passes). Where no
/// frame rate is stated, the gate is dimensions alone (≤1920×1088); the
/// budget form admits cost-equivalent shapes such as 1440p30 when a
/// demuxer states the rate. Tightens on field evidence of weak-CPU
/// struggle in the 1080p30–60 band. Android's routes are all
/// platform-MediaCodec and sit outside the gate.
#[cfg(not(target_os = "android"))]
pub const SOFTWARE_CAP_WIDTH: u32 = 1920;
#[cfg(not(target_os = "android"))]
pub const SOFTWARE_CAP_HEIGHT: u32 = 1088;
#[cfg(not(target_os = "android"))]
pub const SOFTWARE_CAP_FPS: u64 = 60;

/// Whether a software decode route accepts content of this coded shape.
#[cfg(not(target_os = "android"))]
pub fn software_cap_allows(width: u32, height: u32, fps: Option<u32>) -> bool {
    match fps {
        Some(fps) => {
            u64::from(width) * u64::from(height) * u64::from(fps)
                <= u64::from(SOFTWARE_CAP_WIDTH) * u64::from(SOFTWARE_CAP_HEIGHT) * SOFTWARE_CAP_FPS
        }
        None => width <= SOFTWARE_CAP_WIDTH && height <= SOFTWARE_CAP_HEIGHT,
    }
}

#[cfg(not(target_os = "android"))]
fn software_cap_check(width: u32, height: u32) -> Result<(), media_decode::DecodeError> {
    // No demuxer states a frame rate yet, so the dimensions-only arm is
    // the live gate; the pixel-rate form activates when one does.
    if software_cap_allows(width, height, None) {
        Ok(())
    } else {
        Err(media_decode::DecodeError(format!(
            "software decode routes accept up to {SOFTWARE_CAP_WIDTH}x{SOFTWARE_CAP_HEIGHT}@{SOFTWARE_CAP_FPS}; \
             this stream is {width}x{height}"
        )))
    }
}

/// The route a video format resolved to: which decoder, and whether the
/// platform path was absent so the software floor carried it.
pub struct VideoRoute {
    pub decoder: Box<dyn VideoDecoder>,
    pub label: &'static str,
    pub fallback: Option<String>,
    /// The hardware decoder's D3D11 device (`ID3D11Device*`, valid while
    /// the decoder lives): the presenter shares it so decoded slices bind
    /// into the conversion pass with no cross-device copy. `None` on
    /// software routes (the presenter keeps its own device).
    pub decode_device: Option<*mut std::ffi::c_void>,
}

#[cfg(windows)]
pub fn open_video_decoder(
    codec: media_demux::VideoCodec,
    coded_width: u32,
    coded_height: u32,
    _live: bool,
    preference: DecodePreference,
    codec_private: &[u8],
) -> Result<VideoRoute, media_decode::DecodeError> {
    use decode_mf::{HwCodec, HwVideoDecoder};
    use media_demux::VideoCodec;

    let hw_codec = match codec {
        VideoCodec::H264 => Some(HwCodec::H264),
        VideoCodec::H265 => Some(HwCodec::H265),
        VideoCodec::Vp9 => Some(HwCodec::Vp9),
        VideoCodec::Av1 => Some(HwCodec::Av1),
        VideoCodec::Vp8 => None,
    };

    // Hardware rung first unless the user opted out. Construction runs
    // the two-leg claim (MFT present + GPU profile/format/config at this
    // resolution), so a failure here is the honest "no hardware path".
    let mut hw_failure: Option<String> = None;
    if preference != DecodePreference::SoftwareOnly {
        if let Some(hw) = hw_codec {
            match HwVideoDecoder::new(hw, coded_width, coded_height, codec_private) {
                Ok(decoder) => {
                    let device = decoder.device_raw();
                    return Ok(VideoRoute {
                        decoder: Box::new(decoder),
                        label: match hw {
                            HwCodec::H264 => "DXVA H.264",
                            HwCodec::H265 => "DXVA HEVC",
                            HwCodec::Vp9 => "DXVA VP9",
                            HwCodec::Av1 => "DXVA AV1",
                        },
                        fallback: None,
                        decode_device: Some(device),
                    });
                }
                Err(e) => hw_failure = Some(e.0),
            }
        }
        if preference == DecodePreference::HardwareOnly {
            return Err(media_decode::DecodeError(match hw_failure {
                Some(e) => format!("hardware-only decode preference: {e}"),
                None => format!("hardware-only decode preference: no hardware route for {codec:?}"),
            }));
        }
    }

    // Software rung — the direct route under software_only, the reported
    // fallback otherwise. The software cap gates it before any decoder builds.
    software_cap_check(coded_width, coded_height)?;
    let fallback = hw_failure.map(|e| format!("hardware decode unavailable ({e})"));
    open_windows_software(codec, coded_width, coded_height, fallback)
}

/// The Windows CPU routes: in-box
/// H.264 sync MFT, Store VP9 extension, AV1 on rav1d with the Store
/// extension quarantined to last (it misbehaves under sync driving —
/// ProcessInput blocks for over a second when its queue fills).
#[cfg(windows)]
fn open_windows_software(
    codec: media_demux::VideoCodec,
    coded_width: u32,
    coded_height: u32,
    fallback: Option<String>,
) -> Result<VideoRoute, media_decode::DecodeError> {
    use decode_mf::{Av1Decoder, H264Decoder, Vp9Decoder};
    use decode_sw::SwAv1Decoder;
    use media_demux::VideoCodec;
    match codec {
        VideoCodec::H264 => Ok(VideoRoute {
            decoder: Box::new(H264Decoder::new()?),
            label: "MF H.264",
            fallback,
            decode_device: None,
        }),
        VideoCodec::Vp9 => Ok(VideoRoute {
            decoder: Box::new(Vp9Decoder::new(coded_width, coded_height)?),
            label: "MF VP9",
            fallback,
            decode_device: None,
        }),
        VideoCodec::Av1 => match SwAv1Decoder::new() {
            Ok(d) => Ok(VideoRoute {
                decoder: Box::new(d),
                label: "rav1d",
                fallback,
                decode_device: None,
            }),
            Err(floor) => match Av1Decoder::new(coded_width, coded_height) {
                Ok(d) => Ok(VideoRoute {
                    decoder: Box::new(d),
                    label: "MF AV1",
                    fallback: Some(match fallback {
                        Some(f) => format!("{f}; rav1d unavailable ({floor})"),
                        None => format!("rav1d unavailable ({floor})"),
                    }),
                    decode_device: None,
                }),
                Err(platform) => Err(media_decode::DecodeError(format!(
                    "rav1d: {floor}; platform: {platform}"
                ))),
            },
        },
        VideoCodec::H265 => Err(media_decode::DecodeError(
            "no software H.265 route (hardware DXVA is the only Windows HEVC path)".into(),
        )),
        VideoCodec::Vp8 => Err(media_decode::DecodeError(
            "no VP8 decode path (platform ceiling)".into(),
        )),
    }
}

/// Android: every route is the platform MediaCodec stack (§6.7 —
/// platform-decoder-or-typed-refusal is the ceiling for the patented and
/// the royalty-free video codecs alike; Quest has no software fallback
/// for avc/hevc/vp9 and the rav1d floor has no Vulkan upload path yet, so
/// a missing platform decoder is a typed refusal, observable, not
/// silent). The software-only preference has no rung here.
#[cfg(target_os = "android")]
pub fn open_video_decoder(
    codec: media_demux::VideoCodec,
    coded_width: u32,
    coded_height: u32,
    live: bool,
    preference: DecodePreference,
    _codec_private: &[u8],
) -> Result<VideoRoute, media_decode::DecodeError> {
    use decode_mediacodec::{McVideoDecoder, VideoMime};
    use media_demux::VideoCodec;
    if preference == DecodePreference::SoftwareOnly {
        return Err(media_decode::DecodeError(
            "software-only decode preference: no software video route on this platform".into(),
        ));
    }
    let mime = match codec {
        VideoCodec::H264 => VideoMime::H264,
        VideoCodec::H265 => VideoMime::H265,
        VideoCodec::Vp9 => VideoMime::Vp9,
        VideoCodec::Vp8 => VideoMime::Vp8,
        VideoCodec::Av1 => VideoMime::Av1,
    };
    let decoder = McVideoDecoder::new(mime, coded_width, coded_height, live)?;
    Ok(VideoRoute {
        decoder: Box::new(decoder),
        label: "MediaCodec",
        fallback: None,
        decode_device: None,
    })
}

/// Headless platforms (Linux and anything else without a platform
/// decoder adapter): only the in-process floors route. The patented
/// codecs never bundle (§6.7), and the VAAPI adapter is future work, so
/// H.264/H.265/VP9/VP8 are typed refusals here — observable, never
/// silent. The hardware-only preference has no rung.
#[cfg(not(any(windows, target_os = "android")))]
pub fn open_video_decoder(
    codec: media_demux::VideoCodec,
    coded_width: u32,
    coded_height: u32,
    _live: bool,
    preference: DecodePreference,
    _codec_private: &[u8],
) -> Result<VideoRoute, media_decode::DecodeError> {
    use decode_sw::SwAv1Decoder;
    use media_demux::VideoCodec;
    if preference == DecodePreference::HardwareOnly {
        return Err(media_decode::DecodeError(
            "hardware-only decode preference: no hardware video route on this platform".into(),
        ));
    }
    match codec {
        VideoCodec::Av1 => {
            software_cap_check(coded_width, coded_height)?;
            Ok(VideoRoute {
                decoder: Box::new(SwAv1Decoder::new()?),
                label: "rav1d",
                fallback: None,
                decode_device: None,
            })
        }
        VideoCodec::H264 | VideoCodec::H265 => Err(media_decode::DecodeError(
            "no platform decode path on this platform yet (VAAPI adapter pending)".into(),
        )),
        VideoCodec::Vp9 | VideoCodec::Vp8 => Err(media_decode::DecodeError(
            "no VP9/VP8 decode path (platform ceiling; VAAPI adapter pending)".into(),
        )),
    }
}

#[cfg(windows)]
pub fn open_audio_decoder(
    codec: media_demux::AudioCodec,
    sample_rate: u32,
    channels: u32,
    codec_private: &[u8],
) -> Result<Box<dyn AudioDecoder>, media_decode::DecodeError> {
    use decode_mf::{AacDecoder, Mp3Decoder};
    use decode_sw::{FlacDecoder, OpusDecoder};
    use media_demux::AudioCodec;
    Ok(match codec {
        AudioCodec::Aac => Box::new(AacDecoder::new(sample_rate, channels, codec_private)?),
        AudioCodec::Mp3 => Box::new(Mp3Decoder::new(sample_rate, channels)?),
        AudioCodec::Flac => Box::new(FlacDecoder::new(codec_private)?),
        AudioCodec::Opus => Box::new(OpusDecoder::new(codec_private)?),
        AudioCodec::Pcm => {
            return Err(media_decode::DecodeError(
                "no LPCM adapter yet (lands with the multichannel work)".into(),
            ));
        }
    })
}

/// Android: AAC/MP3 decode on the platform (§6.7 — the patented codecs
/// never bundle); FLAC and Opus stay on the in-process floors for one
/// behaviour across platforms.
#[cfg(target_os = "android")]
pub fn open_audio_decoder(
    codec: media_demux::AudioCodec,
    sample_rate: u32,
    channels: u32,
    codec_private: &[u8],
) -> Result<Box<dyn AudioDecoder>, media_decode::DecodeError> {
    use decode_mediacodec::{AudioMime, McAudioDecoder};
    use decode_sw::{FlacDecoder, OpusDecoder};
    use media_demux::AudioCodec;
    Ok(match codec {
        AudioCodec::Aac => Box::new(McAudioDecoder::new(
            AudioMime::Aac,
            sample_rate,
            channels,
            codec_private,
        )?),
        AudioCodec::Mp3 => Box::new(McAudioDecoder::new(
            AudioMime::Mp3,
            sample_rate,
            channels,
            &[],
        )?),
        AudioCodec::Flac => Box::new(FlacDecoder::new(codec_private)?),
        AudioCodec::Opus => Box::new(OpusDecoder::new(codec_private)?),
        AudioCodec::Pcm => {
            return Err(media_decode::DecodeError(
                "no LPCM adapter yet (lands with the multichannel work)".into(),
            ));
        }
    })
}

/// Headless platforms: FLAC and Opus on the in-process floors; AAC and
/// MP3 have no platform decoder here and refuse typed (§6.7 — the
/// patented codecs never bundle).
#[cfg(not(any(windows, target_os = "android")))]
pub fn open_audio_decoder(
    codec: media_demux::AudioCodec,
    _sample_rate: u32,
    _channels: u32,
    codec_private: &[u8],
) -> Result<Box<dyn AudioDecoder>, media_decode::DecodeError> {
    use decode_sw::{FlacDecoder, OpusDecoder};
    use media_demux::AudioCodec;
    Ok(match codec {
        AudioCodec::Flac => Box::new(FlacDecoder::new(codec_private)?),
        AudioCodec::Opus => Box::new(OpusDecoder::new(codec_private)?),
        AudioCodec::Aac | AudioCodec::Mp3 => {
            return Err(media_decode::DecodeError(
                "no AAC/MP3 decode path on this platform (platform decoders only, §6.7)".into(),
            ));
        }
        AudioCodec::Pcm => {
            return Err(media_decode::DecodeError(
                "no LPCM adapter yet (lands with the multichannel work)".into(),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::software_cap_allows;

    #[test]
    fn software_cap_budget_form() {
        // At or under the 1080p60 pixel-rate budget.
        assert!(software_cap_allows(1920, 1088, Some(60)));
        assert!(software_cap_allows(1920, 1080, Some(60)));
        assert!(software_cap_allows(2560, 1440, Some(30))); // ~110 Mpx/s
        assert!(software_cap_allows(1280, 720, Some(120)));
        // Over budget: 1440p60, 4K at any rate, 1080p120.
        assert!(!software_cap_allows(2560, 1440, Some(60)));
        assert!(!software_cap_allows(3840, 2160, Some(30)));
        assert!(!software_cap_allows(1920, 1088, Some(120)));
    }

    /// The enforcement point: a software route resolving for over-cap
    /// content refuses typed before any decoder builds, on every
    /// preference that lands on the software rung.
    #[cfg(windows)]
    #[test]
    fn software_route_refuses_over_cap_content() {
        let err = super::open_video_decoder(
            media_demux::VideoCodec::H264,
            3840,
            2160,
            false,
            crate::DecodePreference::SoftwareOnly,
            &[],
        )
        .err()
        .expect("over-cap software route must refuse");
        assert!(
            err.0.contains("software decode routes accept up to"),
            "{}",
            err.0
        );
    }

    #[test]
    fn software_cap_dims_only_when_fps_unstated() {
        assert!(software_cap_allows(1920, 1088, None));
        assert!(software_cap_allows(1920, 1080, None));
        assert!(software_cap_allows(640, 360, None));
        assert!(!software_cap_allows(2560, 1440, None));
        assert!(!software_cap_allows(3840, 2160, None));
        assert!(!software_cap_allows(1921, 1080, None));
    }
}
