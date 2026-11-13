//! End-to-end test with a real (throwaway) Vulkan device: the built layer .so is loaded through the
//! real Vulkan loader (explicit layer via VK_ADD_LAYER_PATH, process gate forced), together with the
//! Khronos validation layer (incl. synchronization validation) below it. A synthetic "foveation"
//! pipeline with WiVRn's binding signature is dispatched; the frame must arrive in the memfd ring of a
//! fake viewer, correctly cropped / flipped, without any validation message.
//! Skipped (passes) when no Vulkan device, validation layer or layer .so is available.

use ash::vk;
use std::ffi::{c_void, CStr};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::time::Duration;
use wivrn_monitor_protocol as proto;
use wivrn_monitor_protocol::{Mapping, Message, PixelFormat, RingInfo};

static MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "system" fn dbg(
    sev: vk::DebugUtilsMessageSeverityFlagsEXT,
    _t: vk::DebugUtilsMessageTypeFlagsEXT,
    d: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _u: *mut c_void,
) -> vk::Bool32 {
    if sev.intersects(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        let m = CStr::from_ptr((*d).p_message).to_string_lossy().into_owned();
        eprintln!("VALIDATION: {m}");
        MESSAGES.lock().unwrap().push(m);
    }
    vk::FALSE
}

/// Builds (no-op when up to date) and locates the cdylib next to the test profile directory.
fn find_so() -> Option<std::path::PathBuf> {
    let release = !cfg!(debug_assertions);
    let mut cmd = std::process::Command::new(env!("CARGO"));
    cmd.args(["build", "-p", "wivrn-monitor-server-layer"]);
    if release {
        cmd.arg("--release");
    }
    if !cmd.status().ok()?.success() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let top = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "debug" || n == "release"))?;
    let p = top.join("libwivrn_monitor_server_layer.so");
    p.exists().then_some(p)
}

type Px = Box<dyn Fn(u32, u32) -> [i32; 3]>;
/// (rect x0, x1, y0, y1), expected output size, expected pixel.
type Phase = ((u32, u32, u32, u32), (u32, u32), Px);

/// Expected RGB of source pixel (x, y) in layer 1: sRGB-encoded bytes (x, 2*y, 128).
fn expect(x: u32, y: u32) -> [i32; 3] {
    [x as i32, (y * 2) as i32, 128]
}

#[test]
fn hook_path_to_ring_frame() {
    unsafe {
        let Some(so) = find_so() else { return eprintln!("skip: layer .so not built") };
        let dir = std::env::temp_dir().join(format!("wvm-srv-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("layer.json"),
            format!(
                r#"{{"file_format_version":"1.1.2","layer":{{"name":"VK_LAYER_WESLIE_wivrn_monitor_server","type":"GLOBAL","library_path":"{}","api_version":"1.3.0","implementation_version":"1","description":"test"}}}}"#,
                so.display()
            ),
        )
        .unwrap();
        std::env::set_var("VK_ADD_LAYER_PATH", &dir);
        std::env::set_var("WIVRN_MONITOR_SERVER_FORCE", "1");
        std::env::set_var("WIVRN_MONITOR_SOCK", dir.join("monitor.sock"));
        std::env::set_var("WIVRN_MONITOR_FPS", "240");
        std::env::set_var("WIVRN_MONITOR_LOG", "debug");
        std::env::set_var("WIVRN_MONITOR_DIAG", "1");
        std::env::set_var("XDG_DATA_HOME", &dir);

        let Ok(entry) = ash::Entry::load() else { return eprintln!("skip: no libvulkan") };
        let layers = entry.enumerate_instance_layer_properties().unwrap_or_default();
        let has = |n: &str| layers.iter().any(|l| l.layer_name_as_c_str().map(|c| c.to_bytes() == n.as_bytes()).unwrap_or(false));
        if !has("VK_LAYER_WESLIE_wivrn_monitor_server") || !has("VK_LAYER_KHRONOS_validation") {
            return eprintln!("skip: layers not found (ours={}, validation={})", has("VK_LAYER_WESLIE_wivrn_monitor_server"), has("VK_LAYER_KHRONOS_validation"));
        }
        let names = [c"VK_LAYER_WESLIE_wivrn_monitor_server".as_ptr(), c"VK_LAYER_KHRONOS_validation".as_ptr()];
        let exts = [c"VK_EXT_debug_utils".as_ptr()];
        let enables = [vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION];
        let mut vf = vk::ValidationFeaturesEXT::default().enabled_validation_features(&enables);
        let mut mci = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING)
            .message_type(vk::DebugUtilsMessageTypeFlagsEXT::GENERAL | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE)
            .pfn_user_callback(Some(dbg));
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let ici = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_layer_names(&names)
            .enabled_extension_names(&exts)
            .push_next(&mut vf)
            .push_next(&mut mci);
        let Ok(inst) = entry.create_instance(&ici, None) else { return eprintln!("skip: instance creation failed") };
        let dbg_loader = ash::ext::debug_utils::Instance::new(&entry, &inst);
        let messenger = dbg_loader.create_debug_utils_messenger(&mci, None).unwrap();

        let Some((phys, qfam)) = inst.enumerate_physical_devices().unwrap_or_default().into_iter().find_map(|p| {
            let p13 = {
                let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
                let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
                let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f12).push_next(&mut f13);
                inst.get_physical_device_features2(p, &mut f2);
                f12.timeline_semaphore == vk::TRUE && f13.synchronization2 == vk::TRUE
            };
            if !p13 {
                return None;
            }
            inst.get_physical_device_queue_family_properties(p).iter().position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE)).map(|i| (p, i as u32))
        }) else {
            return eprintln!("skip: no suitable gpu");
        };
        let prio = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(qfam).queue_priorities(&prio)];
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        let dev = inst.create_device(phys, &vk::DeviceCreateInfo::default().queue_create_infos(&qci).push_next(&mut f12).push_next(&mut f13), None).expect("device");
        let queue = dev.get_device_queue(qfam, 0);
        let mp = inst.get_physical_device_memory_properties(phys);
        let find = |bits: u32, want: vk::MemoryPropertyFlags| (0..mp.memory_type_count).find(|&i| bits & (1 << i) != 0 && mp.memory_types[i as usize].property_flags.contains(want)).unwrap();

        // Fake viewer socket.
        let l = proto::listen(&proto::socket_path()).unwrap();

        // ---- resources mimicking WiVRn's foveation
        let (sw, sh) = (256u32, 128u32);
        let img = dev
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_SRGB)
                    .extent(vk::Extent3D { width: sw, height: sh, depth: 1 })
                    .mip_levels(1)
                    .array_layers(2)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
            .unwrap();
        let rq = dev.get_image_memory_requirements(img);
        let imem = dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(rq.size).memory_type_index(find(rq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)), None).unwrap();
        dev.bind_image_memory(img, imem, 0).unwrap();
        let mk_view = |im, fmt, ty, layer, layers| {
            dev.create_image_view(
                &vk::ImageViewCreateInfo::default().image(im).view_type(ty).format(fmt).subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: layer, layer_count: layers }),
                None,
            )
            .unwrap()
        };
        let src_views = [mk_view(img, vk::Format::R8G8B8A8_SRGB, vk::ImageViewType::TYPE_2D, 0, 1), mk_view(img, vk::Format::R8G8B8A8_SRGB, vk::ImageViewType::TYPE_2D, 1, 1)];
        let mk_storage = |fmt, w, h| {
            let im = dev
                .create_image(
                    &vk::ImageCreateInfo::default().image_type(vk::ImageType::TYPE_2D).format(fmt).extent(vk::Extent3D { width: w, height: h, depth: 1 }).mip_levels(1).array_layers(2).samples(vk::SampleCountFlags::TYPE_1).usage(vk::ImageUsageFlags::STORAGE),
                    None,
                )
                .unwrap();
            let rq = dev.get_image_memory_requirements(im);
            let mem = dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(rq.size).memory_type_index(find(rq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)), None).unwrap();
            dev.bind_image_memory(im, mem, 0).unwrap();
            (im, mem, mk_view(im, fmt, vk::ImageViewType::TYPE_2D_ARRAY, 0, 2))
        };
        let (luma, luma_mem, luma_view) = mk_storage(vk::Format::R8_UNORM, 16, 16);
        let (chroma, chroma_mem, chroma_view) = mk_storage(vk::Format::R8G8_UNORM, 8, 8);
        let sampler = dev.create_sampler(&vk::SamplerCreateInfo::default().mag_filter(vk::Filter::LINEAR).min_filter(vk::Filter::LINEAR), None).unwrap();

        // Host buffers: pixel staging + SSBO.
        let host = |size: u64, usage| {
            let b = dev.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None).unwrap();
            let rq = dev.get_buffer_memory_requirements(b);
            let m = dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(rq.size).memory_type_index(find(rq.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)), None).unwrap();
            dev.bind_buffer_memory(b, m, 0).unwrap();
            let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).unwrap() as *mut u8;
            (b, m, p)
        };
        let (stg, stg_mem, stg_p) = host((sw * sh * 4) as u64, vk::BufferUsageFlags::TRANSFER_SRC);
        let ssbo_words = 2 * 4097 * 2;
        let (ssbo, ssbo_mem, ssbo_p) = host(ssbo_words as u64 * 4, vk::BufferUsageFlags::STORAGE_BUFFER);
        let ssbo_u = ssbo_p as *mut u32;
        let set_ssbo = |x0: u32, x1: u32, y0: u32, y1: u32| {
            for view in 0..2usize {
                for i in 0..4097usize {
                    *ssbo_u.add(view * 4097 + i) = x1;
                    *ssbo_u.add(2 * 4097 + view * 4097 + i) = y1;
                }
                *ssbo_u.add(view * 4097) = x0;
                *ssbo_u.add(2 * 4097 + view * 4097) = y0;
            }
        };

        // ---- WiVRn-like descriptor set layout / pipeline layout / pipeline
        let c = vk::ShaderStageFlags::COMPUTE;
        let bindings = [
            vk::DescriptorSetLayoutBinding::default().binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(2).stage_flags(c),
            vk::DescriptorSetLayoutBinding::default().binding(1).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(1).stage_flags(c),
            vk::DescriptorSetLayoutBinding::default().binding(2).descriptor_type(vk::DescriptorType::STORAGE_IMAGE).descriptor_count(1).stage_flags(c),
            vk::DescriptorSetLayoutBinding::default().binding(3).descriptor_type(vk::DescriptorType::STORAGE_IMAGE).descriptor_count(1).stage_flags(c),
        ];
        let dsl = dev.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None).unwrap();
        let dsls = [dsl];
        let pl = dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&dsls), None).unwrap();
        let spv = include_bytes!(concat!(env!("OUT_DIR"), "/fake_fov.spv"));
        if spv.is_empty() {
            return eprintln!("skip: no shader compiler for the test shader");
        }
        let words: Vec<u32> = spv.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect();
        let sm = dev.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None).unwrap();
        let pipe = dev
            .create_compute_pipelines(vk::PipelineCache::null(), &[vk::ComputePipelineCreateInfo::default().stage(vk::PipelineShaderStageCreateInfo::default().stage(c).module(sm).name(c"main")).layout(pl)], None)
            .unwrap()[0];
        let sizes = [
            vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 2 },
            vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: 1 },
            vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_IMAGE, descriptor_count: 2 },
        ];
        let dpool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes), None).unwrap();
        let set = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(dpool).set_layouts(&dsls)).unwrap()[0];
        let ii = [
            vk::DescriptorImageInfo { sampler, image_view: src_views[0], image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL },
            vk::DescriptorImageInfo { sampler, image_view: src_views[1], image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL },
        ];
        let bi = [vk::DescriptorBufferInfo { buffer: ssbo, offset: 0, range: vk::WHOLE_SIZE }];
        let yi = [vk::DescriptorImageInfo { sampler: vk::Sampler::null(), image_view: luma_view, image_layout: vk::ImageLayout::GENERAL }];
        let ci = [vk::DescriptorImageInfo { sampler: vk::Sampler::null(), image_view: chroma_view, image_layout: vk::ImageLayout::GENERAL }];
        dev.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&ii),
                vk::WriteDescriptorSet::default().dst_set(set).dst_binding(1).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&bi),
                vk::WriteDescriptorSet::default().dst_set(set).dst_binding(2).descriptor_type(vk::DescriptorType::STORAGE_IMAGE).image_info(&yi),
                vk::WriteDescriptorSet::default().dst_set(set).dst_binding(3).descriptor_type(vk::DescriptorType::STORAGE_IMAGE).image_info(&ci),
            ],
            &[],
        );

        // ---- one-time setup submission: upload both layers, transition images
        let cpool = dev.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(qfam), None).unwrap();
        let cb = dev.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(cpool).command_buffer_count(1)).unwrap()[0];
        let range = |layers| vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: layers };
        let bar = |im, old, new, sa, da, layers| {
            vk::ImageMemoryBarrier::default().old_layout(old).new_layout(new).src_access_mask(sa).dst_access_mask(da).src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED).image(im).subresource_range(range(layers))
        };
        use vk::AccessFlags as A;
        use vk::ImageLayout as L;
        use vk::PipelineStageFlags as S;
        let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
        // layer 0 junk upload
        for (layer, junk) in [(0u32, true), (1, false)] {
            for y in 0..sh {
                for x in 0..sw {
                    let px: [u8; 4] = if junk { [255, 0, 255, 255] } else { [x as u8, (y * 2) as u8, 128, 255] };
                    std::ptr::copy_nonoverlapping(px.as_ptr(), stg_p.add(((y * sw + x) * 4) as usize), 4);
                }
            }
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).unwrap();
            if layer == 0 {
                dev.cmd_pipeline_barrier(cb, S::TOP_OF_PIPE, S::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[bar(img, L::UNDEFINED, L::TRANSFER_DST_OPTIMAL, A::empty(), A::TRANSFER_WRITE, 2)]);
            }
            dev.cmd_copy_buffer_to_image(
                cb,
                stg,
                img,
                L::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default().image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: layer, layer_count: 1 }).image_extent(vk::Extent3D { width: sw, height: sh, depth: 1 })],
            );
            if layer == 1 {
                dev.cmd_pipeline_barrier(
                    cb,
                    S::TRANSFER,
                    S::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[
                        bar(img, L::TRANSFER_DST_OPTIMAL, L::SHADER_READ_ONLY_OPTIMAL, A::TRANSFER_WRITE, A::SHADER_READ, 2),
                        bar(luma, L::UNDEFINED, L::GENERAL, A::empty(), A::SHADER_WRITE, 2),
                        bar(chroma, L::UNDEFINED, L::GENERAL, A::empty(), A::SHADER_WRITE, 2),
                    ],
                );
            }
            dev.end_command_buffer(cb).unwrap();
            let cbs = [cb];
            dev.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).unwrap();
            dev.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            dev.reset_fences(&[fence]).unwrap();
            dev.reset_command_pool(cpool, vk::CommandPoolResetFlags::empty()).unwrap();
        }

        // ---- own timeline semaphore (like WiVRn's compositor) + per-frame command recording
        let mut tci = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
        let sem = dev.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut tci), None).unwrap();
        let mut sem_value = 0u64;

        // ---- run the phases
        let mut peer: Option<std::os::fd::OwnedFd> = None;
        let mut ring: Option<(RingInfo, Mapping)> = None;
        // phase: (x0,x1,y0,y1, expected w,h, expected pixel fn)
        let phases: Vec<Phase> = vec![
            // full 256x128 source, 16:9 crop -> 227x128, cox = 14
            ((0, 256, 0, 128), (227, 128), Box::new(|ox, oy| expect(14 + ox, oy))),
            // x 64..192, y flipped 128..0 -> 128x128 crop 16:9 -> 128x72, coy = 28, rows reversed
            ((64, 192, 128, 0), (128, 72), Box::new(|ox, oy| expect(64 + ox, 127 - 28 - oy))),
        ];
        let mut verified = vec![0u32; phases.len()];
        let mut held: Vec<(u32, u32)> = Vec::new();
        'outer: for iter in 0..2000 {
            let phase = if verified[0] >= 3 { 1 } else { 0 };
            let (rect, _, _) = &phases[phase];
            set_ssbo(rect.0, rect.1, rect.2, rect.3);
            dev.reset_command_pool(cpool, vk::CommandPoolResetFlags::empty()).unwrap();
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).unwrap();
            dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipe);
            dev.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pl, 0, &[set], &[]);
            dev.cmd_dispatch(cb, 2, 2, 2);
            dev.end_command_buffer(cb).unwrap();
            sem_value += 1;
            let cinfo = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let sinfo = [vk::SemaphoreSubmitInfo::default().semaphore(sem).value(sem_value).stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)];
            let sub = [vk::SubmitInfo2::default().command_buffer_infos(&cinfo).signal_semaphore_infos(&sinfo)];
            dev.queue_submit2(queue, &sub, vk::Fence::null()).unwrap();
            let sems = [sem];
            let vals = [sem_value];
            dev.wait_semaphores(&vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&vals), 5_000_000_000).unwrap();
            std::thread::sleep(Duration::from_millis(6));

            if peer.is_none() {
                peer = proto::accept(l.as_raw_fd()).unwrap();
                if let Some(p) = &peer {
                    proto::set_nonblocking(p.as_raw_fd(), true).unwrap();
                }
            }
            let Some(p) = &peer else { continue };
            while let Ok(Some((msg, fd))) = proto::recv_msg(p.as_raw_fd()) {
                match msg {
                    Message::Setup(info) => {
                        info.validate().unwrap();
                        assert_eq!(info.format, PixelFormat::Rgba8Srgb);
                        ring = Some((info, Mapping::new(fd.expect("memfd").as_raw_fd(), info.total_size() as usize, false).unwrap()));
                    }
                    Message::Frame { gen, slot, .. } => {
                        let (info, map) = ring.as_ref().expect("frame before setup");
                        assert_eq!(gen, info.gen);
                        let data = map.slice(info.slot_range(slot).unwrap());
                        let Some(pi) = phases.iter().position(|(_, wh, _)| *wh == (info.width, info.height)) else {
                            panic!("unexpected output size {}x{}", info.width, info.height)
                        };
                        let f = &phases[pi].2;
                        for oy in 0..info.height {
                            for ox in 0..info.width {
                                let o = ((oy * info.width + ox) * 4) as usize;
                                let e = f(ox, oy);
                                for k in 0..3 {
                                    assert!((data[o + k] as i32 - e[k]).abs() <= 2, "phase {pi} pixel ({ox},{oy}) ch {k}: got {:?}, expected {e:?}", &data[o..o + 4]);
                                }
                                assert_eq!(data[o + 3], 255);
                            }
                        }
                        verified[pi] += 1;
                        held.push((gen, slot));
                        if held.len() > 1 {
                            let (g, s) = held.remove(0);
                            proto::send_msg(p.as_raw_fd(), &Message::Release { gen: g, slot: s }, None, true).unwrap();
                        }
                    }
                    _ => {}
                }
            }
            if verified.iter().all(|&v| v >= 3) {
                eprintln!("verified after {iter} iterations: {verified:?}");
                break 'outer;
            }
        }
        assert!(verified.iter().all(|&v| v >= 3), "frames verified per phase: {verified:?}");

        // ---- teardown through the layer (destroy_device must stop the consumer and free everything)
        dev.device_wait_idle().unwrap();
        dev.destroy_semaphore(sem, None);
        dev.destroy_fence(fence, None);
        dev.destroy_command_pool(cpool, None);
        dev.destroy_descriptor_pool(dpool, None);
        dev.destroy_pipeline(pipe, None);
        dev.destroy_shader_module(sm, None);
        dev.destroy_pipeline_layout(pl, None);
        dev.destroy_descriptor_set_layout(dsl, None);
        dev.destroy_sampler(sampler, None);
        for v in [src_views[0], src_views[1], luma_view, chroma_view] {
            dev.destroy_image_view(v, None);
        }
        for (im, m) in [(img, imem), (luma, luma_mem), (chroma, chroma_mem)] {
            dev.destroy_image(im, None);
            dev.free_memory(m, None);
        }
        for (b, m) in [(stg, stg_mem), (ssbo, ssbo_mem)] {
            dev.destroy_buffer(b, None);
            dev.free_memory(m, None);
        }
        dev.destroy_device(None);
        dbg_loader.destroy_debug_utils_messenger(messenger, None);
        inst.destroy_instance(None);
        let msgs = MESSAGES.lock().unwrap();
        assert!(msgs.is_empty(), "validation messages: {msgs:#?}");
        let log = std::fs::read_to_string(dir.join("wivrn-monitor/server-layer.log")).unwrap_or_default();
        assert!(log.contains("mirroring armed"), "layer log lacks 'mirroring armed':\n{log}");
        assert!(log.contains("DIAG foveation descriptor-set-layout signature matched"), "no DIAG signature line:\n{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
