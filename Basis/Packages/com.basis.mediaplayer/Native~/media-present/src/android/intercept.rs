//! Vulkan initialisation interception (§6.8, the M0-validated primary
//! path): hook Unity's `vkGetInstanceProcAddr` chain so device creation
//! is guaranteed to enable the AHardwareBuffer-import extensions and the
//! `samplerYcbcrConversion` feature. Unity's OpenXR path already asks for
//! all of it on Quest; the hook makes that a contract instead of an
//! observation, appending only what the caller didn't already enable.
//!
//! Every callback here is entered from C — Unity's graphics layer or the
//! Vulkan loader — so each one is fenced and degrades rather than
//! unwinding: an unexpected hook state forwards or refuses instead of
//! asserting, and no lock in this module panics on poisoning.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use ash::vk;

use super::unity::{self, logf};

#[derive(Default)]
struct HookState {
    real_gipa: Option<vk::PFN_vkGetInstanceProcAddr>,
    real_create_instance: Option<vk::PFN_vkCreateInstance>,
    real_create_device: Option<vk::PFN_vkCreateDevice>,
    instance: Option<vk::Instance>,
    /// Which spellings of the features query this instance may be
    /// asked, most capable first, and `None` in a slot the instance does
    /// not support. Decided at instance creation because that is where
    /// the API version and the enabled instance extensions are visible.
    /// Both are kept rather than one: an instance created at 1.1 can
    /// still be given a physical device from a 1.0 driver, and the core
    /// name is then the wrong one to have committed to.
    features2: [Option<&'static CStr>; 2],
}

static HOOKS: Mutex<HookState> = Mutex::new(HookState {
    real_gipa: None,
    real_create_instance: None,
    real_create_device: None,
    instance: None,
    features2: [None; 2],
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
        let Some(get_interface_split) = ifs.get_interface_split else {
            unity::log("plugin_load: no GetInterfaceSplit on the interface table");
            return;
        };
        let gfx = get_interface_split(unity::GUID_GRAPHICS.0, unity::GUID_GRAPHICS.1)
            as *mut unity::IUnityGraphics;
        // Both interfaces are published before the callback is armed:
        // registering can dispatch the initialize event from inside the
        // register call, and `device_event` reads the Vulkan interface and
        // gives up where it is absent. That event is the documented — and
        // only — point at which the instance becomes available, so losing
        // it leaves the present path inert for the whole run.
        let v2 = get_interface_split(unity::GUID_VULKAN_V2.0, unity::GUID_VULKAN_V2.1)
            as *mut unity::IUnityGraphicsVulkanV2;
        if v2.is_null() {
            unity::log("plugin_load: IUnityGraphicsVulkanV2 unavailable (not a Vulkan build?)");
        } else {
            *VULKAN_IFACE.lock().unwrap_or_else(|e| e.into_inner()) = v2 as usize;
        }
        if gfx.is_null() {
            unity::log("plugin_load: IUnityGraphics unavailable");
        } else if let Some(register) = (*gfx).register_device_event_callback {
            *GRAPHICS_IFACE.lock().unwrap_or_else(|e| e.into_inner()) = gfx as usize;
            register(on_device_event);
        } else {
            // Without it `device_event` never fires and the interception
            // sits inert for the whole run; every other absent slot here
            // says so, and this one used to go quiet.
            unity::log("plugin_load: no RegisterDeviceEventCallback on the graphics interface");
        }
        if v2.is_null() {
            return;
        }
        let Some(add_intercept) = (*v2).add_intercept_initialization else {
            unity::log("plugin_load: no AddInterceptInitialization on the Vulkan interface");
            return;
        };
        let ok = add_intercept(vulkan_init_cb, core::ptr::null_mut(), 0);
        logf!("plugin_load: vulkan interception registered={ok}");
    }
}

unsafe extern "C" fn on_device_event(event_type: c_int) {
    // SAFETY: forwarding Unity's own argument on Unity's own thread; the
    // fence exists because this frame returns into C, which cannot unwind.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { device_event(event_type) }));
}

/// # Safety
/// Called only from `on_device_event`, on Unity's graphics thread.
unsafe fn device_event(event_type: c_int) {
    if event_type != unity::DEVICE_EVENT_INITIALIZE {
        return;
    }
    let gfx =
        *GRAPHICS_IFACE.lock().unwrap_or_else(|e| e.into_inner()) as *mut unity::IUnityGraphics;
    let v2 = *VULKAN_IFACE.lock().unwrap_or_else(|e| e.into_inner())
        as *mut unity::IUnityGraphicsVulkanV2;
    if gfx.is_null() || v2.is_null() {
        return;
    }
    // SAFETY: Unity-owned vtables, valid for the process lifetime; the
    // initialize event with the Vulkan renderer is the documented point
    // where Instance() becomes valid (the renderer==Null first fire is
    // the M0 segfault trap).
    unsafe {
        let (Some(get_renderer), Some(instance)) = ((*gfx).get_renderer, (*v2).instance) else {
            return;
        };
        if get_renderer() != unity::RENDERER_VULKAN {
            return;
        }
        let uvi = instance();
        if uvi.device == vk::Device::null() {
            return;
        }
        let gipa = {
            let hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
            hooks.real_gipa
        }
        .or(uvi.get_instance_proc_addr);
        let Some(gipa) = gipa else {
            unity::log("device_event: no vkGetInstanceProcAddr to load from");
            return;
        };
        match super::fns::DeviceFns::load(gipa, uvi.instance, uvi.device) {
            Ok(fns) => {
                *CTX.lock().unwrap_or_else(|e| e.into_inner()) = Some(VkCtx {
                    device: uvi.device,
                    queue_family: uvi.queue_family_index,
                    vulkan_iface: v2,
                    fns,
                });
                // No handle values in the log: `vk::Device` is a
                // dispatchable handle, so `{:?}` on it prints a live
                // process address into a sink `adb logcat` and any
                // bug report can read.
                logf!(
                    "device_event: vulkan ctx captured (qfam={})",
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
    gipa: Option<vk::PFN_vkGetInstanceProcAddr>,
    _userdata: *mut c_void,
) -> Option<vk::PFN_vkGetInstanceProcAddr> {
    catch_unwind(AssertUnwindSafe(
        || -> Option<vk::PFN_vkGetInstanceProcAddr> {
            let gipa = gipa?;
            HOOKS.lock().unwrap_or_else(|e| e.into_inner()).real_gipa = Some(gipa);
            unity::log("vulkan_init_cb: interception active");
            Some(hook_gipa)
        },
    ))
    // Hand back the loader's own gipa rather than a hook whose real
    // pointer may not have been stored: no interception, but a live chain.
    .unwrap_or(gipa)
}

unsafe extern "system" fn hook_gipa(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    // SAFETY: forwarding the loader's own arguments; the fence exists
    // because this frame returns into the Vulkan loader's C code.
    catch_unwind(AssertUnwindSafe(|| unsafe { gipa(instance, name) })).unwrap_or(None)
}

/// # Safety
/// Called only from `hook_gipa`, with the loader's own arguments.
unsafe fn gipa(instance: vk::Instance, name: *const c_char) -> vk::PFN_vkVoidFunction {
    let real_gipa = HOOKS.lock().unwrap_or_else(|e| e.into_inner()).real_gipa?;
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
                HOOKS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .real_create_instance = Some(core::mem::transmute::<
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
                    let mut hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
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
    // SAFETY: forwarding the loader's own arguments; the fence as in
    // `hook_gipa`, refusing rather than unwinding into the loader.
    catch_unwind(AssertUnwindSafe(|| unsafe {
        create_instance(create_info, allocator, out_instance)
    }))
    .unwrap_or(vk::Result::ERROR_INITIALIZATION_FAILED)
}

/// # Safety
/// Called only from `hook_create_instance`, with the loader's own arguments.
unsafe fn create_instance(
    create_info: *const vk::InstanceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_instance: *mut vk::Instance,
) -> vk::Result {
    let real = HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .real_create_instance;
    // The loader is only ever handed this pointer from the arm that
    // stores the real one; reaching it without has nothing to forward to.
    let Some(real) = real else {
        unity::log("vkCreateInstance: hook reached with no real fn");
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    // SAFETY: pass-through with the caller's own arguments.
    let result = unsafe { real(create_info, allocator, out_instance) };
    if result == vk::Result::SUCCESS {
        // SAFETY: out_instance was just written by a successful create,
        // and the create info is the caller's own, read within its
        // declared count.
        unsafe {
            let ci = &*create_info;
            let api = ci
                .p_application_info
                .as_ref()
                .map_or(0, |app| app.api_version);
            // The core query needs 1.1 and the extension one needs its
            // instance extension: a loader will hand over a pointer for a
            // name the instance never supported, and calling it is not
            // defined. Both are recorded where both preconditions hold,
            // and the probe takes whichever the loader resolves — the
            // version the application asked for is not the version the
            // driver behind a given physical device need implement, so
            // committing to the core name on the strength of it would
            // leave the extension spelling untried. Neither available
            // means the device features go unprobed rather than guessed
            // at.
            let core = (api >= vk::API_VERSION_1_1).then_some(c"vkGetPhysicalDeviceFeatures2");
            let khr = (0..ci.enabled_extension_count as usize)
                .any(|i| {
                    CStr::from_ptr(*ci.pp_enabled_extension_names.add(i))
                        == c"VK_KHR_get_physical_device_properties2"
                })
                .then_some(c"vkGetPhysicalDeviceFeatures2KHR");
            let features2 = [core, khr];
            let mut hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
            hooks.instance = Some(*out_instance);
            hooks.features2 = features2;
        }
    }
    result
}

/// What the probe could establish. A device that does not advertise the
/// feature and a probe that never ran are the same silence on the wire
/// and different places to look on a device pass: the first is the
/// driver's answer, the second says the interception had nothing to ask
/// with.
enum YcbcrProbe {
    Advertised,
    Absent,
    Unprobed(&'static str),
}

/// Whether the driver advertises `samplerYcbcrConversion` for this
/// device. `Unprobed` where the question cannot be put at all, which is
/// the answer that matters either way: forcing the feature on where the
/// device does not have it fails `vkCreateDevice` with
/// `ERROR_FEATURE_NOT_PRESENT`, and the device this hook sits on is the
/// app's, not the video path's.
///
/// # Safety
/// `instance` and `physical_device` must be the loader's own handles, and
/// `real_gipa` must be the loader's own resolver for that instance. Every
/// name in `features2` must be one the instance supports, since resolving
/// anything else and calling it is not defined however willingly a loader
/// hands the pointer over.
unsafe fn ycbcr_probe(
    real_gipa: Option<vk::PFN_vkGetInstanceProcAddr>,
    instance: Option<vk::Instance>,
    features2: [Option<&'static CStr>; 2],
    physical_device: vk::PhysicalDevice,
) -> YcbcrProbe {
    let (Some(real_gipa), Some(instance)) = (real_gipa, instance) else {
        return YcbcrProbe::Unprobed(match real_gipa.is_some() {
            false => "no captured vkGetInstanceProcAddr",
            true => "no captured instance",
        });
    };
    if features2.iter().all(Option::is_none) {
        return YcbcrProbe::Unprobed("no features query this instance supports");
    }
    // Ordered by what *this* physical device implements rather than by
    // what the instance was created at: an instance at 1.1 can be handed
    // a device from a 1.0 driver, and the core query is the wrong name to
    // put to one. `vkGetPhysicalDeviceProperties` is core 1.0, so it can
    // always be asked. Ordered rather than restricted — a device that
    // answers 1.0 still gets the core name tried behind the extension
    // one, so nothing that resolves today stops being reached.
    // SAFETY: the loader's own resolver, called with its own handles.
    let device_is_1_1 = unsafe {
        real_gipa(instance, c"vkGetPhysicalDeviceProperties".as_ptr()).is_some_and(|f| {
            let f = core::mem::transmute::<
                unsafe extern "system" fn(),
                vk::PFN_vkGetPhysicalDeviceProperties,
            >(f);
            let mut props = vk::PhysicalDeviceProperties::default();
            f(physical_device, &mut props);
            props.api_version >= vk::API_VERSION_1_1
        })
    };
    let [core_name, khr_name] = features2;
    let ordered = if device_is_1_1 {
        [core_name, khr_name]
    } else {
        [khr_name, core_name]
    };
    // SAFETY: the resolved query is called with the loader's own physical
    // device and a features chain owned for the length of the call. Only
    // names the instance supports are offered to the resolver.
    unsafe {
        let Some(f) = ordered
            .iter()
            .flatten()
            .find_map(|name| real_gipa(instance, name.as_ptr()))
        else {
            return YcbcrProbe::Unprobed("the loader resolved no features query it was offered");
        };
        let f = core::mem::transmute::<
            unsafe extern "system" fn(),
            vk::PFN_vkGetPhysicalDeviceFeatures2,
        >(f);
        let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
        let mut probe = vk::PhysicalDeviceFeatures2 {
            p_next: (&raw mut ycbcr).cast::<c_void>(),
            ..Default::default()
        };
        f(physical_device, &mut probe);
        if ycbcr.sampler_ycbcr_conversion == vk::TRUE {
            YcbcrProbe::Advertised
        } else {
            YcbcrProbe::Absent
        }
    }
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
    // SAFETY: forwarding the loader's own arguments; the fence as in
    // `hook_gipa`, refusing rather than unwinding into the loader.
    catch_unwind(AssertUnwindSafe(|| unsafe {
        create_device(physical_device, create_info, allocator, out_device)
    }))
    .unwrap_or(vk::Result::ERROR_INITIALIZATION_FAILED)
}

/// # Safety
/// Called only from `hook_create_device`, with the loader's own arguments.
unsafe fn create_device(
    physical_device: vk::PhysicalDevice,
    create_info: *const vk::DeviceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_device: *mut vk::Device,
) -> vk::Result {
    let (real_gipa, real_create_device, instance, features2) = {
        let hooks = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        (
            hooks.real_gipa,
            hooks.real_create_device,
            hooks.instance,
            hooks.features2,
        )
    };
    // As in `create_instance`: without the real function there is nothing
    // to forward to. A missing gipa costs the two probes below, and both
    // of them read learning nothing as leaving the caller's device alone.
    let Some(real_create_device) = real_create_device else {
        unity::log("vkCreateDevice: hook reached with no real fn");
        return vk::Result::ERROR_INITIALIZATION_FAILED;
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
        if let Some(real_gipa) = real_gipa
            && let Some(instance) = instance
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
        // else prepend our own. Only where the driver has it, the same
        // rule the extension list above follows — asking for a feature a
        // device lacks fails the create outright, and this create is the
        // app's own.
        //
        // The in-place arm writes into Unity's own structures, which is
        // the price of the spec's rule: the two feature structs may not
        // both be chained, so where the caller has one it is the only
        // place the bit can be set, and copying a chain of arbitrary
        // types to edit a copy is not something this hook can do
        // generically. The write is idempotent and turns the feature on
        // rather than off, so a chain Unity reuses for a later create
        // carries a request it already made here.
        let mut our_ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
        let mut local_ci = *ci;
        let probe = ycbcr_probe(real_gipa, instance, features2, physical_device);
        if matches!(probe, YcbcrProbe::Advertised) {
            let mut ycbcr_found = false;
            let mut chain = ci.p_next as *mut vk::BaseOutStructure<'_>;
            // Bounded: the chain is the caller's, and a cycle in one this
            // hook does not own would spin inside `vkCreateDevice` on the
            // app's own thread, where the panic fence is no help. Far
            // past any real chain, so a sound one is unaffected.
            let mut hops = 0u32;
            while !chain.is_null() && hops < 64 {
                hops += 1;
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
            // Prepending is only safe where the walk reached the end of
            // the chain. The two feature structs may not both be
            // chained, so a walk that stopped at the hop cap may have a
            // caller's one past it, and adding ours would make two — the
            // same `ERROR_FEATURE_NOT_PRESENT` class of failure on the
            // app's own device that the probe above exists to avoid.
            if !ycbcr_found {
                if chain.is_null() {
                    our_ycbcr.sampler_ycbcr_conversion = vk::TRUE;
                    our_ycbcr.p_next = ci.p_next as *mut c_void;
                    local_ci.p_next = &raw const our_ycbcr as *const c_void;
                } else {
                    unity::log(
                        "vkCreateDevice: feature chain longer than the hop cap, \
                         samplerYcbcrConversion left as the caller set it",
                    );
                }
            }
        } else {
            let cause = match probe {
                YcbcrProbe::Unprobed(why) => why,
                _ => "the driver does not advertise it",
            };
            logf!("vkCreateDevice: samplerYcbcrConversion left as the caller set it ({cause})");
        }
        local_ci.enabled_extension_count = ext_ptrs.len() as u32;
        local_ci.pp_enabled_extension_names = ext_ptrs.as_ptr();

        let result = real_create_device(physical_device, &local_ci, allocator, out_device);
        logf!("vkCreateDevice: hooked, result={result:?}");
        result
    }
}
