//! Android / Vulkan present path (§6.8, the M0-validated primary path):
//! Vulkan-init interception guarantees the device extensions and YCbCr
//! feature; the decoder's `AHardwareBuffer` imports into Unity's own
//! `VkDevice`; one compute pass converts into Unity's RGBA RenderTexture
//! on the render thread.
//!
//! Managed graphics contract (Vulkan, normative): the output texture is a
//! Unity RenderTexture, **linear** (no sRGB) RGBA32 with
//! `enableRandomWrite = true`, created at the snapshot's display size and
//! registered via `bm_session_set_output_texture(GetNativeTexturePtr())`.
//! The plugin must be preloaded (`PluginImporter.isPreloaded`) so the
//! interception registers before graphics initialisation. Render events
//! are issued as on D3D11; teardown order is likewise unchanged.

mod fns;
mod intercept;
mod renderer;
mod unity;

pub use renderer::SessionRenderer;

/// Forward of `UnityPluginLoad`.
///
/// # Safety
/// `interfaces` must be the live `IUnityInterfaces*` Unity passed.
pub unsafe fn unity_plugin_load(interfaces: *mut core::ffi::c_void) {
    // SAFETY: caller contract forwarded.
    unsafe { intercept::plugin_load(interfaces) }
}

/// logcat line under the `basis-media` tag (stderr goes nowhere on
/// Android; the engine's own eprintln diagnostics still do, so anything
/// load-bearing should reach here too).
pub fn log(line: &str) {
    unity::log(line);
}
