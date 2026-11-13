//! Encoder thread of one output file: RGBA frames in, fragmented MP4 out. Never touches the UI or the network thread.

use super::mp4::{self, Mp4Writer};
use super::yuv::Yuv420;
use super::Pool;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, VuiConfig};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

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
    Saved { path: PathBuf, bytes: u64, frames: u64 },
    Failed { path: Option<PathBuf>, message: String },
}

/// State shared between the thread and the recorder front end.
#[derive(Default)]
pub struct WorkerShared {
    pub done: AtomicBool,
    outcome: Mutex<Option<Outcome>>,
}

impl WorkerShared {
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
pub fn run(dir: PathBuf, stamp: String, rx: Receiver<FrameMsg>, shared: Arc<WorkerShared>, pool: Pool) {
    let mut st = State { dir, stamp, path: None, enc: None, mux: None, prev: Yuv420::default(), cur: Yuv420::default(), next_slot: 0, written: 0 };
    let result = st.process(&rx, &pool);
    let outcome = match result {
        Ok(()) => match st.finish() {
            Ok(Some((bytes, frames))) => st.path.clone().map(|path| Outcome::Saved { path, bytes, frames }),
            Ok(None) => None, // no frame ever arrived: nothing was created
            Err(e) => Some(Outcome::Failed { path: st.path.clone(), message: st.io_message("Cannot finish the video file", &e) }),
        },
        Err(message) => {
            // Close the file as well as possible; it stays playable up to the last full fragment.
            let _ = st.finish();
            Some(Outcome::Failed { path: st.path.clone(), message })
        }
    };
    *shared.outcome.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
    shared.done.store(true, Ordering::Release);
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
        // Ends when the sender is dropped (after all queued frames have been processed).
        while let Ok(f) = rx.recv() {
            let r = self.frame(&f);
            pool.give_back(f.rgba);
            r?;
        }
        Ok(())
    }

    fn frame(&mut self, f: &FrameMsg) -> Result<(), String> {
        if self.enc.is_none() {
            let api = OpenH264API::from_source();
            self.enc = Some(Encoder::with_api_config(api, encoder_config()).map_err(|e| format!("Cannot start the video encoder: {e}"))?);
        }
        // Repeat the previous picture for slots that received no frame (dropped or slow source).
        if self.written > 0 {
            while self.next_slot < f.slot {
                self.encode_prev()?;
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
            let mux = Mp4Writer::new(BufWriter::with_capacity(1 << 20, file), w as u32, h as u32, sps, pps).map_err(|e| self.io_message("Cannot write", &e))?;
            self.mux = Some(mux);
        }
        let mux = self.mux.as_mut().expect("muxer exists");
        mux.push(slot, au.sample, au.key).map_err(|e| describe_io_error("Cannot write", self.path.as_deref().unwrap_or(&self.dir), &e))?;
        self.next_slot = slot + 1;
        self.written += 1;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<Option<(u64, u64)>> {
        let Some(mux) = self.mux.take() else { return Ok(None) };
        let frames = self.written;
        let bytes = mux.bytes_written();
        let w = mux.finish()?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(Some((bytes, frames)))
    }
}
