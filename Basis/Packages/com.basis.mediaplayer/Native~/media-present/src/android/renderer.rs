//! The per-session render pass (§6.8): import the decoder's
//! `AHardwareBuffer` into Unity's `VkDevice` and run one compute dispatch
//! converting into Unity's RGBA RenderTexture, recorded on Unity's
//! current command buffer inside the render event. The driver's
//! per-buffer suggested `VkSamplerYcbcrConversion` does the matrix/range
//! work (read per buffer — decoder buffers carry their own dataspace, the
//! M0 lesson); GPU lifetime rides Unity's frame counters
//! (`safeFrameNumber`), so nothing is destroyed while a submitted command
//! buffer might still read it.

use std::collections::{HashMap, VecDeque};

use ash::vk;
use ash::vk::Handle;
use media_decode::VideoFrame;

use super::fns::DeviceFns;
use super::intercept::{CTX, VkCtx};
use super::unity::{self, logf};

/// Import-cache ceiling: the reader recycles a fixed buffer set (~10), so
/// steady state sits below this; the bound only matters across decoder
/// rebuilds.
const IMPORT_CACHE_CAP: usize = 16;
/// Descriptor ring depth: must exceed the deepest frames-in-flight Unity
/// runs (double/triple buffering), or an in-use set would be rewritten.
const DESC_RING: u32 = 16;

#[repr(C)]
struct Push {
    dst_w: i32,
    dst_h: i32,
    inv_coded_w: f32,
    inv_coded_h: f32,
    flip_x: i32,
    flip_y: i32,
}

/// Everything keyed by the driver's suggested conversion for the current
/// buffer family. Rebuilt when the suggestion changes (a stream/decoder
/// change), retired through the frame-number queue.
struct ConvertObjects {
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    next_set: usize,
}

impl ConvertObjects {
    fn destroy(&self, device: vk::Device, fns: &DeviceFns) {
        // SAFETY: called only once the retire queue proves no submitted
        // command buffer references these objects.
        unsafe {
            (fns.destroy_descriptor_pool)(device, self.pool, core::ptr::null());
            (fns.destroy_pipeline)(device, self.pipeline, core::ptr::null());
            (fns.destroy_pipeline_layout)(device, self.pipe_layout, core::ptr::null());
            (fns.destroy_descriptor_set_layout)(device, self.set_layout, core::ptr::null());
            (fns.destroy_sampler)(device, self.sampler, core::ptr::null());
            (fns.destroy_ycbcr_conversion)(device, self.conversion, core::ptr::null());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConvertKey {
    format: i32,
    external_format: u64,
    model: i32,
    range: i32,
    x_chroma: i32,
    y_chroma: i32,
    filter: i32,
}

struct Imported {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
    conv_gen: u64,
    last_used: u64,
}

impl Imported {
    fn destroy(&self, device: vk::Device, fns: &DeviceFns) {
        // SAFETY: retire-queue discipline as above. Freeing the memory
        // releases the import's AHardwareBuffer reference.
        unsafe {
            (fns.destroy_image_view)(device, self.view, core::ptr::null());
            (fns.destroy_image)(device, self.image, core::ptr::null());
            (fns.free_memory)(device, self.memory, core::ptr::null());
        }
    }
}

enum Retired {
    Frame(#[allow(dead_code)] VideoFrame),
    Import(Imported),
    Convert(ConvertObjects),
    DstView(vk::ImageView),
}

impl Default for SessionRenderer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SessionRenderer {
    convert: Option<ConvertObjects>,
    convert_key: Option<ConvertKey>,
    conv_gen: u64,
    imports: HashMap<usize, Imported>,
    dst_views: HashMap<u64, vk::ImageView>,
    /// (frame number the item was last referenced in, item).
    retired: VecDeque<(u64, Retired)>,
    warned_no_ctx: bool,
    warned_no_recording: bool,
    warned_format: bool,
    warned_cpu_frame: bool,
}

// SAFETY: only ever driven from Unity's render thread; VideoFrame
// payloads are Send.
unsafe impl Send for SessionRenderer {}

impl SessionRenderer {
    pub fn new() -> Self {
        Self {
            convert: None,
            convert_key: None,
            conv_gen: 0,
            imports: HashMap::new(),
            dst_views: HashMap::new(),
            retired: VecDeque::new(),
            warned_no_ctx: false,
            warned_no_recording: false,
            warned_format: false,
            warned_cpu_frame: false,
        }
    }

    /// Draw `frame` (if any) into the registered Unity texture. Returns
    /// true when a fresh frame was recorded. Failures log (once per
    /// cause where repetitive) and drop the frame — the render thread
    /// never blocks and never panics.
    pub fn render(
        &mut self,
        frame: Option<VideoFrame>,
        unity_texture: *mut core::ffi::c_void,
    ) -> bool {
        let mut guard = CTX.lock().expect("ctx lock");
        let Some(ctx) = guard.as_mut() else {
            if !self.warned_no_ctx {
                self.warned_no_ctx = true;
                unity::log("render: no vulkan context (device event not seen) — dropping frames");
            }
            return false;
        };

        // Frame counters first: they drive retirement even on idle calls.
        // SAFETY: Unity vtable calls on the render thread, the documented
        // call site for both.
        let recording = unsafe {
            let mut state = core::mem::zeroed::<unity::UnityVulkanRecordingState>();
            if !((*ctx.vulkan_iface).command_recording_state)(
                &mut state,
                unity::QUEUE_ACCESS_DONT_CARE,
            ) || state.command_buffer == vk::CommandBuffer::null()
            {
                if !self.warned_no_recording {
                    self.warned_no_recording = true;
                    unity::log(
                        "render: no command recording state at the event's call site — frames cannot draw",
                    );
                }
                return false;
            }
            state
        };
        self.drain_retired(ctx, recording.safe_frame_number);
        graveyard::collect(
            ctx,
            recording.current_frame_number,
            recording.safe_frame_number,
        );

        let Some(frame) = frame else {
            return false;
        };
        let opaque = match frame {
            VideoFrame::Opaque(ref f) => f,
            VideoFrame::Nv12(_) => {
                if !self.warned_cpu_frame {
                    self.warned_cpu_frame = true;
                    unity::log("render: CPU frame on the Vulkan path (no upload pass) — dropped");
                }
                return false;
            }
        };
        if unity_texture.is_null() {
            return false;
        }

        let drew = self.draw(ctx, &recording, opaque, unity_texture);
        // The frame's buffer must outlive the submitted command buffer.
        self.retired
            .push_back((recording.current_frame_number, Retired::Frame(frame)));
        drew
    }

    fn draw(
        &mut self,
        ctx: &VkCtx,
        recording: &unity::UnityVulkanRecordingState,
        frame: &media_decode::OpaqueFrame,
        unity_texture: *mut core::ffi::c_void,
    ) -> bool {
        let buffer = frame.image.hardware_buffer();
        if buffer.is_null() {
            return false;
        }

        // Per-buffer import properties + the driver's suggested
        // conversion — read for every buffer, never assumed.
        let (props, fmt_props) = {
            let mut fmt_props = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
            let mut props = vk::AndroidHardwareBufferPropertiesANDROID {
                p_next: (&raw mut fmt_props).cast(),
                ..Default::default()
            };
            // SAFETY: live device + AHB (owned by the frame in hand).
            let result = unsafe { (ctx.fns.get_ahb_props)(ctx.device, buffer.cast(), &mut props) };
            if result != vk::Result::SUCCESS {
                logf!("render: GetAndroidHardwareBufferProperties {result:?}");
                return false;
            }
            (props, fmt_props)
        };

        let chroma_filter = if fmt_props
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER)
        {
            vk::Filter::LINEAR
        } else {
            vk::Filter::NEAREST
        };
        let key = ConvertKey {
            format: fmt_props.format.as_raw(),
            external_format: fmt_props.external_format,
            model: fmt_props.suggested_ycbcr_model.as_raw(),
            range: fmt_props.suggested_ycbcr_range.as_raw(),
            x_chroma: fmt_props.suggested_x_chroma_offset.as_raw(),
            y_chroma: fmt_props.suggested_y_chroma_offset.as_raw(),
            filter: chroma_filter.as_raw(),
        };
        if self.convert_key != Some(key) {
            if let Some(old) = self.convert.take() {
                self.retired
                    .push_back((recording.current_frame_number, Retired::Convert(old)));
            }
            match build_convert(ctx, &fmt_props, chroma_filter) {
                Ok(objects) => {
                    self.convert = Some(objects);
                    self.convert_key = Some(key);
                    self.conv_gen += 1;
                    logf!(
                        "render: conversion built (externalFormat=0x{:x} model={:?} range={:?})",
                        fmt_props.external_format,
                        fmt_props.suggested_ycbcr_model,
                        fmt_props.suggested_ycbcr_range
                    );
                }
                Err(what) => {
                    logf!("render: conversion build failed: {what}");
                    return false;
                }
            }
        }

        // Import (or reuse) this buffer's VkImage. The memory import holds
        // its own AHB reference, so a cached entry stays valid across the
        // reader recycling the buffer.
        let ahb_key = buffer as usize;
        let stale = self
            .imports
            .get(&ahb_key)
            .is_some_and(|entry| entry.conv_gen != self.conv_gen);
        if stale && let Some(old) = self.imports.remove(&ahb_key) {
            self.retired
                .push_back((old.last_used, Retired::Import(old)));
        }
        if !self.imports.contains_key(&ahb_key) {
            let convert = self.convert.as_ref().expect("built above");
            match import_buffer(
                ctx,
                buffer,
                &props,
                &fmt_props,
                convert.conversion,
                frame.width.max(1),
                frame.height.max(1),
                self.conv_gen,
            ) {
                Ok(imported) => {
                    self.evict_imports(recording.current_frame_number);
                    self.imports.insert(ahb_key, imported);
                }
                Err(what) => {
                    logf!("render: import failed: {what}");
                    return false;
                }
            }
        }

        // SAFETY: Unity vtable + Vulkan recording below run on the render
        // thread against live handles; barriers/descriptors follow the
        // §6.8 contract described inline.
        unsafe {
            ((*ctx.vulkan_iface).ensure_outside_render_pass)();

            // Unity handles the destination's transition + tracking.
            let mut dst = core::mem::zeroed::<unity::UnityVulkanImage>();
            if !((*ctx.vulkan_iface).access_texture)(
                unity_texture,
                core::ptr::null(),
                vk::ImageLayout::GENERAL,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::AccessFlags::SHADER_WRITE,
                unity::ACCESS_PIPELINE_BARRIER,
                &mut dst,
            ) {
                return false;
            }
            if dst.image == vk::Image::null() {
                return false;
            }
            // The shader writes through an rgba8 storage view; sRGB (or
            // non-RGBA8) targets cannot take storage writes here. The
            // managed contract mirrors D3D11's: a linear RGBA32 target.
            if dst.format != vk::Format::R8G8B8A8_UNORM {
                if !self.warned_format {
                    self.warned_format = true;
                    logf!(
                        "render: unsupported output format {:?} (need R8G8B8A8_UNORM, linear)",
                        dst.format
                    );
                }
                return false;
            }

            // Access calls invalidate the recording state: re-fetch.
            let mut state = core::mem::zeroed::<unity::UnityVulkanRecordingState>();
            if !((*ctx.vulkan_iface).command_recording_state)(
                &mut state,
                unity::QUEUE_ACCESS_DONT_CARE,
            ) || state.command_buffer == vk::CommandBuffer::null()
            {
                return false;
            }
            let cb = state.command_buffer;

            let dst_view = match self.dst_views.entry(dst.image.as_raw()) {
                std::collections::hash_map::Entry::Occupied(e) => *e.get(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let view_ci = vk::ImageViewCreateInfo {
                        image: dst.image,
                        view_type: vk::ImageViewType::TYPE_2D,
                        format: dst.format,
                        subresource_range: vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        ..Default::default()
                    };
                    let mut view = vk::ImageView::null();
                    let result = (ctx.fns.create_image_view)(
                        ctx.device,
                        &view_ci,
                        core::ptr::null(),
                        &mut view,
                    );
                    if result != vk::Result::SUCCESS {
                        logf!("render: dst view {result:?}");
                        return false;
                    }
                    *e.insert(view)
                }
            };

            let imported = self.imports.get_mut(&ahb_key).expect("imported above");
            imported.last_used = state.current_frame_number;

            // Acquire the buffer from the codec (foreign queue family);
            // contents were written outside Vulkan, so UNDEFINED discards
            // nothing we need... except the content itself arrives via the
            // external-memory guarantee, not the layout transition.
            let barrier = vk::ImageMemoryBarrier {
                src_access_mask: vk::AccessFlags::empty(),
                dst_access_mask: vk::AccessFlags::SHADER_READ,
                old_layout: vk::ImageLayout::UNDEFINED,
                new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_FOREIGN_EXT,
                dst_queue_family_index: ctx.queue_family,
                image: imported.image,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            };
            (ctx.fns.cmd_pipeline_barrier)(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                1,
                &barrier,
            );

            let convert = self.convert.as_mut().expect("built above");
            let set = convert.sets[convert.next_set % convert.sets.len()];
            convert.next_set = convert.next_set.wrapping_add(1);

            let src_info = vk::DescriptorImageInfo {
                sampler: convert.sampler,
                image_view: imported.view,
                image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            };
            let dst_info = vk::DescriptorImageInfo {
                sampler: vk::Sampler::null(),
                image_view: dst_view,
                image_layout: vk::ImageLayout::GENERAL,
            };
            let writes = [
                vk::WriteDescriptorSet {
                    dst_set: set,
                    dst_binding: 0,
                    descriptor_count: 1,
                    descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    p_image_info: &src_info,
                    ..Default::default()
                },
                vk::WriteDescriptorSet {
                    dst_set: set,
                    dst_binding: 1,
                    descriptor_count: 1,
                    descriptor_type: vk::DescriptorType::STORAGE_IMAGE,
                    p_image_info: &dst_info,
                    ..Default::default()
                },
            ];
            (ctx.fns.update_descriptor_sets)(
                ctx.device,
                writes.len() as u32,
                writes.as_ptr(),
                0,
                core::ptr::null(),
            );

            (ctx.fns.cmd_bind_pipeline)(cb, vk::PipelineBindPoint::COMPUTE, convert.pipeline);
            (ctx.fns.cmd_bind_descriptor_sets)(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                convert.pipe_layout,
                0,
                1,
                &set,
                0,
                core::ptr::null(),
            );
            let push = Push {
                dst_w: dst.extent.width as i32,
                dst_h: dst.extent.height as i32,
                inv_coded_w: 1.0 / imported.width.max(1) as f32,
                inv_coded_h: 1.0 / imported.height.max(1) as f32,
                // Unity samples an externally written RenderTexture
                // vertically flipped on Vulkan (row 0 is the on-screen
                // bottom; pinned empirically on the Quest pass), so the
                // pass writes rows inverted.
                flip_x: 0,
                flip_y: 1,
            };
            (ctx.fns.cmd_push_constants)(
                cb,
                convert.pipe_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                core::mem::size_of::<Push>() as u32,
                (&raw const push).cast(),
            );
            (ctx.fns.cmd_dispatch)(
                cb,
                dst.extent.width.div_ceil(8),
                dst.extent.height.div_ceil(8),
                1,
            );
        }
        true
    }

    fn evict_imports(&mut self, frame_number: u64) {
        while self.imports.len() >= IMPORT_CACHE_CAP {
            let Some((&key, _)) = self.imports.iter().min_by_key(|(_, entry)| entry.last_used)
            else {
                return;
            };
            let old = self.imports.remove(&key).expect("key from iteration");
            self.retired.push_back((frame_number, Retired::Import(old)));
        }
    }

    fn drain_retired(&mut self, ctx: &VkCtx, safe_frame_number: u64) {
        while let Some((frame_number, _)) = self.retired.front() {
            if *frame_number > safe_frame_number {
                return;
            }
            match self.retired.pop_front().expect("front checked").1 {
                Retired::Frame(frame) => drop(frame),
                Retired::Import(imported) => imported.destroy(ctx.device, &ctx.fns),
                Retired::Convert(convert) => convert.destroy(ctx.device, &ctx.fns),
                Retired::DstView(view) => {
                    // SAFETY: retire discipline as elsewhere.
                    unsafe { (ctx.fns.destroy_image_view)(ctx.device, view, core::ptr::null()) };
                }
            }
        }
    }
}

impl Drop for SessionRenderer {
    fn drop(&mut self) {
        // Sessions close from the main thread while Unity may still be
        // submitting: Vulkan objects go to the process-wide graveyard,
        // drained by later render events (any session) once their frames
        // are provably retired. Frames (AImages) drop here — the decoder
        // side owns their lifetime rules.
        let mut items: Vec<Retired> = Vec::new();
        if let Some(convert) = self.convert.take() {
            items.push(Retired::Convert(convert));
        }
        for (_, imported) in self.imports.drain() {
            items.push(Retired::Import(imported));
        }
        for (_, view) in self.dst_views.drain() {
            items.push(Retired::DstView(view));
        }
        for (_, item) in self.retired.drain(..) {
            items.push(item);
        }
        graveyard::bury(items);
    }
}

fn build_convert(
    ctx: &VkCtx,
    fmt_props: &vk::AndroidHardwareBufferFormatPropertiesANDROID<'_>,
    chroma_filter: vk::Filter,
) -> Result<ConvertObjects, String> {
    let external = fmt_props.format == vk::Format::UNDEFINED;
    // SAFETY: object creation against the live device; every result is
    // checked and partially built objects are destroyed on the error
    // paths via the small `cleanup` closure discipline below.
    unsafe {
        let ext_format = vk::ExternalFormatANDROID {
            external_format: fmt_props.external_format,
            ..Default::default()
        };
        let conv_ci = vk::SamplerYcbcrConversionCreateInfo {
            p_next: if external {
                (&raw const ext_format).cast()
            } else {
                core::ptr::null()
            },
            format: fmt_props.format,
            ycbcr_model: fmt_props.suggested_ycbcr_model,
            ycbcr_range: fmt_props.suggested_ycbcr_range,
            components: fmt_props.sampler_ycbcr_conversion_components,
            x_chroma_offset: fmt_props.suggested_x_chroma_offset,
            y_chroma_offset: fmt_props.suggested_y_chroma_offset,
            chroma_filter,
            ..Default::default()
        };
        let mut conversion = vk::SamplerYcbcrConversion::null();
        let result = (ctx.fns.create_ycbcr_conversion)(
            ctx.device,
            &conv_ci,
            core::ptr::null(),
            &mut conversion,
        );
        if result != vk::Result::SUCCESS {
            return Err(format!("CreateSamplerYcbcrConversion {result:?}"));
        }

        let conv_info = vk::SamplerYcbcrConversionInfo {
            conversion,
            ..Default::default()
        };
        let samp_ci = vk::SamplerCreateInfo {
            p_next: (&raw const conv_info).cast(),
            mag_filter: chroma_filter,
            min_filter: chroma_filter,
            address_mode_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            address_mode_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            address_mode_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
            max_anisotropy: 1.0,
            ..Default::default()
        };
        let mut sampler = vk::Sampler::null();
        let result =
            (ctx.fns.create_sampler)(ctx.device, &samp_ci, core::ptr::null(), &mut sampler);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_ycbcr_conversion)(ctx.device, conversion, core::ptr::null());
            return Err(format!("CreateSampler {result:?}"));
        }

        // Set layout: binding 0 samples YCbCr through the immutable
        // sampler (required for conversion sampling); binding 1 is the
        // RGBA storage target.
        let bindings = [
            vk::DescriptorSetLayoutBinding {
                binding: 0,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::COMPUTE,
                p_immutable_samplers: &sampler,
                ..Default::default()
            },
            vk::DescriptorSetLayoutBinding {
                binding: 1,
                descriptor_type: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::COMPUTE,
                ..Default::default()
            },
        ];
        let layout_ci = vk::DescriptorSetLayoutCreateInfo {
            binding_count: bindings.len() as u32,
            p_bindings: bindings.as_ptr(),
            ..Default::default()
        };
        let mut set_layout = vk::DescriptorSetLayout::null();
        let result = (ctx.fns.create_descriptor_set_layout)(
            ctx.device,
            &layout_ci,
            core::ptr::null(),
            &mut set_layout,
        );
        let cleanup_base = |upto: u32| {
            if upto >= 2 {
                (ctx.fns.destroy_descriptor_set_layout)(ctx.device, set_layout, core::ptr::null());
            }
            (ctx.fns.destroy_sampler)(ctx.device, sampler, core::ptr::null());
            (ctx.fns.destroy_ycbcr_conversion)(ctx.device, conversion, core::ptr::null());
        };
        if result != vk::Result::SUCCESS {
            cleanup_base(1);
            return Err(format!("CreateDescriptorSetLayout {result:?}"));
        }

        let push_range = vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: core::mem::size_of::<Push>() as u32,
        };
        let pipe_layout_ci = vk::PipelineLayoutCreateInfo {
            set_layout_count: 1,
            p_set_layouts: &set_layout,
            push_constant_range_count: 1,
            p_push_constant_ranges: &push_range,
            ..Default::default()
        };
        let mut pipe_layout = vk::PipelineLayout::null();
        let result = (ctx.fns.create_pipeline_layout)(
            ctx.device,
            &pipe_layout_ci,
            core::ptr::null(),
            &mut pipe_layout,
        );
        if result != vk::Result::SUCCESS {
            cleanup_base(2);
            return Err(format!("CreatePipelineLayout {result:?}"));
        }

        const SPV: &[u8] = include_bytes!("../../shaders/yuv_to_rgba.spv");
        let code: Vec<u32> = SPV
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let module_ci = vk::ShaderModuleCreateInfo {
            code_size: code.len() * 4,
            p_code: code.as_ptr(),
            ..Default::default()
        };
        let mut module = vk::ShaderModule::null();
        let result =
            (ctx.fns.create_shader_module)(ctx.device, &module_ci, core::ptr::null(), &mut module);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_pipeline_layout)(ctx.device, pipe_layout, core::ptr::null());
            cleanup_base(2);
            return Err(format!("CreateShaderModule {result:?}"));
        }

        let stage = vk::PipelineShaderStageCreateInfo {
            stage: vk::ShaderStageFlags::COMPUTE,
            module,
            p_name: c"main".as_ptr(),
            ..Default::default()
        };
        let pipeline_ci = vk::ComputePipelineCreateInfo {
            stage,
            layout: pipe_layout,
            ..Default::default()
        };
        let mut pipeline = vk::Pipeline::null();
        let result = (ctx.fns.create_compute_pipelines)(
            ctx.device,
            vk::PipelineCache::null(),
            1,
            &pipeline_ci,
            core::ptr::null(),
            &mut pipeline,
        );
        (ctx.fns.destroy_shader_module)(ctx.device, module, core::ptr::null());
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_pipeline_layout)(ctx.device, pipe_layout, core::ptr::null());
            cleanup_base(2);
            return Err(format!("CreateComputePipelines {result:?}"));
        }

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: DESC_RING,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: DESC_RING,
            },
        ];
        let pool_ci = vk::DescriptorPoolCreateInfo {
            max_sets: DESC_RING,
            pool_size_count: pool_sizes.len() as u32,
            p_pool_sizes: pool_sizes.as_ptr(),
            ..Default::default()
        };
        let mut pool = vk::DescriptorPool::null();
        let result =
            (ctx.fns.create_descriptor_pool)(ctx.device, &pool_ci, core::ptr::null(), &mut pool);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_pipeline)(ctx.device, pipeline, core::ptr::null());
            (ctx.fns.destroy_pipeline_layout)(ctx.device, pipe_layout, core::ptr::null());
            cleanup_base(2);
            return Err(format!("CreateDescriptorPool {result:?}"));
        }

        let layouts = vec![set_layout; DESC_RING as usize];
        let alloc = vk::DescriptorSetAllocateInfo {
            descriptor_pool: pool,
            descriptor_set_count: DESC_RING,
            p_set_layouts: layouts.as_ptr(),
            ..Default::default()
        };
        let mut sets = vec![vk::DescriptorSet::null(); DESC_RING as usize];
        let result = (ctx.fns.allocate_descriptor_sets)(ctx.device, &alloc, sets.as_mut_ptr());
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_descriptor_pool)(ctx.device, pool, core::ptr::null());
            (ctx.fns.destroy_pipeline)(ctx.device, pipeline, core::ptr::null());
            (ctx.fns.destroy_pipeline_layout)(ctx.device, pipe_layout, core::ptr::null());
            cleanup_base(2);
            return Err(format!("AllocateDescriptorSets {result:?}"));
        }

        Ok(ConvertObjects {
            conversion,
            sampler,
            set_layout,
            pipe_layout,
            pipeline,
            pool,
            sets,
            next_set: 0,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn import_buffer(
    ctx: &VkCtx,
    buffer: *mut core::ffi::c_void,
    props: &vk::AndroidHardwareBufferPropertiesANDROID<'_>,
    fmt_props: &vk::AndroidHardwareBufferFormatPropertiesANDROID<'_>,
    conversion: vk::SamplerYcbcrConversion,
    width: u32,
    height: u32,
    conv_gen: u64,
) -> Result<Imported, String> {
    let external = fmt_props.format == vk::Format::UNDEFINED;
    // SAFETY: the M0-validated import sequence — external-format image,
    // dedicated allocation importing the AHB (which takes its own AHB
    // reference), bind, then a view carrying the conversion. All results
    // checked; partial objects destroyed on error.
    unsafe {
        let img_ext_format = vk::ExternalFormatANDROID {
            external_format: fmt_props.external_format,
            ..Default::default()
        };
        let ext_mem = vk::ExternalMemoryImageCreateInfo {
            p_next: if external {
                (&raw const img_ext_format).cast()
            } else {
                core::ptr::null()
            },
            handle_types: vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID,
            ..Default::default()
        };
        let img_ci = vk::ImageCreateInfo {
            p_next: (&raw const ext_mem).cast(),
            image_type: vk::ImageType::TYPE_2D,
            format: fmt_props.format,
            extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: 1,
            samples: vk::SampleCountFlags::TYPE_1,
            tiling: vk::ImageTiling::OPTIMAL,
            usage: vk::ImageUsageFlags::SAMPLED,
            sharing_mode: vk::SharingMode::EXCLUSIVE,
            initial_layout: vk::ImageLayout::UNDEFINED,
            ..Default::default()
        };
        let mut image = vk::Image::null();
        let result = (ctx.fns.create_image)(ctx.device, &img_ci, core::ptr::null(), &mut image);
        if result != vk::Result::SUCCESS {
            return Err(format!("CreateImage {result:?}"));
        }

        let import_info = vk::ImportAndroidHardwareBufferInfoANDROID {
            buffer: buffer.cast(),
            ..Default::default()
        };
        let dedicated = vk::MemoryDedicatedAllocateInfo {
            p_next: (&raw const import_info).cast(),
            image,
            ..Default::default()
        };
        let alloc = vk::MemoryAllocateInfo {
            p_next: (&raw const dedicated).cast(),
            allocation_size: props.allocation_size,
            memory_type_index: props.memory_type_bits.trailing_zeros(),
            ..Default::default()
        };
        let mut memory = vk::DeviceMemory::null();
        let result = (ctx.fns.allocate_memory)(ctx.device, &alloc, core::ptr::null(), &mut memory);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_image)(ctx.device, image, core::ptr::null());
            return Err(format!("AllocateMemory (import) {result:?}"));
        }
        let result = (ctx.fns.bind_image_memory)(ctx.device, image, memory, 0);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_image)(ctx.device, image, core::ptr::null());
            (ctx.fns.free_memory)(ctx.device, memory, core::ptr::null());
            return Err(format!("BindImageMemory {result:?}"));
        }

        let conv_info = vk::SamplerYcbcrConversionInfo {
            conversion,
            ..Default::default()
        };
        let view_ci = vk::ImageViewCreateInfo {
            p_next: (&raw const conv_info).cast(),
            image,
            view_type: vk::ImageViewType::TYPE_2D,
            format: fmt_props.format,
            subresource_range: vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..Default::default()
        };
        let mut view = vk::ImageView::null();
        let result =
            (ctx.fns.create_image_view)(ctx.device, &view_ci, core::ptr::null(), &mut view);
        if result != vk::Result::SUCCESS {
            (ctx.fns.destroy_image)(ctx.device, image, core::ptr::null());
            (ctx.fns.free_memory)(ctx.device, memory, core::ptr::null());
            return Err(format!("CreateImageView {result:?}"));
        }

        Ok(Imported {
            image,
            memory,
            view,
            width,
            height,
            conv_gen,
            last_used: 0,
        })
    }
}

mod graveyard {
    //! Vulkan objects whose owner (a closing session) cannot prove GPU
    //! quiescence: parked here, destroyed by later render events once
    //! `safeFrameNumber` passes the burial frame. If no render event ever
    //! runs again the objects leak until process end — bounded and
    //! preferable to destroying in-flight resources.

    use std::sync::Mutex;

    use super::{Retired, VkCtx};

    static GRAVE: Mutex<Vec<(Option<u64>, Vec<Retired>)>> = Mutex::new(Vec::new());

    pub fn bury(items: Vec<Retired>) {
        if items.is_empty() {
            return;
        }
        GRAVE.lock().expect("grave lock").push((None, items));
    }

    /// Called from render events: stamp new burials with the current
    /// frame, destroy those safely past.
    pub fn collect(ctx: &VkCtx, current_frame: u64, safe_frame: u64) {
        let mut grave = GRAVE.lock().expect("grave lock");
        for (stamp, _) in grave.iter_mut() {
            stamp.get_or_insert(current_frame);
        }
        let mut index = 0;
        while index < grave.len() {
            if grave[index].0.is_some_and(|stamp| stamp <= safe_frame) {
                let (_, items) = grave.swap_remove(index);
                for item in items {
                    match item {
                        Retired::Frame(frame) => drop(frame),
                        Retired::Import(imported) => imported.destroy(ctx.device, &ctx.fns),
                        Retired::Convert(convert) => convert.destroy(ctx.device, &ctx.fns),
                        Retired::DstView(view) => {
                            // SAFETY: safe-frame discipline as above.
                            unsafe {
                                (ctx.fns.destroy_image_view)(ctx.device, view, core::ptr::null())
                            };
                        }
                    }
                }
            } else {
                index += 1;
            }
        }
    }
}
