//! The Vulkan device functions the present pass calls, loaded through
//! Unity's `vkGetInstanceProcAddr` (the M0 discipline: no loader, no
//! global function tables — everything comes off the chain Unity itself
//! uses).

use core::ffi::CStr;

use ash::vk;

macro_rules! device_fns {
    ($( $field:ident : $ty:ident => $name:literal $(| $fallback:literal)? ),+ $(,)?) => {
        pub(crate) struct DeviceFns {
            $( pub $field: vk::$ty, )+
        }

        impl DeviceFns {
            /// Load every function or name the first one missing.
            pub fn load(
                gipa: vk::PFN_vkGetInstanceProcAddr,
                instance: vk::Instance,
                device: vk::Device,
            ) -> Result<Self, &'static str> {
                // SAFETY: standard two-step Vulkan function loading with
                // live instance/device handles; each PFN transmute casts
                // the erased pointer to its declared type.
                unsafe {
                    let gdpa = gipa(instance, c"vkGetDeviceProcAddr".as_ptr())
                        .ok_or("vkGetDeviceProcAddr")?;
                    let gdpa = core::mem::transmute::<
                        unsafe extern "system" fn(),
                        vk::PFN_vkGetDeviceProcAddr,
                    >(gdpa);
                    let load = |names: &[&CStr]| -> Option<unsafe extern "system" fn()> {
                        names.iter().find_map(|n| gdpa(device, n.as_ptr()))
                    };
                    Ok(Self {
                        $(
                            $field: {
                                let names: &[&CStr] = &[
                                    {
                                        const BYTES: &[u8] = concat!($name, "\0").as_bytes();
                                        CStr::from_bytes_with_nul(BYTES).unwrap()
                                    },
                                    $(
                                        {
                                            const BYTES: &[u8] =
                                                concat!($fallback, "\0").as_bytes();
                                            CStr::from_bytes_with_nul(BYTES).unwrap()
                                        },
                                    )?
                                ];
                                let f = load(names).ok_or($name)?;
                                core::mem::transmute::<
                                    unsafe extern "system" fn(),
                                    vk::$ty,
                                >(f)
                            },
                        )+
                    })
                }
            }
        }
    };
}

device_fns! {
    get_ahb_props: PFN_vkGetAndroidHardwareBufferPropertiesANDROID
        => "vkGetAndroidHardwareBufferPropertiesANDROID",
    create_ycbcr_conversion: PFN_vkCreateSamplerYcbcrConversion
        => "vkCreateSamplerYcbcrConversion" | "vkCreateSamplerYcbcrConversionKHR",
    destroy_ycbcr_conversion: PFN_vkDestroySamplerYcbcrConversion
        => "vkDestroySamplerYcbcrConversion" | "vkDestroySamplerYcbcrConversionKHR",
    create_image: PFN_vkCreateImage => "vkCreateImage",
    destroy_image: PFN_vkDestroyImage => "vkDestroyImage",
    allocate_memory: PFN_vkAllocateMemory => "vkAllocateMemory",
    free_memory: PFN_vkFreeMemory => "vkFreeMemory",
    bind_image_memory: PFN_vkBindImageMemory => "vkBindImageMemory",
    create_image_view: PFN_vkCreateImageView => "vkCreateImageView",
    destroy_image_view: PFN_vkDestroyImageView => "vkDestroyImageView",
    create_sampler: PFN_vkCreateSampler => "vkCreateSampler",
    destroy_sampler: PFN_vkDestroySampler => "vkDestroySampler",
    create_shader_module: PFN_vkCreateShaderModule => "vkCreateShaderModule",
    destroy_shader_module: PFN_vkDestroyShaderModule => "vkDestroyShaderModule",
    create_descriptor_set_layout: PFN_vkCreateDescriptorSetLayout
        => "vkCreateDescriptorSetLayout",
    destroy_descriptor_set_layout: PFN_vkDestroyDescriptorSetLayout
        => "vkDestroyDescriptorSetLayout",
    create_pipeline_layout: PFN_vkCreatePipelineLayout => "vkCreatePipelineLayout",
    destroy_pipeline_layout: PFN_vkDestroyPipelineLayout => "vkDestroyPipelineLayout",
    create_compute_pipelines: PFN_vkCreateComputePipelines => "vkCreateComputePipelines",
    destroy_pipeline: PFN_vkDestroyPipeline => "vkDestroyPipeline",
    create_descriptor_pool: PFN_vkCreateDescriptorPool => "vkCreateDescriptorPool",
    destroy_descriptor_pool: PFN_vkDestroyDescriptorPool => "vkDestroyDescriptorPool",
    allocate_descriptor_sets: PFN_vkAllocateDescriptorSets => "vkAllocateDescriptorSets",
    update_descriptor_sets: PFN_vkUpdateDescriptorSets => "vkUpdateDescriptorSets",
    cmd_pipeline_barrier: PFN_vkCmdPipelineBarrier => "vkCmdPipelineBarrier",
    cmd_bind_pipeline: PFN_vkCmdBindPipeline => "vkCmdBindPipeline",
    cmd_bind_descriptor_sets: PFN_vkCmdBindDescriptorSets => "vkCmdBindDescriptorSets",
    cmd_push_constants: PFN_vkCmdPushConstants => "vkCmdPushConstants",
    cmd_dispatch: PFN_vkCmdDispatch => "vkCmdDispatch",
}
