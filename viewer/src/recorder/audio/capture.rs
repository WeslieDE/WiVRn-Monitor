//! Capture back ends. The [`Capture`] trait is what the engine needs; [`PulseCapture`] implements it with
//! `libpulse-simple`, loaded at run time with `dlopen` (so the viewer starts without any audio library and
//! recording simply continues without sound). PipeWire systems provide the same API through `pipewire-pulse`.

use super::RATE;
use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::time::Duration;

/// A blocking audio source that delivers 48 kHz float samples (interleaved).
pub trait Capture: Send {
    /// Number of interleaved channels (1 for the microphone, 2 for the application sound).
    fn channels(&self) -> usize;
    /// Blocks until `out.len() / channels()` frames were captured and writes them to `out`.
    fn read(&mut self, out: &mut [f32]) -> Result<(), String>;
    /// Age of the oldest frame not yet read (capture time to now), if the back end knows it.
    fn latency(&mut self) -> Duration;
}

/// Opens a capture device. Called on the capture thread (opening may block).
pub type Opener = Box<dyn FnOnce() -> Result<Box<dyn Capture>, String> + Send>;

// ------------------------------------------------------------------ libpulse-simple via dlopen

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type NewFn = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *const c_char, *const c_char, *const SampleSpec, *const c_void, *const BufferAttr, *mut c_int) -> *mut c_void;
type ReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type LatencyFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type StrErrorFn = unsafe extern "C" fn(c_int) -> *const c_char;

/// Loaded `libpulse-simple` (kept alive as long as any handle exists).
pub struct PulseLib {
    _lib: Library,
    _core: Option<Library>,
    new: NewFn,
    read: ReadFn,
    latency: LatencyFn,
    free: FreeFn,
    strerror: Option<StrErrorFn>,
}

impl PulseLib {
    /// Loads the library; the error message is user-facing.
    pub fn load() -> Result<PulseLib, String> {
        // SAFETY: loading a shared library runs its initialisers; libpulse is a well-behaved system library.
        let lib = ["libpulse-simple.so.0", "libpulse-simple.so"]
            .iter()
            .find_map(|n| unsafe { Library::new(n).ok() })
            .ok_or_else(|| "the PulseAudio client library (libpulse-simple) is not installed".to_string())?;
        let core = ["libpulse.so.0", "libpulse.so"].iter().find_map(|n| unsafe { Library::new(n).ok() });
        unsafe {
            let new = *lib.get::<NewFn>(b"pa_simple_new\0").map_err(|e| e.to_string())?;
            let read = *lib.get::<ReadFn>(b"pa_simple_read\0").map_err(|e| e.to_string())?;
            let latency = *lib.get::<LatencyFn>(b"pa_simple_get_latency\0").map_err(|e| e.to_string())?;
            let free = *lib.get::<FreeFn>(b"pa_simple_free\0").map_err(|e| e.to_string())?;
            let strerror = core.as_ref().and_then(|c| c.get::<StrErrorFn>(b"pa_strerror\0").ok().map(|s: Symbol<StrErrorFn>| *s));
            Ok(PulseLib { _lib: lib, _core: core, new, read, latency, free, strerror })
        }
    }

    fn err(&self, code: c_int) -> String {
        if let Some(f) = self.strerror {
            // SAFETY: pa_strerror returns a static NUL-terminated string (or NULL).
            let p = unsafe { f(code) };
            if !p.is_null() {
                return unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
            }
        }
        format!("PulseAudio error {code}")
    }
}

/// One PulseAudio record stream.
pub struct PulseCapture {
    lib: std::sync::Arc<PulseLib>,
    handle: *mut c_void,
    channels: usize,
}

// SAFETY: the handle is only used from one thread at a time (it is moved into the capture thread).
unsafe impl Send for PulseCapture {}

impl PulseCapture {
    /// Records `device` (`None` = server default; `"@DEFAULT_MONITOR@"` or `"<sink>.monitor"` = what a sink plays).
    /// The stream is read-only: it never changes defaults or routing.
    pub fn open(lib: std::sync::Arc<PulseLib>, device: Option<&str>, channels: u8, stream_name: &str) -> Result<PulseCapture, String> {
        let dev = device.map(|d| CString::new(d).map_err(|_| "invalid device name".to_string())).transpose()?;
        let name = CString::new("WiVRn-Monitor").expect("no NUL");
        let sname = CString::new(stream_name).map_err(|_| "invalid stream name".to_string())?;
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: RATE, channels };
        // 20 ms fragments: low latency, cheap enough. Everything else: server default.
        let attr = BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: RATE / 50 * 4 * channels as u32 };
        let mut error: c_int = 0;
        // SAFETY: all pointers are valid for the duration of the call; the C function copies what it keeps.
        let handle = unsafe {
            (lib.new)(std::ptr::null(), name.as_ptr(), PA_STREAM_RECORD, dev.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), sname.as_ptr(), &spec, std::ptr::null(), &attr, &mut error)
        };
        if handle.is_null() {
            return Err(lib.err(error));
        }
        Ok(PulseCapture { lib, handle, channels: channels as usize })
    }
}

impl Capture for PulseCapture {
    fn channels(&self) -> usize {
        self.channels
    }

    fn read(&mut self, out: &mut [f32]) -> Result<(), String> {
        let mut error: c_int = 0;
        // SAFETY: `out` is valid for `len * 4` bytes; the handle is alive.
        let r = unsafe { (self.lib.read)(self.handle, out.as_mut_ptr().cast(), std::mem::size_of_val(out), &mut error) };
        if r < 0 {
            return Err(self.lib.err(error));
        }
        Ok(())
    }

    fn latency(&mut self) -> Duration {
        let mut error: c_int = 0;
        // SAFETY: the handle is alive. On error PulseAudio returns (pa_usec_t) -1.
        let us = unsafe { (self.lib.latency)(self.handle, &mut error) };
        if us == u64::MAX || us > 5_000_000 {
            Duration::ZERO
        } else {
            Duration::from_micros(us)
        }
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        // SAFETY: the handle was created by pa_simple_new and is freed once.
        unsafe { (self.lib.free)(self.handle) };
    }
}

// ------------------------------------------------------------------ synthetic capture for tests

/// A fake device that plays a sine wave in real time. Public so that integration tests can use it.
pub mod fake {
    use super::*;
    use std::time::Instant;

    /// What a [`SineCapture`] delivers.
    #[derive(Clone)]
    pub struct SineSpec {
        pub freq: f32,
        pub amplitude: f32,
        pub channels: usize,
        /// Silence before the tone starts (measured on the device clock).
        pub tone_starts_after: Duration,
        /// Device clock speed relative to the real clock (1.0005 = 0.05 % fast).
        pub clock: f64,
        /// Constant latency: data is delivered this long after it was captured, and reported as such.
        pub latency: Duration,
        /// Stops with an error after this many frames (device unplugged).
        pub fail_after_frames: Option<u64>,
    }

    impl Default for SineSpec {
        fn default() -> Self {
            SineSpec { freq: 440.0, amplitude: 0.5, channels: 1, tone_starts_after: Duration::ZERO, clock: 1.0, latency: Duration::from_millis(20), fail_after_frames: None }
        }
    }

    pub struct SineCapture {
        spec: SineSpec,
        started: Instant,
        frames: u64,
    }

    impl SineCapture {
        /// The device "starts capturing" now.
        pub fn new(spec: SineSpec) -> Self {
            SineCapture { spec, started: Instant::now(), frames: 0 }
        }
    }

    impl Capture for SineCapture {
        fn channels(&self) -> usize {
            self.spec.channels
        }

        fn read(&mut self, out: &mut [f32]) -> Result<(), String> {
            let ch = self.spec.channels;
            let n = (out.len() / ch) as u64;
            if self.spec.fail_after_frames.is_some_and(|f| self.frames >= f) {
                return Err("device removed".into());
            }
            // The last frame of this chunk is captured at start + (frames + n) / (RATE * clock); it is delivered `latency` later.
            let captured = Duration::from_secs_f64((self.frames + n) as f64 / (RATE as f64 * self.spec.clock));
            let due = self.started + captured + self.spec.latency;
            if let Some(w) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(w);
            }
            let tone_start = (self.spec.tone_starts_after.as_secs_f64() * RATE as f64) as u64;
            for i in 0..n {
                let j = self.frames + i;
                let v = if j < tone_start { 0.0 } else { self.spec.amplitude * (2.0 * std::f32::consts::PI * self.spec.freq * (j - tone_start) as f32 / RATE as f32).sin() };
                for c in 0..ch {
                    out[i as usize * ch + c] = v;
                }
            }
            self.frames += n;
            Ok(())
        }

        fn latency(&mut self) -> Duration {
            self.spec.latency
        }
    }
}
