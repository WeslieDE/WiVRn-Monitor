//! Troubleshooting helper: opens one real capture device and prints levels and timing, nothing is stored.
//! Usage: audio_probe monitor|mic [seconds]   (default 3 s; honours WIVRN_MONITOR_AUDIO_SINK / _SOURCE)
use std::sync::Arc;
use std::time::Instant;
use wivrn_monitor_viewer::recorder::audio::capture::{Capture, PulseCapture, PulseLib};
use wivrn_monitor_viewer::recorder::audio::device;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = args.get(1).map(String::as_str).unwrap_or("monitor");
    let secs: f64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(3.0);
    let lib = Arc::new(PulseLib::load().unwrap_or_else(|e| panic!("{e}")));
    let (dev, ch) = match kind {
        "mic" => (std::env::var(device::SOURCE_ENV).ok(), 1u8),
        _ => (Some(device::monitor_of(std::env::var(device::SINK_ENV).ok().as_deref())), 2u8),
    };
    println!("opening {dev:?}, {ch} channel(s)");
    let mut cap = PulseCapture::open(lib, dev.as_deref(), ch, "WiVRn-Monitor probe").unwrap_or_else(|e| panic!("open failed: {e}"));
    let start = Instant::now();
    let mut buf = vec![0f32; 480 * ch as usize];
    let (mut frames, mut peak, mut sum2) = (0u64, 0f32, 0f64);
    let mut next_report = 0.5;
    let (mut c, mut sn, mut all) = (0f64, 0f64, 0u64);
    while start.elapsed().as_secs_f64() < secs {
        cap.read(&mut buf).unwrap_or_else(|e| panic!("read failed: {e}"));
        frames += 480;
        for (i, v) in buf.iter().enumerate() {
            if i % ch as usize == 0 {
                let ph = 2.0 * std::f64::consts::PI * 440.0 * all as f64 / 48000.0;
                c += *v as f64 * ph.cos();
                sn += *v as f64 * ph.sin();
                all += 1;
            }
            peak = peak.max(v.abs());
            sum2 += (*v as f64).powi(2);
        }
        if start.elapsed().as_secs_f64() >= next_report {
            println!("t={:.2}s frames={frames} (device clock {:.3}s) latency={:?} peak={peak:.4} rms={:.4} a440(all so far)={:.4}", start.elapsed().as_secs_f64(), frames as f64 / 48000.0, cap.latency(), (sum2 / (frames as f64 * ch as f64)).sqrt(), 2.0 / all as f64 * c.hypot(sn));
            next_report += 0.5;
        }
    }
}
