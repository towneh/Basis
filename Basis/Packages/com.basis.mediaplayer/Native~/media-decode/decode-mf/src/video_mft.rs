//! The shared sync-MFT driver for the video decoders: NV12 output
//! negotiation (matrix/range re-read on every stream change, §6.8), the
//! fresh-sample-per-call output contract, strided copies and the
//! drain/flush protocol are identical across codecs — only the input type
//! configured by the adapter differs.

use media_decode::{ColorInfo, DecodeError, Nv12Frame, SubmitOutcome, YuvMatrix, YuvRange};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFNominalRange_0_255,
    MFNominalRange_16_235, MFT_CATEGORY_VIDEO_DECODER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
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
    default_stride: u32,
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
                    self.default_stride = ty
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .unwrap_or(self.out_width);
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

    fn copy_frame(&self, sample: &IMFSample) -> Result<Nv12Frame, DecodeError> {
        // SAFETY: Lock2D/Lock expose a buffer valid until the matching unlock;
        // copy_nv12 reads at most height rows of `pitch` bytes from it, which
        // is within the locked allocation for an NV12 frame of the negotiated
        // size, and the destination slice is sized width*height*3/2 up front.
        unsafe {
            let pts_us = sample.GetSampleTime().map(|t| t / 10).unwrap_or(0);
            let width = self.out_width as usize;
            let height = self.out_height as usize;
            let mut data = vec![0u8; width * height * 3 / 2];

            let buffer = mf(sample.GetBufferByIndex(0), "GetBufferByIndex")?;
            if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
                let mut scanline0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                mf(buf2d.Lock2D(&mut scanline0, &mut pitch), "Lock2D")?;
                let pitch = pitch as usize;
                copy_nv12(scanline0, pitch, width, height, &mut data);
                mf(buf2d.Unlock2D(), "Unlock2D")?;
            } else {
                let mut ptr = std::ptr::null_mut();
                let mut current = 0u32;
                mf(
                    buffer.Lock(&mut ptr, None, Some(&mut current)),
                    "buffer Lock",
                )?;
                let stride = self.default_stride.max(self.out_width) as usize;
                copy_nv12(ptr, stride, width, height, &mut data);
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

/// Copy a strided NV12 image into a tightly packed buffer (Y then UV).
// SAFETY: caller guarantees `src` points to a locked NV12 image of at least
// `pitch * height * 3 / 2` bytes with `pitch >= width`, and `dst` holds
// `width * height * 3 / 2` bytes; every row copy below stays inside
// both bounds.
unsafe fn copy_nv12(src: *mut u8, pitch: usize, width: usize, height: usize, dst: &mut [u8]) {
    // SAFETY: per the function contract above — src covers pitch*height*3/2
    // bytes, dst covers width*height*3/2, pitch >= width, so every row copy
    // stays in bounds on both sides.
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
