//! Connection to the viewer (the viewer is the server; the layer is the client).
//!
//! A background thread (started with the first Vulkan session) retries `connect` once per second and
//! reads the viewer's `Release` messages. The render thread only does an atomic check
//! ([`is_connected`]) and non-blocking sends, so the game never waits for the viewer.

use crate::log::Level;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use wivrn_monitor_protocol as proto;
use wivrn_monitor_protocol::{Message, SlotMask};

pub struct Conn {
    pub id: u64,
    fd: OwnedFd,
    pub busy: SlotMask,
    /// Generation of the ring currently in use on this connection.
    pub gen: AtomicU32,
    dead: AtomicBool,
}

pub enum SendResult {
    Ok,
    WouldBlock,
    Dead,
}

impl Conn {
    pub fn send(&self, msg: &Message, fd: Option<std::os::fd::RawFd>) -> SendResult {
        if self.dead.load(Ordering::Acquire) {
            return SendResult::Dead;
        }
        match proto::send_msg(self.fd.as_raw_fd(), msg, fd, true) {
            Ok(()) => SendResult::Ok,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => SendResult::WouldBlock,
            Err(e) => {
                log_at!(Level::Info, "viewer connection lost (send: {e})");
                self.kill();
                SendResult::Dead
            }
        }
    }

    /// Marks the connection dead; the reader thread notices via shutdown and reconnects.
    pub fn kill(&self) {
        self.dead.store(true, Ordering::Release);
        CONNECTED.store(false, Ordering::Release);
        proto::shutdown(self.fd.as_raw_fd());
    }
}

static CONNECTED: AtomicBool = AtomicBool::new(false);
static CURRENT: Mutex<Option<Arc<Conn>>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The only check on the hot path when no viewer is connected.
#[inline]
pub fn is_connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

pub fn current() -> Option<Arc<Conn>> {
    if !is_connected() {
        return None;
    }
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|c| !c.dead.load(Ordering::Acquire))
}

/// Mirrored devices that are alive (armed by foveation detection). We only connect while one exists;
/// processes that never create the foveation pass must not occupy the viewer.
static SESSIONS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static SESSION_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn session_opened(raw: u64) {
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.push(raw);
    SESSION_COUNT.store(s.len() as u32, Ordering::Release);
    drop(s);
    start();
}

pub fn session_closed(raw: u64) {
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = s.iter().position(|&x| x == raw) else { return };
    s.swap_remove(i);
    SESSION_COUNT.store(s.len() as u32, Ordering::Release);
    drop(s);
    if SESSION_COUNT.load(Ordering::Acquire) == 0 {
        // Release the viewer so another process can be shown.
        let cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(c) = cur {
            c.kill();
        }
    }
}

pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let r = std::thread::Builder::new().name("wivrn-monitor-srv-link".into()).spawn(|| loop {
            // A panic must never take the game down; just restart the loop.
            let _ = std::panic::catch_unwind(run_once);
            std::thread::sleep(Duration::from_millis(500));
        });
        if let Err(e) = r {
            log_at!(Level::Error, "cannot start link thread: {e}");
        }
    });
}

/// One connection attempt; if it succeeds, serves it until it ends.
fn run_once() {
    if SESSION_COUNT.load(Ordering::Acquire) == 0 {
        std::thread::sleep(Duration::from_millis(200));
        return;
    }
    let path = proto::socket_path();
    let fd = match proto::connect(&path) {
        Ok(fd) => fd,
        Err(_) => {
            std::thread::sleep(Duration::from_secs(1));
            return;
        }
    };
    let conn = Arc::new(Conn {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        fd,
        busy: SlotMask::new(),
        gen: AtomicU32::new(0),
        dead: AtomicBool::new(false),
    });
    if let Err(e) = proto::send_msg(
        conn.fd.as_raw_fd(),
        &Message::Hello { version: proto::VERSION, pid: std::process::id() },
        None,
        true,
    ) {
        log_at!(Level::Warn, "viewer hello failed: {e}");
        return;
    }
    log_at!(Level::Info, "viewer connected (conn {})", conn.id);
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn.clone());
    CONNECTED.store(true, Ordering::Release);
    loop {
        match proto::recv_msg(conn.fd.as_raw_fd()) {
            Ok(Some((Message::Release { gen, slot }, _))) => {
                if gen == conn.gen.load(Ordering::Acquire) {
                    conn.busy.release(slot);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                log_at!(Level::Info, "viewer connection lost (recv: {e})");
                break;
            }
        }
    }
    conn.kill();
    let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if cur.as_ref().is_some_and(|c| c.id == conn.id) {
        *cur = None;
    }
    log_at!(Level::Info, "viewer disconnected (conn {})", conn.id);
}
