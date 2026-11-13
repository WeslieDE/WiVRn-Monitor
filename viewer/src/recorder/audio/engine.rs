//! The audio engine of one recording: starts the capture threads, mixes on the recording timeline and encodes AAC.
//!
//! The engine is driven by the monotonic clock, not by the devices: every 10 ms block of the timeline
//! (`t0 + m / 48000`) is rendered from whatever the sources delivered for that time (silence if nothing), a fixed
//! 200 ms behind real time so that capture latency and jitter are absorbed. Device clock drift is absorbed by
//! [`pace::DriftReader`](super::pace::DriftReader).

use super::capture::Capture;
use super::mix::mix_block;
use super::pace::{DriftReader, SrcBuf};
use super::{SourceFactory, RATE};
use filmcraft_aac::{Encoder, EncoderConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// AAC bitrate (bits per second).
pub const BITRATE: u32 = 160_000;
/// Samples mixed at a time (10 ms).
pub const BLOCK: usize = RATE as usize / 100;
/// The engine stays this far behind real time.
const HORIZON: Duration = Duration::from_millis(200);
/// Frames each capture thread asks the device for at a time (10 ms).
const CHUNK: usize = BLOCK;
/// Longest time a source may take to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(4);
/// Buffered source frames at most (a stalled mixer must not eat memory).
const MAX_BUFFERED: usize = 10 * RATE as usize;

#[derive(Debug)]
pub enum AudioMsg {
    /// Sources are open, the encoder runs. `warnings` name sources that could not be opened.
    Started { asc: Vec<u8>, priming: u32, warnings: Vec<String> },
    /// Nothing could be set up: record without sound (the message is user-facing).
    Failed(String),
    /// Something went wrong while recording (a device vanished); the recording continues.
    Warning(String),
    /// One AAC access unit (1024 frames).
    Packet(Vec<u8>),
    /// Last message. `pcm_frames` = length of the audio without encoder padding.
    End { pcm_frames: u64 },
}

/// Handle to end the engine (shared with the engine thread).
#[derive(Clone, Default)]
pub struct AudioControl {
    end: Arc<Mutex<Option<Instant>>>,
}

impl AudioControl {
    /// Tells the engine where the recording ends (it renders up to there, flushes and sends `End`).
    pub fn finish(&self, end: Instant) {
        *self.end.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
    }

    fn get(&self) -> Option<Instant> {
        *self.end.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct AudioEngine {
    pub control: AudioControl,
    pub rx: Receiver<AudioMsg>,
    pub handle: JoinHandle<()>,
}

/// Seconds from `a` to `b`, negative if `b` is earlier.
pub fn signed_secs(a: Instant, b: Instant) -> f64 {
    match b.checked_duration_since(a) {
        Some(d) => d.as_secs_f64(),
        None => -a.duration_since(b).as_secs_f64(),
    }
}

struct Source {
    name: &'static str,
    buf: Arc<Mutex<SrcBuf>>,
    reader: DriftReader,
    /// The capture failed; this source is silent from then on.
    dead: bool,
    thread: JoinHandle<()>,
}

/// Starts the engine thread. `t0` is the time of recording timeline position 0 (arrival of the first video frame).
pub fn start(factory: SourceFactory, t0: Instant) -> std::io::Result<AudioEngine> {
    let (tx, rx) = channel();
    let control = AudioControl::default();
    let c = control.clone();
    let handle = std::thread::Builder::new().name("rec-audio".into()).spawn(move || run(factory, t0, c, tx))?;
    Ok(AudioEngine { control, rx, handle })
}

fn lock(b: &Mutex<SrcBuf>) -> std::sync::MutexGuard<'_, SrcBuf> {
    b.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn_capture(
    name: &'static str,
    opener: super::capture::Opener,
    channels: usize,
    t0: Instant,
    stop: Arc<AtomicBool>,
) -> (Source, Receiver<Result<(), String>>) {
    let buf = Arc::new(Mutex::new(SrcBuf::new(channels)));
    let (otx, orx) = sync_channel(1);
    let b = buf.clone();
    let thread = std::thread::Builder::new()
        .name(format!("rec-cap-{name}"))
        .spawn(move || {
            let mut cap: Box<dyn Capture> = match opener() {
                Ok(c) if c.channels() == channels => c,
                Ok(_) => {
                    let _ = otx.send(Err("unexpected channel count".into()));
                    return;
                }
                Err(e) => {
                    let _ = otx.send(Err(e));
                    return;
                }
            };
            let _ = otx.send(Ok(()));
            let mut chunk = vec![0f32; CHUNK * channels];
            let (mut latency, mut total, mut n) = (Duration::ZERO, 0u64, 0u32);
            while !stop.load(Ordering::Relaxed) {
                if let Err(e) = cap.read(&mut chunk) {
                    lock(&b).error = Some(e);
                    return;
                }
                let read_at = Instant::now();
                if n % 5 == 0 {
                    latency = cap.latency();
                }
                n = n.wrapping_add(1);
                total += CHUNK as u64;
                // Capture time of the next unread frame (index `total`) relative to t0, minus its index: c0.
                let c0 = signed_secs(t0, read_at) - latency.as_secs_f64() - total as f64 / RATE as f64;
                let mut g = lock(&b);
                g.push(&chunk, MAX_BUFFERED);
                g.est.update(c0);
            }
        })
        .expect("spawn capture thread");
    (Source { name, buf, reader: DriftReader::default(), dead: false, thread }, orx)
}

fn run(factory: SourceFactory, t0: Instant, ctl: AudioControl, tx: Sender<AudioMsg>) {
    let sources = match factory() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(AudioMsg::Failed(e));
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let mut pending = Vec::new();
    if let Some(o) = sources.mic {
        pending.push(spawn_capture("microphone", o, 1, t0, stop.clone()));
    }
    if let Some(o) = sources.app {
        pending.push(spawn_capture("app sound", o, 2, t0, stop.clone()));
    }
    let (mut mic, mut app): (Option<Source>, Option<Source>) = (None, None);
    let mut warnings = Vec::new();
    for (src, orx) in pending {
        match orx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => {
                if src.name == "microphone" {
                    mic = Some(src);
                } else {
                    app = Some(src);
                }
            }
            Ok(Err(e)) => warnings.push(format!("{} not available ({e})", src.name)),
            Err(_) => warnings.push(format!("{} did not open in time", src.name)),
        }
    }
    let shutdown = |stop: &AtomicBool, srcs: Vec<Source>| {
        stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_millis(400);
        for s in srcs {
            while !s.thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if s.thread.is_finished() {
                let _ = s.thread.join(); // else: blocked in the device; the thread ends by itself later
            }
        }
    };
    if mic.is_none() && app.is_none() {
        let _ = tx.send(AudioMsg::Failed(warnings.join("; ")));
        return;
    }
    let mut enc = match Encoder::new(EncoderConfig::cbr(RATE, 2, BITRATE)) {
        Ok(e) => e,
        Err(e) => {
            let _ = tx.send(AudioMsg::Failed(format!("AAC encoder: {e}")));
            shutdown(&stop, mic.into_iter().chain(app).collect());
            return;
        }
    };
    if tx.send(AudioMsg::Started { asc: enc.audio_specific_config(), priming: enc.priming_samples() as u32, warnings }).is_err() {
        shutdown(&stop, mic.into_iter().chain(app).collect());
        return;
    }

    let (mut left, mut right) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let (mut mic_pcm, mut app_pcm) = (vec![0f32; BLOCK], vec![0f32; BLOCK * 2]);
    let mut m: u64 = 0;
    let mut alive = true;
    // Renders `n` frames at position `m` and sends the encoded access units. False if the receiver is gone.
    let mut render = |m: u64, n: usize, mic: &mut Option<Source>, app: &mut Option<Source>, tx: &Sender<AudioMsg>| -> bool {
        let mut used = [false; 2];
        for (i, slot) in [&mut *mic, &mut *app].into_iter().enumerate() {
            let Some(s) = slot else { continue };
            if s.dead {
                continue;
            }
            let mut g = lock(&s.buf);
            if let Some(e) = g.error.take() {
                s.dead = true;
                let _ = tx.send(AudioMsg::Warning(format!("The {} stopped ({e}); continuing without it", s.name)));
                continue;
            }
            let Some(c0) = g.est.get() else { continue }; // nothing delivered yet
            let out = if i == 0 { &mut mic_pcm[..n] } else { &mut app_pcm[..2 * n] };
            s.reader.render(&g, c0, m, n, out);
            let from = s.reader.needed_from();
            g.drop_front(from);
            used[i] = true;
        }
        mix_block(used[0].then_some(&mic_pcm[..n]), used[1].then_some(&app_pcm[..2 * n]), &mut left[..n], &mut right[..n]);
        for au in enc.encode(&[&left[..n], &right[..n]]) {
            if tx.send(AudioMsg::Packet(au)).is_err() {
                return false;
            }
        }
        true
    };
    loop {
        let now = Instant::now();
        let avail = (signed_secs(t0, now) - HORIZON.as_secs_f64()).max(0.0) * RATE as f64;
        let avail = avail as u64;
        let end = ctl.get();
        let limit = end.map_or(u64::MAX, |e| (signed_secs(t0, e).max(0.0) * RATE as f64) as u64);
        while alive && m + BLOCK as u64 <= avail.min(limit) {
            alive = render(m, BLOCK, &mut mic, &mut app, &tx);
            m += BLOCK as u64;
        }
        if !alive {
            break;
        }
        if end.is_some() && avail >= limit {
            if m < limit {
                let n = (limit - m) as usize;
                render(m, n, &mut mic, &mut app, &tx);
                m = limit;
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(render);
    if alive {
        for au in enc.flush() {
            let _ = tx.send(AudioMsg::Packet(au));
        }
        let _ = tx.send(AudioMsg::End { pcm_frames: m });
    }
    shutdown(&stop, mic.into_iter().chain(app).collect());
}

#[cfg(test)]
mod tests {
    use super::super::capture::fake::{SineCapture, SineSpec};
    use super::super::capture::Opener;
    use super::super::Sources;
    use super::*;

    fn collect(rx: &Receiver<AudioMsg>) -> (Vec<Vec<u8>>, Option<u64>, Vec<String>, Option<u32>) {
        let (mut aus, mut end, mut warns, mut priming) = (vec![], None, vec![], None);
        while let Ok(m) = rx.recv_timeout(Duration::from_secs(5)) {
            match m {
                AudioMsg::Started { priming: p, warnings, .. } => {
                    priming = Some(p);
                    warns.extend(warnings);
                }
                AudioMsg::Packet(a) => aus.push(a),
                AudioMsg::End { pcm_frames } => {
                    end = Some(pcm_frames);
                    break;
                }
                AudioMsg::Warning(w) | AudioMsg::Failed(w) => warns.push(w),
            }
        }
        (aus, end, warns, priming)
    }

    fn opener(s: SineSpec) -> Opener {
        Box::new(move || Ok(Box::new(SineCapture::new(s)) as Box<dyn Capture>))
    }

    #[test]
    fn length_follows_the_end_time_and_access_units_cover_it() {
        let t0 = Instant::now();
        let factory: SourceFactory = Box::new(|| Ok(Sources { mic: Some(opener(SineSpec::default())), app: None }));
        let e = start(factory, t0).unwrap();
        e.control.finish(t0 + Duration::from_millis(1500));
        let (aus, end, warns, priming) = collect(&e.rx);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(end, Some(72_000)); // 1.5 s at 48 kHz, exactly
        // Access units hold the encoder delay plus the signal (padded to whole units).
        let need = (72_000 + priming.unwrap() as usize).div_ceil(1024);
        assert!((need..=need + 2).contains(&aus.len()), "{} units, need {need}", aus.len());
        assert!(aus.iter().all(|a| !a.is_empty()));
        e.handle.join().unwrap();
    }

    #[test]
    fn nothing_to_open_is_reported_as_failure() {
        let factory: SourceFactory = Box::new(|| Ok(Sources { mic: Some(Box::new(|| Err("gone".into()))), app: None }));
        let e = start(factory, Instant::now()).unwrap();
        assert!(matches!(e.rx.recv_timeout(Duration::from_secs(5)), Ok(AudioMsg::Failed(m)) if m.contains("gone")));
        let factory: SourceFactory = Box::new(|| Err("library missing".into()));
        let e = start(factory, Instant::now()).unwrap();
        assert!(matches!(e.rx.recv_timeout(Duration::from_secs(5)), Ok(AudioMsg::Failed(m)) if m == "library missing"));
    }

    #[test]
    fn signed_time_differences() {
        let a = Instant::now();
        let b = a + Duration::from_millis(250);
        assert!((signed_secs(a, b) - 0.25).abs() < 1e-9 && (signed_secs(b, a) + 0.25).abs() < 1e-9);
    }
}
