# wivrn-monitor

Desktop window (viewer, `viewer/`) that shows the right eye of a WiVRn VR session, without modifying WiVRn.
A Vulkan layer (`server-layer/`, loaded inside `wivrn-server`) copies the finished composed right eye and
sends it to the viewer over a memfd ring (3 slots, `SCM_RIGHTS` on `~/.local/share/wivrn-monitor/monitor.sock`,
`SOCK_SEQPACKET`). Crates: `protocol/` (messages, socket/memfd helpers), `viewer/`, `server-layer/`.
(The old OpenXR layer was removed; the viewer menu still offers cleanup of its installed files.)

Run: install the layer (viewer menu or script), restart wivrn-server, start `wivrn-monitor-viewer` (any order; it reconnects).

## Build
Build data goes to `/run/media/weslie/Daten/cargo-target/wivrn-monitor` (see `.cargo/config.toml`).

    cargo build --release

## AppImage
    scripts/build-appimage.sh      # -> /run/media/weslie/Daten/appimage-out/WiVRn-Monitor-x86_64.AppImage

Reproducible: the Rust build runs in an Ubuntu 22.04 container (`scripts/Containerfile.build`, podman or docker),
so the binaries need at most **glibc 2.35** (viewer) and **glibc 2.34** (layer `.so`). appimagetool is downloaded to
`/run/media/weslie/Daten/tools/appimagetool-dl/`; no FUSE needed on the build host. Layout: `usr/bin/wivrn-monitor-viewer`,
`usr/lib/wivrn-monitor/{libwivrn_monitor_server_layer.so,wivrn-monitor-server-layer.json}`. The viewer dlopens
Wayland/X11/GL from the host (nothing bundled). The install menu copies the `.so` out of the (temporary) AppImage mount to
`~/.local/share/wivrn-monitor/`; the status check compares hashes against the bundled `.so`.
Headless CLI flags of the viewer: `--install-layer`, `--uninstall-layer`, `--layer-status`, `--self-test-install`
(use with `WIVRN_MONITOR_HOME_OVERRIDE=/tmp/h`). Limitation: Ubuntu-22.04-era distros or newer (glibc >= 2.35 for the viewer).

## CI / Releases
Every push to `main` builds the AppImage (clippy with `-D warnings` and `cargo test --release` run first) and replaces the
rolling release **`latest`** (asset `WiVRn-Monitor-x86_64.AppImage`, body names the commit SHA). Tags `v*` create a regular release.
The CI build uses `scripts/build-appimage.sh --native` inside an Ubuntu 22.04 environment (no podman-in-podman);
dependencies come from `scripts/ci-deps.sh`.
- **Gitea** (`.gitea/workflows/release.yml`): job runs in `container: ubuntu:22.04` on an act_runner. `runs-on: ubuntu-latest` is a
  guess: set it to a label your runner actually has (Site Admin -> Actions -> Runners; labels come from the runner config / `.runner`)
  and that runs Docker containers. No extra secrets: the job token (`secrets.GITHUB_TOKEN` / `gitea.token`) is used; if token
  permissions are restricted, allow read+write for Actions. No JS actions are used (checkout via `git clone` + token), so it
  works without github.com access. Gitea only reads `.github/workflows` if `.gitea/workflows` does not exist.
- **GitHub** (`.github/workflows/release.yml`): `ubuntu-22.04` runner, `permissions: contents: write`, `GITHUB_TOKEN` only,
  releases via `gh`; also uploads a workflow artifact. Guarded by `github.server_url == 'https://github.com'`.
- Not runnable locally: the workflows themselves; the `--native` path was tested in a clean `ubuntu:22.04` container.

## Icon
Source `assets/icon-src.png` (+ 512/256/64 px). The viewer embeds `icon-256.png` as window icon; the taskbar/menu icon under
Wayland comes from `packaging/wivrn-monitor.desktop` (`Icon=wivrn-monitor`, `StartupWMClass=wivrn-monitor`, app_id `wivrn-monitor`),
so it appears when run from the AppImage (via e.g. AppImageLauncher) or after installing the `.desktop` file and icon
(`~/.local/share/applications`, `~/.local/share/icons/hicolor/256x256/apps/wivrn-monitor.png`).

## Install / uninstall (per user)
Viewer menu Datei, or `scripts/install-server-layer.sh [--debug|--uninstall]`; restart wivrn-server afterwards.

## Viewer (egui)
`wivrn-monitor-viewer` (eframe/glow, Wayland + X11): menu bar (Datei: Layer installieren/deinstallieren,
Alten OpenXR-Layer entfernen, Beenden; Ansicht: FPS-Overlay; Hilfe), status bar (Layer installiert?,
wivrn-server laeuft?, Verbindung, Frames/FPS/Aufloesung). Install/uninstall of the Vulkan server layer
(`libwivrn_monitor_server_layer.so` + `wivrn-monitor-server-layer.json`) happens in `viewer/src/layer_install.rs`;
the files are searched in `$WIVRN_MONITOR_LAYER_DIR`, `<exe>/../lib/wivrn-monitor/`, next to the binary and in
`server-layer/`. After (un)installing, wivrn-server must be restarted manually.
`WIVRN_MONITOR_HOME_OVERRIDE` redirects all install paths (tests). Test client without VR:
`cargo run -p wivrn-monitor-viewer --example fake_layer [w h]`.

## Server layer (`server-layer/`, Vulkan layer inside `wivrn-server`)
Sees the *finished composed image* (all apps + overlays) instead of one app's swapchain. Implicit Vulkan layer
`VK_LAYER_WESLIE_wivrn_monitor_server` (`libwivrn_monitor_server_layer.so`). It is loaded by the Vulkan loader in
every process but is a pure pass-through unless the executable is `wivrn-server` (rejecting the negotiation instead
makes the loader print ERROR lines in foreign processes). In `wivrn-server` it detects WiVRn's foveation compute
pass (descriptor-set-layout signature sampler[2] / SSBO / storage image / storage image), appends its own compute
dispatch (reads `source[1]` = unfoveated right eye through WiVRn's own set 0, source rect from the foveation SSBO,
centre-crop to 16:9, linear->sRGB, downscale into a host-visible buffer), adds a layer-owned timeline-semaphore signal to
the same `vkQueueSubmit2`; a consumer thread waits, copies into the memfd ring and sends the frame (`Rgba8Srgb`).
Env: `DISABLE_WIVRN_MONITOR_SERVER=1`, `WIVRN_MONITOR_LOG`, `WIVRN_MONITOR_DIAG=1` (signature / group sizes / sizes in
the log), `WIVRN_MONITOR_FPS` (30), `WIVRN_MONITOR_WIDTH` (1280), `WIVRN_MONITOR_ASPECT` (`16:9` default, `native`, `a:b`),
`WIVRN_MONITOR_SERVER_FORCE=1` (tests only). Log: `~/.local/share/wivrn-monitor/server-layer.log`.

    cargo build --release -p wivrn-monitor-server-layer
    scripts/install-server-layer.sh [--debug]      # or --uninstall; then restart wivrn-server

Shader: `server-layer/shaders/mirror.comp` (compiled by `build.rs`; `mirror.spv` is checked in as fallback).
