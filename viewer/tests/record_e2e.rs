//! Encodes synthetic frames through the real recorder, writes an MP4 and (if `ffprobe`/`ffmpeg` exist) verifies it.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use wivrn_monitor_viewer::recorder::{Event, Recorder};

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Moving gradient plus a block of noise, so the encoder has real work to do.
fn frame(w: usize, h: usize, n: usize, buf: &mut Vec<u8>) {
    buf.resize(w * h * 4, 255);
    let mut seed = (n as u32).wrapping_mul(2654435761) | 1;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let noise = if x > w / 2 && y > h / 2 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed & 0x3F) as u8
            } else {
                0
            };
            buf[i] = (((x + n * 6) * 255 / w) as u8).wrapping_add(noise);
            buf[i + 1] = (y * 255 / h) as u8;
            buf[i + 2] = 64u8.wrapping_add(noise);
        }
    }
}

fn probe(path: &Path, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", entries, "-of", "default=nw=1"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn field(s: &str, key: &str) -> String {
    s.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).unwrap_or("").to_string()
}

#[test]
fn records_synthetic_frames_to_playable_mp4() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("VR"); // does not exist yet: must be created
    let rec = Recorder::with_dir(false, out.clone());
    rec.start();
    let (w, h, frames) = (1280usize, 720usize, 90usize);
    let t0 = Instant::now();
    let mut buf = Vec::new();
    for n in 0..frames {
        frame(w, h, n, &mut buf);
        rec.on_frame(w as u32, h as u32, &buf, t0 + Duration::from_micros(n as u64 * 33_333));
        std::thread::sleep(Duration::from_millis(12)); // let the encoder keep up
    }
    rec.stop();
    rec.shutdown();
    let st = rec.status();
    let Some(Event::Saved { path, bytes, frames: written, .. }) = st.events.first().cloned() else { panic!("no file saved: {:?}", st.events) };
    assert!(st.events.len() == 1 && written >= 80, "{st:?}");
    assert!(path.starts_with(&out) && path.extension().unwrap() == "mp4");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size >= bytes && size > 10_000, "size {size}");
    eprintln!("e2e: {} frames, {} dropped, {size} bytes", written, st.dropped);

    if !have("ffprobe") {
        eprintln!("ffprobe not found: skipping container checks");
        return;
    }
    let info = probe(&path, "stream=codec_name,profile,width,height,pix_fmt,color_space,color_range,avg_frame_rate,nb_read_frames:format=duration");
    assert_eq!(field(&info, "codec_name"), "h264", "{info}");
    assert_eq!((field(&info, "width").as_str(), field(&info, "height").as_str()), ("1280", "720"), "{info}");
    assert_eq!(field(&info, "pix_fmt"), "yuv420p", "{info}");
    assert_eq!(field(&info, "color_space"), "bt709", "{info}");
    assert_eq!(field(&info, "avg_frame_rate"), "30/1", "{info}");
    let dur: f64 = field(&info, "duration").parse().unwrap();
    assert!((2.7..=3.4).contains(&dur), "duration {dur}: {info}");
    let n: usize = field(&info, "nb_read_frames").parse().unwrap();
    assert!((88..=92).contains(&n), "frames {n}");
    if have("ffmpeg") {
        let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
        assert!(o.status.success() && o.stderr.is_empty(), "ffmpeg: {}", String::from_utf8_lossy(&o.stderr));
    }
}

#[test]
fn resolution_change_starts_a_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::with_dir(true, dir.path().to_path_buf());
    let t0 = Instant::now();
    let mut buf = Vec::new();
    for (i, (w, h)) in [(320usize, 180usize), (320, 180), (640, 360), (640, 360)].into_iter().enumerate() {
        frame(w, h, i, &mut buf);
        rec.on_frame(w as u32, h as u32, &buf, t0 + Duration::from_millis(i as u64 * 33));
        std::thread::sleep(Duration::from_millis(20));
    }
    rec.shutdown();
    let saved = rec.status().events.into_iter().filter(|e| matches!(e, Event::Saved { .. })).count();
    assert_eq!(saved, 2);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}
