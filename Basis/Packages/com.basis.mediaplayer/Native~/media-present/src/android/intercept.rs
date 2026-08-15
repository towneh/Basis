//! Vulkan initialisation interception (§6.8, the M0-validated primary
//! path): hook Unity's `vkGetInstanceProcAddr` chain so device creation
//! is guaranteed to enable the AHardwareBuffer-import extensions and the
//! `samplerYcbcrConversion` feature. Unity's OpenXR path already asks for
//! all of it on Quest; the hook makes that a contract instead of an
//! observation, appending only what the caller didn't already enable.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Mutex;

use ash::vk;

use super::unity::{self, logf};

#[derive(Default)]
struct HookState {
    real_gipa: Option<vk::PFN_vkGetInstanceProcAddr>,
    real_create_instance: Option<vk::PFN_vkCreateInstance>,
    real_create_device: Option<vk::PFN_vkCreateDevice>,
    instance: Option<vk::Instance>,
}

static HOOKS: Mutex<HookState> = Mutex::new(HookState {
    real_gipa: None,
    real_create_instance: None,
    real_create_device: None,
    instance: None,
});

/// Everything the render path needs, captured at device creation (or,
/// as a fallback, from `UnityVulkanInstance()` at the first device
/// event).
pub(crate) struct VkCtx {
    pub device: vk::Device,
    pub queue_family: u32,
    pub vulkan_iface: *mut unity::IUnityGraphicsVulkanV2,
    pub fns: super::fns::DeviceFns,
}

// SAFETY: the context is only used on Unity's render thread (render
// events and device events); the raw interface pointer is Unity-owned
// and process-lifetime.
unsafe impl Send for VkCtx {}

pub(crate) static CTX: Mutex<Option<VkCtx>> = Mutex::new(None);

static GRAPHICS_IFACE: Mutex<usize> = Mutex::new(0);
static VULKAN_IFACE: Mutex<usize> = Mutex::new(0);

/// Entry from `UnityPluginLoad` (media-ffi forwards). Must run before
/// graphics initialisation — the plugin has to be preloaded
/// (`PluginImporter.isPreloaded`, the M0 lesson).
///
/// # Safety
/// `interfaces` must be the live `IUnityInterfaces*` Unity passed.
pub unsafe fn plugin_load(interfaces: *mut c_void) {
    if interfaces.is_null() {
        return;
    }
    // SAFETY: caller contract — live IUnityInterfaces vtable.
    unsafe {
        let ifs = &*(interfaces as *mut unity::IUnityInterfaces);
        let gfx = (ifs.get_interface_split)(unity::GUID_GRAPHICS.0, unity::GUID_GRAPHICS.1)
            as *mut unity::IUnityGraphics;
        if !gfx.is_null() {
            *GRAPHICS_IFACE.lock().expect("graphics iface lock") = gfx as usize;
            ((*gfx).register_device_event_callback)(on_device_event);
        }
        let v2 = (ifs.get_interface_split)(unity::GUID_VULKAN_V2.0, unity::GUID_VULKAN_V2.1)
            as *mut unity::IUnityGraphicsVulkanV2;
        if v2.is_null() {
            unity::log("plugin_load: IUnityGraphicsVulkanV2 unavailable (not a Vulkan build?)");
            return;
        }
        *VULKAN_IFACE.lock().expect("vulkan iface lock") = v2 as usize;
        let ok = ((*v2).add_intercept_initialization)(vulkan_init_cb, core::ptr::null_mut(), 0);
        logf!("plugin_load: vulkan interception registered={ok}");
    }
}

unsafe extern "C" fn on_device_event(event_type: c_int) {
    if event_type != unity::DEVICE_EVENT_INITIALIZE {
        return;
    }
    let gfx = *GRAPHICS_IFACE.lock().expect("graphics iface lock") as *mut unity::IUnityGraphics;
    let v2 = *VULKAN_IFACE.lock().expect("vulkan iface lock") as *mut unity::IUnityGraphicsVulkanV2;
    if gfx.is_null() || v2.is_null() {
        return;
    }
    // SAFETY: Unity-owned vtables, valid for the process lifetime; the
    // initialize event with the Vulkan renderer is the documented point
    // where Instance() becomes valid (the renderer==Null first fire is
    // the M0 segfault trap).
    unsafe {
        if ((*gfx).get_renderer)() != unity::RENDERER_VULKAN {
            return;
        }
        let uvi = ((*v2).instance)();
        if uvi.device == vk::Device::null() {
            return;
        }
        let gipa = {
            let hooks = HOOKS.lock().expect("hooks lock");
            hooks.real_gipa
        }
        .unwrap_or(uvi.get_instance_proc_addr);
        match super::fns::DeviceFns::load(gipa, uvi.instance, uvi.device) {
            Ok(fns) => {
                *CTX.lock().expect("ctx lock") = Some(VkCtx {
                    device: uvi.device,
                    queue_family: uvi.queue_family_index,
                    vulkan_iface: v2,
                    fns,
                });
                logf!(
                    "device_event: vulkan ctx captured (device={:?} qfam={})",
                    uvi.device,
                    uvi.queue_family_index
                );
            }
            Err(missing) => {
                logf!("device_event: vulkan fn load FAILED ({missing}) — present path disabled");
            }
        }
    }
}

unsafe extern "C" fn vulkan_init_cb(
    gipa: vk::PFN_vkGetInstanceProcAddr,
    _userdata: *mut c_void,
) -> vk::PFN_vkGetInstanceProcAddr {
    HOOKS.lock().expect("hooks lock").real_gipa = Some(gipa);
    unity::log("vulkan_init_cb: interception active");
    hook_gipa
}

unsafe extern "system" fn hook_gipa(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let real_gipa = HOOKS.lock().expect("hooks lock").real_gipa?;
    if name.is_null() {
        // SAFETY: forwarding to the loader's gipa verbatim.
        return unsafe { real_gipa(instance, name) };
    }
    // SAFETY: non-null NUL-terminated name per the Vulkan contract; the
    // transmutes cast concrete PFN types to the erased return type.
    unsafe {
        match CStr::from_ptr(name).to_bytes() {
            b"vkGetInstanceProcAddr" => Some(core::mem::transmute::<
                vk::PFN_vkGetInstanceProcAddr,
                unsafe extern "system" fn(),
            >(hook_gipa)),
            b"vkCreateInstance" => {
                let real = real_gipa(instance, name)?;
                HOOKS.lock().expect("hooks lock").real_create_instance =
                    Some(core::mem::transmute::<
                        unsafe extern "system" fn(),
                        vk::PFN_vkCreateInstance,
                    >(real));
                Some(core::mem::transmute::<
                    vk::PFN_vkCreateInstance,
                    unsafe extern "system" fn(),
                >(hook_create_instance))
            }
            b"vkCreateDevice" => {
                let real = real_gipa(instance, name)?;
                {
                    let mut hooks = HOOKS.lock().expect("hooks lock");
                    hooks.real_create_device = Some(core::mem::transmute::<
                        unsafe extern "system" fn(),
                        vk::PFN_vkCreateDevice,
                    >(real));
                    if hooks.instance.is_none() && instance != vk::Instance::null() {
                        hooks.instance = Some(instance);
                    }
                }
                Some(core::mem::transmute::<
                    vk::PFN_vkCreateDevice,
                    unsafe extern "system" fn(),
                >(hook_create_device))
            }
            _ => real_gipa(instance, name),
        }
    }
}

unsafe extern "system" fn hook_create_instance(
    create_info: *const vk::InstanceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_instance: *mut vk::Instance,
) -> vk::Result {
    let real = HOOKS
        .lock()
        .expect("hooks lock")
        .real_create_instance
        .expect("hooked without real fn");
    // SAFETY: pass-through with the caller's own arguments.
    let result = unsafe { real(create_info, allocator, out_instance) };
    if result == vk::Result::SUCCESS {
        // SAFETY: out_instance was just written by a successful create.
        HOOKS.lock().expect("hooks lock").instance = Some(unsafe { *out_instance });
    }
    result
}

const WANTED_DEVICE_EXTS: &[&CStr] = &[
    c"VK_ANDROID_external_memory_android_hardware_buffer",
    c"VK_EXT_queue_family_foreign",
    c"VK_KHR_sampler_ycbcr_conversion",
    c"VK_KHR_external_memory",
    c"VK_KHR_dedicated_allocation",
    c"VK_KHR_bind_memory2",
    c"VK_KHR_get_memory_requirements2",
    c"VK_KHR_maintenance1",
];

unsafe extern "system" fn hook_create_device(
    physical_device: vk::PhysicalDevice,
    create_info: *const vk::DeviceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_device: *mut vk::Device,
) -> vk::Result {
    let (real_gipa, real_create_device, instance) = {
        let hooks = HOOKS.lock().expect("hooks lock");
        (
            hooks.real_gipa.expect("hooked without gipa"),
            hooks.real_create_device.expect("hooked without real fn"),
            hooks.instance,
        )
    };

    // SAFETY: reads of the caller's create-info arrays within their
    // declared counts; probe calls go through the loader's gipa on the
    // captured instance.
    unsafe {
        let ci = &*create_info;
        let requested: Vec<&CStr> = (0..ci.enabled_extension_count as usize)
            .map(|i| CStr::from_ptr(*ci.pp_enabled_extension_names.add(i)))
            .collect();

        // What the driver advertises (probe only works with an instance).
        let mut advertised: Vec<String> = Vec::new();
        if let Some(instance) = instance
            && let Some(f) = real_gipa(instance, c"vkEnumerateDeviceExtensionProperties".as_ptr())
        {
            let f = core::mem::transmute::<
                unsafe extern "system" fn(),
                vk::PFN_vkEnumerateDeviceExtensionProperties,
            >(f);
            let mut count = 0u32;
            if f(
                physical_device,
                core::ptr::null(),
                &mut count,
                core::ptr::null_mut(),
            ) == vk::Result::SUCCESS
            {
                let mut props = vec![vk::ExtensionProperties::default(); count as usize];
                if f(
                    physical_device,
                    core::ptr::null(),
                    &mut count,
                    props.as_mut_ptr(),
                ) == vk::Result::SUCCESS
                {
                    advertised = props
                        .iter()
                        .map(|p| {
                            CStr::from_ptr(p.extension_name.as_ptr())
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                }
            }
        }

        let mut ext_ptrs: Vec<*const c_char> = (0..ci.enabled_extension_count as usize)
            .map(|i| *ci.pp_enabled_extension_names.add(i))
            .collect();
        let mut appended: Vec<&str> = Vec::new();
        for want in WANTED_DEVICE_EXTS {
            let already = requested.iter().any(|e| e == want);
            let available = advertised
                .iter()
                .any(|e| e.as_str() == want.to_str().unwrap());
            if !already && available {
                ext_ptrs.push(want.as_ptr());
                appended.push(want.to_str().unwrap());
            } else if !already && !available && !advertised.is_empty() {
                logf!(
                    "vkCreateDevice: wanted extension {} not advertised",
                    want.to_string_lossy()
                );
            }
        }
        if !appended.is_empty() {
            logf!("vkCreateDevice: appended [{}]", appended.join(","));
        }

        // samplerYcbcrConversion: flip in place when the caller already
        // chains a features struct (mixing both is a spec violation),
        // else prepend our own.
        let mut ycbcr_found = false;
        let mut chain = ci.p_next as *mut vk::BaseOutStructure<'_>;
        while !chain.is_null() {
            match (*chain).s_type {
                vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_1_FEATURES => {
                    (*(chain as *mut vk::PhysicalDeviceVulkan11Features<'_>))
                        .sampler_ycbcr_conversion = vk::TRUE;
                    ycbcr_found = true;
                    break;
                }
                vk::StructureType::PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES => {
                    (*(chain as *mut vk::PhysicalDeviceSamplerYcbcrConversionFeatures<'_>))
                        .sampler_ycbcr_conversion = vk::TRUE;
                    ycbcr_found = true;
                    break;
                }
                _ => chain = (*chain).p_next,
            }
        }
        let mut our_ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
        let mut local_ci = *ci;
        if !ycbcr_found {
            our_ycbcr.sampler_ycbcr_conversion = vk::TRUE;
            our_ycbcr.p_next = ci.p_next as *mut c_void;
            local_ci.p_next = &raw const our_ycbcr as *const c_void;
        }
        local_ci.enabled_extension_count = ext_ptrs.len() as u32;
        local_ci.pp_enabled_extension_names = ext_ptrs.as_ptr();

        let result = real_create_device(physical_device, &local_ci, allocator, out_device);
        logf!("vkCreateDevice: hooked, result={result:?}");
        result
    }
}
