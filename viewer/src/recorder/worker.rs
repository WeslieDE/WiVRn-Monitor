//! Encoder thread of one output file: RGBA frames in, fragmented MP4 out. Never touches the UI or the network thread.

use super::audio::engine::{AudioEngine, AudioMsg, BITRATE as AUDIO_BITRATE};
use super::mp4::{self, AudioTrack, Mp4Writer};
use super::yuv::Yuv420;
use super::Pool;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, VuiConfig};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Target video bitrate (bits per second).
pub const BITRATE_BPS: u32 = 8_000_000;
/// Keyframe interval in frames (2 s).
pub const KEYFRAME_INTERVAL: u32 = 2 * mp4::FPS;

pub struct FrameMsg {
    /// Output frame slot (constant frame rate timeline); gaps are filled by repeating the previous picture.
    pub slot: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub enum Outcome {
    Saved { path: PathBuf, bytes: u64, frames: u64, audio: bool },
    Failed { path: Option<PathBuf>, message: String },
}

/// State shared between the thread and the recorder front end.
#[derive(Default)]
pub struct WorkerShared {
    pub done: AtomicBool,
    /// The file has an audio track (set when the file is created).
    pub audio_on: AtomicBool,
    outcome: Mutex<Option<Outcome>>,
    warnings: Mutex<Vec<String>>,
}

impl WorkerShared {
    pub fn warn(&self, message: String) {
        eprintln!("recorder: {message}");
        self.warnings.lock().unwrap_or_else(|e| e.into_inner()).push(message);
    }

    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn take_outcome(&self) -> Option<Outcome> {
        self.outcome.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Human-readable message for a file system error.
pub fn describe_io_error(what: &str, path: &Path, e: &io::Error) -> String {
    let reason = match e.raw_os_error() {
        Some(28) => "the disk is full".to_string(),
        Some(122) => "the disk quota is exceeded".to_string(),
        Some(13) | Some(1) => "permission denied".to_string(),
        Some(30) => "the file system is read-only".to_string(),
        _ => e.to_string(),
    };
    format!("{what} {}: {reason}.", path.display())
}

/// Entry point of the thread.
pub fn run(dir: PathBuf, stamp: String, rx: Receiver<FrameMsg>, shared: Arc<WorkerShared>, pool: Pool, audio: Option<AudioEngine>) {
    let audio = audio.map(|a| AudioLink { rx: a.rx, handle: Some(a.handle), started: None, early: Vec::new(), pcm_frames: None, over: false });
    let mut st = State { dir, stamp, path: None, enc: None, mux: None, prev: Yuv420::default(), cur: Yuv420::default(), next_slot: 0, written: 0, audio, shared: shared.clone() };
    let result = st.process(&rx, &pool);
    let outcome = match result {
        Ok(()) => match st.finish(true) {
            Ok(Some((bytes, frames, audio))) => st.path.clone().map(|path| Outcome::Saved { path, bytes, frames, audio }),
            Ok(None) => None, // no frame ever arrived: nothing was created
            Err(e) => Some(Outcome::Failed { path: st.path.clone(), message: st.io_message("Cannot finish the video file", &e) }),
        },
        Err(message) => {
            // Close the file as well as possible; it stays playable up to the last full fragment.
            let _ = st.finish(false);
            Some(Outcome::Failed { path: st.path.clone(), message })
        }
    };
    *shared.outcome.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
    shared.done.store(true, Ordering::Release);
}

/// The audio engine as seen by the encoder thread.
struct AudioLink {
    rx: Receiver<AudioMsg>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Parameters once the engine reported that it runs.
    started: Option<AudioTrack>,
    /// Access units that arrived before the file existed.
    early: Vec<Vec<u8>>,
    pcm_frames: Option<u64>,
    /// No more messages will come (failed, ended or gone).
    over: bool,
}

struct State {
    dir: PathBuf,
    stamp: String,
    path: Option<PathBuf>,
    enc: Option<Encoder>,
    mux: Option<Mp4Writer<BufWriter<File>>>,
    /// Previously encoded picture (repeated when the timeline has gaps) and the picture being converted.
    prev: Yuv420,
    cur: Yuv420,
    next_slot: u64,
    written: u64,
    audio: Option<AudioLink>,
    shared: Arc<WorkerShared>,
}

fn encoder_config() -> EncoderConfig {
    EncoderConfig::new()
        .bitrate(BitRate::from_bps(BITRATE_BPS))
        .max_frame_rate(FrameRate::from_hz(mp4::FPS as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(true)
        .intra_frame_period(IntraFramePeriod::from_num_frames(KEYFRAME_INTERVAL))
        .vui(VuiConfig::bt709())
        .num_threads(2)
}

impl State {
    fn io_message(&self, what: &str, e: &io::Error) -> String {
        describe_io_error(what, self.path.as_deref().unwrap_or(&self.dir), e)
    }

    fn process(&mut self, rx: &Receiver<FrameMsg>, pool: &Pool) -> Result<(), String> {
        // Ends when the sender is dropped (after all queued frames have been processed). While no frames arrive
        // the audio is still written (about once a second), so a crash during a pause loses little.
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(f) => {
                    let r = self.frame(&f);
                    pool.give_back(f.rgba);
                    r?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            self.pump_audio(Duration::ZERO)?;
        }
    }

    /// Moves what the audio engine has delivered into the file (or the early buffer). Waits up to `wait` for the
    /// next message if there is none.
    fn pump_audio(&mut self, wait: Duration) -> Result<(), String> {
        loop {
            let Some(a) = self.audio.as_mut().filter(|a| !a.over) else { return Ok(()) };
            let msg = match if wait.is_zero() { a.rx.try_recv().map_err(|e| matches!(e, std::sync::mpsc::TryRecvError::Disconnected)) } else { a.rx.recv_timeout(wait).map_err(|e| matches!(e, RecvTimeoutError::Disconnected)) } {
                Ok(m) => m,
                Err(disconnected) => {
                    a.over |= disconnected;
                    return Ok(());
                }
            };
            match msg {
                AudioMsg::Started { asc, priming, warnings } => {
                    a.started = Some(AudioTrack { asc, priming, bitrate: AUDIO_BITRATE });
                    for w in warnings {
                        self.shared.warn(format!("Recording without {w}."));
                    }
                }
                AudioMsg::Failed(e) => {
                    a.over = true;
                    self.shared.warn(format!("No sound in this recording: {e}."));
                }
                AudioMsg::Warning(w) => self.shared.warn(w),
                AudioMsg::Packet(au) => match self.mux.as_mut() {
                    Some(m) => m.push_audio(au).map_err(|e| describe_io_error("Cannot write", self.path.as_deref().unwrap_or(&self.dir), &e))?,
                    None => a.early.push(au),
                },
                AudioMsg::End { pcm_frames } => {
                    a.pcm_frames = Some(pcm_frames);
                    a.over = true;
                }
            }
        }
    }

    /// Before the file is created: waits (bounded) until the engine says whether it can record sound.
    fn await_audio_start(&mut self) -> Result<Option<AudioTrack>, String> {
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while self.audio.as_ref().is_some_and(|a| !a.over && a.started.is_none()) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                self.shared.warn("No sound in this recording: the audio devices did not respond in time.".into());
                self.audio = None; // the engine ends when its channel is gone
                break;
            }
            self.pump_audio(left.min(Duration::from_millis(50)))?;
        }
        Ok(self.audio.as_ref().and_then(|a| a.started.clone()))
    }

    fn frame(&mut self, f: &FrameMsg) -> Result<(), String> {
        if self.enc.is_none() {
            let api = OpenH264API::from_source();
            self.enc = Some(Encoder::with_api_config(api, encoder_config()).map_err(|e| format!("Cannot start the video encoder: {e}"))?);
        }
        // Repeat the previous picture for slots that received no frame (dropped or slow source).
        if self.written > 0 {
            if f.slot - self.next_slot.min(f.slot) <= super::MAX_FILLED_GAP {
                while self.next_slot < f.slot {
                    self.encode_prev()?;
                }
            } else {
                // Long pause on a timeline that is not cut (sound is recorded): the last picture stays on screen.
                self.next_slot = f.slot;
            }
        }
        if !self.cur.convert_from_rgba(&f.rgba, f.width as usize, f.height as usize) {
            return Err(format!("Cannot encode a {}x{} frame.", f.width, f.height));
        }
        self.next_slot = self.next_slot.max(f.slot);
        self.encode()?;
        std::mem::swap(&mut self.prev, &mut self.cur);
        Ok(())
    }

    fn encode_prev(&mut self) -> Result<(), String> {
        std::mem::swap(&mut self.prev, &mut self.cur);
        let r = self.encode();
        std::mem::swap(&mut self.prev, &mut self.cur);
        r
    }

    /// Encodes `self.cur` as the picture of `self.next_slot` and appends it to the file.
    fn encode(&mut self) -> Result<(), String> {
        let (w, h) = (self.cur.width, self.cur.height);
        let src = YUVSlices::new((self.cur.y.as_slice(), self.cur.u.as_slice(), self.cur.v.as_slice()), (w, h), (w, w / 2, w / 2));
        let slot = self.next_slot;
        let enc = self.enc.as_mut().expect("encoder exists");
        let stream = enc.encode_at(&src, Timestamp::from_millis(slot * 1000 / mp4::FPS as u64)).map_err(|e| format!("The video encoder failed: {e}"))?;
        let mut annexb = Vec::new();
        stream.write_vec(&mut annexb);
        let au = mp4::annexb_to_avcc(&annexb);
        if au.sample.is_empty() {
            self.next_slot = slot + 1; // encoder produced nothing (skipped picture); the slot stays empty
            return Ok(());
        }
        if self.mux.is_none() {
            let (Some(sps), Some(pps)) = (au.sps.as_deref(), au.pps.as_deref()) else {
                return Err("The video encoder did not start with a keyframe.".into());
            };
            let (path, file) = super::create_unique_file(&self.dir, &self.stamp).map_err(|e| describe_io_error("Cannot create the video file in", &self.dir, &e))?;
            self.path = Some(path);
            let track = self.await_audio_start()?;
            let mux = Mp4Writer::with_audio(BufWriter::with_capacity(1 << 20, file), w as u32, h as u32, sps, pps, track).map_err(|e| self.io_message("Cannot write", &e))?;
            self.mux = Some(mux);
            if self.mux.as_ref().is_some_and(|m| m.has_audio()) {
                self.shared.audio_on.store(true, Ordering::Release);
                let early = self.audio.as_mut().map(|a| std::mem::take(&mut a.early)).unwrap_or_default();
                for au in early {
                    self.mux.as_mut().expect("just created").push_audio(au).map_err(|e| self.io_message("Cannot write", &e))?;
                }
            }
        }
        let mux = self.mux.as_mut().expect("muxer exists");
        mux.push(slot, au.sample, au.key).map_err(|e| describe_io_error("Cannot write", self.path.as_deref().unwrap_or(&self.dir), &e))?;
        self.next_slot = slot + 1;
        self.written += 1;
        Ok(())
    }

    /// `drain`: wait for the audio engine to flush its last access units (normal end).
    fn finish(&mut self, drain: bool) -> io::Result<Option<(u64, u64, bool)>> {
        if drain {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while self.audio.as_ref().is_some_and(|a| !a.over) && std::time::Instant::now() < deadline {
                if self.pump_audio(Duration::from_millis(50)).is_err() {
                    break;
                }
            }
        }
        let _ = self.pump_audio(Duration::ZERO);
        if let Some(a) = self.audio.as_mut() {
            if let Some(h) = a.handle.take() {
                if a.over {
                    let _ = h.join();
                }
            }
        }
        let Some(mut mux) = self.mux.take() else { return Ok(None) };
        if let Some(p) = self.audio.as_ref().and_then(|a| a.pcm_frames) {
            mux.set_audio_pcm_frames(p);
        }
        let audio = mux.has_audio();
        let frames = self.written;
        let bytes = mux.bytes_written();
        let w = mux.finish()?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(Some((bytes, frames, audio)))
    }
}
