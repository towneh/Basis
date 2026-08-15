//! Elementary-stream bitstream parsing shared by the demux layer: Annex-B
//! NAL walking, H.264 SPS dimensions, keyframe detection, ADTS headers and
//! the AudioSpecificConfig (spec §6.1). Ported from the C player's
//! `basis_bitstream`, whose guards were paid for by fuzzing — the ue(v)
//! shift cap, the frozen bit reader past end-of-data, and the i64 crop
//! arithmetic all correspond to pinned crash testcases.
//!
//! Everything here parses attacker-controlled bytes.

#![forbid(unsafe_code)]

mod adts;
mod annexb;
mod cea608;
mod h264;
mod sei;

pub use adts::{AAC_RATES, AdtsHeader, aac_channels_from_config, build_asc, parse_adts};
pub use annexb::{h264_is_keyframe, h264_nal_type, h265_is_keyframe, h265_nal_type, nal_units};
pub use cea608::{CaptionCue, CaptionScanner, a53_cc_triples};
pub use h264::sps_dimensions;
pub use sei::{scan_au_sei, sei_messages, unescape_rbsp};
