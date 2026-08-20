//! The §6.8 conversion pass validated against the CPU reference: synthetic
//! sweeps across every stated matrix/range, continuity with the integer
//! maths the CPU path shipped with, and real decoded fixture frames
//! through the full producer→consumer handoff.

#![cfg(windows)]

use media_decode::{ColorInfo, VideoDecoder, YuvMatrix, YuvRange};
use media_present::{SharedTextureConsumer, SharedTexturePresenter, TestConsumerTarget, reference};

/// GPU UNORM rounding vs CPU f32 rounding can differ by one code value
/// either side.
const TOLERANCE: u8 = 2;

/// A deterministic NV12 image exercising the full code range, including
/// the out-of-swing values a limited-range transform clamps.
fn synthetic_nv12(width: usize, height: usize) -> Vec<u8> {
    let mut data = vec![0u8; width * height * 3 / 2];
    for row in 0..height {
        for col in 0..width {
            data[row * width + col] = ((row * 7 + col * 13) % 256) as u8;
        }
    }
    let uv = &mut data[width * height..];
    for row in 0..height / 2 {
        for col in 0..width / 2 {
            uv[row * width + col * 2] = ((row * 11 + col * 5) % 256) as u8;
            uv[row * width + col * 2 + 1] = ((row * 3 + col * 17 + 128) % 256) as u8;
        }
    }
    data
}

fn max_channel_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// Round-trip one frame through presenter → shared texture → consumer →
/// readback, returning tightly packed BGRA.
fn gpu_convert(
    presenter: &mut SharedTexturePresenter,
    consumer: &mut SharedTextureConsumer,
    target: &TestConsumerTarget,
    width: u32,
    height: u32,
    nv12: &[u8],
    color: ColorInfo,
) -> Vec<u8> {
    assert!(
        presenter
            .present_planes(width, height, nv12, color)
            .expect("present"),
        "producer failed to acquire its own fresh texture"
    );
    assert!(consumer.copy_if_fresh().expect("copy"), "no fresh frame");
    target.read_back().expect("read back")
}

#[test]
fn gpu_matches_reference_across_matrices_and_ranges() {
    let (w, h) = (64u32, 32u32);
    let nv12 = synthetic_nv12(w as usize, h as usize);
    let mut presenter = SharedTexturePresenter::new(w, h).expect("presenter");
    let target = TestConsumerTarget::new(w, h).expect("target");
    let mut consumer =
        // SAFETY: the target texture is live for the whole test and its
        // device can open the presenter's shared handle.
        unsafe { SharedTextureConsumer::open(target.texture_ptr(), presenter.shared_handle()) }
            .expect("consumer");

    for matrix in [
        YuvMatrix::Unspecified,
        YuvMatrix::Bt601,
        YuvMatrix::Bt709,
        YuvMatrix::Bt2020,
    ] {
        for range in [YuvRange::Unspecified, YuvRange::Limited, YuvRange::Full] {
            let color = ColorInfo { matrix, range };
            let gpu = gpu_convert(&mut presenter, &mut consumer, &target, w, h, &nv12, color);
            let mut cpu = Vec::new();
            reference::nv12_to_bgra(w, h, &nv12, color, w, h, &mut cpu);
            let diff = max_channel_diff(&gpu, &cpu);
            assert!(
                diff <= TOLERANCE,
                "GPU vs reference diverged by {diff} for {color:?}"
            );
        }
    }
}

/// Continuity with the CPU path this pass replaced: the shipped integer
/// BT.601-limited conversion, pinned here as the historical oracle.
#[test]
fn reference_agrees_with_the_shipped_integer_convert() {
    fn integer_bt601_limited(width: usize, height: usize, data: &[u8], out: &mut Vec<u8>) {
        out.resize(width * height * 4, 0);
        let y_plane = &data[..width * height];
        let uv_plane = &data[width * height..];
        for row in 0..height {
            for col in 0..width {
                let c = y_plane[row * width + col] as i32 - 16;
                let d = uv_plane[(row / 2) * width + (col & !1)] as i32 - 128;
                let e = uv_plane[(row / 2) * width + (col | 1)] as i32 - 128;
                let r = (298 * c + 409 * e + 128) >> 8;
                let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
                let b = (298 * c + 516 * d + 128) >> 8;
                let px = &mut out[(row * width + col) * 4..][..4];
                px[0] = b.clamp(0, 255) as u8;
                px[1] = g.clamp(0, 255) as u8;
                px[2] = r.clamp(0, 255) as u8;
                px[3] = 255;
            }
        }
    }

    let (w, h) = (64usize, 64usize);
    let nv12 = synthetic_nv12(w, h);
    let mut old = Vec::new();
    integer_bt601_limited(w, h, &nv12, &mut old);
    let mut new = Vec::new();
    reference::nv12_to_bgra(
        w as u32,
        h as u32,
        &nv12,
        ColorInfo::default(),
        w as u32,
        h as u32,
        &mut new,
    );
    let diff = max_channel_diff(&old, &new);
    assert!(
        diff <= TOLERANCE,
        "reference departs from the shipped integer maths by {diff}"
    );
}

/// Real decoded frames from the A/V fixture through the full handoff.
#[test]
fn gpu_matches_reference_on_fixture_frames() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/h264-aac-640x360-30fps.mp4");
    let bytes = std::fs::read(&path).expect("fixture bytes");
    let mut demuxer = media_demux::open_auto(
        Box::new(media_demux::MemSource(bytes)),
        media_demux::DemuxLimits::default(),
        media_clock::Generation(0),
    )
    .expect("open fixture");

    let mut video_track = None;
    let mut decoder = decode_mf::H264Decoder::new().expect("decoder");
    let mut checked = 0usize;
    let mut rig: Option<(
        SharedTexturePresenter,
        TestConsumerTarget,
        SharedTextureConsumer,
    )> = None;

    'outer: loop {
        let event = demuxer.next_event().expect("demux event");
        match event {
            media_demux::StreamEvent::Format(track, media_demux::Format::Video { .. }) => {
                video_track = Some(track);
            }
            media_demux::StreamEvent::Au(au) if Some(au.track) == video_track => {
                let mut submitted = false;
                while !submitted {
                    submitted = matches!(
                        decoder
                            .submit(&au.data, au.pts.as_micros())
                            .expect("submit"),
                        media_decode::SubmitOutcome::Accepted
                    );
                    while let Some(frame) = decoder.try_output().expect("output") {
                        let frame = frame.as_nv12().expect("MF frames are NV12");
                        let (presenter, target, consumer) = rig.get_or_insert_with(|| {
                            let presenter = SharedTexturePresenter::new(frame.width, frame.height)
                                .expect("presenter");
                            let target =
                                TestConsumerTarget::new(frame.width, frame.height).expect("target");
                            let consumer =
                                // SAFETY: target texture lives alongside the
                                // consumer in the same rig tuple.
                                unsafe {
                                    SharedTextureConsumer::open(
                                        target.texture_ptr(),
                                        presenter.shared_handle(),
                                    )
                                }
                                .expect("consumer");
                            (presenter, target, consumer)
                        });
                        let gpu = gpu_convert(
                            presenter,
                            consumer,
                            target,
                            frame.width,
                            frame.height,
                            &frame.data,
                            frame.color,
                        );
                        let mut cpu = Vec::new();
                        reference::nv12_to_bgra(
                            frame.width,
                            frame.height,
                            &frame.data,
                            frame.color,
                            frame.width,
                            frame.height,
                            &mut cpu,
                        );
                        let diff = max_channel_diff(&gpu, &cpu);
                        assert!(
                            diff <= TOLERANCE,
                            "GPU vs reference diverged by {diff} on fixture frame {checked} \
                             ({:?})",
                            frame.color
                        );
                        checked += 1;
                        if checked >= 8 {
                            break 'outer;
                        }
                    }
                }
            }
            media_demux::StreamEvent::Eos(_) => break,
            _ => {}
        }
    }
    assert!(checked >= 8, "only validated {checked} fixture frames");
}

/// The DXVA input path: `present_slice` on a presenter sharing the
/// decode device must convert exactly the addressed texture-array slice
/// (the MFT's subresource index is load-bearing — never slice 0 by
/// assumption). Two slices carry distinct patterns; each present must
/// reproduce its own slice's reference conversion.
#[test]
fn present_slice_honours_the_subresource_index() {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_WRITE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
    use windows::core::Interface;

    let (w, h) = (64u32, 32u32);
    let patterns: Vec<Vec<u8>> = (0..2u8)
        .map(|slice| {
            let mut p = synthetic_nv12(w as usize, h as usize);
            for byte in p.iter_mut() {
                *byte = byte.wrapping_add(slice * 41);
            }
            p
        })
        .collect();

    // SAFETY: D3D11 object creation through owned wrappers; the staging
    // map writes stay inside RowPitch * height * 3 / 2 (the planar
    // layout), and every interface out-param is checked.
    unsafe {
        let mut device = None;
        let mut context = None;
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
        )
        .expect("device");
        let device = device.expect("device");
        let context = context.expect("context");

        let array_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 2,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut array = None;
        device
            .CreateTexture2D(&array_desc, None, Some(&mut array))
            .expect("array texture");
        let array = array.expect("array texture");

        let staging_desc = D3D11_TEXTURE2D_DESC {
            ArraySize: 1,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..array_desc
        };
        for (slice, pattern) in patterns.iter().enumerate() {
            let mut staging = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .expect("staging");
            let staging = staging.expect("staging");
            let mut mapped = Default::default();
            context
                .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .expect("map");
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *mut u8;
            let (wu, hu) = (w as usize, h as usize);
            for row in 0..hu {
                std::ptr::copy_nonoverlapping(
                    pattern.as_ptr().add(row * wu),
                    base.add(row * pitch),
                    wu,
                );
            }
            let uv = pattern.as_ptr().add(wu * hu);
            let uv_base = base.add(pitch * hu);
            for row in 0..hu / 2 {
                std::ptr::copy_nonoverlapping(uv.add(row * wu), uv_base.add(row * pitch), wu);
            }
            context.Unmap(&staging, 0);
            context.CopySubresourceRegion(&array, slice as u32, 0, 0, 0, &staging, 0, None);
        }

        let mut presenter =
            SharedTexturePresenter::new_on_device(device.as_raw(), w, h).expect("presenter");
        let target = TestConsumerTarget::new(w, h).expect("target");
        let mut consumer =
            SharedTextureConsumer::open(target.texture_ptr(), presenter.shared_handle())
                .expect("consumer");

        let color = ColorInfo {
            matrix: YuvMatrix::Bt709,
            range: YuvRange::Limited,
        };
        // Slice 1 first — an index-ignoring implementation would show
        // slice 0 here and fail against pattern 1's reference.
        for slice in [1usize, 0] {
            assert!(
                presenter
                    .present_slice(array.as_raw(), slice as u32, color)
                    .expect("present_slice"),
                "producer failed to acquire its own fresh texture"
            );
            assert!(consumer.copy_if_fresh().expect("copy"), "no fresh frame");
            let gpu = target.read_back().expect("read back");
            let mut cpu = Vec::new();
            reference::nv12_to_bgra(w, h, &patterns[slice], color, w, h, &mut cpu);
            let diff = max_channel_diff(&gpu, &cpu);
            assert!(
                diff <= TOLERANCE,
                "slice {slice}: GPU vs reference diverged by {diff}"
            );
        }
    }
}

/// The presenter owns the NT handle `CreateSharedHandle` hands back, so
/// dropping it must close the handle — otherwise every rebuild strands a
/// kernel handle and pins the texture's video memory for the process's life.
///
/// A duplicate pins the kernel object for the length of the test, so a
/// handle value the OS recycles after the close still reads as "no longer
/// the shared texture" rather than as a leak.
#[test]
fn dropping_the_presenter_closes_its_shared_handle() {
    use windows::Win32::Foundation::{
        CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle,
        GetHandleInformation, HANDLE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    let presenter = SharedTexturePresenter::new(64, 64).expect("presenter");
    let raw = HANDLE(presenter.shared_handle() as usize as *mut std::ffi::c_void);
    let mut dup = HANDLE::default();
    // SAFETY: handles this process owns; the duplicate is closed below.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, raw, me, &mut dup, 0, false, DUPLICATE_SAME_ACCESS)
            .expect("duplicate the shared handle");
    }
    // SAFETY: both handles are live and name the same shared texture.
    let duplicated = unsafe { CompareObjectHandles(raw, dup) }.as_bool();
    if !duplicated {
        // Closed before failing: panicking here would leak the duplicate
        // for the rest of the binary, and the sibling rows in it build
        // D3D11 devices that recycle freed handle values.
        // SAFETY: `dup` was created above and is closed exactly once.
        unsafe {
            let _ = CloseHandle(dup);
        }
        panic!("the duplicate does not name the shared texture");
    }

    drop(presenter);

    let mut flags = 0u32;
    // SAFETY: reading a handle's flags and comparing object identity.
    // A closed handle is reported as invalid under an ordinary run, which
    // is what this asks about — the duplicate above is what pins the
    // object, since a sibling row can recycle the freed value. Note that
    // a process with strict handle checking on, which a debugger turns on
    // for its child, raises on the closed value instead of answering; the
    // row is written for `cargo test` and would have to ask a different
    // way under one.
    let still_ours = unsafe {
        GetHandleInformation(raw, &mut flags).is_ok() && CompareObjectHandles(raw, dup).as_bool()
    };
    // SAFETY: the duplicate is this test's own handle, closed exactly once.
    unsafe {
        let _ = CloseHandle(dup);
    }
    assert!(
        !still_ours,
        "shared texture handle still open after the presenter dropped"
    );
}
