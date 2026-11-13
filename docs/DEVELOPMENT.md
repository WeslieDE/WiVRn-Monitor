# WiVRn Monitor – development

How to build, package and release. How the program works internally is described in [TECHNICAL.md](TECHNICAL.md).

## Build

By default cargo builds into `./target`. To keep build data on another drive, copy `.cargo/config.example.toml` to `.cargo/config.toml` (git-ignored) and set `target-dir`, or export `CARGO_TARGET_DIR`.

    cargo build --release

Only the server layer:

    cargo build --release -p wivrn-monitor-server-layer
    scripts/install-server-layer.sh [--debug]      # or --uninstall; then restart wivrn-server

## Tests

    cargo test --release
    cargo clippy --all-targets -- -D warnings

Test client without VR (sends a moving colour gradient to a running viewer):

    cargo run -p wivrn-monitor-viewer --example fake_layer [w h]

To try the viewer without touching your real install, use a temporary home and socket:

    WIVRN_MONITOR_HOME_OVERRIDE=/tmp/h WIVRN_MONITOR_SOCK=/tmp/m.sock wivrn-monitor-viewer
    WIVRN_MONITOR_SOCK=/tmp/m.sock cargo run -p wivrn-monitor-viewer --example fake_layer

Install round trip without a GUI: `WIVRN_MONITOR_HOME_OVERRIDE=/tmp/h wivrn-monitor-viewer --self-test-install`.

Recording tests: unit tests plus `viewer/tests/record_e2e.rs` (encodes synthetic frames; checks the MP4 with `ffprobe`/`ffmpeg` if installed,
otherwise skips that part). Tests never touch `~/Videos/VR`. Benchmark: `viewer/examples/rec_bench.rs` (see its header).

## Build dependencies for recording

The `openh264` crate compiles OpenH264 from source: a C++ compiler (`g++`, part of `build-essential`) is required, `nasm` is recommended
(assembly speed-ups; without it the build still succeeds, silently using slower C++ code). `scripts/ci-deps.sh` and
`scripts/Containerfile.build` install `nasm`. `cmake` is not needed. Builds take roughly a minute longer than before.

## AppImage

    scripts/build-appimage.sh      # -> /run/media/weslie/Daten/appimage-out/WiVRn-Monitor-x86_64.AppImage

Reproducible: the Rust build runs in an Ubuntu 22.04 container (`scripts/Containerfile.build`, podman or docker),
so the binaries need at most **glibc 2.35** (viewer) and **glibc 2.34** (layer `.so`). This means Ubuntu 22.04-era
distros or newer.

appimagetool is downloaded to `/run/media/weslie/Daten/tools/appimagetool-dl/`; no FUSE needed on the build host.

Layout inside the AppImage:
- `usr/bin/wivrn-monitor-viewer`
- `usr/lib/wivrn-monitor/{libwivrn_monitor_server_layer.so,wivrn-monitor-server-layer.json}`

The viewer dlopens Wayland/X11/GL from the host (nothing bundled).

## CI / Releases

Every push to `main` builds the AppImage (clippy with `-D warnings` and `cargo test --release` run first) and replaces the
rolling release **`latest`** (asset `WiVRn-Monitor-x86_64.AppImage`, the release text names the commit SHA).
Tags `v*` create a regular release.

The CI build uses `scripts/build-appimage.sh --native` inside an Ubuntu 22.04 environment (no podman-in-podman);
dependencies come from `scripts/ci-deps.sh`.

- **Gitea** (`.gitea/workflows/release.yml`): the job runs in `container: ubuntu:22.04` on an act_runner. `runs-on: ubuntu-latest`
  is a guess: set it to a label your runner actually has (Site Admin → Actions → Runners; labels come from the runner config /
  `.runner`) and that runs Docker containers. No extra secrets: the job token (`secrets.GITHUB_TOKEN` / `gitea.token`) is used;
  if token permissions are restricted, allow read+write for Actions. No JS actions are used (checkout via `git clone` + token),
  so it works without github.com access. Gitea only reads `.github/workflows` if `.gitea/workflows` does not exist.
- **GitHub** (`.github/workflows/release.yml`): `ubuntu-22.04` runner, `permissions: contents: write`, `GITHUB_TOKEN` only,
  releases via `gh`; also uploads a workflow artifact. Guarded by `github.server_url == 'https://github.com'`.
- Not runnable locally: the workflows themselves; the `--native` path was tested in a clean `ubuntu:22.04` container.

## Icon

Source `assets/icon-src.png` (+ 512/256/64 px). The viewer embeds `icon-256.png` as window icon. The taskbar/menu icon under
Wayland comes from `packaging/wivrn-monitor.desktop` (`Icon=wivrn-monitor`, `StartupWMClass=wivrn-monitor`, app_id
`wivrn-monitor`), so it appears when run from the AppImage via an integration tool (e.g. AppImageLauncher) or after installing
the `.desktop` file and icon (`~/.local/share/applications`, `~/.local/share/icons/hicolor/256x256/apps/wivrn-monitor.png`).

The README screenshot `assets/screenshot.png` shows the test pattern from `fake_layer`.
