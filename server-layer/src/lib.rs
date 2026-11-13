//! WiVRn-Monitor server layer: an implicit Vulkan layer that is only active inside `wivrn-server`.
//!
//! Loaded by the Vulkan loader in every process (implicit layer) but a pure pass-through unless the
//! process is `wivrn-server`. (Rejecting the negotiation instead makes the loader print ERROR lines.)
//!
//! It recognises WiVRn's foveation compute pass by its descriptor-set-layout signature
//! (combined-image-sampler[2] / SSBO / storage image / storage image), appends its own compute
//! dispatch after it (reading the unfoveated right-eye source through WiVRn's own descriptor set 0,
//! centre-cropping to 16:9 and downscaling into a host-visible buffer), signals a layer-owned timeline
//! semaphore from the same `vkQueueSubmit2`, and a consumer thread forwards the finished frames over
//! the monitor protocol to the viewer. Every failure only disables the mirroring.
//!
//! Environment: `DISABLE_WIVRN_MONITOR_SERVER=1` (off), `WIVRN_MONITOR_LOG`, `WIVRN_MONITOR_DIAG=1`,
//! `WIVRN_MONITOR_FPS` (30), `WIVRN_MONITOR_WIDTH` (1280), `WIVRN_MONITOR_ASPECT` (16:9 | native | a:b),
//! `WIVRN_MONITOR_SERVER_FORCE=1` (load in any process; tests only).

#[macro_use]
pub mod log;
pub mod link;
pub mod mirror;

use ash::vk::{self, Handle};
use log::Level;
use mirror::{Config, Mirror, WaitSemaphoresFn};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};

// ---------------------------------------------------------------- loader-layer ABI (vk_layer.h)

#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: u32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    pfn_get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    pfn_get_device_proc_addr: Option<vk::PFN_vkGetDeviceProcAddr>,
    pfn_get_physical_device_proc_addr: *const c_void,
}

#[repr(C)]
struct LayerInstanceLink {
    p_next: *mut LayerInstanceLink,
    pfn_next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pfn_next_get_physical_device_proc_addr: *const c_void,
}

#[repr(C)]
struct LayerDeviceLink {
    p_next: *mut LayerDeviceLink,
    pfn_next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pfn_next_get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
}

/// VkLayerInstanceCreateInfo / VkLayerDeviceCreateInfo (the union is one pointer wide).
#[repr(C)]
struct LayerCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    function: u32,
    u: *mut c_void,
}

const STYPE_LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const STYPE_LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: u32 = 0;

// ---------------------------------------------------------------- process gate

/// Only `wivrn-server` (or a forced test process) gets the layer.
pub fn process_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(process_ok_uncached)
}

fn process_ok_uncached() -> bool {
    let env = |n: &str| std::env::var(n).map(|v| v == "1").unwrap_or(false);
    if env("DISABLE_WIVRN_MONITOR_SERVER") {
        return false;
    }
    if env("WIVRN_MONITOR_SERVER_FORCE") {
        return true;
    }
    let Ok(p) = std::fs::read_link("/proc/self/exe") else { return false };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.trim_end_matches(" (deleted)") == "wivrn-server"
}

/// # Safety
/// Called by the Vulkan loader with a valid negotiation struct.
#[no_mangle]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(p: *mut NegotiateLayerInterface) -> vk::Result {
    let r = catch_unwind(|| {
        if p.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let p = &mut *p;
        if p.loader_layer_interface_version < 2 {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        p.loader_layer_interface_version = 2;
        p.pfn_get_instance_proc_addr = Some(hk_get_instance_proc_addr);
        p.pfn_get_device_proc_addr = Some(hk_get_device_proc_addr);
        p.pfn_get_physical_device_proc_addr = std::ptr::null();
        vk::Result::SUCCESS
    });
    r.unwrap_or(vk::Result::ERROR_INITIALIZATION_FAILED)
}

// ---------------------------------------------------------------- state

struct InstData {
    /// false: foreign process, pure pass-through.
    active: bool,
    instance: ash::Instance,
    gipa: vk::PFN_vkGetInstanceProcAddr,
}

#[derive(Default)]
struct Fov {
    dsls: HashSet<u64>,
    /// pipeline layout -> its (only) descriptor set layout
    layouts: HashMap<u64, u64>,
    pipelines: HashSet<u64>,
}

#[derive(Default)]
struct CmdState {
    pool: u64,
    /// Currently bound compute pipeline if it is a foveation pipeline.
    fov_pipeline: Option<u64>,
    reserved: Vec<usize>,
}

struct DevData {
    active: bool,
    dev: ash::Device,
    gdpa: vk::PFN_vkGetDeviceProcAddr,
    inst: Arc<InstData>,
    phys: vk::PhysicalDevice,
    raw: u64,
    timeline: bool,
    submit2: Option<vk::PFN_vkQueueSubmit2>,
    wait_sem: Option<WaitSemaphoresFn>,
    cfg: Config,
    fov: Mutex<Fov>,
    mirror: OnceLock<Arc<Mirror>>,
    cmds: Mutex<HashMap<u64, CmdState>>,
    diag_logs: std::sync::atomic::AtomicU32,
}

static INSTANCES: LazyLock<RwLock<HashMap<usize, Arc<InstData>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static DEVICES: LazyLock<RwLock<HashMap<usize, Arc<DevData>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Dispatch key of any dispatchable handle (first word of the object).
fn key<H: Handle>(h: H) -> usize {
    // SAFETY: dispatchable Vulkan handles point to an object whose first word is the loader dispatch pointer.
    unsafe { *(h.as_raw() as *const usize) }
}

fn dev_of<H: Handle>(h: H) -> Option<Arc<DevData>> {
    DEVICES.read().unwrap_or_else(|e| e.into_inner()).get(&key(h)).cloned()
}

fn diag_log(dd: &DevData, args: std::fmt::Arguments) {
    if dd.cfg.diag && dd.diag_logs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 400 {
        log::write(Level::Info, format_args!("DIAG {args}"));
    }
}

// ---------------------------------------------------------------- instance level

unsafe fn find_link<T>(mut p: *const c_void, stype: i32) -> Option<*mut LayerCreateInfo> {
    let _ = std::marker::PhantomData::<T>;
    while !p.is_null() {
        let c = p as *mut LayerCreateInfo;
        if (*c).s_type == stype && (*c).function == VK_LAYER_LINK_INFO {
            return Some(c);
        }
        p = (*c).p_next;
    }
    None
}

unsafe extern "system" fn hk_create_instance(ci: *const vk::InstanceCreateInfo<'_>, alloc: *const vk::AllocationCallbacks<'_>, out: *mut vk::Instance) -> vk::Result {
    let Some(link) = find_link::<LayerInstanceLink>((*ci).p_next, STYPE_LOADER_INSTANCE_CREATE_INFO) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let info = (*link).u as *mut LayerInstanceLink;
    if info.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let gipa = (*info).pfn_next_get_instance_proc_addr;
    // Advance the chain for the next layer.
    (*link).u = (*info).p_next as *mut c_void;
    let Some(create) = gipa(vk::Instance::null(), c"vkCreateInstance".as_ptr()) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let create: vk::PFN_vkCreateInstance = std::mem::transmute(create);
    let r = create(ci, alloc, out);
    if r != vk::Result::SUCCESS {
        return r;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let inst = ash::Instance::load(&ash::StaticFn { get_instance_proc_addr: gipa }, *out);
        INSTANCES.write().unwrap_or_else(|e| e.into_inner()).insert(key(*out), Arc::new(InstData { active: process_ok(), instance: inst, gipa }));
    }));
    r
}

unsafe extern "system" fn hk_destroy_instance(instance: vk::Instance, alloc: *const vk::AllocationCallbacks<'_>) {
    let id = INSTANCES.write().unwrap_or_else(|e| e.into_inner()).remove(&key(instance));
    if let Some(id) = id {
        (id.instance.fp_v1_0().destroy_instance)(instance, alloc);
    }
}

unsafe fn scan_timeline(mut p: *const vk::BaseInStructure<'_>) -> bool {
    while !p.is_null() {
        match (*p).s_type {
            vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES => {
                return (*(p as *const vk::PhysicalDeviceVulkan12Features<'_>)).timeline_semaphore == vk::TRUE;
            }
            vk::StructureType::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES => {
                return (*(p as *const vk::PhysicalDeviceTimelineSemaphoreFeatures<'_>)).timeline_semaphore == vk::TRUE;
            }
            _ => {}
        }
        p = (*p).p_next;
    }
    false
}

unsafe fn load_dev<T>(gdpa: vk::PFN_vkGetDeviceProcAddr, dev: vk::Device, names: &[&CStr]) -> Option<T> {
    for n in names {
        if let Some(f) = gdpa(dev, n.as_ptr()) {
            return Some(std::mem::transmute_copy::<_, T>(&f));
        }
    }
    None
}

unsafe extern "system" fn hk_create_device(
    phys: vk::PhysicalDevice,
    ci: *const vk::DeviceCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Device,
) -> vk::Result {
    let Some(inst) = INSTANCES.read().unwrap_or_else(|e| e.into_inner()).get(&key(phys)).cloned() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(link) = find_link::<LayerDeviceLink>((*ci).p_next, STYPE_LOADER_DEVICE_CREATE_INFO) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let info = (*link).u as *mut LayerDeviceLink;
    if info.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let gipa = (*info).pfn_next_get_instance_proc_addr;
    let gdpa = (*info).pfn_next_get_device_proc_addr;
    (*link).u = (*info).p_next as *mut c_void;
    let Some(create) = gipa(inst.instance.handle(), c"vkCreateDevice".as_ptr()) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let create: vk::PFN_vkCreateDevice = std::mem::transmute(create);
    let r = create(phys, ci, alloc, out);
    if r != vk::Result::SUCCESS {
        return r;
    }
    let device = *out;
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let dev = ash::Device::load_with(|n| gdpa(device, n.as_ptr()).map_or(std::ptr::null(), |f| f as *const c_void), device);
        let timeline = inst.active && scan_timeline((*ci).p_next as *const vk::BaseInStructure<'_>);
        let submit2 = load_dev::<vk::PFN_vkQueueSubmit2>(gdpa, device, &[c"vkQueueSubmit2", c"vkQueueSubmit2KHR"]);
        let wait_sem = load_dev::<WaitSemaphoresFn>(gdpa, device, &[c"vkWaitSemaphores", c"vkWaitSemaphoresKHR"]);
        let dd = Arc::new(DevData {
            active: inst.active,
            dev,
            gdpa,
            inst: inst.clone(),
            phys,
            raw: device.as_raw(),
            timeline,
            submit2,
            wait_sem,
            cfg: Config::from_env(),
            fov: Mutex::new(Fov::default()),
            mirror: OnceLock::new(),
            cmds: Mutex::new(HashMap::new()),
            diag_logs: Default::default(),
        });
        diag_log(&dd, format_args!("device created: timelineSemaphore={timeline} queueSubmit2={} waitSemaphores={}", submit2.is_some(), wait_sem.is_some()));
        DEVICES.write().unwrap_or_else(|e| e.into_inner()).insert(key(device), dd);
    }));
    r
}

unsafe extern "system" fn hk_destroy_device(device: vk::Device, alloc: *const vk::AllocationCallbacks<'_>) {
    let dd = DEVICES.write().unwrap_or_else(|e| e.into_inner()).remove(&key(device));
    if let Some(dd) = dd {
        if let Some(m) = dd.mirror.get() {
            let _ = catch_unwind(AssertUnwindSafe(|| m.shutdown()));
            link::session_closed(dd.raw);
        }
        (dd.dev.fp_v1_0().destroy_device)(device, alloc);
    }
}

// ---------------------------------------------------------------- detection

fn is_fov_dsl(ci: &vk::DescriptorSetLayoutCreateInfo<'_>) -> bool {
    if !ci.flags.is_empty() || ci.binding_count != 4 || ci.p_bindings.is_null() {
        return false;
    }
    // SAFETY: the app passed binding_count valid bindings.
    let b = unsafe { std::slice::from_raw_parts(ci.p_bindings, 4) };
    let mut v: Vec<_> = b.iter().map(|x| (x.binding, x.descriptor_type, x.descriptor_count, x.stage_flags, x.p_immutable_samplers.is_null())).collect();
    v.sort_by_key(|x| x.0);
    let c = vk::ShaderStageFlags::COMPUTE;
    v == [
        (0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER, 2, c, true),
        (1, vk::DescriptorType::STORAGE_BUFFER, 1, c, true),
        (2, vk::DescriptorType::STORAGE_IMAGE, 1, c, true),
        (3, vk::DescriptorType::STORAGE_IMAGE, 1, c, true),
    ]
}

unsafe extern "system" fn hk_create_dsl(
    device: vk::Device,
    ci: *const vk::DescriptorSetLayoutCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::DescriptorSetLayout,
) -> vk::Result {
    let Some(dd) = dev_of(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let r = (dd.dev.fp_v1_0().create_descriptor_set_layout)(device, ci, alloc, out);
    if r == vk::Result::SUCCESS {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if is_fov_dsl(&*ci) {
                lock(&dd.fov).dsls.insert((*out).as_raw());
                diag_log(&dd, format_args!("foveation descriptor-set-layout signature matched ({:#x})", (*out).as_raw()));
            } else if (*ci).binding_count >= 3 && (*ci).binding_count <= 5 {
                let b = std::slice::from_raw_parts((*ci).p_bindings, (*ci).binding_count as usize);
                let s: Vec<String> = b.iter().map(|x| format!("{}:{:?}x{}", x.binding, x.descriptor_type, x.descriptor_count)).collect();
                diag_log(&dd, format_args!("other descriptor-set-layout: [{}] flags={:?}", s.join(", "), (*ci).flags));
            }
        }));
    }
    r
}

unsafe extern "system" fn hk_destroy_dsl(device: vk::Device, dsl: vk::DescriptorSetLayout, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(dd) = dev_of(device) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut f = lock(&dd.fov);
        f.dsls.remove(&dsl.as_raw());
        f.layouts.retain(|_, d| *d != dsl.as_raw());
    }));
    (dd.dev.fp_v1_0().destroy_descriptor_set_layout)(device, dsl, alloc);
}

unsafe extern "system" fn hk_create_pl(
    device: vk::Device,
    ci: *const vk::PipelineLayoutCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::PipelineLayout,
) -> vk::Result {
    let Some(dd) = dev_of(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let r = (dd.dev.fp_v1_0().create_pipeline_layout)(device, ci, alloc, out);
    if r == vk::Result::SUCCESS {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let c = &*ci;
            if c.set_layout_count == 1 && c.push_constant_range_count == 0 && !c.p_set_layouts.is_null() {
                let dsl = (*c.p_set_layouts).as_raw();
                let mut f = lock(&dd.fov);
                if f.dsls.contains(&dsl) {
                    f.layouts.insert((*out).as_raw(), dsl);
                }
            }
        }));
    }
    r
}

unsafe extern "system" fn hk_destroy_pl(device: vk::Device, pl: vk::PipelineLayout, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(dd) = dev_of(device) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| lock(&dd.fov).layouts.remove(&pl.as_raw())));
    (dd.dev.fp_v1_0().destroy_pipeline_layout)(device, pl, alloc);
}

unsafe extern "system" fn hk_create_compute_pipelines(
    device: vk::Device,
    cache: vk::PipelineCache,
    count: u32,
    cis: *const vk::ComputePipelineCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Pipeline,
) -> vk::Result {
    let Some(dd) = dev_of(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let r = (dd.dev.fp_v1_0().create_compute_pipelines)(device, cache, count, cis, alloc, out);
    if r == vk::Result::SUCCESS {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            for i in 0..count as usize {
                let layout = (*cis.add(i)).layout.as_raw();
                let dsl = lock(&dd.fov).layouts.get(&layout).copied();
                match dsl {
                    Some(dsl) => {
                        lock(&dd.fov).pipelines.insert((*out.add(i)).as_raw());
                        diag_log(&dd, format_args!("foveation compute pipeline detected ({:#x}, layout {layout:#x})", (*out.add(i)).as_raw()));
                        arm(&dd, vk::DescriptorSetLayout::from_raw(dsl));
                    }
                    None => diag_log(&dd, format_args!("compute pipeline {:#x} with unrelated layout {layout:#x}", (*out.add(i)).as_raw())),
                }
            }
        }));
    }
    r
}

unsafe extern "system" fn hk_destroy_pipeline(device: vk::Device, p: vk::Pipeline, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(dd) = dev_of(device) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| lock(&dd.fov).pipelines.remove(&p.as_raw())));
    (dd.dev.fp_v1_0().destroy_pipeline)(device, p, alloc);
}

/// Creates the mirror once per device (first foveation pipeline). Failures only disable it.
fn arm(dd: &Arc<DevData>, fov_dsl: vk::DescriptorSetLayout) {
    if dd.mirror.get().is_some() {
        return;
    }
    let Some(wait) = dd.wait_sem else { return log_at!(Level::Warn, "no vkWaitSemaphores: mirroring disabled") };
    if !dd.timeline || dd.submit2.is_none() {
        return log_at!(Level::Warn, "device lacks timelineSemaphore/queueSubmit2: mirroring disabled");
    }
    // SAFETY: plain query on a live physical device.
    let mp = unsafe { dd.inst.instance.get_physical_device_memory_properties(dd.phys) };
    match Mirror::new(dd.dev.clone(), mp, fov_dsl, wait, dd.cfg) {
        Ok(m) => {
            if dd.mirror.set(m).is_ok() {
                link::session_opened(dd.raw);
                log_at!(Level::Info, "foveation pass detected; mirroring armed ({}x? aspect {:?}, {} fps)", dd.cfg.width, dd.cfg.aspect, dd.cfg.fps);
            }
        }
        Err(e) => log_at!(Level::Error, "cannot create mirror: {e}; mirroring disabled"),
    }
}

// ---------------------------------------------------------------- command buffer tracking

fn release_all(dd: &DevData, st: &mut CmdState) {
    if let Some(m) = dd.mirror.get() {
        for s in st.reserved.drain(..) {
            m.release(s);
        }
    }
    st.reserved.clear();
    st.fov_pipeline = None;
}

unsafe extern "system" fn hk_allocate_command_buffers(
    device: vk::Device,
    ai: *const vk::CommandBufferAllocateInfo<'_>,
    out: *mut vk::CommandBuffer,
) -> vk::Result {
    let Some(dd) = dev_of(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let r = (dd.dev.fp_v1_0().allocate_command_buffers)(device, ai, out);
    if r == vk::Result::SUCCESS {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let pool = (*ai).command_pool.as_raw();
            let mut c = lock(&dd.cmds);
            for i in 0..(*ai).command_buffer_count as usize {
                c.insert((*out.add(i)).as_raw(), CmdState { pool, ..Default::default() });
            }
        }));
    }
    r
}

unsafe extern "system" fn hk_free_command_buffers(device: vk::Device, pool: vk::CommandPool, count: u32, cmds: *const vk::CommandBuffer) {
    let Some(dd) = dev_of(device) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut c = lock(&dd.cmds);
        for i in 0..count as usize {
            if let Some(mut st) = c.remove(&(*cmds.add(i)).as_raw()) {
                release_all(&dd, &mut st);
            }
        }
    }));
    (dd.dev.fp_v1_0().free_command_buffers)(device, pool, count, cmds);
}

unsafe extern "system" fn hk_reset_command_pool(device: vk::Device, pool: vk::CommandPool, flags: vk::CommandPoolResetFlags) -> vk::Result {
    let Some(dd) = dev_of(device) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut c = lock(&dd.cmds);
        for st in c.values_mut().filter(|s| s.pool == pool.as_raw()) {
            release_all(&dd, st);
        }
    }));
    (dd.dev.fp_v1_0().reset_command_pool)(device, pool, flags)
}

unsafe extern "system" fn hk_destroy_command_pool(device: vk::Device, pool: vk::CommandPool, alloc: *const vk::AllocationCallbacks<'_>) {
    let Some(dd) = dev_of(device) else { return };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut c = lock(&dd.cmds);
        let dead: Vec<u64> = c.iter().filter(|(_, s)| s.pool == pool.as_raw()).map(|(k, _)| *k).collect();
        for k in dead {
            if let Some(mut st) = c.remove(&k) {
                release_all(&dd, &mut st);
            }
        }
    }));
    (dd.dev.fp_v1_0().destroy_command_pool)(device, pool, alloc);
}

fn clear_cmd(dd: &DevData, cmd: vk::CommandBuffer) {
    let mut c = lock(&dd.cmds);
    if let Some(st) = c.get_mut(&cmd.as_raw()) {
        release_all(dd, st);
    }
}

unsafe extern "system" fn hk_begin_command_buffer(cmd: vk::CommandBuffer, bi: *const vk::CommandBufferBeginInfo<'_>) -> vk::Result {
    let Some(dd) = dev_of(cmd) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    if dd.mirror.get().is_some() {
        let _ = catch_unwind(AssertUnwindSafe(|| clear_cmd(&dd, cmd)));
    }
    (dd.dev.fp_v1_0().begin_command_buffer)(cmd, bi)
}

unsafe extern "system" fn hk_reset_command_buffer(cmd: vk::CommandBuffer, flags: vk::CommandBufferResetFlags) -> vk::Result {
    let Some(dd) = dev_of(cmd) else { return vk::Result::ERROR_INITIALIZATION_FAILED };
    if dd.mirror.get().is_some() {
        let _ = catch_unwind(AssertUnwindSafe(|| clear_cmd(&dd, cmd)));
    }
    (dd.dev.fp_v1_0().reset_command_buffer)(cmd, flags)
}

unsafe extern "system" fn hk_cmd_bind_pipeline(cmd: vk::CommandBuffer, point: vk::PipelineBindPoint, pipeline: vk::Pipeline) {
    let Some(dd) = dev_of(cmd) else { return };
    (dd.dev.fp_v1_0().cmd_bind_pipeline)(cmd, point, pipeline);
    if point == vk::PipelineBindPoint::COMPUTE && dd.mirror.get().is_some() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let is_fov = lock(&dd.fov).pipelines.contains(&pipeline.as_raw());
            if let Some(st) = lock(&dd.cmds).get_mut(&cmd.as_raw()) {
                st.fov_pipeline = is_fov.then_some(pipeline.as_raw());
            }
        }));
    }
}

unsafe extern "system" fn hk_cmd_dispatch(cmd: vk::CommandBuffer, x: u32, y: u32, z: u32) {
    let Some(dd) = dev_of(cmd) else { return };
    (dd.dev.fp_v1_0().cmd_dispatch)(cmd, x, y, z);
    let Some(m) = dd.mirror.get() else { return };
    if m.is_dead() {
        return;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut c = lock(&dd.cmds);
        let Some(st) = c.get_mut(&cmd.as_raw()) else { return };
        let Some(fp) = st.fov_pipeline else { return };
        if !(1..=520).contains(&x) || !(1..=520).contains(&y) || z != 2 {
            diag_log(&dd, format_args!("foveation-pipeline dispatch with unexpected size {x}x{y}x{z}: skipped"));
            return;
        }
        let stage = m.try_reserve();
        let n = m.dispatch_count();
        if n == 1 || n % 300 == 0 {
            diag_log(&dd, format_args!("foveation dispatch #{n}: groups {x}x{y}x{z} (foveated image ~{}x{} px); our pass {:?}", x * 8, y * 8, m.groups()));
        }
        if let Some(stage) = stage {
            m.record(cmd, stage, vk::Pipeline::from_raw(fp));
            st.reserved.push(stage);
        }
    }));
    if r.is_err() {
        log_at!(Level::Error, "panic while recording mirror pass; mirroring disabled");
        m.kill();
    }
}

// ---------------------------------------------------------------- submit

unsafe fn submit_with_mirror(
    dd: &DevData,
    m: &Mirror,
    next: vk::PFN_vkQueueSubmit2,
    queue: vk::Queue,
    count: u32,
    infos: *const vk::SubmitInfo2<'_>,
    fence: vk::Fence,
) -> vk::Result {
    let batches = std::slice::from_raw_parts(infos, count as usize);
    let mut per_batch: Vec<Vec<usize>> = Vec::with_capacity(batches.len());
    {
        let mut c = lock(&dd.cmds);
        for b in batches {
            let mut stages = Vec::new();
            if !b.p_command_buffer_infos.is_null() {
                for ci in std::slice::from_raw_parts(b.p_command_buffer_infos, b.command_buffer_info_count as usize) {
                    if let Some(st) = c.get_mut(&ci.command_buffer.as_raw()) {
                        stages.append(&mut st.reserved);
                    }
                }
            }
            per_batch.push(stages);
        }
    }
    if per_batch.iter().all(|s| s.is_empty()) {
        return next(queue, count, infos, fence);
    }
    let mut guard = lock(&m.submit_lock);
    let mut new_infos = batches.to_vec();
    let mut sigs: Vec<Vec<vk::SemaphoreSubmitInfo<'_>>> = Vec::new();
    let mut values = vec![0u64; batches.len()];
    for (i, stages) in per_batch.iter().enumerate() {
        if stages.is_empty() {
            continue;
        }
        *guard += 1;
        values[i] = *guard;
        let mut v: Vec<vk::SemaphoreSubmitInfo<'_>> = if batches[i].p_signal_semaphore_infos.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(batches[i].p_signal_semaphore_infos, batches[i].signal_semaphore_info_count as usize).to_vec()
        };
        v.push(vk::SemaphoreSubmitInfo::default().semaphore(m.semaphore()).value(*guard).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS));
        sigs.push(v);
        let v = sigs.last().unwrap();
        new_infos[i].signal_semaphore_info_count = v.len() as u32;
        new_infos[i].p_signal_semaphore_infos = v.as_ptr();
    }
    let r = next(queue, count, new_infos.as_ptr(), fence);
    drop(guard);
    for (i, stages) in per_batch.iter().enumerate() {
        if r == vk::Result::SUCCESS {
            m.submitted(stages, values[i]);
        } else {
            for &s in stages {
                m.release(s);
            }
        }
    }
    r
}

unsafe extern "system" fn hk_queue_submit2(queue: vk::Queue, count: u32, infos: *const vk::SubmitInfo2<'_>, fence: vk::Fence) -> vk::Result {
    let Some(dd) = dev_of(queue) else { return vk::Result::ERROR_UNKNOWN };
    let Some(next) = dd.submit2 else { return vk::Result::ERROR_UNKNOWN };
    if let Some(m) = dd.mirror.get() {
        if m.reserved_count.load(std::sync::atomic::Ordering::Acquire) > 0 && !m.is_dead() {
            // A panic inside leaves the submission untouched only if it happens before `next` ran;
            // the code before `next` is allocation/copy only.
            return match catch_unwind(AssertUnwindSafe(|| submit_with_mirror(&dd, m, next, queue, count, infos, fence))) {
                Ok(r) => r,
                Err(_) => {
                    log_at!(Level::Error, "panic while attaching the mirror signal; mirroring disabled");
                    m.kill();
                    next(queue, count, infos, fence)
                }
            };
        }
    }
    next(queue, count, infos, fence)
}

unsafe extern "system" fn hk_queue_submit(queue: vk::Queue, count: u32, infos: *const vk::SubmitInfo<'_>, fence: vk::Fence) -> vk::Result {
    let Some(dd) = dev_of(queue) else { return vk::Result::ERROR_UNKNOWN };
    if let Some(m) = dd.mirror.get() {
        if m.reserved_count.load(std::sync::atomic::Ordering::Acquire) > 0 && !m.is_dead() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let c = lock(&dd.cmds);
                for b in std::slice::from_raw_parts(infos, count as usize) {
                    if b.p_command_buffers.is_null() {
                        continue;
                    }
                    for cb in std::slice::from_raw_parts(b.p_command_buffers, b.command_buffer_count as usize) {
                        if c.get(&cb.as_raw()).is_some_and(|s| !s.reserved.is_empty()) {
                            log_at!(Level::Error, "mirror pass submitted through vkQueueSubmit (needs vkQueueSubmit2): mirroring disabled");
                            m.kill();
                        }
                    }
                }
            }));
        }
    }
    (dd.dev.fp_v1_0().queue_submit)(queue, count, infos, fence)
}

// ---------------------------------------------------------------- proc addr

macro_rules! pfn {
    ($f:expr) => {
        Some(std::mem::transmute::<*const (), unsafe extern "system" fn()>($f as *const ()))
    };
}

unsafe fn device_hook(name: &CStr, has_submit2: bool) -> vk::PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkDestroyDevice" => pfn!(hk_destroy_device),
        b"vkCreateDescriptorSetLayout" => pfn!(hk_create_dsl),
        b"vkDestroyDescriptorSetLayout" => pfn!(hk_destroy_dsl),
        b"vkCreatePipelineLayout" => pfn!(hk_create_pl),
        b"vkDestroyPipelineLayout" => pfn!(hk_destroy_pl),
        b"vkCreateComputePipelines" => pfn!(hk_create_compute_pipelines),
        b"vkDestroyPipeline" => pfn!(hk_destroy_pipeline),
        b"vkAllocateCommandBuffers" => pfn!(hk_allocate_command_buffers),
        b"vkFreeCommandBuffers" => pfn!(hk_free_command_buffers),
        b"vkResetCommandPool" => pfn!(hk_reset_command_pool),
        b"vkDestroyCommandPool" => pfn!(hk_destroy_command_pool),
        b"vkBeginCommandBuffer" => pfn!(hk_begin_command_buffer),
        b"vkResetCommandBuffer" => pfn!(hk_reset_command_buffer),
        b"vkCmdBindPipeline" => pfn!(hk_cmd_bind_pipeline),
        b"vkCmdDispatch" => pfn!(hk_cmd_dispatch),
        b"vkQueueSubmit" => pfn!(hk_queue_submit),
        b"vkQueueSubmit2" | b"vkQueueSubmit2KHR" if has_submit2 => pfn!(hk_queue_submit2),
        b"vkGetDeviceProcAddr" => pfn!(hk_get_device_proc_addr),
        _ => None,
    }
}

unsafe extern "system" fn hk_get_device_proc_addr(device: vk::Device, name: *const c_char) -> vk::PFN_vkVoidFunction {
    if device == vk::Device::null() || name.is_null() {
        return None;
    }
    let dd = dev_of(device)?;
    let n = CStr::from_ptr(name);
    if matches!(n.to_bytes(), b"vkDestroyDevice" | b"vkGetDeviceProcAddr") {
        return device_hook(n, false);
    }
    if dd.active {
        if let Some(f) = device_hook(n, dd.submit2.is_some()) {
            return Some(f);
        }
    }
    (dd.gdpa)(device, name)
}

unsafe extern "system" fn hk_get_instance_proc_addr(instance: vk::Instance, name: *const c_char) -> vk::PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let n = CStr::from_ptr(name);
    match n.to_bytes() {
        b"vkCreateInstance" => return pfn!(hk_create_instance),
        b"vkGetInstanceProcAddr" => return pfn!(hk_get_instance_proc_addr),
        _ => {}
    }
    if instance == vk::Instance::null() {
        return None;
    }
    let inst = INSTANCES.read().unwrap_or_else(|e| e.into_inner()).get(&key(instance)).cloned()?;
    match n.to_bytes() {
        b"vkDestroyInstance" => return pfn!(hk_destroy_instance),
        b"vkCreateDevice" => return pfn!(hk_create_device),
        _ => {}
    }
    if let Some(f) = inst.active.then(|| device_hook(n, true)).flatten() {
        // Device-level entry points are resolved per device via vkGetDeviceProcAddr; only answer here
        // if the real implementation exists at instance level too.
        if (inst.gipa)(instance, name).is_some() {
            return Some(f);
        }
    }
    (inst.gipa)(instance, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_process_is_inactive() {
        // The test binary is not wivrn-server and FORCE is not set: pure pass-through.
        if std::env::var_os("WIVRN_MONITOR_SERVER_FORCE").is_some() {
            return;
        }
        assert!(!process_ok());
        let mut n = NegotiateLayerInterface {
            s_type: 1,
            p_next: std::ptr::null_mut(),
            loader_layer_interface_version: 2,
            pfn_get_instance_proc_addr: None,
            pfn_get_device_proc_addr: None,
            pfn_get_physical_device_proc_addr: std::ptr::null(),
        };
        let r = unsafe { vkNegotiateLoaderLayerInterfaceVersion(&mut n) };
        assert_eq!(r, vk::Result::SUCCESS);
        assert!(n.pfn_get_instance_proc_addr.is_some());

    }
}
