//! Recorder benchmark: reads raw RGBA frames from stdin and records them paced at 30 fps.
//! Usage: ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 20 -pix_fmt rgba -f rawvideo - | \
//!        WIVRN_MONITOR_VIDEO_DIR=/tmp/vr rec_bench [width height]
//! Sound: `BENCH_AUDIO=fake` records two synthetic tones (440 Hz "microphone", 880 Hz "app"), `BENCH_AUDIO=real` the
//! real devices (microphone included!); without it the recording has no sound.
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wivrn_monitor_viewer::recorder::audio::capture::fake::{SineCapture, SineSpec};
use wivrn_monitor_viewer::recorder::audio::{self, AudioOptions, Sources};
use wivrn_monitor_viewer::recorder::{Event, Recorder};

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (w, h) = (args.first().copied().unwrap_or(1280), args.get(1).copied().unwrap_or(720));
    let rec = match std::env::var("BENCH_AUDIO").as_deref() {
        Ok("real") => Recorder::with_audio(true, AudioOptions::default(), Arc::new(audio::pulse_factory)),
        Ok("fake") => Recorder::with_audio(
            true,
            AudioOptions::default(),
            Arc::new(|_| {
                Box::new(|| {
                    Ok(Sources {
                        mic: Some(Box::new(|| Ok(Box::new(SineCapture::new(SineSpec::default())) as _))),
                        app: Some(Box::new(|| Ok(Box::new(SineCapture::new(SineSpec { freq: 880.0, channels: 2, ..Default::default() })) as _))),
                    })
                })
            }),
        ),
        _ => Recorder::new(true),
    };
    let mut buf = vec![0u8; w * h * 4];
    let mut stdin = std::io::stdin().lock();
    let start = Instant::now();
    let mut n = 0u32;
    while stdin.read_exact(&mut buf).is_ok() {
        let due = start + Duration::from_micros(n as u64 * 33_333);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        rec.on_frame(w as u32, h as u32, &buf, Instant::now());
        n += 1;
    }
    let st = rec.status();
    rec.shutdown();
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let cpu = ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 / 1e6 + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 / 1e6;
    let wall = start.elapsed().as_secs_f64();
    for e in rec.status().events {
        match e {
            Event::Saved { path, bytes, frames, audio } => println!("saved (audio: {audio}) {} frames, {:.2} MB ({:.2} MB/min), {}", frames, bytes as f64 / 1e6, bytes as f64 / 1e6 / (frames as f64 / 30.0 / 60.0), path.display()),
            Event::Warning(m) => println!("warning: {m}"),
            Event::Error(m) => println!("error: {m}"),
        }
    }
    println!("input frames {n}, dropped {}, cpu {:.1}% of one core over {:.1} s", st.dropped, cpu / wall * 100.0, wall);
}
