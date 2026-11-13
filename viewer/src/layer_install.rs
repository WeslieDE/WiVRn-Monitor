//! Installation and status of the Vulkan server layer (`libwivrn_monitor_server_layer.so` plus its
//! manifest), done per user without root. Mirrors `scripts/install-server-layer.sh`.
//!
//! All paths derive from [`home_dir`]; tests (and nothing else) set `WIVRN_MONITOR_HOME_OVERRIDE`
//! so that real user files are never touched.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const LAYER_SO: &str = "libwivrn_monitor_server_layer.so";
pub const LAYER_MANIFEST: &str = "wivrn-monitor-server-layer.json";
const LEGACY_SO: &str = "libwivrn_monitor_layer.so";
const LEGACY_MANIFEST: &str = "wivrn-monitor-layer.json";

pub fn home_dir() -> PathBuf {
    for var in ["WIVRN_MONITOR_HOME_OVERRIDE", "HOME"] {
        if let Some(h) = std::env::var_os(var).filter(|h| !h.is_empty()) {
            return PathBuf::from(h);
        }
    }
    PathBuf::from("/tmp")
}

/// `~/.local/share` (XDG_DATA_HOME is honoured unless the home override is active).
fn data_home() -> PathBuf {
    if std::env::var_os("WIVRN_MONITOR_HOME_OVERRIDE").is_none() {
        if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            return PathBuf::from(d);
        }
    }
    home_dir().join(".local/share")
}

/// Where the `.so` (and a copy of the manifest) live.
pub fn install_dir() -> PathBuf {
    data_home().join("wivrn-monitor")
}
pub fn vulkan_manifest_path() -> PathBuf {
    data_home().join("vulkan/implicit_layer.d").join(LAYER_MANIFEST)
}
pub fn installed_so_path() -> PathBuf {
    install_dir().join(LAYER_SO)
}
fn legacy_manifest_path() -> PathBuf {
    data_home().join("openxr/1/api_layers/implicit.d").join(LEGACY_MANIFEST)
}

/// Files shipped with the viewer.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub so: PathBuf,
    pub manifest_template: PathBuf,
}

/// Searches `$WIVRN_MONITOR_LAYER_DIR`, `<exe dir>/../lib/wivrn-monitor/` (AppImage) and the
/// directory of the viewer binary plus the source tree (development).
pub fn find_bundle() -> Option<Bundle> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("WIVRN_MONITOR_LAYER_DIR").filter(|d| !d.is_empty()) {
        dirs.push(d.into());
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("../lib/wivrn-monitor"));
        dirs.push(exe_dir);
    }
    find_bundle_in(&dirs, &[Path::new(env!("CARGO_MANIFEST_DIR")).join("../server-layer")])
}

/// `so_dirs` must contain the `.so`; the manifest template is looked up there first, then in `extra`.
pub fn find_bundle_in(so_dirs: &[PathBuf], extra_manifest_dirs: &[PathBuf]) -> Option<Bundle> {
    let so_dir = so_dirs.iter().find(|d| d.join(LAYER_SO).is_file())?;
    let manifest = std::iter::once(so_dir).chain(extra_manifest_dirs).map(|d| d.join(LAYER_MANIFEST)).find(|p| p.is_file())?;
    Some(Bundle { so: so_dir.join(LAYER_SO), manifest_template: manifest })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerState {
    NotInstalled,
    /// Installed and identical to the bundled version (or no bundle to compare with).
    Installed,
    /// Installed but broken or different from the bundled version.
    Outdated(String),
}

impl LayerState {
    pub fn label(&self) -> String {
        match self {
            LayerState::NotInstalled => "not installed".into(),
            LayerState::Installed => "installed".into(),
            LayerState::Outdated(why) => format!("outdated – reinstall ({why})"),
        }
    }
}

fn hash_file(p: &Path) -> io::Result<u64> {
    // FNV-1a 64; enough to detect a different build.
    let data = fs::read(p)?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    Ok(h)
}

fn manifest_library_path(p: &Path) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
    v.get("layer")?.get("library_path")?.as_str().map(PathBuf::from)
}

pub fn layer_state(bundle: Option<&Bundle>) -> LayerState {
    let manifest = vulkan_manifest_path();
    let so = installed_so_path();
    if !manifest.exists() && !so.exists() {
        return LayerState::NotInstalled;
    }
    if !manifest.exists() {
        return LayerState::Outdated("manifest missing".into());
    }
    let Some(lib) = manifest_library_path(&manifest) else {
        return LayerState::Outdated("manifest unreadable".into());
    };
    if !lib.is_file() {
        return LayerState::Outdated("library missing".into());
    }
    if let Some(b) = bundle {
        match (hash_file(&lib), hash_file(&b.so)) {
            (Ok(a), Ok(c)) if a == c => {}
            _ => return LayerState::Outdated("different version".into()),
        }
    }
    LayerState::Installed
}

pub fn legacy_installed() -> bool {
    legacy_manifest_path().exists() || install_dir().join(LEGACY_SO).exists()
}

fn ctx<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> Result<T, String> {
    r.map_err(|e| format!("{}: {e}", what()))
}

/// Writes `data` next to `dest` and renames it over it (running processes keep the old inode).
fn write_atomic(dest: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let dir = dest.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no target directory"))?;
    fs::create_dir_all(dir)?;
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.new", std::process::id()));
    let r = fs::write(&tmp, data)
        .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)))
        .and_then(|_| fs::rename(&tmp, dest));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Builds the manifest text with `library_path` pointing to `lib`.
pub fn render_manifest(template: &str, lib: &Path) -> Result<String, String> {
    let mut v: serde_json::Value = serde_json::from_str(template).map_err(|e| format!("Invalid manifest template: {e}"))?;
    let layer = v
        .get_mut("layer")
        .and_then(|l| l.as_object_mut())
        .ok_or("Manifest template has no \"layer\" object")?;
    layer.insert("library_path".into(), serde_json::Value::String(lib.to_string_lossy().into_owned()));
    serde_json::to_string_pretty(&v).map(|s| s + "\n").map_err(|e| e.to_string())
}

pub fn install(bundle: Option<&Bundle>) -> Result<(), String> {
    let b = bundle.ok_or("Layer files not found (check WIVRN_MONITOR_LAYER_DIR or rebuild the viewer)")?;
    let so_data = ctx(fs::read(&b.so), || format!("Reading {}", b.so.display()))?;
    let template = ctx(fs::read_to_string(&b.manifest_template), || format!("Reading {}", b.manifest_template.display()))?;
    let dest_so = installed_so_path();
    let manifest = render_manifest(&template, &dest_so)?;
    ctx(write_atomic(&dest_so, &so_data, 0o755), || format!("Writing {} failed", dest_so.display()))?;
    let dest_manifest = vulkan_manifest_path();
    ctx(write_atomic(&dest_manifest, manifest.as_bytes(), 0o644), || {
        format!("Writing {} failed", dest_manifest.display())
    })
}

fn remove_if_exists(p: &Path) -> Result<(), String> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Removing {} failed: {e}", p.display())),
    }
}

pub fn uninstall() -> Result<(), String> {
    remove_if_exists(&vulkan_manifest_path())?;
    remove_if_exists(&installed_so_path())?;
    let _ = fs::remove_dir(install_dir()); // only if empty
    Ok(())
}

pub fn remove_legacy() -> Result<(), String> {
    remove_if_exists(&legacy_manifest_path())?;
    remove_if_exists(&install_dir().join(LEGACY_SO))?;
    remove_if_exists(&install_dir().join(LEGACY_MANIFEST))?;
    let _ = fs::remove_dir(install_dir());
    Ok(())
}

/// True if a process named exactly `wivrn-server` runs (`/proc/<pid>/comm`; cannot match ourselves).
pub fn wivrn_server_running() -> bool {
    let Ok(rd) = fs::read_dir("/proc") else { return false };
    rd.flatten().any(|e| {
        e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())
            && fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim_end() == "wivrn-server")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    const TEMPLATE: &str = r#"{"file_format_version":"1.0.0","layer":{"name":"VK_LAYER_X","type":"GLOBAL","library_path":"x.so","api_version":"1.3.0"}}"#;

    fn setup(so_bytes: &[u8]) -> (tempfile::TempDir, tempfile::TempDir, Bundle) {
        let home = tempfile::tempdir().unwrap();
        let bundle_dir = tempfile::tempdir().unwrap();
        std::env::set_var("WIVRN_MONITOR_HOME_OVERRIDE", home.path());
        fs::write(bundle_dir.path().join(LAYER_SO), so_bytes).unwrap();
        fs::write(bundle_dir.path().join(LAYER_MANIFEST), TEMPLATE).unwrap();
        let b = find_bundle_in(&[bundle_dir.path().to_path_buf()], &[]).unwrap();
        (home, bundle_dir, b)
    }

    #[test]
    fn install_status_uninstall() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (home, _bd, b) = setup(b"v1");
        assert_eq!(layer_state(Some(&b)), LayerState::NotInstalled);
        install(Some(&b)).unwrap();
        assert_eq!(layer_state(Some(&b)), LayerState::Installed);
        let m = fs::read_to_string(vulkan_manifest_path()).unwrap();
        assert!(m.contains(&home.path().join(".local/share/wivrn-monitor").join(LAYER_SO).to_string_lossy().into_owned()));
        assert!(vulkan_manifest_path().starts_with(home.path()));
        // no temp files left over
        assert!(fs::read_dir(install_dir()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".new")));
        uninstall().unwrap();
        assert_eq!(layer_state(Some(&b)), LayerState::NotInstalled);
        assert!(!install_dir().exists());
        uninstall().unwrap(); // idempotent
    }

    #[test]
    fn detects_outdated_and_broken() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, bd, b) = setup(b"v1");
        install(Some(&b)).unwrap();
        fs::write(bd.path().join(LAYER_SO), b"v2").unwrap();
        assert!(matches!(layer_state(Some(&b)), LayerState::Outdated(_)));
        install(Some(&b)).unwrap();
        assert_eq!(layer_state(Some(&b)), LayerState::Installed);
        fs::remove_file(installed_so_path()).unwrap();
        assert!(matches!(layer_state(Some(&b)), LayerState::Outdated(_)));
        fs::remove_file(vulkan_manifest_path()).unwrap();
        assert_eq!(layer_state(Some(&b)), LayerState::NotInstalled);
    }

    #[test]
    fn errors_are_reported_not_panics() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _bd, b) = setup(b"v1");
        assert!(install(None).is_err());
        // a regular file where the directory should be
        fs::create_dir_all(data_home()).unwrap();
        fs::write(data_home().join("vulkan"), b"x").unwrap();
        let e = install(Some(&b)).unwrap_err();
        assert!(e.contains("failed"), "{e}");
    }

    #[test]
    fn legacy_removal() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _bd, _b) = setup(b"v1");
        assert!(!legacy_installed());
        fs::create_dir_all(legacy_manifest_path().parent().unwrap()).unwrap();
        fs::write(legacy_manifest_path(), "{}").unwrap();
        fs::create_dir_all(install_dir()).unwrap();
        fs::write(install_dir().join(LEGACY_SO), "x").unwrap();
        assert!(legacy_installed());
        remove_legacy().unwrap();
        assert!(!legacy_installed());
    }

    #[test]
    fn bundle_search_and_manifest_render() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        assert!(find_bundle_in(&[d.path().to_path_buf()], &[]).is_none());
        fs::write(d.path().join(LAYER_SO), "x").unwrap();
        assert!(find_bundle_in(&[d.path().to_path_buf()], &[]).is_none()); // manifest missing
        let m = render_manifest(TEMPLATE, Path::new("/abs/lib.so")).unwrap();
        assert!(m.contains("\"/abs/lib.so\""));
        assert!(render_manifest("{}", Path::new("/a")).is_err());
    }

    #[test]
    fn server_scan_does_not_match_itself() {
        // The test binary is not called wivrn-server; just make sure the scan runs.
        let _ = wivrn_server_running();
    }
}
