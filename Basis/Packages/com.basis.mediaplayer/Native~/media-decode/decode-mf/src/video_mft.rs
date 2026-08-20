//! The shared sync-MFT driver for the video decoders: NV12 output
//! negotiation (matrix/range re-read on every stream change, §6.8), the
//! fresh-sample-per-call output contract, strided copies and the
//! drain/flush protocol are identical across codecs — only the input type
//! configured by the adapter differs.

use media_decode::{
    ColorInfo, DecodeError, Nv12Frame, SubmitOutcome, YuvMatrix, YuvRange, packed_nv12_len,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMF2DBuffer2, IMFActivate, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MF2DBuffer_LockFlags_Read, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_0_255, MFNominalRange_16_235, MFT_CATEGORY_VIDEO_DECODER,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_NV12,
    MFVideoTransferMatrix_BT601, MFVideoTransferMatrix_BT709, MFVideoTransferMatrix_BT2020_10,
    MFVideoTransferMatrix_BT2020_12,
};
use windows::core::Interface;

use crate::mf;
use std::mem::ManuallyDrop;

/// Probe for a registered sync video decoder MFT taking `subtype` input
/// (§6.7: how the Store-extension decoders are found — their absence is a
/// typed error the engine reports, never a mystery).
pub(crate) fn create_decoder_for(
    subtype: &windows::core::GUID,
    what: &str,
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
) -> Result<IMFTransform, DecodeError> {
    // SAFETY: MFTEnumEx's out-params are a caller-freed activate array;
    // every element is wrapped or dropped below and the array itself is
    // freed with CoTaskMemFree on all paths.
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: *subtype,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        mf(
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input),
                None,
                &mut activates,
                &mut count,
            ),
            "MFTEnumEx",
        )?;
        if activates.is_null() {
            return Err(DecodeError(format!("no {what} decoder installed")));
        }
        let slice = std::slice::from_raw_parts_mut(activates, count as usize);
        let mut found: Result<IMFTransform, DecodeError> =
            Err(DecodeError(format!("no {what} decoder installed")));
        for slot in slice.iter_mut() {
            // Every activate must be taken so its COM reference drops even
            // once a decoder has been picked.
            let Some(activate) = slot.take() else {
                continue;
            };
            if found.is_ok() {
                continue;
            }
            match activate.ActivateObject::<IMFTransform>() {
                Ok(mft) => found = Ok(mft),
                Err(e) => found = Err(DecodeError(format!("activate {what} decoder: {e}"))),
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        found
    }
}

/// Matrix/range from an MFT's output media type. The MFT states them
/// from the bitstream's own colour description; absent attributes stay
/// Unspecified rather than being guessed here (§6.8).
pub(crate) fn parse_output_color(
    ty: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> ColorInfo {
    let mut color = ColorInfo::default();
    // SAFETY: attribute reads on a live owned media type; no pointers cross.
    unsafe {
        let matrix = ty.GetUINT32(&MF_MT_YUV_MATRIX).map(|v| v as i32);
        color.matrix = match matrix {
            Ok(v) if v == MFVideoTransferMatrix_BT709.0 => YuvMatrix::Bt709,
            Ok(v) if v == MFVideoTransferMatrix_BT601.0 => YuvMatrix::Bt601,
            Ok(v)
                if v == MFVideoTransferMatrix_BT2020_10.0
                    || v == MFVideoTransferMatrix_BT2020_12.0 =>
            {
                YuvMatrix::Bt2020
            }
            _ => YuvMatrix::Unspecified,
        };
        let range = ty.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).map(|v| v as i32);
        color.range = match range {
            Ok(v) if v == MFNominalRange_0_255.0 => YuvRange::Full,
            Ok(v) if v == MFNominalRange_16_235.0 => YuvRange::Limited,
            _ => YuvRange::Unspecified,
        };
    }
    color
}

pub(crate) struct VideoMft {
    mft: IMFTransform,
    tag: &'static str,
    output_provides_samples: bool,
    output_buffer_size: u32,
    out_width: u32,
    out_height: u32,
    default_stride: i32,
    color: ColorInfo,
}

impl VideoMft {
    /// Wrap a created MFT whose input type is already set; negotiates the
    /// NV12 output and starts streaming.
    pub(crate) fn start(mft: IMFTransform, tag: &'static str) -> Result<Self, DecodeError> {
        let mut this = Self {
            mft,
            tag,
            output_provides_samples: false,
            output_buffer_size: 0,
            out_width: 0,
            out_height: 0,
            default_stride: 0,
            color: ColorInfo::default(),
        };
        this.negotiate_output()?;
        // SAFETY: message-only COM calls on the owned MFT; no pointers cross.
        unsafe {
            mf(
                this.mft
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0),
                "BEGIN_STREAMING",
            )?;
            mf(
                this.mft
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0),
                "START_OF_STREAM",
            )?;
        }
        Ok(this)
    }

    pub(crate) fn output_size(&self) -> (u32, u32) {
        (self.out_width, self.out_height)
    }

    fn negotiate_output(&mut self) -> Result<(), DecodeError> {
        // SAFETY: COM calls on the owned MFT; the returned media types are
        // wrapped interfaces whose lifetimes the wrappers manage.
        unsafe {
            let mut index = 0;
            loop {
                let ty = mf(
                    self.mft.GetOutputAvailableType(0, index),
                    "GetOutputAvailableType (NV12 not offered?)",
                )?;
                let subtype = mf(ty.GetGUID(&MF_MT_SUBTYPE), "get output subtype")?;
                if subtype == MFVideoFormat_NV12 {
                    mf(self.mft.SetOutputType(0, &ty, 0), "SetOutputType")?;
                    let size = mf(ty.GetUINT64(&MF_MT_FRAME_SIZE), "get MF_MT_FRAME_SIZE")?;
                    self.out_width = (size >> 32) as u32;
                    self.out_height = size as u32;
                    // The attribute is stored as a UINT32 but defined as
                    // a signed LONG: negative means a bottom-up surface.
                    self.default_stride = ty
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .map(|v| v as i32)
                        .unwrap_or(self.out_width as i32);
                    self.color = parse_output_color(&ty);
                    break;
                }
                index += 1;
            }

            let info = mf(self.mft.GetOutputStreamInfo(0), "GetOutputStreamInfo")?;
            self.output_provides_samples =
                info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_buffer_size = info.cbSize;
            Ok(())
        }
    }

    fn caller_sample(&mut self) -> Result<Option<IMFSample>, DecodeError> {
        if self.output_provides_samples {
            return Ok(None);
        }
        // SAFETY: COM object creation with no raw pointers; the fresh sample
        // and buffer are owned wrappers. A fresh sample per call is the
        // discovered MFT contract (reuse fails on the second frame).
        unsafe {
            let sample = mf(MFCreateSample(), "MFCreateSample (output)")?;
            let buffer = mf(
                MFCreateMemoryBuffer(self.output_buffer_size),
                "MFCreateMemoryBuffer (output)",
            )?;
            mf(sample.AddBuffer(&buffer), "AddBuffer (output)")?;
            Ok(Some(sample))
        }
    }

    /// Copy one decoded sample out as NV12.
    ///
    /// The two lock paths are not equally bounded. `Lock` reports the
    /// mapped length, so the extent check has a real ceiling to test the
    /// strided read against. `Lock2D` reports none — `mapped_len` answers
    /// `None` and `check_extent` accepts any extent — so on that path the
    /// stride check is the whole of what stands between a decoder's stated
    /// pitch and the read. That is the design and not an oversight: the
    /// 2D lock hands back a scanline pointer with no buffer bounds to ask
    /// for. A reader should not assume every path here is length-checked.
    fn copy_frame(&self, sample: &IMFSample) -> Result<Nv12Frame, DecodeError> {
        // SAFETY: Lock2DSize/Lock2D/Lock expose a buffer valid until the
        // matching unlock; the stride is checked forwards and at least a
        // row wide before it reaches copy_nv12, the read it will perform
        // is checked against the length the buffer maps wherever the
        // buffer states one, and the destination slice is sized for
        // the packed frame up front.
        unsafe {
            let pts_us = sample.GetSampleTime().map(|t| t / 10).unwrap_or(0);
            let width = self.out_width as usize;
            let height = self.out_height as usize;
            let mut data = vec![0u8; packed_nv12_len(self.tag, width, height)?];

            let buffer = mf(sample.GetBufferByIndex(0), "GetBufferByIndex")?;
            if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
                let mut scanline0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                // Lock2DSize also reports where the mapping starts and how
                // long it is, which is what bounds the copy below. Lock2D
                // is the fallback for a buffer without the newer interface
                // and leaves only the stride to go on.
                let sized = buf2d.cast::<IMF2DBuffer2>().ok();
                let mut start = std::ptr::null_mut();
                let mut len = 0u32;
                match &sized {
                    Some(buf2d2) => mf(
                        buf2d2.Lock2DSize(
                            MF2DBuffer_LockFlags_Read,
                            &mut scanline0,
                            &mut pitch,
                            &mut start,
                            &mut len,
                        ),
                        "Lock2DSize",
                    )?,
                    None => mf(buf2d.Lock2D(&mut scanline0, &mut pitch), "Lock2D")?,
                }
                let mapping = sized.is_some().then_some((start, len));
                let checked = mapped_len(self.tag, scanline0, mapping).and_then(|available| {
                    checked_pitch(self.tag, pitch, width)
                        .and_then(|pitch| check_extent(self.tag, pitch, width, height, available))
                });
                match checked {
                    Ok(pitch) => copy_nv12(scanline0, pitch, width, height, &mut data),
                    Err(e) => {
                        let _ = buf2d.Unlock2D();
                        return Err(e);
                    }
                }
                mf(buf2d.Unlock2D(), "Unlock2D")?;
            } else {
                let mut ptr = std::ptr::null_mut();
                let mut max = 0u32;
                let mut current = 0u32;
                mf(
                    buffer.Lock(&mut ptr, Some(&mut max), Some(&mut current)),
                    "buffer Lock",
                )?;
                // MF_MT_DEFAULT_STRIDE is under-reported by some decoders,
                // so the row width stays the floor here; what bounds the
                // copy is the decoded length rather than the capacity the
                // buffer was created with, which is the larger of the two
                // and covers bytes this sample never wrote. A buffer
                // claiming more valid data than it maps has contradicted
                // itself, and taking that claim is the whole of what this
                // bound exists to refuse.
                let checked = (current <= max)
                    .then_some(current as usize)
                    .ok_or_else(|| {
                        DecodeError(format!(
                            "{}: NV12 sample states {current} valid bytes in a \
                             {max}-byte buffer",
                            self.tag
                        ))
                    })
                    .and_then(|valid| {
                        forward_stride(self.tag, self.default_stride)
                            .map(|stride| stride.max(width))
                            .and_then(|pitch| {
                                check_extent(self.tag, pitch, width, height, Some(valid))
                            })
                    });
                match checked {
                    Ok(pitch) => copy_nv12(ptr, pitch, width, height, &mut data),
                    Err(e) => {
                        let _ = buffer.Unlock();
                        return Err(e);
                    }
                }
                mf(buffer.Unlock(), "buffer Unlock")?;
            }

            Ok(Nv12Frame {
                width: self.out_width,
                height: self.out_height,
                pts_us,
                color: self.color,
                data,
            })
        }
    }

    pub(crate) fn submit(&mut self, au: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        // SAFETY: the input buffer is created with au.len() bytes and locked
        // before the copy_nonoverlapping of exactly au.len() bytes; all
        // interface pointers are owned wrappers.
        unsafe {
            let sample = mf(MFCreateSample(), "MFCreateSample (input)")?;
            let buffer = mf(
                MFCreateMemoryBuffer(au.len() as u32),
                "MFCreateMemoryBuffer (input)",
            )?;
            let mut ptr = std::ptr::null_mut();
            mf(buffer.Lock(&mut ptr, None, None), "input Lock")?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
            mf(buffer.Unlock(), "input Unlock")?;
            mf(buffer.SetCurrentLength(au.len() as u32), "SetCurrentLength")?;
            mf(sample.AddBuffer(&buffer), "AddBuffer (input)")?;
            mf(sample.SetSampleTime(pts_us * 10), "SetSampleTime")?;

            match self.mft.ProcessInput(0, &sample, 0) {
                Ok(()) => Ok(SubmitOutcome::Accepted),
                Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(SubmitOutcome::NotAccepting),
                Err(e) => Err(DecodeError(format!("ProcessInput ({}): {e}", self.tag))),
            }
        }
    }

    pub(crate) fn try_output(&mut self) -> Result<Option<Nv12Frame>, DecodeError> {
        // SAFETY: ProcessOutput's out-parameter struct is built here with
        // ManuallyDrop-wrapped COM pointers; both are reclaimed via
        // ManuallyDrop::take on every path after the call, so references are
        // neither leaked nor double-released.
        unsafe {
            loop {
                let sample = self.caller_sample()?;
                let mut out = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let result = self
                    .mft
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
                // Reclaim COM references stashed in the ManuallyDrop fields.
                let sample = ManuallyDrop::take(&mut out.pSample);
                drop(ManuallyDrop::take(&mut out.pEvents));

                match result {
                    Ok(()) => {
                        let sample = sample.ok_or_else(|| {
                            DecodeError("ProcessOutput returned no sample".into())
                        })?;
                        return Ok(Some(self.copy_frame(&sample)?));
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.negotiate_output()?;
                        continue;
                    }
                    Err(e) => {
                        return Err(DecodeError(format!("ProcessOutput ({}): {e}", self.tag)));
                    }
                }
            }
        }
    }

    pub(crate) fn begin_drain(&mut self) -> Result<(), DecodeError> {
        // SAFETY: message-only COM calls on the owned MFT; no pointers cross.
        unsafe {
            mf(
                self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0),
                "END_OF_STREAM",
            )?;
            mf(
                self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0),
                "COMMAND_DRAIN",
            )?;
            Ok(())
        }
    }

    pub(crate) fn reset(&mut self) -> Result<(), DecodeError> {
        // SAFETY: message-only COM calls on the owned MFT; no pointers cross.
        unsafe {
            mf(
                self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0),
                "COMMAND_FLUSH",
            )?;
            mf(
                self.mft
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0),
                "START_OF_STREAM",
            )?;
            Ok(())
        }
    }
}

/// Media Foundation reports strides as signed LONGs, and a negative one
/// means the surface is stored bottom-up — `copy_nv12` reads rows forwards
/// from the first scanline and cannot follow that. Refuse it rather than
/// casting it into a huge unsigned offset.
fn forward_stride(tag: &str, pitch: i32) -> Result<usize, DecodeError> {
    usize::try_from(pitch)
        .map_err(|_| DecodeError(format!("{tag}: bottom-up NV12 surface (stride {pitch})")))
}

/// As [`forward_stride`], and additionally a stride shorter than a row
/// cannot cover the copy — where the buffer states its own pitch there is
/// nothing to clamp it to, so refuse that too.
fn checked_pitch(tag: &str, pitch: i32, width: usize) -> Result<usize, DecodeError> {
    match forward_stride(tag, pitch)? {
        checked if checked >= width => Ok(checked),
        checked => Err(DecodeError(format!(
            "{tag}: NV12 stride {checked} is shorter than the {width}-byte row"
        ))),
    }
}

/// Check the strided read `copy_nv12` will perform against the length the
/// buffer actually mapped, where the buffer reports one.
fn check_extent(
    tag: &str,
    pitch: usize,
    width: usize,
    height: usize,
    available: Option<usize>,
) -> Result<usize, DecodeError> {
    let Some(needed) = nv12_extent(pitch, width, height) else {
        return Err(DecodeError(format!(
            "{tag}: NV12 geometry {width}x{height} at stride {pitch} overflows"
        )));
    };
    match available {
        Some(available) if available < needed => Err(DecodeError(format!(
            "{tag}: NV12 buffer maps {available} bytes, {needed} needed for \
             {width}x{height} at stride {pitch}"
        ))),
        _ => Ok(pitch),
    }
}

/// Bytes `copy_nv12` reads from `src` for a strided NV12 image. The last
/// chroma row ends the read, and it stops at `width` rather than at the
/// end of its stride. `None` when the arithmetic overflows.
fn nv12_extent(pitch: usize, width: usize, height: usize) -> Option<usize> {
    if height == 0 {
        return Some(0);
    }
    let uv_rows = height / 2;
    let last_row_start = if uv_rows > 0 {
        pitch
            .checked_mul(height)?
            .checked_add(pitch.checked_mul(uv_rows - 1)?)?
    } else {
        pitch.checked_mul(height - 1)?
    };
    last_row_start.checked_add(width)
}

/// How much of a mapped buffer sits at or after `scanline0`.
///
/// `mapping` is the buffer's own start and length where the lock reported
/// them, and `None` for a lock that states no mapping at all — `Lock2D`
/// gives back a stride and nothing else, so there is nothing to check the
/// copy against and `Ok(None)` says so. A mapping the scanline is *not*
/// inside is a different thing: the buffer has described itself
/// incoherently, and treating that as "no information" would quietly drop
/// the length check, so it refuses.
fn mapped_len(
    tag: &str,
    scanline0: *mut u8,
    mapping: Option<(*mut u8, u32)>,
) -> Result<Option<usize>, DecodeError> {
    let Some((start, len)) = mapping else {
        return Ok(None);
    };
    let outside = || {
        DecodeError(format!(
            "{tag}: locked scanline lies outside its own mapping"
        ))
    };
    if start.is_null() || scanline0.is_null() {
        return Err(outside());
    }
    // Unsigned: the same answer where the scanline is at or after the
    // mapping's start, and a refusal rather than an overflowing
    // subtraction where it is not. Signed, the most incoherent pair a
    // buffer could state is the one that panics a debug build instead of
    // being refused, which is the whole job here.
    let offset = (scanline0 as usize)
        .checked_sub(start as usize)
        .ok_or_else(outside)?;
    (len as usize)
        .checked_sub(offset)
        .map(Some)
        .ok_or_else(outside)
}

/// Copy a strided NV12 image into a tightly packed buffer (Y then UV).
///
/// # Safety
/// - `src` must be readable for [`nv12_extent`]`(pitch, width, height)`
///   bytes — the extent the strided read touches, which ends the last
///   chroma row at `width` rather than at the end of its stride.
/// - `pitch` must be at least `width`.
/// - `dst` must hold at least [`packed_nv12_len`]`(width, height)` bytes.
unsafe fn copy_nv12(src: *mut u8, pitch: usize, width: usize, height: usize, dst: &mut [u8]) {
    // SAFETY: the extent clause above covers the strided read and
    // `pitch >= width` holds each row inside its own stride, so every source
    // read lands in the mapping; the packed writes advance `width` a row and
    // the last one ends at `dst`'s stated length.
    unsafe {
        for row in 0..height {
            std::ptr::copy_nonoverlapping(
                src.add(row * pitch),
                dst.as_mut_ptr().add(row * width),
                width,
            );
        }
        let uv_src = src.add(pitch * height);
        let uv_dst = dst.as_mut_ptr().add(width * height);
        for row in 0..height / 2 {
            std::ptr::copy_nonoverlapping(uv_src.add(row * pitch), uv_dst.add(row * width), width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_extent, checked_pitch, forward_stride, mapped_len, nv12_extent, packed_nv12_len,
    };

    const TAG: &str = "test";

    #[test]
    fn a_bottom_up_stride_is_refused_rather_than_wrapped() {
        assert!(forward_stride(TAG, -1920).is_err());
        assert!(checked_pitch(TAG, -1920, 1920).is_err());
        assert_eq!(forward_stride(TAG, 1920).ok(), Some(1920));
    }

    #[test]
    fn a_stride_shorter_than_a_row_is_refused() {
        assert!(checked_pitch(TAG, 1919, 1920).is_err());
        assert_eq!(checked_pitch(TAG, 1920, 1920).ok(), Some(1920));
        assert_eq!(checked_pitch(TAG, 2048, 1920).ok(), Some(2048));
    }

    #[test]
    fn the_extent_is_the_last_chroma_row_not_a_whole_plane() {
        // 4x4 at stride 8: Y rows 0..4 then two chroma rows, the last of
        // which is read for `width` bytes only.
        assert_eq!(nv12_extent(8, 4, 4), Some(8 * 4 + 8 + 4));
        assert_eq!(nv12_extent(4, 4, 0), Some(0));
        assert_eq!(nv12_extent(usize::MAX, 4, 4), None);
    }

    #[test]
    fn the_packed_destination_is_the_two_planes_copy_nv12_writes() {
        // Y in full, chroma at half height.
        assert_eq!(packed_nv12_len(TAG, 4, 4).ok(), Some(16 + 8));
        assert_eq!(
            packed_nv12_len(TAG, 1920, 1080).ok(),
            Some(1920 * 1080 * 3 / 2)
        );
    }

    #[test]
    fn an_odd_dimension_is_refused_rather_than_rounded() {
        // NV12's chroma is exactly half the luma in each axis, so an odd
        // dimension has no representation in it. Rounding down returns a
        // length that fits a copy running half-height rows and silently
        // drops the bottom row of the picture; rounding up returns one a
        // caller writing the other way overruns. Neither is a size this
        // can answer with, on either axis or both.
        for (width, height) in [(4, 3), (3, 4), (3, 3)] {
            assert!(
                packed_nv12_len(TAG, width, height).is_err(),
                "{width}x{height} has no NV12 representation and must refuse"
            );
        }
        // The even neighbours on each side still answer.
        assert!(packed_nv12_len(TAG, 4, 2).is_ok());
        assert!(packed_nv12_len(TAG, 4, 4).is_ok());
        assert!(packed_nv12_len(TAG, 2, 4).is_ok());
    }

    #[test]
    fn a_frame_size_whose_planes_overflow_is_refused_before_it_allocates() {
        // The wrap this refuses allocates short and leaves the copy
        // writing the geometry it was given past the end of it. Even on
        // both axes, so it is the product that refuses these and not the
        // representability check above.
        assert!(packed_nv12_len(TAG, usize::MAX / 2 + 1, 2).is_err());
        assert!(packed_nv12_len(TAG, usize::MAX / 3 + 1, 2).is_err());
        // And a geometry far larger than any real frame, but inside the
        // type, still answers.
        assert_eq!(
            packed_nv12_len(TAG, 1 << 20, 1 << 20).ok(),
            Some((1 << 40) + (1 << 39))
        );
    }

    #[test]
    fn a_buffer_shorter_than_the_strided_read_is_refused() {
        let needed = nv12_extent(8, 4, 4).expect("extent");
        assert!(check_extent(TAG, 8, 4, 4, Some(needed - 1)).is_err());
        assert_eq!(check_extent(TAG, 8, 4, 4, Some(needed)).ok(), Some(8));
        // No stated length: the stride check is all there is to go on.
        assert_eq!(check_extent(TAG, 8, 4, 4, None).ok(), Some(8));
    }

    #[test]
    fn the_mapped_length_is_measured_from_the_first_scanline() {
        let start = 0x1000usize as *mut u8;
        // A scanline inside the mapping leaves the remainder.
        let inside = mapped_len(TAG, start.wrapping_add(64), Some((start, 256)));
        assert_eq!(inside.ok(), Some(Some(192)));
        assert_eq!(
            mapped_len(TAG, start, Some((start, 256))).ok(),
            Some(Some(256))
        );
    }

    #[test]
    fn an_address_pair_too_far_apart_to_subtract_is_refused_not_panicked() {
        // One from each half of the address space. Their difference does
        // not fit in a signed word, so taking it that way overflows —
        // and the most incoherent mapping a buffer could state is the
        // one input this function exists to refuse, which makes a debug
        // build's panic on it exactly the wrong answer.
        let start = (usize::MAX / 2 + 1) as *mut u8;
        let scanline0 = (usize::MAX / 2) as *mut u8;
        assert!(mapped_len(TAG, scanline0, Some((start, 256))).is_err());
    }

    #[test]
    fn a_scanline_outside_its_mapping_is_refused_not_ignored() {
        let start = 0x1000usize as *mut u8;
        // Before the mapping, past its end, or no mapping start at all:
        // the buffer has contradicted itself, so the copy cannot go ahead
        // on the stride check alone.
        for scanline0 in [start.wrapping_sub(1), start.wrapping_add(300)] {
            assert!(mapped_len(TAG, scanline0, Some((start, 256))).is_err());
        }
        assert!(mapped_len(TAG, start, Some((core::ptr::null_mut(), 256))).is_err());
        assert!(mapped_len(TAG, core::ptr::null_mut(), Some((start, 256))).is_err());
        // A lock that states no mapping is not the same claim: there is
        // simply nothing to check against.
        assert_eq!(mapped_len(TAG, start, None).ok(), Some(None));
    }
}
