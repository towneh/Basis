//! Software decoders (§6.7's software floor): permissively licensed,
//! in-process, one behaviour on every platform.
//!
//! The AV1 floor (rav1d) is absent on Android for now: the crates.io
//! rav1d package cannot build its arm64 assembly (`src/arm/asm-offsets.h`
//! is missing from the published crate — an upstream report
//! candidate), and the Vulkan present path has no CPU-frame upload yet, so
//! Android's AV1 route is platform-decoder-or-typed-refusal until both
//! land.

#[cfg(not(target_os = "android"))]
mod av1;
mod flac;
mod opus;

pub use self::opus::{OpusDecoder, OpusHead};
#[cfg(not(target_os = "android"))]
pub use av1::SwAv1Decoder;
pub use flac::FlacDecoder;
