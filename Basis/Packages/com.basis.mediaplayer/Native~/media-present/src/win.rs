//! D3D11 shared-texture handoff into Unity via a keyed mutex.
//!
//! Producer (decode thread, own D3D11 device) converts the decoder's NV12
//! frame into a shared BGRA texture with one GPU pass (§6.8, `gpu`);
//! consumer (Unity render thread) opens the shared handle on Unity's
//! device and copies into a Unity-created texture. Keyed-mutex protocol:
//! producer acquires key 0 / releases key 1, consumer acquires key 1 /
//! releases key 0, both with timeouts so neither side ever blocks the other.

use std::ffi::c_void;

use media_decode::{ColorInfo, Nv12Frame};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::core::Interface;

use crate::PresentError;
use crate::gpu;

fn d3d<T>(r: windows::core::Result<T>, what: &str) -> Result<T, PresentError> {
    r.map_err(|e| PresentError(format!("{what}: {e}")))
}

enum Acquire {
    Acquired,
    TimedOut,
}

/// `AcquireSync` reports a timeout as the success HRESULT `WAIT_TIMEOUT`
/// (0x102), which `windows`' `Result<()>` projection collapses into `Ok` —
/// so go through the raw vtable to keep the distinction.
fn acquire_sync(
    keyed: &IDXGIKeyedMutex,
    key: u64,
    timeout_ms: u32,
) -> Result<Acquire, PresentError> {
    const WAIT_TIMEOUT_HR: i32 = 0x102;
    // SAFETY: raw vtable call on a live IDXGIKeyedMutex with the same
    // arguments the safe wrapper would pass; raw only because the wrapper
    // collapses the WAIT_TIMEOUT success HRESULT into Ok.
    let hr = unsafe {
        (Interface::vtable(keyed).AcquireSync)(Interface::as_raw(keyed), key, timeout_ms)
    };
    if hr.is_ok() && hr.0 != WAIT_TIMEOUT_HR {
        Ok(Acquire::Acquired)
    } else if hr.0 == WAIT_TIMEOUT_HR {
        Ok(Acquire::TimedOut)
    } else {
        Err(PresentError(format!("AcquireSync({key}): {hr}")))
    }
}

/// Decode-side owner of the shared texture.
pub struct SharedTexturePresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _texture: ID3D11Texture2D,
    keyed: IDXGIKeyedMutex,
    shared_handle: HANDLE,
    pass: gpu::ConvertPass,
}

// SAFETY: the presenter is owned and driven by one decode thread at a
// time; the wrapped D3D11 interfaces are free-threaded.
unsafe impl Send for SharedTexturePresenter {}

impl SharedTexturePresenter {
    pub fn new(width: u32, height: u32) -> Result<Self, PresentError> {
        // SAFETY: D3D11 object creation through owned wrappers; the texture
        // descriptor is a plain struct and every interface out-parameter is
        // checked before use.
        unsafe {
            let mut device = None;
            let mut context = None;
            d3d(
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
                "D3D11CreateDevice",
            )?;
            let device = device.ok_or_else(|| PresentError("no device".into()))?;
            let context = context.ok_or_else(|| PresentError("no context".into()))?;
            Self::build(device, context, width, height)
        }
    }

    /// Build the presenter on an existing device — the hardware decode
    /// path, where the decoder's NV12 slices must live on the same device
    /// the conversion pass samples them from.
    ///
    /// # Safety
    /// `device_ptr` must be a live `ID3D11Device*`; a reference is cloned,
    /// so the caller's may drop afterwards.
    pub unsafe fn new_on_device(
        device_ptr: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self, PresentError> {
        // SAFETY: caller guarantees a live ID3D11Device*; from_raw_borrowed
        // does not consume the caller's reference and the clone AddRefs.
        unsafe {
            let device = ID3D11Device::from_raw_borrowed(&device_ptr)
                .ok_or_else(|| PresentError("null decode device".into()))?
                .clone();
            let context = d3d(device.GetImmediateContext(), "GetImmediateContext")?;
            Self::build(device, context, width, height)
        }
    }

    fn build(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, PresentError> {
        // SAFETY: D3D11 object creation through owned wrappers; the texture
        // descriptor is a plain struct and every interface out-parameter is
        // checked before use.
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0
                    | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
            };
            let mut texture = None;
            d3d(
                device.CreateTexture2D(&desc, None, Some(&mut texture)),
                "CreateTexture2D (shared)",
            )?;
            let texture = texture.ok_or_else(|| PresentError("no texture".into()))?;

            let resource: IDXGIResource1 = d3d(texture.cast(), "cast IDXGIResource1")?;
            let shared_handle = d3d(
                resource.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    windows::core::PCWSTR::null(),
                ),
                "CreateSharedHandle",
            )?;
            let keyed: IDXGIKeyedMutex = d3d(texture.cast(), "cast IDXGIKeyedMutex")?;
            let pass = gpu::ConvertPass::new(&device, &texture, width, height)?;

            Ok(Self {
                device,
                context,
                _texture: texture,
                keyed,
                shared_handle,
                pass,
            })
        }
    }

    /// Process-wide NT handle value for `ID3D11Device1::OpenSharedResource1`.
    pub fn shared_handle(&self) -> u64 {
        self.shared_handle.0 as usize as u64
    }

    /// Convert and write one frame. Returns `false` if the consumer still owns
    /// the texture (frame dropped, never blocks the pipeline).
    pub fn present_nv12(&mut self, frame: &Nv12Frame) -> Result<bool, PresentError> {
        self.present_planes(frame.width, frame.height, &frame.data, frame.color)
    }

    /// As [`Self::present_nv12`], borrowing the packed NV12 bytes directly
    /// (the FramePool lease path).
    pub fn present_planes(
        &mut self,
        frame_width: u32,
        frame_height: u32,
        nv12: &[u8],
        color: ColorInfo,
    ) -> Result<bool, PresentError> {
        // The upload only touches the pass's private textures, so it runs
        // before the mutex; a timed-out acquire then wastes the upload, not
        // the pipeline's cadence.
        self.pass
            .upload(&self.device, &self.context, frame_width, frame_height, nv12)?;
        self.convert(color)
    }

    /// Present a decoder-owned NV12 texture-array slice (the DXVA path):
    /// one GPU subresource copy into the pass's sampled texture, then the
    /// same conversion draw. The caller must keep the slice's owning
    /// sample alive until this returns — the copy is submitted on this
    /// device's immediate context before the sample is released, which
    /// orders it ahead of any decoder reuse of the surface.
    ///
    /// # Safety
    /// `texture_ptr` must be a live `ID3D11Texture2D*` on this
    /// presenter's device with a valid `subresource` index.
    pub unsafe fn present_slice(
        &mut self,
        texture_ptr: *mut c_void,
        subresource: u32,
        color: ColorInfo,
    ) -> Result<bool, PresentError> {
        // SAFETY: caller guarantees a live texture on this device.
        unsafe {
            self.pass
                .upload_slice(&self.device, &self.context, texture_ptr, subresource)?;
        }
        self.convert(color)
    }

    fn convert(&mut self, color: ColorInfo) -> Result<bool, PresentError> {
        match acquire_sync(&self.keyed, 0, 4)? {
            Acquire::TimedOut => Ok(false),
            Acquire::Acquired => {
                self.pass.draw(&self.context, color);
                // SAFETY: Flush and ReleaseSync on live owned interfaces;
                // the mutex is held from the acquire above.
                unsafe {
                    self.context.Flush();
                    d3d(self.keyed.ReleaseSync(1), "ReleaseSync(1)")?;
                }
                Ok(true)
            }
        }
    }
}

/// Headless stand-in for Unity's side of the handoff: a second D3D11 device
/// with a plain BGRA texture to consume into. Test/smoke use only.
pub struct TestConsumerTarget {
    device: ID3D11Device,
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl TestConsumerTarget {
    pub fn new(width: u32, height: u32) -> Result<Self, PresentError> {
        // SAFETY: D3D11 object creation through owned wrappers, as above.
        unsafe {
            let mut device = None;
            d3d(
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                ),
                "D3D11CreateDevice (test consumer)",
            )?;
            let device = device.ok_or_else(|| PresentError("no device".into()))?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            d3d(
                device.CreateTexture2D(&desc, None, Some(&mut texture)),
                "CreateTexture2D (test consumer)",
            )?;
            let texture = texture.ok_or_else(|| PresentError("no texture".into()))?;
            Ok(Self {
                device,
                texture,
                width,
                height,
            })
        }
    }

    /// Raw pointer usable as the "Unity texture" for `SharedTextureConsumer::open`.
    pub fn texture_ptr(&self) -> *mut c_void {
        self.texture.as_raw()
    }

    /// Read the destination texture back as tightly packed BGRA rows.
    /// Test/validation use only.
    pub fn read_back(&self) -> Result<Vec<u8>, PresentError> {
        // SAFETY: staging-texture creation and copy through owned wrappers;
        // the Map(READ) region spans RowPitch*height bytes, and every row
        // copy reads `width * 4 <= RowPitch` bytes inside it.
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            d3d(
                self.device.CreateTexture2D(&desc, None, Some(&mut staging)),
                "CreateTexture2D (readback staging)",
            )?;
            let staging = staging.ok_or_else(|| PresentError("no readback staging".into()))?;
            let context = d3d(self.device.GetImmediateContext(), "GetImmediateContext")?;
            context.CopyResource(&staging, &self.texture);

            let mut mapped = Default::default();
            d3d(
                context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)),
                "Map (readback)",
            )?;
            let row_pitch = mapped.RowPitch as usize;
            let (w, h) = (self.width as usize, self.height as usize);
            let mut out = vec![0u8; w * h * 4];
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    (mapped.pData as *const u8).add(row * row_pitch),
                    out.as_mut_ptr().add(row * w * 4),
                    w * 4,
                );
            }
            context.Unmap(&staging, 0);
            Ok(out)
        }
    }
}

/// Render-thread consumer living on Unity's device.
pub struct SharedTextureConsumer {
    context: ID3D11DeviceContext,
    destination: ID3D11Texture2D,
    shared: ID3D11Texture2D,
    keyed: IDXGIKeyedMutex,
}

// SAFETY: only ever touched from the Unity render thread (or the smoke
// test's consumer thread); the wrapped D3D11 interfaces are free-threaded.
unsafe impl Send for SharedTextureConsumer {}

impl SharedTextureConsumer {
    /// # Safety
    /// `destination_texture` must be a live `ID3D11Texture2D*` whose device
    /// can open `shared_handle`, and whose dimensions/format match the
    /// producer's texture.
    pub unsafe fn open(
        destination_texture: *mut c_void,
        shared_handle: u64,
    ) -> Result<Self, PresentError> {
        // SAFETY: caller guarantees destination_texture is a live
        // ID3D11Texture2D*; from_raw_borrowed does not take over the caller's
        // reference and the clone AddRefs, so the struct owns what it holds.
        unsafe {
            let destination = ID3D11Texture2D::from_raw_borrowed(&destination_texture)
                .ok_or_else(|| PresentError("null destination texture".into()))?
                .clone();
            let device = d3d(destination.GetDevice(), "GetDevice")?;
            let device1: ID3D11Device1 = d3d(device.cast(), "cast ID3D11Device1")?;
            let shared: ID3D11Texture2D = d3d(
                device1.OpenSharedResource1(HANDLE(shared_handle as usize as *mut c_void)),
                "OpenSharedResource1",
            )?;
            let keyed: IDXGIKeyedMutex = d3d(shared.cast(), "cast IDXGIKeyedMutex (consumer)")?;
            let context = d3d(device.GetImmediateContext(), "GetImmediateContext")?;
            Ok(Self {
                context,
                destination,
                shared,
                keyed,
            })
        }
    }

    /// Copy the newest frame into the destination texture if the producer has
    /// published one since the last copy. Never blocks.
    pub fn copy_if_fresh(&mut self) -> Result<bool, PresentError> {
        match acquire_sync(&self.keyed, 1, 0)? {
            Acquire::TimedOut => Ok(false),
            // SAFETY: the mutex is held (acquired above) and both textures are
            // owned live interfaces on the same device.
            Acquire::Acquired => unsafe {
                self.context.CopyResource(&self.destination, &self.shared);
                d3d(self.keyed.ReleaseSync(0), "ReleaseSync(0)")?;
                Ok(true)
            },
        }
    }
}
