//! DXVA hardware decode (§6.7, phase 1): the same sync-MFT driving
//! model as `video_mft`, bound to a D3D11 device through the DXGI device
//! manager, so the decoder allocates NV12 texture-array slices GPU-side
//! and output never touches system memory. Frames leave as
//! `VideoFrame::Opaque` whose payload owns the `IMFSample` — dropping it
//! returns the surface to the MFT's pool, which is the release
//! discipline (an unreleased output sample drains the pool and
//! `ProcessOutput` returns `NEED_MORE_INPUT` forever).
//!
//! The hardware claim is two-legged: a decoder MFT must enumerate for
//! the subtype *and* `ID3D11VideoDevice` must report the DXVA profile
//! with NV12 output and a decoder configuration at the target
//! resolution — the Store VP9/AV1 extensions pass enumeration and then
//! decode on the CPU internally on GPUs without the profile. The runtime
//! backstop for probe false-positives is an output sample without DXGI
//! backing: reported through [`media_decode::VideoDecoder::hardware_fell_back`]
//! so the engine reroutes to the software path instead of failing.

use media_decode::{
    ColorInfo, DecodeError, OpaqueFrame, OpaqueImage, SubmitOutcome, VideoFrame, packed_nv12_len,
};
use media_diag::diag_warn;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11_VIDEO_DECODER_DESC, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MFCreateDXGIDeviceManager, MFCreateMemoryBuffer,
    MFCreateSample, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoArea, MFVideoFormat_AV1, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_VP90, MFVideoInterlace_Progressive,
};
use windows::core::{GUID, Interface};

use crate::video_mft::{create_decoder_for, parse_output_color};
use crate::{mf, mf_startup, video_input_type};
use std::mem::ManuallyDrop;

/// Setting this environment variable (to any value) makes every hardware
/// probe report absent, so sessions take the software rung with a
/// `DecodeFallbackHwToSw` diagnostic — the forced-fallback test lever and
/// a field escape hatch for broken drivers.
pub const DISABLE_HW_DECODE_ENV: &str = "BASIS_MEDIA_DISABLE_HW_DECODE";

fn hw_disabled() -> bool {
    std::env::var_os(DISABLE_HW_DECODE_ENV).is_some()
}

/// D3D11 decoder-profile GUIDs, defined locally so SDK header vintage
/// never gates the build (the values are the documented DXVA profile
/// GUIDs; 8-bit profile 0 only for VP9/AV1 — 10-bit is deliberately
/// unprobed, P010 is not negotiated in Phase 1).
const PROFILE_H264_VLD_NOFGT: GUID = GUID::from_values(
    0x1b81be68,
    0xa0c7,
    0x11d3,
    [0xb9, 0x84, 0x00, 0xc0, 0x4f, 0x2e, 0x73, 0xc5],
);
const PROFILE_HEVC_VLD_MAIN: GUID = GUID::from_values(
    0x5b11d51b,
    0x2f4c,
    0x4452,
    [0xbc, 0xc3, 0x09, 0xf2, 0xa1, 0x16, 0x0c, 0xc0],
);
const PROFILE_VP9_PROFILE0: GUID = GUID::from_values(
    0x463707f8,
    0xa1d0,
    0x4585,
    [0x87, 0x6d, 0x83, 0xaa, 0x6d, 0x60, 0xb8, 0x9e],
);
const PROFILE_AV1_PROFILE0: GUID = GUID::from_values(
    0xb8be4ccb,
    0xcf53,
    0x46ba,
    [0x8d, 0x59, 0xd6, 0xb8, 0xa6, 0xda, 0x5d, 0x2a],
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwCodec {
    H264,
    H265,
    Vp9,
    Av1,
}

impl HwCodec {
    fn subtype(self) -> &'static GUID {
        match self {
            HwCodec::H264 => &MFVideoFormat_H264,
            HwCodec::H265 => &MFVideoFormat_HEVC,
            HwCodec::Vp9 => &MFVideoFormat_VP90,
            HwCodec::Av1 => &MFVideoFormat_AV1,
        }
    }

    fn profile(self) -> &'static GUID {
        match self {
            HwCodec::H264 => &PROFILE_H264_VLD_NOFGT,
            HwCodec::H265 => &PROFILE_HEVC_VLD_MAIN,
            HwCodec::Vp9 => &PROFILE_VP9_PROFILE0,
            HwCodec::Av1 => &PROFILE_AV1_PROFILE0,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            HwCodec::H264 => "dxva-h264",
            HwCodec::H265 => "dxva-hevc",
            HwCodec::Vp9 => "dxva-vp9",
            HwCodec::Av1 => "dxva-av1",
        }
    }
}

/// The decode device: hardware D3D11 with video support, multithread
/// protection (the MFT's internal workers share it), and the DXGI device
/// manager that binds it to the MFT.
struct DxvaDevice {
    device: ID3D11Device,
    _context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
    video: ID3D11VideoDevice,
}

impl DxvaDevice {
    fn new() -> Result<Self, DecodeError> {
        // SAFETY: D3D11/MF object creation through owned wrappers; every
        // out-param is checked before use and the reset token is plain data.
        unsafe {
            let mut device = None;
            let mut context = None;
            mf(
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
                "D3D11CreateDevice (decode)",
            )?;
            let device = device.ok_or_else(|| DecodeError("no decode device".into()))?;
            let context = context.ok_or_else(|| DecodeError("no decode context".into()))?;
            // The MFT decodes from its own worker threads while the video
            // thread and the render event use the same device/context.
            let multithread: ID3D11Multithread = mf(context.cast(), "cast ID3D11Multithread")?;
            // Returns the previous state, not a failure signal.
            let _ = multithread.SetMultithreadProtected(true);

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            mf(
                MFCreateDXGIDeviceManager(&mut token, &mut manager),
                "MFCreateDXGIDeviceManager",
            )?;
            let manager = manager.ok_or_else(|| DecodeError("no DXGI device manager".into()))?;
            mf(manager.ResetDevice(&device, token), "ResetDevice")?;

            let video: ID3D11VideoDevice = mf(device.cast(), "cast ID3D11VideoDevice")?;
            Ok(Self {
                device,
                _context: context,
                manager,
                video,
            })
        }
    }

    /// Leg 2 of the hardware claim: the GPU reports the DXVA profile,
    /// decodes it to NV12, and offers at least one decoder configuration
    /// at the target resolution.
    fn supports(&self, profile: &GUID, width: u32, height: u32) -> bool {
        // SAFETY: COM calls on the owned video device; the decoder desc is
        // a plain struct and every out-param is checked.
        unsafe {
            let count = self.video.GetVideoDecoderProfileCount();
            let listed = (0..count).any(|i| {
                self.video
                    .GetVideoDecoderProfile(i)
                    .map(|g| g == *profile)
                    .unwrap_or(false)
            });
            if !listed {
                return false;
            }
            if !self
                .video
                .CheckVideoDecoderFormat(profile, DXGI_FORMAT_NV12)
                .map(|supported| supported.as_bool())
                .unwrap_or(false)
            {
                return false;
            }
            let desc = D3D11_VIDEO_DECODER_DESC {
                Guid: *profile,
                SampleWidth: width,
                SampleHeight: height,
                OutputFormat: DXGI_FORMAT_NV12,
            };
            self.video
                .GetVideoDecoderConfigCount(&desc)
                .map(|configs| configs > 0)
                .unwrap_or(false)
        }
    }
}

/// Both legs of the hardware claim for a codec at a resolution, on a
/// transient device (the capability probe can run with no session open).
/// [`DISABLE_HW_DECODE_ENV`] forces absent.
pub fn probe_hardware(codec: HwCodec, width: u32, height: u32) -> bool {
    if hw_disabled() || mf_startup().is_err() {
        return false;
    }
    let Ok(device) = DxvaDevice::new() else {
        return false;
    };
    device.supports(codec.profile(), width, height)
        && create_decoder_for(
            codec.subtype(),
            codec.tag(),
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )
        .is_ok()
}

/// The measured resolution ceiling for the hardware route: the highest
/// rung of a 1080p → 4K → 8K ladder the GPU offers a decoder
/// configuration at (§6.11 — honest numbers for the resolver to rank
/// on). `None` = no hardware route for the codec at all.
pub fn probe_hardware_ceiling(codec: HwCodec) -> Option<(u32, u32)> {
    if hw_disabled() || mf_startup().is_err() {
        return None;
    }
    let device = DxvaDevice::new().ok()?;
    if create_decoder_for(
        codec.subtype(),
        codec.tag(),
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
    )
    .is_err()
    {
        return None;
    }
    let ladder = [(1920u32, 1088u32), (3840, 2160), (7680, 4320)];
    let mut best = None;
    for (w, h) in ladder {
        if device.supports(codec.profile(), w, h) {
            best = Some((w, h));
        }
    }
    best
}

/// Read a DXVA opaque frame back to packed NV12 — the conformance-test
/// oracle (hardware decode is bit-exact against the software route, so
/// the readback hashes must match). The play path never touches the CPU;
/// this exists for tests and diagnostics only.
pub fn read_back_nv12(frame: &OpaqueFrame) -> Result<media_decode::Nv12Frame, DecodeError> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    let Some((texture_ptr, subresource)) = frame.image.d3d11_slice() else {
        return Err(DecodeError("not a DXVA frame".into()));
    };
    // SAFETY: the payload guarantees a live texture for its lifetime;
    // from_raw_borrowed does not consume its reference. The staging map
    // spans RowPitch*height*3/2 bytes (the D3D11 planar layout) and every
    // row copy stays inside it and the destination.
    unsafe {
        let texture = ID3D11Texture2D::from_raw_borrowed(&texture_ptr)
            .ok_or_else(|| DecodeError("null slice texture".into()))?;
        let device = mf(texture.GetDevice(), "GetDevice")?;
        let context = mf(device.GetImmediateContext(), "GetImmediateContext")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let staging_desc = D3D11_TEXTURE2D_DESC {
            ArraySize: 1,
            MipLevels: 1,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        mf(
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging)),
            "CreateTexture2D (readback)",
        )?;
        let staging = staging.ok_or_else(|| DecodeError("no readback staging".into()))?;
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, subresource, None);
        let (w, h) = (frame.width as usize, frame.height as usize);
        // Sized before the map rather than inside it: a fallible step
        // between `Map` and `Unmap` returns with the staging texture
        // still mapped, and it is then dropped in that state. The lock
        // paths in the software route keep the same discipline.
        let packed = packed_nv12_len("dxva readback", w, h)?;
        let mut mapped = Default::default();
        mf(
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)),
            "Map (readback)",
        )?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        // The strided read is bounded by the mapping, not by the
        // destination `packed` sizes: the row copies read `w` bytes at
        // `row * pitch`, and the chroma copies start at
        // `pitch * desc.Height`, so a pitch narrower than the row or a
        // texture shorter than the frame reads outside it. The software
        // route checks the same shape before its own copy.
        if base.is_null() || pitch < w || (desc.Height as usize) < h {
            // Unmapped before the return: a fallible step inside the
            // mapped window is what leaves the staging texture mapped
            // when it drops.
            context.Unmap(&staging, 0);
            return Err(DecodeError(format!(
                "dxva readback: {w}x{h} does not fit stride {pitch} in a {}-row texture",
                desc.Height
            )));
        }
        let mut data = vec![0u8; packed];
        for row in 0..h {
            std::ptr::copy_nonoverlapping(base.add(row * pitch), data.as_mut_ptr().add(row * w), w);
        }
        let uv_base = base.add(pitch * desc.Height as usize);
        let uv_dst = data.as_mut_ptr().add(w * h);
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(uv_base.add(row * pitch), uv_dst.add(row * w), w);
        }
        context.Unmap(&staging, 0);
        Ok(media_decode::Nv12Frame {
            width: frame.width,
            height: frame.height,
            pts_us: frame.pts_us,
            color: frame.color,
            data,
        })
    }
}

/// A decoded DXVA frame: the MFT's own output sample plus the resolved
/// texture-array slice. Dropping it releases the sample, returning the
/// surface to the MFT's pool.
struct DxvaImage {
    sample: IMFSample,
    texture: ID3D11Texture2D,
    subresource: u32,
}

// SAFETY: the wrapped D3D11 interfaces are free-threaded and IMFSample is
// an agile MF object; the payload only crosses threads whole (video
// thread → render event via the FramePool), never shared mutably.
unsafe impl Send for DxvaImage {}

impl OpaqueImage for DxvaImage {
    fn hardware_buffer(&self) -> *mut core::ffi::c_void {
        self.sample.as_raw()
    }

    fn d3d11_slice(&self) -> Option<(*mut core::ffi::c_void, u32)> {
        Some((self.texture.as_raw(), self.subresource))
    }
}

/// Hardware video decoder: one type for every DXVA codec — only the
/// subtype/profile pair and the AV1 config-OBU carriage differ.
pub struct HwVideoDecoder {
    mft: IMFTransform,
    dxva: DxvaDevice,
    tag: &'static str,
    out_width: u32,
    out_height: u32,
    default_stride: u32,
    color: ColorInfo,
    /// Clean aperture from `MF_MT_MINIMUM_DISPLAY_APERTURE`, re-read at
    /// every stream change (many decoders only populate it then).
    aperture: Option<(i32, i32, u32, u32)>,
    /// AV1 config OBUs held until the first accepted input: they ride the
    /// first real AU (a duplicated sequence header is legal OBU syntax; a
    /// config-only sample is of unverified MFT tolerance), cleared only
    /// once `ProcessInput` consumed the carrier.
    config_obus: Vec<u8>,
    /// Post-reset output gate: the MFT's reorder pipeline can emit a
    /// garbage frame after a flush; anything with a pts before the first
    /// post-reset submission is dropped. Bounded so a re-stamped output
    /// can never hold video shut.
    output_floor_pts: Option<i64>,
    floor_armed: bool,
    floor_dropped: u32,
    fell_back: bool,
}

// SAFETY: owned COM interfaces on free-threaded MF/D3D11 objects; the
// decoder is driven by one video thread at a time.
unsafe impl Send for HwVideoDecoder {}

const FLOOR_DROP_BOUND: u32 = 16;

impl HwVideoDecoder {
    /// `width`/`height` are the container-stated coded dimensions,
    /// required: the input type always states `MF_MT_FRAME_SIZE` (the
    /// Store HEVC MFT null-derefs its worker thread when data arrives on
    /// a sizeless input type, so sizeless streams refuse before the MFT
    /// is ever configured). `config` carries AV1 config OBUs (empty for
    /// every other codec, and for AV1 streams whose sequence header rides
    /// in-band).
    pub fn new(
        codec: HwCodec,
        width: u32,
        height: u32,
        config: &[u8],
    ) -> Result<Self, DecodeError> {
        if hw_disabled() {
            return Err(DecodeError(format!(
                "hardware decode disabled ({DISABLE_HW_DECODE_ENV})"
            )));
        }
        if width == 0 || height == 0 {
            return Err(DecodeError(
                "video track announced no frame size, so the hardware decoder cannot be configured"
                    .into(),
            ));
        }
        mf_startup()?;
        let dxva = DxvaDevice::new()?;
        if !dxva.supports(codec.profile(), width, height) {
            return Err(DecodeError(format!(
                "GPU does not hardware-decode {} at {width}x{height}",
                codec.tag()
            )));
        }
        // SAFETY: COM calls through owned wrappers after mf_startup; the
        // device-manager pointer passed as ProcessMessage's ULONG_PTR is
        // the live manager the struct keeps alive.
        unsafe {
            let mft = create_decoder_for(
                codec.subtype(),
                codec.tag(),
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            )?;
            // Bind the device manager before the input/output types (the
            // C-proven ordering). A refusal is logged, not fatal: the
            // output would then lack DXGI backing and the runtime
            // fallback signal reroutes to software.
            if let Err(e) =
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dxva.manager.as_raw() as usize)
            {
                diag_warn!("{}: SET_D3D_MANAGER refused: {e}", codec.tag());
            }
            let input = video_input_type(codec.subtype(), Some((width, height)))?;
            mf(
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32),
                "set interlace mode",
            )?;
            mf(mft.SetInputType(0, &input, 0), "SetInputType (dxva)")?;

            let mut this = Self {
                mft,
                dxva,
                tag: codec.tag(),
                out_width: 0,
                out_height: 0,
                default_stride: 0,
                color: ColorInfo::default(),
                aperture: None,
                config_obus: config.to_vec(),
                output_floor_pts: None,
                floor_armed: false,
                floor_dropped: 0,
                fell_back: false,
            };
            this.negotiate_output()?;
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
            Ok(this)
        }
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.out_width, self.out_height)
    }

    /// The clean-aperture rect `(x, y, w, h)` the MFT states, when it
    /// states one; refreshed at every stream change.
    pub fn display_aperture(&self) -> Option<(i32, i32, u32, u32)> {
        self.aperture
    }

    /// The decode device, for the presenter to share (`ID3D11Device*`,
    /// valid while this decoder is alive; the callee clones its own
    /// reference).
    pub fn device_raw(&self) -> *mut core::ffi::c_void {
        self.dxva.device.as_raw()
    }

    fn negotiate_output(&mut self) -> Result<(), DecodeError> {
        // SAFETY: COM calls on the owned MFT; returned media types are
        // owned wrappers.
        unsafe {
            let mut index = 0;
            loop {
                let ty = mf(
                    self.mft.GetOutputAvailableType(0, index),
                    "GetOutputAvailableType (NV12 not offered?)",
                )?;
                let subtype = mf(ty.GetGUID(&MF_MT_SUBTYPE), "get output subtype")?;
                if subtype == MFVideoFormat_NV12 {
                    mf(self.mft.SetOutputType(0, &ty, 0), "SetOutputType (dxva)")?;
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
            self.read_aperture();
            Ok(())
        }
    }

    /// Re-read `MF_MT_MINIMUM_DISPLAY_APERTURE` from the current output
    /// type. Called at configure *and* every stream change — many
    /// decoders only populate it after the first-frame stream change.
    fn read_aperture(&mut self) {
        self.aperture = None;
        // SAFETY: GetBlob writes at most the passed buffer's length; the
        // buffer is sized to MFVideoArea and read back with
        // read_unaligned only on success with the full size reported.
        unsafe {
            let Ok(current) = self.mft.GetOutputCurrentType(0) else {
                return;
            };
            let mut raw = [0u8; std::mem::size_of::<MFVideoArea>()];
            let mut written = 0u32;
            if current
                .GetBlob(
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    &mut raw,
                    Some(&mut written),
                )
                .is_ok()
                && written as usize == raw.len()
            {
                let area: MFVideoArea = std::ptr::read_unaligned(raw.as_ptr() as *const _);
                if area.Area.cx > 0 && area.Area.cy > 0 {
                    self.aperture = Some((
                        area.OffsetX.value as i32,
                        area.OffsetY.value as i32,
                        area.Area.cx as u32,
                        area.Area.cy as u32,
                    ));
                }
            }
        }
    }

    fn resolve_frame(&mut self, sample: IMFSample) -> Result<VideoFrame, DecodeError> {
        // SAFETY: COM calls on the owned sample; GetResource's out-param
        // is checked before use.
        unsafe {
            let pts_us = sample.GetSampleTime().map(|t| t / 10).unwrap_or(0);
            let buffer = mf(sample.GetBufferByIndex(0), "GetBufferByIndex (dxva)")?;
            let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else {
                // The runtime software-fallback signal: the MFT decoded on
                // the CPU internally (a probe false-positive). The engine
                // reads `hardware_fell_back` and reroutes.
                self.fell_back = true;
                return Err(DecodeError(format!(
                    "{}: decoder produced software frames (no GPU decode path engaged)",
                    self.tag
                )));
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            mf(
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _),
                "IMFDXGIBuffer::GetResource",
            )?;
            let texture =
                texture.ok_or_else(|| DecodeError("DXGI buffer without texture".into()))?;
            let subresource = mf(dxgi.GetSubresourceIndex(), "GetSubresourceIndex")?;
            Ok(VideoFrame::Opaque(OpaqueFrame {
                width: self.out_width,
                height: self.out_height,
                pts_us,
                color: self.color,
                image: Box::new(DxvaImage {
                    sample,
                    texture,
                    subresource,
                }),
            }))
        }
    }
}

impl media_decode::VideoDecoder for HwVideoDecoder {
    fn submit(&mut self, annexb: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        if self.floor_armed {
            self.output_floor_pts = Some(pts_us);
            self.floor_armed = false;
            self.floor_dropped = 0;
        }
        let carried_config = !self.config_obus.is_empty();
        let payload: std::borrow::Cow<'_, [u8]> = if carried_config {
            let mut joined = Vec::with_capacity(self.config_obus.len() + annexb.len());
            joined.extend_from_slice(&self.config_obus);
            joined.extend_from_slice(annexb);
            std::borrow::Cow::Owned(joined)
        } else {
            std::borrow::Cow::Borrowed(annexb)
        };
        // SAFETY: the input buffer is created with payload.len() bytes and
        // locked before a copy of exactly that many; all interfaces are
        // owned wrappers.
        unsafe {
            let sample = mf(MFCreateSample(), "MFCreateSample (input)")?;
            let buffer = mf(
                MFCreateMemoryBuffer(payload.len() as u32),
                "MFCreateMemoryBuffer (input)",
            )?;
            let mut ptr = std::ptr::null_mut();
            mf(buffer.Lock(&mut ptr, None, None), "input Lock")?;
            std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
            mf(buffer.Unlock(), "input Unlock")?;
            mf(
                buffer.SetCurrentLength(payload.len() as u32),
                "SetCurrentLength",
            )?;
            mf(sample.AddBuffer(&buffer), "AddBuffer (input)")?;
            mf(sample.SetSampleTime(pts_us * 10), "SetSampleTime")?;

            match self.mft.ProcessInput(0, &sample, 0) {
                Ok(()) => {
                    // Drop held config OBUs only once their carrier was
                    // accepted; a NotAccepting retry must re-prepend them.
                    if carried_config {
                        self.config_obus.clear();
                    }
                    Ok(SubmitOutcome::Accepted)
                }
                Err(e) if e.code() == windows::Win32::Media::MediaFoundation::MF_E_NOTACCEPTING => {
                    Ok(SubmitOutcome::NotAccepting)
                }
                Err(e) => Err(DecodeError(format!("ProcessInput ({}): {e}", self.tag))),
            }
        }
    }

    fn try_output(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        // SAFETY: ProcessOutput's out-struct wraps COM pointers in
        // ManuallyDrop; both are reclaimed on every path after the call.
        unsafe {
            loop {
                // The output stream info is re-read every iteration, not
                // cached: PROVIDES_SAMPLES and cbSize can change across a
                // stream change (the C-discovered contract).
                let info = mf(self.mft.GetOutputStreamInfo(0), "GetOutputStreamInfo")?;
                let provides = info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                    != 0;
                let sample = if provides {
                    None
                } else {
                    let sample = mf(MFCreateSample(), "MFCreateSample (output)")?;
                    let buffer = mf(
                        MFCreateMemoryBuffer(info.cbSize),
                        "MFCreateMemoryBuffer (output)",
                    )?;
                    mf(sample.AddBuffer(&buffer), "AddBuffer (output)")?;
                    Some(sample)
                };
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
                // Reclaim COM references on every path (the DXVA sample IS
                // the MFT's pooled surface — leaking one stalls decode).
                let sample = ManuallyDrop::take(&mut out.pSample);
                drop(ManuallyDrop::take(&mut out.pEvents));

                match result {
                    Ok(()) => {
                        let sample = sample.ok_or_else(|| {
                            DecodeError("ProcessOutput returned no sample".into())
                        })?;
                        let frame = self.resolve_frame(sample)?;
                        // Post-reset garbage gate: the reorder pipeline
                        // can emit a stale frame after a flush. Anything
                        // before the first post-reset submission's pts is
                        // dropped (its sample released), bounded so a
                        // re-stamped output can't hold video shut.
                        if let Some(floor) = self.output_floor_pts {
                            if frame.pts_us() < floor && self.floor_dropped < FLOOR_DROP_BOUND {
                                self.floor_dropped += 1;
                                continue;
                            }
                            self.output_floor_pts = None;
                        }
                        return Ok(Some(frame));
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

    fn begin_drain(&mut self) -> Result<(), DecodeError> {
        // SAFETY: message-only COM calls on the owned MFT.
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

    fn reset(&mut self) -> Result<(), DecodeError> {
        // SAFETY: message-only COM calls on the owned MFT.
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
        }
        self.floor_armed = true;
        self.output_floor_pts = None;
        Ok(())
    }

    fn hardware_fell_back(&self) -> bool {
        self.fell_back
    }
}
