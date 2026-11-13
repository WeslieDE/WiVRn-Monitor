//! Recording of the received frames: front end (this file), colour conversion (`yuv`), MP4 writer (`mp4`) and the
//! encoder thread (`worker`). See docs/TECHNICAL.md ("Recording").
//!
//! Threads: the network thread calls [`Recorder::on_frame`] (a memcpy into a pooled buffer and a non-blocking
//! queue push, nothing else); one encoder thread per output file does conversion, H.264 encoding and file I/O;
//! the UI thread toggles recording and reads the status. If the encoder falls behind, frames are dropped (counted).

pub mod mp4;
mod worker;
pub mod yuv;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use worker::{FrameMsg, Outcome, WorkerShared};

pub use worker::{describe_io_error, BITRATE_BPS, KEYFRAME_INTERVAL};

/// Environment variable that replaces `~/Videos/VR` (used by tests).
pub const VIDEO_DIR_ENV: &str = "WIVRN_MONITOR_VIDEO_DIR";
/// Without frames for this long, the recording stops (auto mode) or the file is closed (manual mode).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames waiting for the encoder (about 29 MB at 1280x720).
const QUEUE_CAP: usize = 8;
/// A gap longer than this many frames is not filled with repeated pictures but cut out of the timeline.
const MAX_FILLED_GAP: u64 = mp4::FPS as u64;

// ---------------------------------------------------------------- folder and file names

fn resolve_video_dir(env: Option<OsString>, home: &Path) -> PathBuf {
    match env.filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => home.join("Videos/VR"),
    }
}

/// `~/Videos/VR`, or `$WIVRN_MONITOR_VIDEO_DIR`.
pub fn video_dir() -> PathBuf {
    resolve_video_dir(std::env::var_os(VIDEO_DIR_ENV), &crate::layer_install::home_dir())
}

/// Local time as `YYYY-MM-DD_HH-MM-SS`.
pub fn timestamp_now() -> String {
    let t = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
}

fn file_name(stamp: &str, n: u32) -> String {
    if n <= 1 {
        format!("VR_{stamp}.mp4")
    } else {
        format!("VR_{stamp}_{n}.mp4")
    }
}

/// Creates `dir` if needed and a new file `VR_<stamp>.mp4` (`_2`, `_3`, ... on collision; never overwrites).
pub fn create_unique_file(dir: &Path, stamp: &str) -> io::Result<(PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    for n in 1..10_000 {
        let path = dir.join(file_name(stamp, n));
        match OpenOptions::new().write(true).read(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many files with the same name"))
}

/// Opens the recordings folder (created if missing) in the file manager. Errors are returned for logging.
pub fn open_folder(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| describe_io_error("Cannot create the folder", dir, &e))?;
    let mut child = std::process::Command::new("xdg-open").arg(dir).spawn().map_err(|e| format!("xdg-open: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ---------------------------------------------------------------- timeline and policy helpers

/// Maps arrival times to slots of the constant 30 fps output timeline.
#[derive(Default)]
pub struct SlotClock {
    t0: Option<Instant>,
    next: u64,
}

impl SlotClock {
    /// Slot for a frame arriving at `now`, or `None` if that slot is already taken (source faster than 30 fps).
    pub fn slot_for(&mut self, now: Instant) -> Option<u64> {
        let t0 = *self.t0.get_or_insert(now);
        let mut slot = (now.saturating_duration_since(t0).as_secs_f64() * mp4::FPS as f64).round() as u64;
        if slot < self.next {
            return None;
        }
        if slot - self.next > MAX_FILLED_GAP {
            // Long pause: do not fill it with a frozen picture, continue right after the last frame.
            self.t0 = Some(t0 + Duration::from_secs_f64((slot - self.next) as f64 / mp4::FPS as f64));
            slot = self.next;
        }
        self.next = slot + 1;
        Some(slot)
    }
}

/// True if no frame arrived for `timeout` (or ever) since `last_frame`.
pub fn idle_too_long(last_frame: Option<Instant>, now: Instant, timeout: Duration) -> bool {
    last_frame.is_none_or(|t| now.saturating_duration_since(t) >= timeout)
}

// ---------------------------------------------------------------- recorder

/// Reusable RGBA buffers, so that no 3.7 MB allocation happens per frame.
#[derive(Clone, Default)]
pub struct Pool(Arc<Mutex<Vec<Vec<u8>>>>);

impl Pool {
    fn take(&self) -> Vec<u8> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap_or_default()
    }
    pub(crate) fn give_back(&self, v: Vec<u8>) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() < QUEUE_CAP + 2 {
            p.push(v);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Saved { path: PathBuf, bytes: u64, frames: u64 },
    /// Recording stopped because of this problem (message is user-facing English).
    Error(String),
}

#[derive(Debug, Default)]
pub struct RecStatus {
    pub recording: bool,
    pub elapsed: Duration,
    pub dropped: u64,
    /// Waiting for the first frame / the connection.
    pub waiting: bool,
    pub events: Vec<Event>,
}

struct Active {
    tx: SyncSender<FrameMsg>,
    handle: JoinHandle<()>,
    shared: Arc<WorkerShared>,
    dims: (u32, u32),
}

struct Inner {
    auto_enabled: bool,
    /// `Some(auto)` while recording is switched on.
    armed: Option<bool>,
    armed_since: Option<Instant>,
    /// After a manual stop or an error, auto mode waits for the next session.
    auto_blocked: bool,
    worker: Option<Active>,
    finishing: Vec<(JoinHandle<()>, Arc<WorkerShared>)>,
    pool: Pool,
    clock: SlotClock,
    last_frame: Option<Instant>,
    dropped: u64,
    events: Vec<Event>,
    dir: Option<PathBuf>,
}

pub struct Recorder {
    inner: Mutex<Inner>,
    pool: Pool,
}

impl Recorder {
    pub fn new(auto_enabled: bool) -> Self {
        let pool = Pool::default();
        Recorder {
            inner: Mutex::new(Inner {
                auto_enabled,
                armed: None,
                armed_since: None,
                auto_blocked: false,
                worker: None,
                finishing: Vec::new(),
                pool: pool.clone(),
                clock: SlotClock::default(),
                last_frame: None,
                dropped: 0,
                events: Vec::new(),
                dir: None,
            }),
            pool,
        }
    }

    /// Records into `dir` instead of [`video_dir`] (tests).
    pub fn with_dir(auto_enabled: bool, dir: PathBuf) -> Self {
        let r = Self::new(auto_enabled);
        r.lock().dir = Some(dir);
        r
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_auto(&self, on: bool) {
        self.lock().auto_enabled = on;
    }

    pub fn is_recording(&self) -> bool {
        self.lock().armed.is_some()
    }

    pub fn start(&self) {
        let mut i = self.lock();
        i.auto_blocked = false;
        match i.armed {
            Some(_) => i.armed = Some(false), // an automatic recording becomes a manual one
            None => i.arm(false, Instant::now()),
        }
    }

    pub fn stop(&self) {
        let mut i = self.lock();
        i.disarm();
        i.auto_blocked = true;
    }

    pub fn toggle(&self) {
        if self.is_recording() {
            self.stop();
        } else {
            self.start();
        }
    }

    /// Called by the network thread for every received frame (`rgba` = `width * height * 4` bytes).
    pub fn on_frame(&self, width: u32, height: u32, rgba: &[u8], now: Instant) {
        let mut i = self.lock();
        i.poll();
        if i.armed.is_none() {
            if i.auto_enabled && !i.auto_blocked {
                i.arm(true, now);
            } else {
                return;
            }
        }
        i.last_frame = Some(now);
        if i.worker.as_ref().is_none_or(|a| a.dims != (width, height)) {
            i.close_worker();
            if let Err(e) = i.spawn_worker(width, height) {
                i.fail(format!("Cannot start the recording: {e}"));
                return;
            }
        }
        let Some(slot) = i.clock.slot_for(now) else { return };
        let mut buf = self.pool.take();
        buf.clear();
        buf.extend_from_slice(rgba);
        let w = i.worker.as_ref().expect("worker exists");
        match w.tx.try_send(FrameMsg { slot, width, height, rgba: buf }) {
            Ok(()) => {}
            Err(TrySendError::Full(m)) => {
                self.pool.give_back(m.rgba);
                i.dropped += 1;
                if i.dropped == 1 || i.dropped.is_multiple_of(100) {
                    eprintln!("recorder: encoder is behind, {} frame(s) dropped so far", i.dropped);
                }
            }
            Err(TrySendError::Disconnected(m)) => self.pool.give_back(m.rgba), // worker ended; poll() reports why
        }
    }

    /// Called when the layer connection ends: the file is closed, the next session gets a new file.
    pub fn on_disconnect(&self) {
        let mut i = self.lock();
        i.close_worker();
        i.last_frame = None;
        i.auto_blocked = false;
        if i.armed == Some(true) {
            i.armed = None;
            i.armed_since = None;
        }
    }

    /// Periodic housekeeping from the UI thread (idle detection, collecting results).
    pub fn tick(&self, now: Instant) {
        let mut i = self.lock();
        i.poll();
        if i.armed.is_some() && idle_too_long(i.last_frame, now, IDLE_TIMEOUT) && (i.worker.is_some() || i.armed == Some(true)) {
            i.close_worker();
            if i.armed == Some(true) {
                i.disarm();
            }
        }
    }

    pub fn status(&self) -> RecStatus {
        let mut i = self.lock();
        i.poll();
        RecStatus {
            recording: i.armed.is_some(),
            elapsed: i.armed_since.map(|t| t.elapsed()).unwrap_or_default(),
            dropped: i.dropped,
            waiting: i.armed.is_some() && i.worker.is_none(),
            events: std::mem::take(&mut i.events),
        }
    }

    /// Stops recording and waits until all files are finished (window close).
    pub fn shutdown(&self) {
        let mut i = self.lock();
        i.disarm();
        for (handle, shared) in std::mem::take(&mut i.finishing) {
            let _ = handle.join();
            i.collect(&shared);
        }
    }
}

impl Inner {
    fn arm(&mut self, auto: bool, now: Instant) {
        self.armed = Some(auto);
        self.armed_since = Some(now);
        self.dropped = 0;
    }

    fn disarm(&mut self) {
        self.close_worker();
        self.armed = None;
        self.armed_since = None;
    }

    fn fail(&mut self, message: String) {
        eprintln!("recorder: {message}");
        self.events.push(Event::Error(message));
        self.disarm();
        self.auto_blocked = true;
    }

    fn spawn_worker(&mut self, width: u32, height: u32) -> io::Result<()> {
        let dir = self.dir.clone().unwrap_or_else(video_dir);
        let (tx, rx) = sync_channel(QUEUE_CAP);
        let shared = Arc::new(WorkerShared::default());
        let (s, pool, stamp) = (shared.clone(), self.pool.clone(), timestamp_now());
        let handle = std::thread::Builder::new().name("rec-encoder".into()).spawn(move || worker::run(dir, stamp, rx, s, pool))?;
        self.clock = SlotClock::default();
        self.worker = Some(Active { tx, handle, shared, dims: (width, height) });
        Ok(())
    }

    /// Ends the current file: dropping the sender lets the thread finish the queued frames and close the file.
    fn close_worker(&mut self) {
        if let Some(a) = self.worker.take() {
            let Active { tx, handle, shared, .. } = a;
            drop(tx);
            self.finishing.push((handle, shared));
        }
    }

    fn collect(&mut self, shared: &WorkerShared) -> bool {
        match shared.take_outcome() {
            Some(Outcome::Saved { path, bytes, frames }) => self.events.push(Event::Saved { path, bytes, frames }),
            Some(Outcome::Failed { message, path }) => {
                eprintln!("recorder: {message} ({path:?})");
                self.events.push(Event::Error(format!("Recording stopped. {message}")));
                return true;
            }
            None => {}
        }
        false
    }

    /// Collects results of finished encoder threads.
    fn poll(&mut self) {
        use std::sync::atomic::Ordering;
        let mut failed = false;
        if self.worker.as_ref().is_some_and(|a| a.shared.done.load(Ordering::Acquire)) {
            let a = self.worker.take().expect("checked");
            let _ = a.handle.join();
            failed |= self.collect(&a.shared);
        }
        let mut still = Vec::new();
        for (handle, shared) in std::mem::take(&mut self.finishing) {
            if shared.done.load(Ordering::Acquire) {
                let _ = handle.join();
                self.collect(&shared);
            } else {
                still.push((handle, shared));
            }
        }
        self.finishing = still;
        if failed {
            self.armed = None;
            self.armed_since = None;
            self.auto_blocked = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_and_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let (p1, _f1) = create_unique_file(dir.path(), "2026-01-02_03-04-05").unwrap();
        let (p2, _f2) = create_unique_file(dir.path(), "2026-01-02_03-04-05").unwrap();
        let (p3, _f3) = create_unique_file(dir.path(), "2026-01-02_03-04-05").unwrap();
        assert_eq!(p1.file_name().unwrap(), "VR_2026-01-02_03-04-05.mp4");
        assert_eq!(p2.file_name().unwrap(), "VR_2026-01-02_03-04-05_2.mp4");
        assert_eq!(p3.file_name().unwrap(), "VR_2026-01-02_03-04-05_3.mp4");
        let stamp = timestamp_now();
        assert_eq!(stamp.len(), 19);
        assert_eq!(&stamp[10..11], "_");
    }

    #[test]
    fn creates_missing_folder_and_honours_env_override() {
        let base = tempfile::tempdir().unwrap();
        let home = Path::new("/home/someone");
        assert_eq!(resolve_video_dir(None, home), Path::new("/home/someone/Videos/VR"));
        assert_eq!(resolve_video_dir(Some("".into()), home), Path::new("/home/someone/Videos/VR"));
        assert_eq!(resolve_video_dir(Some(base.path().into()), home), base.path());
        // The real environment variable (only this test touches it).
        let nested = base.path().join("a/b/VR");
        std::env::set_var(VIDEO_DIR_ENV, &nested);
        assert_eq!(video_dir(), nested);
        assert!(!nested.exists());
        let (path, _f) = create_unique_file(&video_dir(), "x").unwrap();
        assert!(nested.is_dir() && path.starts_with(&nested));
        std::env::remove_var(VIDEO_DIR_ENV);
    }

    #[test]
    fn unwritable_folder_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_unique_file(&file.join("sub"), "x").is_err());
    }

    #[test]
    fn auto_stop_logic() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        assert!(!idle_too_long(Some(s(0)), s(4), IDLE_TIMEOUT));
        assert!(idle_too_long(Some(s(0)), s(5), IDLE_TIMEOUT));
        assert!(idle_too_long(None, s(0), IDLE_TIMEOUT));
        // End to end with the recorder: auto mode starts at the first frame and stops after the idle time.
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::with_dir(true, dir.path().to_path_buf());
        assert!(!rec.is_recording());
        let frame = vec![90u8; 64 * 48 * 4];
        rec.on_frame(64, 48, &frame, t0);
        assert!(rec.is_recording());
        rec.tick(s(3));
        assert!(rec.is_recording());
        rec.tick(s(6));
        assert!(!rec.is_recording());
        rec.shutdown();
        let evs = rec.status().events;
        assert!(matches!(evs.as_slice(), [Event::Saved { frames: 1, .. }]), "{evs:?}");
        // A new session after the stop is recorded to a new file.
        rec.on_frame(64, 48, &frame, s(10));
        assert!(rec.is_recording());
        rec.on_disconnect();
        assert!(!rec.is_recording());
        rec.shutdown();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn slot_clock_is_constant_rate() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = SlotClock::default();
        assert_eq!(c.slot_for(ms(0)), Some(0));
        assert_eq!(c.slot_for(ms(10)), None); // 100 fps source: surplus frame skipped
        assert_eq!(c.slot_for(ms(34)), Some(1));
        assert_eq!(c.slot_for(ms(100)), Some(3)); // slot 2 will be a repeated picture
        // Long pause is cut out of the timeline instead of being filled.
        assert_eq!(c.slot_for(ms(100 + 60_000)), Some(4));
        assert_eq!(c.slot_for(ms(100 + 60_000 + 34)), Some(5));
    }

    #[test]
    fn manual_start_stop_without_frames_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::with_dir(false, dir.path().join("VR"));
        rec.start();
        assert!(rec.status().recording);
        rec.stop();
        rec.shutdown();
        assert!(!dir.path().join("VR").exists());
        assert!(rec.status().events.is_empty());
    }

    #[test]
    fn disk_error_stops_recording_with_message() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let rec = Recorder::with_dir(true, blocker.join("VR"));
        let frame = vec![0u8; 64 * 48 * 4];
        rec.on_frame(64, 48, &frame, Instant::now());
        rec.shutdown();
        let st = rec.status();
        assert!(!st.recording);
        assert!(matches!(st.events.as_slice(), [Event::Error(m)] if m.contains("Cannot create")), "{:?}", st.events);
    }
}
