# Device probes

On-device capability dumps that feed decode/present design decisions. Each file is the
raw output of a probe run on real hardware; filenames carry the device and OS build so
later runs on other devices or OS versions can sit alongside.

## Quest Pro, Meta OS v206 (Android 14, Adreno 650) — 2026-08-13

- `quest-pro-v206-mediacodec.json` — full `MediaCodecList` dump: codec names, hardware/
  software flags, per-mime profile/level lists, resolution/frame-rate/bitrate ranges and
  performance points. Gathered in-app via `MediaCodecList.getCodecInfoAt`.
- `quest-pro-v206-vulkan-interception.txt` — report from a Unity 6 (6000.5) + OpenXR +
  Vulkan harness whose native plugin registered a Vulkan init interceptor via
  `IUnityGraphicsVulkanV2::AddInterceptInitialization`, then imported a synthetic YUV420
  `AHardwareBuffer` into Unity's `VkDevice` as a YCbCr image (dedicated allocation,
  driver-suggested conversion). OpenXR rendering stayed active throughout the run.

Headline facts from these two:

- Vulkan device-creation interception coexists with Unity's OpenXR path on Quest;
  device creation routes through the intercepted `vkGetInstanceProcAddr` chain.
- Unity's OpenXR/Quest device already enables `VK_ANDROID_external_memory_android_
  hardware_buffer`, `VK_EXT_queue_family_foreign`, `VK_KHR_sampler_ycbcr_conversion`
  (and friends) and chains `VkPhysicalDeviceSamplerYcbcrConversionFeatures` itself.
- YUV420 `AHardwareBuffer` import uses the external-format path (`externalFormat`
  0x287 on this driver) with suggested BT.601 narrow, midpoint chroma, linear
  chroma filtering supported.
- Hardware decoders: `c2.qti.avc.decoder`, `c2.qti.hevc.decoder`, `c2.qti.vp9.decoder`,
  `c2.qti.vp8.decoder`, `c2.qti.mpeg2.decoder` (plus legacy `OMX.qcom.*` aliases).
  AVC/HEVC/VP9 all reach 4096x2304@30 and 3840x2160@60.
- No AV1 decoder of any kind on Quest Pro — hardware or software.
- No Android software fallback for avc/hevc/vp9 (`c2.android.*` exists only for
  vp8/mpeg4): if the hardware decoder is unavailable, there is no platform fallback.
