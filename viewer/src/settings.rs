//! Tiny persistent viewer settings: `~/.local/share/wivrn-monitor/viewer.json`. Read/write errors are tolerated.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Record automatically when VR frames arrive.
    pub auto_record: bool,
    /// Record sound (main switch; default on).
    pub record_audio: bool,
    /// Record the microphone (default on).
    pub audio_mic: bool,
    /// Record what the applications play (default on).
    pub audio_app: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_record: false, record_audio: true, audio_mic: true, audio_app: true }
    }
}

impl Settings {
    pub fn audio_options(&self) -> crate::recorder::audio::AudioOptions {
        crate::recorder::audio::AudioOptions { enabled: self.record_audio, mic: self.audio_mic, app: self.audio_app }
    }
}

pub fn settings_path() -> PathBuf {
    crate::layer_install::install_dir().join("viewer.json")
}

impl Settings {
    /// Missing or broken files give the defaults.
    pub fn load_from(path: &Path) -> Self {
        let v: Option<serde_json::Value> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
        let get = |k: &str, default: bool| v.as_ref().and_then(|v| v.get(k)).and_then(|v| v.as_bool()).unwrap_or(default);
        let d = Settings::default();
        Settings { auto_record: get("auto_record", d.auto_record), record_audio: get("record_audio", d.record_audio), audio_mic: get("audio_mic", d.audio_mic), audio_app: get("audio_app", d.audio_app) }
    }

    /// Writes atomically (temp file + rename). Failures are only logged.
    pub fn save_to(&self, path: &Path) {
        let r = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::json!({ "auto_record": self.auto_record, "record_audio": self.record_audio, "audio_mic": self.audio_mic, "audio_app": self.audio_app }).to_string() + "\n")?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = r {
            eprintln!("settings: cannot save {}: {e}", path.display());
        }
    }

    pub fn load() -> Self {
        Self::load_from(&settings_path())
    }

    pub fn save(&self) {
        self.save_to(&settings_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerance() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/viewer.json");
        assert_eq!(Settings::load_from(&p), Settings::default());
        let s = Settings { auto_record: true, record_audio: false, audio_mic: false, audio_app: true };
        s.save_to(&p);
        assert_eq!(Settings::load_from(&p), s);
        // Old files without the audio keys: audio defaults to on.
        std::fs::write(&p, "{\"auto_record\": true}").unwrap();
        let old = Settings::load_from(&p);
        assert!(old.auto_record && old.record_audio && old.audio_mic && old.audio_app);
        assert_eq!(old.audio_options(), crate::recorder::audio::AudioOptions { enabled: true, mic: true, app: true });
        // Wrong types fall back to the defaults.
        std::fs::write(&p, "{\"record_audio\": \"no\", \"audio_mic\": 0}").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        // Unwritable location: no panic.
        Settings::default().save_to(&dir.path().join("sub/viewer.json/x/y"));
    }
}
