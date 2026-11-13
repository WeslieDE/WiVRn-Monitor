//! Records synthetic video plus synthetic audio (sine "microphone" and "application sound" fakes) through the real
//! recorder and checks the MP4 with ffprobe/ffmpeg: streams, durations, content of both tones and A/V sync.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wivrn_monitor_viewer::recorder::audio::capture::fake::{SineCapture, SineSpec};
use wivrn_monitor_viewer::recorder::audio::capture::{Capture, Opener};
use wivrn_monitor_viewer::recorder::audio::{AudioOptions, Sources};
use wivrn_monitor_viewer::recorder::{Event, Recorder};

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn tools() -> bool {
    let ok = have("ffprobe") && have("ffmpeg");
    if !ok {
        eprintln!("ffmpeg/ffprobe not found: skipping");
    }
    ok
}

fn opener(s: SineSpec) -> Opener {
    Box::new(move || Ok(Box::new(SineCapture::new(s)) as Box<dyn Capture>))
}

type Specs = (Option<SineSpec>, Option<SineSpec>);

fn recorder(dir: PathBuf, opts: AudioOptions, specs: Specs) -> Recorder {
    let r = Recorder::with_audio(
        false,
        opts,
        Arc::new(move |_| {
            let (mic, app) = specs.clone();
            Box::new(move || {
                Ok(Sources {
                    mic: mic.map(opener),
                    app: app.map(opener),
                })
            })
        }),
    );
    // `Recorder::with_audio` records into the default folder; point it at the test folder.
    r.set_dir(dir);
    r
}

fn mic_spec() -> SineSpec {
    SineSpec { freq: 440.0, amplitude: 0.5, channels: 1, ..Default::default() }
}

fn app_spec() -> SineSpec {
    SineSpec { freq: 880.0, amplitude: 0.5, channels: 2, latency: Duration::from_millis(45), ..Default::default() }
}

/// Feeds 30 fps frames in real time; `luma(t)` gives the grey level of the picture at time `t` seconds.
/// `pause` = (at second, length): no frames for that long.
fn feed(rec: &Recorder, secs: f64, pause: Option<(f64, f64)>, luma: impl Fn(f64) -> u8) {
    let (w, h) = (64usize, 48usize);
    let t0 = Instant::now();
    let mut n = 0u32;
    loop {
        let mut t = n as f64 / 30.0;
        if let Some((at, len)) = pause {
            if t >= at {
                t += len;
            }
        }
        if t >= secs {
            break;
        }
        let due = t0 + Duration::from_secs_f64(t);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let frame = vec![luma(t); w * h * 4];
        rec.on_frame(w as u32, h as u32, &frame, Instant::now());
        n += 1;
    }
}

fn finish(rec: &Recorder) -> (PathBuf, Vec<Event>) {
    rec.stop();
    rec.shutdown();
    let events = rec.status().events;
    let path = events.iter().find_map(|e| if let Event::Saved { path, .. } = e { Some(path.clone()) } else { None }).unwrap_or_else(|| panic!("no file: {events:?}"));
    (path, events)
}

fn ffprobe(path: &Path) -> String {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=index,codec_type,codec_name,sample_rate,channels,duration,avg_frame_rate", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The (key, value) lines of stream `n` (counting `index=` lines).
fn stream(info: &str, codec_type: &str) -> Vec<(String, String)> {
    let mut cur: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for l in info.lines() {
        let (k, v) = l.split_once('=').unwrap();
        if k == "index" && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push((k.to_string(), v.to_string()));
    }
    out.push(cur);
    out.into_iter().find(|s| s.iter().any(|(k, v)| k == "codec_type" && v == codec_type)).unwrap_or_else(|| panic!("no {codec_type} stream in {info}"))
}

fn get(s: &[(String, String)], key: &str) -> String {
    s.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn decode_audio(path: &Path) -> Vec<[f32; 2]> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-map", "0:a:0", "-f", "f32le", "-ac", "2", "-ar", "48000", "-"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    o.stdout.as_chunks::<8>().0.iter().map(|c| [f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())]).collect()
}

/// Amplitude of the sine of `freq` in `x` (single-frequency correlation).
fn amplitude(x: &[f32], freq: f32) -> f32 {
    let (mut c, mut s) = (0.0f64, 0.0f64);
    for (n, v) in x.iter().enumerate() {
        let ph = 2.0 * std::f64::consts::PI * freq as f64 * n as f64 / 48_000.0;
        c += *v as f64 * ph.cos();
        s += *v as f64 * ph.sin();
    }
    (2.0 / x.len() as f64 * (c * c + s * s).sqrt()) as f32
}

/// Time (s) at which the tone of `freq` starts: first 128-sample window above half of its steady amplitude.
fn onset(x: &[f32], freq: f32) -> f64 {
    let win = 128;
    let amps: Vec<f32> = x.chunks_exact(win).map(|w| amplitude(w, freq)).collect();
    let max = amps.iter().cloned().fold(0.0, f32::max);
    let i = amps.iter().position(|a| *a > max * 0.5).expect("tone present");
    (i * win) as f64 / 48_000.0 + win as f64 / 2.0 / 48_000.0
}

fn left(a: &[[f32; 2]]) -> Vec<f32> {
    a.iter().map(|f| f[0]).collect()
}

#[test]
fn mic_and_app_sound_end_up_in_one_stereo_aac_track() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A pause in the middle: sound runs through, so the pause must stay on the timeline (not be cut).
    let rec = recorder(dir.path().into(), AudioOptions::default(), (Some(mic_spec()), Some(app_spec())));
    rec.start();
    feed(&rec, 5.2, Some((1.5, 2.2)), |_| 90);
    let (path, events) = finish(&rec);
    assert!(matches!(events.as_slice(), [Event::Saved { audio: true, .. }]), "{events:?}");

    let info = ffprobe(&path);
    let (v, a) = (stream(&info, "video"), stream(&info, "audio"));
    assert_eq!(get(&v, "codec_name"), "h264");
    assert_eq!((get(&a, "codec_name").as_str(), get(&a, "sample_rate").as_str(), get(&a, "channels").as_str()), ("aac", "48000", "2"), "{info}");
    let (vd, ad): (f64, f64) = (get(&v, "duration").parse().unwrap(), get(&a, "duration").parse().unwrap());
    assert!((5.0..5.5).contains(&vd) && (ad - vd).abs() < 0.1, "video {vd} s, audio {ad} s");
    let ff = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
    assert!(ff.status.success() && ff.stderr.is_empty(), "{}", String::from_utf8_lossy(&ff.stderr));

    let pcm = decode_audio(&path);
    assert!((pcm.len() as f64 / 48_000.0 - ad).abs() < 0.05, "decoded {} frames", pcm.len());
    let (l, r): (Vec<f32>, Vec<f32>) = pcm.iter().map(|f| (f[0], f[1])).unzip();
    // Steady middle part (skip the start and the end).
    let mid = &l[48_000..4 * 48_000];
    let (a440, a880, a1500) = (amplitude(mid, 440.0), amplitude(mid, 880.0), amplitude(mid, 1500.0));
    // Each tone: 0.5 * -3 dB = 0.354 (AAC at 160 kbit/s is not exact).
    for (name, a) in [("440 (microphone)", a440), ("880 (app sound)", a880)] {
        assert!((0.30..0.40).contains(&a), "{name}: amplitude {a}");
    }
    assert!(a1500 < 0.01, "unrelated frequency {a1500}");
    // The microphone is on both channels, the (identical) app sound too.
    let (a440r, a880r) = (amplitude(&r[48_000..4 * 48_000], 440.0), amplitude(&r[48_000..4 * 48_000], 880.0));
    assert!((a440 - a440r).abs() < 0.03 && (a880 - a880r).abs() < 0.03, "channels differ");
    // Never louder than full scale.
    assert!(pcm.iter().all(|f| f[0].abs() <= 1.0 && f[1].abs() <= 1.0));
}

#[test]
fn audio_and_video_start_together_and_stay_locked_with_a_drifting_device() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // The "app sound" starts at 6.0 s on its own clock, which runs 0.3 % fast: on the monotonic clock that is at
    // 5.982 s. Its latency is 45 ms. The picture turns white at 6.0 s real time.
    let app = SineSpec { tone_starts_after: Duration::from_secs(6), clock: 1.003, ..app_spec() };
    let rec = recorder(dir.path().into(), AudioOptions { enabled: true, mic: false, app: true }, (None, Some(app)));
    rec.start();
    feed(&rec, 9.0, None, |t| if t >= 6.0 { 255 } else { 0 });
    let (path, events) = finish(&rec);
    assert!(matches!(events.as_slice(), [Event::Saved { audio: true, .. }]), "{events:?}");

    let pcm = decode_audio(&path);
    let audio_onset = onset(&left(&pcm), 880.0);
    // First white frame of the video.
    let o = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-map", "0:v:0", "-vf", "scale=2:2,format=gray", "-f", "rawvideo", "-"])
        .output()
        .unwrap();
    let idx = o.stdout.as_chunks::<4>().0.iter().position(|f| f[0] > 128).expect("white frame");
    let video_onset = idx as f64 / 30.0;
    eprintln!("audio onset {audio_onset:.3} s, video onset {video_onset:.3} s");
    assert!((audio_onset - 5.982).abs() < 0.010, "audio onset {audio_onset}");
    assert!((video_onset - 6.0).abs() < 0.05, "video onset {video_onset}");
    assert!((audio_onset - video_onset).abs() < 0.05, "a/v offset {} ms", (audio_onset - video_onset) * 1000.0);
}

#[test]
fn no_sound_library_or_devices_means_video_only_with_a_warning() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::with_audio(false, AudioOptions::default(), Arc::new(|_| Box::new(|| Err("the PulseAudio client library (libpulse-simple) is not installed".to_string()))));
    rec.set_dir(dir.path().into());
    rec.start();
    feed(&rec, 1.5, None, |_| 50);
    let (path, events) = finish(&rec);
    assert!(events.iter().any(|e| matches!(e, Event::Warning(m) if m.contains("No sound") && m.contains("libpulse"))), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Saved { audio: false, .. })), "{events:?}");
    let info = ffprobe(&path);
    assert!(info.contains("codec_type=video") && !info.contains("codec_type=audio"), "{info}");

    // Devices that fail to open: same.
    let rec = recorder(dir.path().join("b"), AudioOptions::default(), (None, None));
    rec.start();
    feed(&rec, 1.0, None, |_| 50);
    let (path, events) = finish(&rec);
    assert!(events.iter().any(|e| matches!(e, Event::Warning(_))), "{events:?}");
    assert!(!ffprobe(&path).contains("codec_type=audio"));
}

#[test]
fn one_missing_source_still_records_the_other() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // The microphone factory delivers nothing (mic: None in the specs means "not opened" -> no warning), so use a
    // failing opener for the microphone instead.
    let app = app_spec();
    let rec = Recorder::with_audio(
        false,
        AudioOptions::default(),
        Arc::new(move |_| {
            let app = app.clone();
            Box::new(move || {
                Ok(Sources {
                    mic: Some(Box::new(|| Err("no such device".to_string()))),
                    app: Some(opener(app)),
                })
            })
        }),
    );
    rec.set_dir(dir.path().into());
    rec.start();
    feed(&rec, 2.0, None, |_| 50);
    let (path, events) = finish(&rec);
    assert!(events.iter().any(|e| matches!(e, Event::Warning(m) if m.contains("microphone") && m.contains("no such device"))), "{events:?}");
    let pcm = decode_audio(&path);
    let mid = &left(&pcm)[24_000..72_000];
    assert!(amplitude(mid, 880.0) > 0.3 && amplitude(mid, 440.0) < 0.01);
}

#[test]
fn audio_off_gives_a_video_only_file_and_a_vanishing_device_is_survived() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rec = recorder(dir.path().join("a"), AudioOptions { enabled: false, mic: true, app: true }, (Some(mic_spec()), Some(app_spec())));
    rec.start();
    feed(&rec, 1.0, None, |_| 50);
    let (path, events) = finish(&rec);
    assert!(matches!(events.as_slice(), [Event::Saved { audio: false, .. }]), "{events:?}");
    assert!(!ffprobe(&path).contains("codec_type=audio"));
    // Both sources enabled, but main switch on with both off = nothing to record.
    let rec = recorder(dir.path().join("b"), AudioOptions { enabled: true, mic: false, app: false }, (Some(mic_spec()), Some(app_spec())));
    rec.start();
    feed(&rec, 1.0, None, |_| 50);
    let (path, _) = finish(&rec);
    assert!(!ffprobe(&path).contains("codec_type=audio"));

    // The microphone disappears after 1 s: warning, the app sound continues, the video is untouched.
    let mic = SineSpec { fail_after_frames: Some(48_000), ..mic_spec() };
    let rec = recorder(dir.path().join("c"), AudioOptions::default(), (Some(mic), Some(app_spec())));
    rec.start();
    feed(&rec, 3.0, None, |_| 50);
    let (path, events) = finish(&rec);
    assert!(events.iter().any(|e| matches!(e, Event::Warning(m) if m.contains("stopped"))), "{events:?}");
    let info = ffprobe(&path);
    let ad: f64 = get(&stream(&info, "audio"), "duration").parse().unwrap();
    assert!((2.8..3.3).contains(&ad), "{ad}");
    let pcm = left(&decode_audio(&path));
    assert!(amplitude(&pcm[96_000..130_000], 880.0) > 0.3 && amplitude(&pcm[96_000..130_000], 440.0) < 0.02);
}

#[test]
fn truncated_file_stays_playable_with_both_tracks() {
    if !tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rec = recorder(dir.path().into(), AudioOptions::default(), (Some(mic_spec()), Some(app_spec())));
    rec.start();
    feed(&rec, 6.0, None, |_| 90);
    let (path, _) = finish(&rec);
    // Simulate a crash: only the first 55 % of the bytes (the header still says duration 0 for the part that
    // was not finished; here the finished header is in place, which is the harder case for sloppy players).
    let data = std::fs::read(&path).unwrap();
    let cut = dir.path().join("crashed.mp4");
    std::fs::write(&cut, &data[..data.len() * 55 / 100]).unwrap();
    let info = ffprobe(&cut);
    assert!(info.contains("codec_type=video") && info.contains("codec_type=audio"), "{info}");
    let pcm = decode_audio(&cut);
    let secs = pcm.len() as f64 / 48_000.0;
    assert!((2.5..4.0).contains(&secs), "{secs} s of audio survive");
    assert!(amplitude(&left(&pcm)[48_000..96_000], 440.0) > 0.3);
}
