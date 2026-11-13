//! Tiny persistent viewer settings: `~/.local/share/wivrn-monitor/viewer.json`. Read/write errors are tolerated.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings {
    /// Record automatically when VR frames arrive.
    pub auto_record: bool,
}

pub fn settings_path() -> PathBuf {
    crate::layer_install::install_dir().join("viewer.json")
}

impl Settings {
    /// Missing or broken files give the defaults.
    pub fn load_from(path: &Path) -> Self {
        let v: Option<serde_json::Value> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
        Settings { auto_record: v.as_ref().and_then(|v| v.get("auto_record")).and_then(|v| v.as_bool()).unwrap_or(false) }
    }

    /// Writes atomically (temp file + rename). Failures are only logged.
    pub fn save_to(&self, path: &Path) {
        let r = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::json!({ "auto_record": self.auto_record }).to_string() + "\n")?;
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
        Settings { auto_record: true }.save_to(&p);
        assert!(Settings::load_from(&p).auto_record);
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        // Unwritable location: no panic.
        Settings { auto_record: true }.save_to(&dir.path().join("sub/viewer.json/x/y"));
    }
}
