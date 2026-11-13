//! The mirror for one VkDevice: our compute pipeline (appended to WiVRn's foveation dispatch), the
//! host-visible output stages, the timeline semaphore and the consumer thread that waits for it and
//! hands finished frames to the viewer.
//!
//! Stage life cycle (`Stage::state`): FREE -> RESERVED (a dispatch was recorded into a command
//! buffer) -> SUBMITTED (the vkQueueSubmit2 carrying it signalled `sem` = value) -> FREE (consumer
//! copied it into the ring). A reserved stage whose command buffer is re-begun/reset/freed is
//! released by the command-buffer tracking in lib.rs.

use crate::link::{self, SendResult};
use crate::log::Level;
use ash::vk;
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use wivrn_monitor_protocol::{self as proto, Mapping, Message, PixelFormat, RingInfo};

pub const SLOTS: u32 = 3;
pub const NSTAGES: usize = 3;
const HEADER_BYTES: u64 = 256;
const MAGIC: u32 = 0x574d_5652;
static SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mirror.spv"));

const FREE: u8 = 0;
const RESERVED: u8 = 1;
const SUBMITTED: u8 = 2;

pub type WaitSemaphoresFn = unsafe extern "system" fn(vk::Device, *const vk::SemaphoreWaitInfo<'_>, u64) -> vk::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub fps: u32,
    pub width: u32,
    /// (num, den); num == 0: native (no crop).
    pub aspect: (u32, u32),
    pub diag: bool,
}

impl Config {
    pub fn from_env() -> Config {
        let num = |n: &str, d: u32, lo: u32, hi: u32| {
            std::env::var(n).ok().and_then(|v| v.trim().parse::<u32>().ok()).map(|v| v.clamp(lo, hi)).unwrap_or(d)
        };
        let aspect = parse_aspect(&std::env::var("WIVRN_MONITOR_ASPECT").unwrap_or_default());
        Config {
            fps: num("WIVRN_MONITOR_FPS", 30, 1, 240),
            width: num("WIVRN_MONITOR_WIDTH", 1280, 64, 4096),
            aspect,
            diag: std::env::var("WIVRN_MONITOR_DIAG").map(|v| v == "1").unwrap_or(false),
        }
    }

    pub fn cap_h(&self) -> u32 {
        let h = if self.aspect.0 == 0 {
            self.width as u64 * 3 / 2
        } else {
            (self.width as u64 * self.aspect.1 as u64).div_ceil(self.aspect.0 as u64)
        };
        h.clamp(1, 4096) as u32
    }
}

/// "16:9" (default), "native"/"none"/"0", "4:3", ... Invalid values fall back to 16:9.
pub fn parse_aspect(s: &str) -> (u32, u32) {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return (16, 9);
    }
    if matches!(s.as_str(), "native" | "none" | "off" | "0") {
        return (0, 0);
    }
    if let Some((a, b)) = s.split_once(':') {
        if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
            if (1..=64).contains(&a) && (1..=64).contains(&b) {
                return (a, b);
            }
        }
    }
    (16, 9)
}

struct Stage {
    buffer: vk::Buffer,
    mem: vk::DeviceMemory,
    map: *mut u8,
    set: vk::DescriptorSet,
    state: AtomicU8,
}

#[derive(Clone, Copy)]
struct Item {
    stage: usize,
    value: u64,
    frame_nr: u64,
    ts_ns: u64,
    queued: Instant,
}

pub struct Mirror {
    pub dev: ash::Device,
    pub cfg: Config,
    cap_h: u32,
    shader: vk::ShaderModule,
    dsl: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    stages: Vec<Stage>,
    sem: vk::Semaphore,
    coherent: bool,
    wait_semaphores: WaitSemaphoresFn,
    /// Serialises value assignment + submission of signals (timeline values must increase).
    pub submit_lock: Mutex<u64>,
    next_capture: Mutex<Instant>,
    interval: Duration,
    frame_nr: AtomicU64,
    start: Instant,
    queue: Mutex<VecDeque<Item>>,
    cv: Condvar,
    stop: AtomicBool,
    dead: AtomicBool,
    pub reserved_count: AtomicU32,
    dispatch_count: AtomicU64,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

// SAFETY: raw map pointers point to Vulkan memory owned by the Mirror; stage access is serialised by
// the stage state machine.
unsafe impl Send for Mirror {}
unsafe impl Sync for Mirror {}

fn ve(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |r| format!("{what}: {r:?}")
}

impl Mirror {
    /// Creates all Vulkan objects. `fov_dsl` is WiVRn's foveation descriptor set layout (our set 0).
    pub fn new(
        dev: ash::Device,
        mem_props: vk::PhysicalDeviceMemoryProperties,
        fov_dsl: vk::DescriptorSetLayout,
        wait_semaphores: WaitSemaphoresFn,
        cfg: Config,
    ) -> Result<Arc<Mirror>, String> {
        let cap_h = cfg.cap_h();
        let mut m = Mirror {
            dev,
            cfg,
            cap_h,
            shader: vk::ShaderModule::null(),
            dsl: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            pool: vk::DescriptorPool::null(),
            stages: Vec::new(),
            sem: vk::Semaphore::null(),
            coherent: true,
            wait_semaphores,
            submit_lock: Mutex::new(0),
            next_capture: Mutex::new(Instant::now()),
            interval: Duration::from_micros(1_000_000 / cfg.fps as u64),
            frame_nr: AtomicU64::new(0),
            start: Instant::now(),
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            reserved_count: AtomicU32::new(0),
            dispatch_count: AtomicU64::new(0),
            thread: Mutex::new(None),
        };
        if let Err(e) = m.create(mem_props, fov_dsl) {
            m.destroy_resources();
            return Err(e);
        }
        let m = Arc::new(m);
        let m2 = m.clone();
        let h = std::thread::Builder::new()
            .name("wivrn-monitor-srv-consumer".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m2.consumer())).is_err() {
                    log_at!(Level::Error, "consumer thread panicked; mirroring disabled");
                    m2.dead.store(true, Ordering::Release);
                }
            })
            .map_err(|e| format!("thread: {e}"))?;
        *m.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
        Ok(m)
    }

    fn create(&mut self, mp: vk::PhysicalDeviceMemoryProperties, fov_dsl: vk::DescriptorSetLayout) -> Result<(), String> {
        let d = self.dev.clone();
        // SAFETY: plain object creation on a valid device; every handle is stored for destroy_resources.
        unsafe {
            let words: Vec<u32> = SPIRV.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
            if words.is_empty() {
                return Err("empty SPIR-V".into());
            }
            self.shader = d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None).map_err(ve("vkCreateShaderModule"))?;
            let b = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)];
            self.dsl = d.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&b), None).map_err(ve("vkCreateDescriptorSetLayout"))?;
            let sets = [fov_dsl, self.dsl];
            self.layout = d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&sets), None).map_err(ve("vkCreatePipelineLayout"))?;
            let spec_data: [u32; 4] = [self.cfg.width, self.cap_h, self.cfg.aspect.0, self.cfg.aspect.1];
            let spec_bytes: Vec<u8> = spec_data.iter().flat_map(|v| v.to_le_bytes()).collect();
            let entries: Vec<vk::SpecializationMapEntry> =
                (0..4u32).map(|i| vk::SpecializationMapEntry { constant_id: i, offset: i * 4, size: 4 }).collect();
            let spec = vk::SpecializationInfo::default().map_entries(&entries).data(&spec_bytes);
            let ci = vk::ComputePipelineCreateInfo::default()
                .stage(vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(self.shader).name(c"main").specialization_info(&spec))
                .layout(self.layout);
            self.pipeline = d.create_compute_pipelines(vk::PipelineCache::null(), &[ci], None).map_err(|(_, e)| format!("vkCreateComputePipelines: {e:?}"))?[0];

            let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: NSTAGES as u32 }];
            self.pool = d.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(NSTAGES as u32).pool_sizes(&sizes), None).map_err(ve("vkCreateDescriptorPool"))?;

            let mut tci = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
            self.sem = d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut tci), None).map_err(ve("vkCreateSemaphore(timeline)"))?;

            let size = HEADER_BYTES + self.cfg.width as u64 * self.cap_h as u64 * 4;
            for _ in 0..NSTAGES {
                let buffer = d.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::STORAGE_BUFFER).sharing_mode(vk::SharingMode::EXCLUSIVE), None).map_err(ve("vkCreateBuffer"))?;
                let mut st = Stage { buffer, mem: vk::DeviceMemory::null(), map: std::ptr::null_mut(), set: vk::DescriptorSet::null(), state: AtomicU8::new(FREE) };
                let rq = d.get_buffer_memory_requirements(buffer);
                let wants = [
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    vk::MemoryPropertyFlags::HOST_VISIBLE,
                ];
                let found = wants.iter().find_map(|w| {
                    (0..mp.memory_type_count).find(|&i| rq.memory_type_bits & (1 << i) != 0 && mp.memory_types[i as usize].property_flags.contains(*w)).map(|i| (i, *w))
                });
                let Some((ti, w)) = found else {
                    d.destroy_buffer(buffer, None);
                    return Err("no host-visible memory type".into());
                };
                if !w.contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
                    self.coherent = false;
                }
                st.mem = match d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(rq.size).memory_type_index(ti), None) {
                    Ok(m) => m,
                    Err(e) => {
                        d.destroy_buffer(buffer, None);
                        return Err(format!("vkAllocateMemory: {e:?}"));
                    }
                };
                // From here the stage owns its objects; push first so destroy_resources frees them on error.
                self.stages.push(Stage { buffer: st.buffer, mem: st.mem, map: std::ptr::null_mut(), set: vk::DescriptorSet::null(), state: AtomicU8::new(FREE) });
                let idx = self.stages.len() - 1;
                d.bind_buffer_memory(buffer, st.mem, 0).map_err(ve("vkBindBufferMemory"))?;
                let p = d.map_memory(st.mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).map_err(ve("vkMapMemory"))?;
                self.stages[idx].map = p as *mut u8;
                let layouts = [self.dsl];
                let set = d.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.pool).set_layouts(&layouts)).map_err(ve("vkAllocateDescriptorSets"))?[0];
                self.stages[idx].set = set;
                let bi = [vk::DescriptorBufferInfo { buffer, offset: 0, range: vk::WHOLE_SIZE }];
                d.update_descriptor_sets(&[vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0).descriptor_type(vk::DescriptorType::STORAGE_BUFFER).buffer_info(&bi)], &[]);
            }
        }
        Ok(())
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    pub fn kill(&self) {
        self.dead.store(true, Ordering::Release);
    }

    /// Dispatch size of our pass.
    pub fn groups(&self) -> (u32, u32) {
        (self.cfg.width.div_ceil(8), self.cap_h.div_ceil(8))
    }

    /// Called per fov dispatch. Returns a reserved stage if a frame should be captured now.
    pub fn try_reserve(&self) -> Option<usize> {
        self.dispatch_count.fetch_add(1, Ordering::Relaxed);
        if self.is_dead() || !link::is_connected() {
            return None;
        }
        let mut nc = self.next_capture.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if now < *nc {
            return None;
        }
        let i = self.stages.iter().position(|s| s.state.compare_exchange(FREE, RESERVED, Ordering::AcqRel, Ordering::Acquire).is_ok())?;
        *nc = (*nc + self.interval).max(now);
        self.reserved_count.fetch_add(1, Ordering::AcqRel);
        Some(i)
    }

    pub fn dispatch_count(&self) -> u64 {
        self.dispatch_count.load(Ordering::Relaxed)
    }

    /// Records our pass after WiVRn's foveation dispatch. `restore` = their pipeline to re-bind.
    ///
    /// # Safety
    /// `cmd` must be in the recording state, with `restore` bound as compute pipeline and its set 0 bound.
    pub unsafe fn record(&self, cmd: vk::CommandBuffer, stage: usize, restore: vk::Pipeline) {
        let d = &self.dev;
        let st = &self.stages[stage];
        let (gx, gy) = self.groups();
        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
        d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::COMPUTE, self.layout, 1, &[st.set], &[]);
        d.cmd_dispatch(cmd, gx, gy, 1);
        let bar = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(st.buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        d.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, vk::PipelineStageFlags::HOST, vk::DependencyFlags::empty(), &[], &[bar], &[]);
        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, restore);
    }

    /// Gives a reserved (never submitted) stage back.
    pub fn release(&self, stage: usize) {
        if self.stages[stage].state.compare_exchange(RESERVED, FREE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.reserved_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Marks stages as submitted with the given timeline value and wakes the consumer.
    pub fn submitted(&self, stages: &[usize], value: u64) {
        let nr = self.frame_nr.fetch_add(1, Ordering::Relaxed) + 1;
        let ts_ns = self.start.elapsed().as_nanos() as u64;
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        for &s in stages {
            if self.stages[s].state.compare_exchange(RESERVED, SUBMITTED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                self.reserved_count.fetch_sub(1, Ordering::AcqRel);
                q.push_back(Item { stage: s, value, frame_nr: nr, ts_ns, queued: Instant::now() });
            }
        }
        drop(q);
        self.cv.notify_one();
    }

    pub fn semaphore(&self) -> vk::Semaphore {
        self.sem
    }

    // ---------------------------------------------------------------- consumer

    fn consumer(&self) {
        let mut ring: Option<(u64, RingInfo, Mapping)> = None;
        let mut shown = 0u64;
        loop {
            let item = {
                let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if self.stop.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(i) = q.front().copied() {
                        break i;
                    }
                    q = self.cv.wait_timeout(q, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner()).0;
                }
            };
            let done = self.wait_value(item);
            self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            if let Some(true) = done {
                if let Err(e) = self.deliver(&item, &mut ring, &mut shown) {
                    log_at!(Level::Error, "deliver failed: {e}; mirroring disabled");
                    self.kill();
                }
            }
            self.stages[item.stage].state.store(FREE, Ordering::Release);
        }
    }

    /// Some(true) = signalled, Some(false) = gave up (timeout / error), None = stop requested.
    fn wait_value(&self, item: Item) -> Option<bool> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return None;
            }
            let sems = [self.sem];
            let vals = [item.value];
            let info = vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&vals);
            // SAFETY: valid device + semaphore; the function pointer was loaded for this device.
            let r = unsafe { (self.wait_semaphores)(self.dev.handle(), &info, 100_000_000) };
            match r {
                vk::Result::SUCCESS => return Some(true),
                vk::Result::TIMEOUT => {
                    if item.queued.elapsed() > Duration::from_secs(5) {
                        log_at!(Level::Warn, "timeline value {} not reached after 5 s; dropping frame", item.value);
                        return Some(false);
                    }
                }
                e => {
                    log_at!(Level::Error, "vkWaitSemaphores: {e:?}; mirroring disabled");
                    self.kill();
                    return Some(false);
                }
            }
        }
    }

    fn deliver(&self, item: &Item, ring: &mut Option<(u64, RingInfo, Mapping)>, shown: &mut u64) -> Result<(), String> {
        let st = &self.stages[item.stage];
        if !self.coherent {
            let r = vk::MappedMemoryRange::default().memory(st.mem).offset(0).size(vk::WHOLE_SIZE);
            // SAFETY: memory is mapped and belongs to this device.
            unsafe { self.dev.invalidate_mapped_memory_ranges(&[r]) }.map_err(ve("vkInvalidateMappedMemoryRanges"))?;
        }
        // SAFETY: the stage buffer is at least HEADER_BYTES long; the GPU finished writing (semaphore).
        let hdr: [u32; 4] = unsafe {
            let p = st.map as *const u32;
            [p.read_volatile(), p.add(1).read_volatile(), p.add(2).read_volatile(), p.add(3).read_volatile()]
        };
        if hdr[2] != MAGIC {
            return Err(format!("bad stage header {hdr:x?} (output not written)"));
        }
        let (w, h) = (hdr[0], hdr[1]);
        if w == 0 || h == 0 {
            return Ok(()); // empty source rect this frame
        }
        if w > self.cfg.width || h > self.cap_h {
            return Err(format!("stage header size {w}x{h} exceeds limits"));
        }
        *shown += 1;
        if self.cfg.diag && (*shown == 1 || (*shown).is_multiple_of(300)) {
            log_at!(Level::Info, "DIAG frame #{}: source rect {}x{} -> output {}x{} (aspect {:?})", shown, hdr[3] & 0xffff, hdr[3] >> 16, w, h, self.cfg.aspect);
        }
        let Some(conn) = link::current() else { return Ok(()) };
        let reuse = matches!(ring, Some((id, info, _)) if *id == conn.id && info.width == w && info.height == h);
        if !reuse {
            *ring = None;
            static GEN: AtomicU32 = AtomicU32::new(0);
            let gen = GEN.fetch_add(1, Ordering::Relaxed) + 1;
            let info = RingInfo::new(gen, w, h, PixelFormat::Rgba8Srgb, SLOTS);
            info.validate().map_err(|e| format!("ring: {e}"))?;
            let fd = proto::create_memfd("wivrn-monitor-ring", info.total_size()).map_err(|e| format!("memfd: {e}"))?;
            let map = Mapping::new(fd.as_raw_fd(), info.total_size() as usize, true).map_err(|e| format!("mmap: {e}"))?;
            conn.busy.reset();
            conn.gen.store(gen, Ordering::Release);
            match conn.send(&Message::Setup(info), Some(fd.as_raw_fd())) {
                SendResult::Ok => {
                    log_at!(Level::Info, "ring set up: {w}x{h} gen {gen}");
                    *ring = Some((conn.id, info, map));
                }
                _ => return Ok(()),
            }
        }
        let (_, info, map) = ring.as_ref().unwrap();
        let Some(slot) = conn.busy.try_acquire(info.slots) else { return Ok(()) }; // viewer slow: drop frame
        let range = info.slot_range(slot).unwrap();
        // SAFETY: slot reserved for us; the stage holds w*h*4 pixel bytes after the header.
        unsafe {
            let dst = map.slice_mut(range.clone());
            std::ptr::copy_nonoverlapping(st.map.add(HEADER_BYTES as usize), dst.as_mut_ptr(), range.len());
        }
        match conn.send(&Message::Frame { gen: info.gen, slot, frame_nr: item.frame_nr, timestamp_ns: item.ts_ns }, None) {
            SendResult::Ok => {}
            SendResult::WouldBlock | SendResult::Dead => conn.busy.release(slot),
        }
        Ok(())
    }

    // ---------------------------------------------------------------- teardown

    /// Stops the consumer thread and destroys everything. Device must still be alive; the caller
    /// guarantees no submission referencing our objects is still pending (we wait for idle first).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.cv.notify_all();
        let h = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
        // SAFETY: valid device; makes sure our recorded work is finished before objects go away.
        unsafe {
            let _ = self.dev.device_wait_idle();
        }
        self.destroy_resources_ref();
    }

    fn destroy_resources(&mut self) {
        self.destroy_resources_ref();
    }

    fn destroy_resources_ref(&self) {
        let d = &self.dev;
        // SAFETY: only called when nothing uses these objects any more.
        unsafe {
            for s in &self.stages {
                if !s.map.is_null() {
                    d.unmap_memory(s.mem);
                }
                d.destroy_buffer(s.buffer, None);
                d.free_memory(s.mem, None);
            }
            // Sets die with the pool.
            if self.pool != vk::DescriptorPool::null() {
                d.destroy_descriptor_pool(self.pool, None);
            }
            if self.sem != vk::Semaphore::null() {
                d.destroy_semaphore(self.sem, None);
            }
            if self.pipeline != vk::Pipeline::null() {
                d.destroy_pipeline(self.pipeline, None);
            }
            if self.layout != vk::PipelineLayout::null() {
                d.destroy_pipeline_layout(self.layout, None);
            }
            if self.dsl != vk::DescriptorSetLayout::null() {
                d.destroy_descriptor_set_layout(self.dsl, None);
            }
            if self.shader != vk::ShaderModule::null() {
                d.destroy_shader_module(self.shader, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_parsing() {
        assert_eq!(parse_aspect(""), (16, 9));
        assert_eq!(parse_aspect("16:9"), (16, 9));
        assert_eq!(parse_aspect(" 4:3 "), (4, 3));
        assert_eq!(parse_aspect("native"), (0, 0));
        assert_eq!(parse_aspect("garbage"), (16, 9));
        assert_eq!(parse_aspect("0:9"), (16, 9));
    }

    #[test]
    fn cap_height() {
        let c = Config { fps: 30, width: 1280, aspect: (16, 9), diag: false };
        assert_eq!(c.cap_h(), 720);
        assert_eq!(Config { aspect: (0, 0), ..c }.cap_h(), 1920);
        assert_eq!(Config { width: 4096, aspect: (1, 2), ..c }.cap_h(), 4096);
    }
}
