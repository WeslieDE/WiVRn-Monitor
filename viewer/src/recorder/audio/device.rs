//! Choice of the capture devices. Only reads; the server defaults are never changed.

use super::capture::{Capture, Opener, PulseCapture, PulseLib};
use super::{AudioOptions, Sources};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Environment variable: name of the sink whose output is recorded (its `.monitor`).
pub const SINK_ENV: &str = "WIVRN_MONITOR_AUDIO_SINK";
/// Environment variable: name of the microphone source.
pub const SOURCE_ENV: &str = "WIVRN_MONITOR_AUDIO_SOURCE";
/// Special names understood by PulseAudio and pipewire-pulse.
pub const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// Names (2nd column) from `pactl list short sinks|sources`.
pub fn parse_short_list(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split('\t').nth(1)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn is_wivrn(name: &str) -> bool {
    name.to_ascii_lowercase().contains("wivrn")
}

/// Which sink to record: the override, else a WiVRn sink if one exists, else `None` (= the default sink).
pub fn choose_sink(sinks: &[String], env: Option<&str>) -> Option<String> {
    env.filter(|e| !e.is_empty()).map(str::to_string).or_else(|| sinks.iter().find(|s| is_wivrn(s)).cloned())
}

/// Which microphone: the override, else a WiVRn source (never a monitor), else `None` (= the default source).
pub fn choose_source(sources: &[String], env: Option<&str>) -> Option<String> {
    env.filter(|e| !e.is_empty()).map(str::to_string).or_else(|| sources.iter().find(|s| is_wivrn(s) && !s.ends_with(".monitor")).cloned())
}

/// Device name of the recording of a sink's output.
pub fn monitor_of(sink: Option<&str>) -> String {
    sink.map_or_else(|| DEFAULT_MONITOR.to_string(), |s| format!("{s}.monitor"))
}

/// `pactl list short <what>` with a time limit; `None` if `pactl` is missing, fails or hangs.
fn pactl_list(what: &str) -> Option<Vec<String>> {
    let mut child = Command::new("pactl").args(["list", "short", what]).stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(st) if st.success() => break,
            Some(_) => return None,
            None if start.elapsed() > Duration::from_millis(1500) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    Some(parse_short_list(&text))
}

/// Sources for the real devices (PulseAudio / PipeWire). Runs on the audio thread (loads the library, asks `pactl`).
pub fn pulse_sources(opts: AudioOptions) -> Result<Sources, String> {
    let lib = Arc::new(PulseLib::load()?);
    let sink = choose_sink(&if opts.app { pactl_list("sinks").unwrap_or_default() } else { Vec::new() }, std::env::var(SINK_ENV).ok().as_deref());
    let source = choose_source(&if opts.mic { pactl_list("sources").unwrap_or_default() } else { Vec::new() }, std::env::var(SOURCE_ENV).ok().as_deref());
    let open = |lib: Arc<PulseLib>, dev: Option<String>, ch: u8, name: &'static str| -> Opener {
        Box::new(move || PulseCapture::open(lib, dev.as_deref(), ch, name).map(|c| Box::new(c) as Box<dyn Capture>))
    };
    Ok(Sources {
        mic: opts.mic.then(|| open(lib.clone(), source, 1, "Microphone (recording)")),
        app: opts.app.then(|| open(lib.clone(), Some(monitor_of(sink.as_deref())), 2, "Application sound (recording)")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINKS: &str = "67\talsa_output.pci-0000_01_00.1.hdmi-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n73\talsa_output.usb-Logitech.analog-stereo\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n";

    #[test]
    fn parses_pactl_lists() {
        let l = parse_short_list(SINKS);
        assert_eq!(l, ["alsa_output.pci-0000_01_00.1.hdmi-stereo", "alsa_output.usb-Logitech.analog-stereo"]);
        assert!(parse_short_list("").is_empty());
        assert!(parse_short_list("garbage without tabs").is_empty());
    }

    #[test]
    fn prefers_override_then_wivrn_then_default() {
        let mut sinks = parse_short_list(SINKS);
        assert_eq!(choose_sink(&sinks, None), None);
        sinks.push("WiVRn.sink".into());
        assert_eq!(choose_sink(&sinks, None).as_deref(), Some("WiVRn.sink"));
        assert_eq!(choose_sink(&sinks, Some("mysink")).as_deref(), Some("mysink"));
        assert_eq!(choose_sink(&sinks, Some("")).as_deref(), Some("WiVRn.sink"));
        let sources: Vec<String> = ["alsa_input.usb-Logitech.mono-fallback", "wivrn.sink.monitor", "wivrn.source"].map(String::from).into();
        assert_eq!(choose_source(&sources, None).as_deref(), Some("wivrn.source"));
        assert_eq!(choose_source(&sources[..2], None), None, "a monitor is never a microphone");
        assert_eq!(choose_source(&sources, Some("x")).as_deref(), Some("x"));
    }

    #[test]
    fn monitor_names() {
        assert_eq!(monitor_of(None), "@DEFAULT_MONITOR@");
        assert_eq!(monitor_of(Some("wivrn.sink")), "wivrn.sink.monitor");
    }
}
