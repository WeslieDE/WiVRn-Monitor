//! Clock handling for one capture source: where on the recording timeline are its samples, and how to read them
//! so that audio stays locked to the monotonic clock even if the device clock runs slightly fast or slow.
//!
//! The timeline starts at `t0` (arrival time of the first video frame). Output frame `m` is at `t0 + m / RATE`.
//! A source delivers frames with a running index `j`; frame `j` was captured at `t0 + c0 + j / RATE`, where `c0`
//! (seconds, may be negative) is estimated from the read times and the latency the capture API reports.

use super::RATE;
use std::collections::VecDeque;

/// Estimates `c0` from noisy observations. Read jitter and scheduling delays only make observations too large,
/// so the estimate follows downwards quickly and upwards slowly (a one-way-delay minimum filter).
#[derive(Debug, Default, Clone)]
pub struct OffsetEstimator {
    est: Option<f64>,
}

impl OffsetEstimator {
    const DOWN: f64 = 0.25;
    const UP: f64 = 0.002;

    pub fn update(&mut self, observation: f64) {
        self.est = Some(match self.est {
            None => observation,
            Some(e) if observation < e => e + Self::DOWN * (observation - e),
            Some(e) => e + Self::UP * (observation - e),
        });
    }

    pub fn get(&self) -> Option<f64> {
        self.est
    }
}

/// Frames delivered by a capture thread, waiting for the mixing thread.
#[derive(Debug, Default)]
pub struct SrcBuf {
    pub channels: usize,
    /// Interleaved samples; the first frame has absolute index `base`.
    pub data: VecDeque<f32>,
    pub base: u64,
    pub est: OffsetEstimator,
    /// Set by the capture thread when it stopped because of an error.
    pub error: Option<String>,
}

impl SrcBuf {
    pub fn new(channels: usize) -> Self {
        SrcBuf { channels, ..Default::default() }
    }

    /// Absolute index one past the last frame.
    pub fn end(&self) -> u64 {
        self.base + (self.data.len() / self.channels) as u64
    }

    pub fn push(&mut self, samples: &[f32], max_frames: usize) {
        self.data.extend(samples.iter().copied());
        let over = (self.data.len() / self.channels).saturating_sub(max_frames);
        self.drop_front(self.base + over as u64);
    }

    /// Discards all frames before absolute index `index`.
    pub fn drop_front(&mut self, index: u64) {
        let n = (index.saturating_sub(self.base) as usize).min(self.data.len() / self.channels);
        self.data.drain(..n * self.channels);
        self.base += n as u64;
    }

    /// Sample of `channel` at absolute frame `j`; silence outside the buffered range.
    fn at(&self, j: i64, channel: usize) -> f32 {
        if j < self.base as i64 || j >= self.end() as i64 {
            return 0.0;
        }
        self.data[(j as u64 - self.base) as usize * self.channels + channel]
    }
}

/// Reads a source at a position that follows the timeline, with a slowly adapting step (linear interpolation).
#[derive(Debug, Default)]
pub struct DriftReader {
    pos: Option<f64>,
}

/// More error than this (frames) is not worth smoothing out: jump instead.
const RESYNC_FRAMES: f64 = 0.1 * RATE as f64;
/// Largest speed change used to catch up (0.4 %, inaudible).
const MAX_STEP_DEVIATION: f64 = 0.004;
/// Catch-up time constant in frames.
const TAU_FRAMES: f64 = RATE as f64;

impl DriftReader {
    /// Renders `frames` output frames, the first of which is at timeline position `m`, into `out` (interleaved,
    /// `buf.channels` per frame). `c0` is the capture time of source frame 0 in seconds relative to `t0`.
    /// Returns true if the source had no data for (part of) the block.
    pub fn render(&mut self, buf: &SrcBuf, c0: f64, m: u64, frames: usize, out: &mut [f32]) -> bool {
        let ch = buf.channels;
        let target = m as f64 - c0 * RATE as f64;
        let mut pos = self.pos.unwrap_or(target);
        let err = target - pos;
        if err.abs() > RESYNC_FRAMES {
            pos = target;
        }
        let step = 1.0 + ((target - pos) / TAU_FRAMES).clamp(-MAX_STEP_DEVIATION, MAX_STEP_DEVIATION);
        let mut underrun = false;
        for i in 0..frames {
            let j = pos.floor();
            let frac = (pos - j) as f32;
            let j = j as i64;
            if j + 1 >= buf.end() as i64 {
                underrun = true;
            }
            for c in 0..ch {
                let (a, b) = (buf.at(j, c), buf.at(j + 1, c));
                out[i * ch + c] = a + (b - a) * frac;
            }
            pos += step;
        }
        self.pos = Some(pos);
        underrun
    }

    /// Absolute index below which frames are no longer needed.
    pub fn needed_from(&self) -> u64 {
        self.pos.map_or(0, |p| (p.floor() as i64 - 2).max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_buf(frames: usize, freq: f32) -> SrcBuf {
        let mut b = SrcBuf::new(1);
        let v: Vec<f32> = (0..frames).map(|n| (2.0 * std::f32::consts::PI * freq * n as f32 / RATE as f32).sin()).collect();
        b.push(&v, usize::MAX);
        b
    }

    #[test]
    fn estimator_follows_minimum_fast_and_increase_slowly() {
        let mut e = OffsetEstimator::default();
        e.update(0.030);
        for _ in 0..40 {
            e.update(0.010);
        }
        assert!((e.get().unwrap() - 0.010).abs() < 0.0005);
        for _ in 0..10 {
            e.update(0.050); // jitter: barely moves
        }
        assert!(e.get().unwrap() < 0.012);
    }

    #[test]
    fn untouched_source_is_copied_exactly() {
        let buf = sine_buf(4800, 440.0);
        let mut r = DriftReader::default();
        let mut out = vec![0.0; 480];
        assert!(!r.render(&buf, 0.0, 0, 480, &mut out));
        let mut out2 = vec![0.0; 480];
        r.render(&buf, 0.0, 480, 480, &mut out2);
        for (i, v) in out.iter().chain(&out2).enumerate() {
            assert!((v - buf.data[i]).abs() < 1e-5, "frame {i}");
        }
    }

    #[test]
    fn source_offset_shifts_the_signal_and_leading_part_is_silent() {
        // Source frame 0 was captured 10 ms after t0: output frame 480 equals source frame 0.
        let buf = sine_buf(4800, 440.0);
        let mut r = DriftReader::default();
        let mut out = vec![1.0; 960];
        r.render(&buf, 0.010, 0, 960, &mut out);
        assert!(out[..479].iter().all(|v| *v == 0.0));
        assert!((out[480] - buf.data[0]).abs() < 1e-5 && (out[600] - buf.data[120]).abs() < 1e-5);
    }

    #[test]
    fn slow_source_clock_is_caught_up_without_jumps() {
        // The device delivers 0.1 % fewer frames than the clock says: c0 grows by 1 ms per second.
        let buf = sine_buf(48_000 * 12, 100.0);
        let mut r = DriftReader::default();
        let mut out = vec![0.0; 480];
        let (mut m, mut maxjump) = (0u64, 0.0f32);
        let mut last = 0.0f32;
        for _ in 0..(10 * 100) {
            let t = m as f64 / RATE as f64;
            r.render(&buf, 0.001 * t, m, 480, &mut out);
            maxjump = maxjump.max((out[0] - last).abs());
            last = out[479];
            m += 480;
        }
        // 100 Hz at 48 kHz moves at most ~0.013 per frame: no discontinuity above that (plus interpolation slack).
        assert!(maxjump < 0.03, "{maxjump}");
        // After 10 s the reader has consumed 0.01 s more source than output (source clock is 0.1 % slow).
        let consumed = r.pos.unwrap();
        let expected = m as f64 - 0.001 * 10.0 * RATE as f64;
        assert!((consumed - expected).abs() < 200.0, "{consumed} vs {expected}");
    }

    #[test]
    fn large_error_resyncs_and_underrun_is_reported() {
        let buf = sine_buf(1000, 440.0);
        let mut r = DriftReader::default();
        let mut out = vec![0.0; 480];
        r.render(&buf, 0.0, 0, 480, &mut out);
        r.render(&buf, 0.0, 48_000, 480, &mut out); // far ahead of the data
        assert!(out.iter().all(|v| *v == 0.0));
        assert!(r.render(&buf, 0.0, 48_480, 480, &mut out));
    }

    #[test]
    fn buffer_trimming_keeps_absolute_indices() {
        let mut b = SrcBuf::new(2);
        b.push(&[0.0; 20], usize::MAX);
        assert_eq!((b.base, b.end()), (0, 10));
        b.push(&[1.0; 20], 12);
        assert_eq!((b.base, b.end()), (8, 20));
        b.drop_front(15);
        assert_eq!(b.base, 15);
        assert_eq!(b.at(14, 0), 0.0);
        assert_eq!(b.at(15, 1), 1.0);
    }
}
