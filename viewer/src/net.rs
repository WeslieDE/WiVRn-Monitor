//! Viewer-side socket server: accepts layer connections, receives frames from the shared-memory ring.

use crate::recorder::Recorder;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use wivrn_monitor_protocol as proto;
use wivrn_monitor_protocol::{Mapping, Message, RingInfo};

#[derive(Default)]
pub struct FrameBuf {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Incremented per received frame; 0 = no frame yet.
    pub seq: u64,
    pub connected: bool,
    pub last_frame: Option<Instant>,
    /// Arrival times of the frames of the last second (for the FPS measurement).
    pub recent: VecDeque<Instant>,
}

impl FrameBuf {
    /// Frames received during the last second (sliding window).
    pub fn fps(&self) -> usize {
        let now = Instant::now();
        self.recent.iter().filter(|t| now.duration_since(**t).as_secs_f32() <= 1.0).count()
    }
}

pub type Shared = Arc<Mutex<FrameBuf>>;

/// Called from the net thread whenever the UI should redraw.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

struct Active {
    fd: OwnedFd,
    ring: Option<(RingInfo, Mapping)>,
}

/// Runs forever (own thread). Several layer processes may connect; the first is shown, the others
/// wait in line and take over when it disconnects.
pub fn run(listener: OwnedFd, shared: Shared, proxy: Wake, rec: Arc<Recorder>) {
    let mut waiting: VecDeque<OwnedFd> = VecDeque::new();
    let mut active: Option<Active> = None;
    loop {
        let mut fds = vec![listener.as_raw_fd()];
        if let Some(a) = &active {
            fds.push(a.fd.as_raw_fd());
        }
        let ready = match proto::poll_readable(&fds, 250) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("poll: {e}");
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
        };
        if ready[0] {
            while let Ok(Some(fd)) = proto::accept(listener.as_raw_fd()) {
                let _ = proto::set_nonblocking(fd.as_raw_fd(), true);
                waiting.push_back(fd);
            }
        }
        if let Some(a) = active.as_mut() {
            if ready.get(1) == Some(&true) && !pump(a, &shared, &proxy, &rec) {
                active = None;
                rec.on_disconnect();
                set_state(&shared, false);
                proxy();
            }
        }
        if active.is_none() {
            if let Some(fd) = waiting.pop_front() {
                // Skip connections that died while waiting (the first read will tell).
                active = Some(Active { fd, ring: None });
                set_state(&shared, true);
                proxy();
            }
        }
    }
}

fn set_state(shared: &Shared, connected: bool) {
    let mut f = shared.lock().unwrap_or_else(|e| e.into_inner());
    f.connected = connected;
    f.last_frame = None;
    f.width = 0;
    f.height = 0;
    f.rgba.clear();
    f.seq = 0;
    f.recent.clear();
}

/// Handles all currently readable messages. Returns false if the connection ended.
fn pump(a: &mut Active, shared: &Shared, proxy: &Wake, rec: &Recorder) -> bool {
    loop {
        match proto::recv_msg(a.fd.as_raw_fd()) {
            Ok(None) => return false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(e) => {
                eprintln!("layer connection: {e}");
                return false;
            }
            Ok(Some((msg, fd))) => {
                if !handle(a, msg, fd, shared, proxy, rec) {
                    return false;
                }
            }
        }
    }
}

fn handle(a: &mut Active, msg: Message, fd: Option<OwnedFd>, shared: &Shared, proxy: &Wake, rec: &Recorder) -> bool {
    match msg {
        Message::Hello { version, pid } => {
            if version != proto::VERSION {
                eprintln!("layer (pid {pid}) speaks protocol {version}, expected {}", proto::VERSION);
                return false;
            }
            eprintln!("layer connected (pid {pid})");
        }
        Message::Setup(info) => {
            let Some(fd) = fd else { return false };
            if let Err(e) = info.validate() {
                eprintln!("invalid ring: {e}");
                return false;
            }
            match proto::fd_size(fd.as_raw_fd()) {
                Ok(sz) if sz >= info.total_size() => {}
                _ => {
                    eprintln!("ring memfd too small");
                    return false;
                }
            }
            match Mapping::new(fd.as_raw_fd(), info.total_size() as usize, false) {
                Ok(m) => {
                    eprintln!("ring: {}x{} {:?} gen {}", info.width, info.height, info.format, info.gen);
                    a.ring = Some((info, m));
                }
                Err(e) => {
                    eprintln!("mmap: {e}");
                    return false;
                }
            }
        }
        Message::Frame { gen, slot, .. } => {
            let Some((info, map)) = a.ring.as_ref() else { return true };
            if info.gen != gen {
                return true;
            }
            let Some(range) = info.slot_range(slot) else { return false };
            // SAFETY: the layer does not touch the slot until we send Release.
            let data = unsafe { map.slice(range) };
            {
                let mut f = shared.lock().unwrap_or_else(|e| e.into_inner());
                f.rgba.clear();
                f.rgba.extend_from_slice(data);
                f.width = info.width;
                f.height = info.height;
                f.seq += 1;
                let now = Instant::now();
                f.last_frame = Some(now);
                f.recent.push_back(now);
                while f.recent.front().is_some_and(|t| now.duration_since(*t).as_secs_f32() > 1.0) {
                    f.recent.pop_front();
                }
            }
            // Recording gets exactly the received pixels (no UI); it only copies them and never blocks.
            rec.on_frame(info.width, info.height, data, Instant::now());
            if proto::send_msg(a.fd.as_raw_fd(), &Message::Release { gen, slot }, None, true).is_err() {
                // Layer vanished or its socket buffer is full; a dead peer is noticed by recv.
            }
            proxy();
        }
        Message::Release { .. } => {}
    }
    true
}
