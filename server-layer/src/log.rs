//! Minimal logger: stderr + file `~/.local/share/wivrn-monitor/server-layer.log`.
//! Level via env `WIVRN_MONITOR_LOG` = off|error|warn|info|debug|trace (default: info).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
pub enum Level {
    Off = 0,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

struct Logger {
    level: Level,
    file: Mutex<Option<File>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| {
        let level = match std::env::var("WIVRN_MONITOR_LOG")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "0" => Level::Off,
            "error" => Level::Error,
            "warn" => Level::Warn,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => Level::Info,
        };
        let dir = wivrn_monitor_protocol::data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let file = if level == Level::Off {
            None
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("server-layer.log"))
                .ok()
        };
        Logger { level, file: Mutex::new(file) }
    })
}

pub fn enabled(level: Level) -> bool {
    level <= logger().level
}

pub fn write(level: Level, args: std::fmt::Arguments) {
    let l = logger();
    if level > l.level {
        return;
    }
    let tag = match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        _ => "TRACE",
    };
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let line = format!(
        "[wivrn-monitor-server {}.{:03} pid={} {tag}] {args}\n",
        t.as_secs(),
        t.subsec_millis(),
        std::process::id()
    );
    if level <= Level::Error {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
    let mut f = l.file.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = f.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $($a:tt)*) => { $crate::log::write($lvl, format_args!($($a)*)) };
}
