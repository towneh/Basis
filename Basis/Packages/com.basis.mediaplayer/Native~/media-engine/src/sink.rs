//! Platform video sink (§6.8): the video thread's half of presentation.
//! Frame *selection* normally lives in the Unity render event (see
//! `present.rs` — due-ness at display cadence with a vsync of lookahead);
//! this sink configures the output target and carries the tick-paced
//! fallback for consumers that issue no render events (headless sessions,
//! a non-rendering app). On Windows the shared D3D11 texture presenter
//! lives in `PipelineShared::presenter` so the render event and the
//! fallback drive the same conversion pass; on Android the fallback has
//! nowhere to draw (the conversion pass needs Unity's device), so a
//! fallback-presented frame is simply consumed.

use crate::pipeline::PipelineShared;
use crate::pool::Lease;

#[cfg(windows)]
pub struct VideoSink {
    configured: bool,
}

#[cfg(windows)]
impl VideoSink {
    pub fn new() -> Self {
        Self { configured: false }
    }

    /// (Re)build the shared texture for a newly announced coded size and
    /// expose its handle to the managed side. `decode_device` is the
    /// hardware route's D3D11 device — the presenter builds on it so
    /// decoded slices bind straight into the conversion pass; `None`
    /// (software routes) keeps the presenter's own device.
    ///
    /// # Safety
    /// `decode_device`, when `Some`, must be a live `ID3D11Device*` that
    /// stays live for the duration of the call. The presenter clones its
    /// own reference, so the caller's may drop once this returns; what it
    /// cannot survive is the device going away underneath the call, which
    /// turns the vtable dispatch inside `new_on_device` into a read of
    /// freed memory as a function pointer.
    pub unsafe fn configure(
        &mut self,
        px: &PipelineShared,
        coded_width: u32,
        coded_height: u32,
        decode_device: Option<*mut std::ffi::c_void>,
    ) -> Result<(), media_present::PresentError> {
        let presenter = match decode_device {
            // SAFETY: the contract above admits only a device live
            // across this call; new_on_device clones its own reference.
            Some(device) => unsafe {
                media_present::SharedTexturePresenter::new_on_device(
                    device,
                    coded_width.max(2),
                    coded_height.max(2),
                )?
            },
            None => {
                media_present::SharedTexturePresenter::new(coded_width.max(2), coded_height.max(2))?
            }
        };
        px.shared.shared_texture_handle.store(
            presenter.shared_handle(),
            std::sync::atomic::Ordering::Release,
        );
        *px.presenter.lock().expect("presenter lock") = Some(presenter);
        self.configured = true;
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.configured
    }

    /// Fallback present: convert and publish one due frame. `Ok(false)` =
    /// the consumer still owned the texture (frame dropped, never blocks
    /// the pipeline).
    pub fn present(
        &mut self,
        px: &PipelineShared,
        lease: &mut Lease,
    ) -> Result<bool, media_present::PresentError> {
        let mut slot = px.presenter.lock().expect("presenter lock");
        let Some(presenter) = slot.as_mut() else {
            return Ok(false);
        };
        present_lease_frame(presenter, lease)
    }
}

/// Convert one leased frame through the presenter — shared by the video
/// thread's fallback present and the render event (`present.rs`). The
/// DXVA slice stays alive for the whole call (the lease holds the frame),
/// so the GPU copy is ordered ahead of the decoder reusing the surface.
#[cfg(windows)]
pub fn present_lease_frame(
    presenter: &mut media_present::SharedTexturePresenter,
    lease: &Lease,
) -> Result<bool, media_present::PresentError> {
    match lease.frame() {
        Some(media_decode::VideoFrame::Nv12(frame)) => {
            presenter.present_planes(frame.width, frame.height, &frame.data, frame.color)
        }
        Some(media_decode::VideoFrame::Opaque(frame)) => {
            match frame.image.d3d11_slice() {
                // SAFETY: the payload guarantees texture+index valid for
                // its own lifetime, which spans this call via the lease.
                Some((texture, subresource)) => unsafe {
                    presenter.present_slice(texture, subresource, frame.color)
                },
                // A non-D3D11 opaque payload cannot occur on Windows;
                // tolerate rather than crash the thread.
                None => Ok(false),
            }
        }
        None => Ok(false),
    }
}

#[cfg(target_os = "android")]
pub struct VideoSink {
    configured: bool,
}

#[cfg(target_os = "android")]
impl VideoSink {
    pub fn new() -> Self {
        Self { configured: false }
    }

    /// # Safety
    /// As the Windows implementation, so the signature is the same on
    /// every target. This one never dereferences `decode_device`.
    pub unsafe fn configure(
        &mut self,
        _px: &PipelineShared,
        _coded_width: u32,
        _coded_height: u32,
        _decode_device: Option<*mut std::ffi::c_void>,
    ) -> Result<(), media_present::PresentError> {
        // The output target is Unity's own RenderTexture, registered
        // through the ABI; nothing to build producer-side.
        self.configured = true;
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.configured
    }

    /// Fallback present: the Vulkan conversion pass runs only inside a
    /// render event, so with no render consumer live the due frame is
    /// consumed here — the pipeline keeps flowing (position, EOS, buffer
    /// accounting) and the frame's buffer returns to its image reader.
    pub fn present(
        &mut self,
        _px: &PipelineShared,
        lease: &mut Lease,
    ) -> Result<bool, media_present::PresentError> {
        Ok(lease.take_frame().is_some())
    }
}

/// Headless platforms: no present target exists, so the tick-paced
/// fallback consumes each due frame — the pipeline keeps flowing
/// (position, EOS, buffer accounting) and a consumed frame counts as
/// presented for the null-sink counters.
#[cfg(not(any(windows, target_os = "android")))]
pub struct VideoSink {
    configured: bool,
}

#[cfg(not(any(windows, target_os = "android")))]
impl VideoSink {
    pub fn new() -> Self {
        Self { configured: false }
    }

    /// # Safety
    /// As the Windows implementation, so the signature is the same on
    /// every target. This one never dereferences `decode_device`.
    pub unsafe fn configure(
        &mut self,
        _px: &PipelineShared,
        _coded_width: u32,
        _coded_height: u32,
        _decode_device: Option<*mut std::ffi::c_void>,
    ) -> Result<(), media_present::PresentError> {
        self.configured = true;
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.configured
    }

    pub fn present(
        &mut self,
        _px: &PipelineShared,
        lease: &mut Lease,
    ) -> Result<bool, media_present::PresentError> {
        Ok(lease.take_frame().is_some())
    }
}
