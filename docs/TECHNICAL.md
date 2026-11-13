# WiVRn Monitor – how it works

Technical background for the curious and for troubleshooting. For normal use, the [README](../README.md) is all you need.
Building, AppImage, CI and tests are covered in [DEVELOPMENT.md](DEVELOPMENT.md).

## Overview

A desktop window (viewer, `viewer/`) shows the right eye of a WiVRn VR session, without modifying WiVRn.
A Vulkan layer (`server-layer/`, loaded inside `wivrn-server`) copies the finished composed right eye and
sends it to the viewer over a memfd ring (3 slots, `SCM_RIGHTS` on `~/.local/share/wivrn-monitor/monitor.sock`,
`SOCK_SEQPACKET`).

Crates:
- `protocol/` – messages, socket and memfd helpers
- `viewer/` – the desktop window (`wivrn-monitor-viewer`)
- `server-layer/` – the Vulkan layer inside `wivrn-server`

The old OpenXR layer was removed; the viewer menu still offers cleanup of its installed files
(File → "Remove old OpenXR layer", only shown if those files are found).

Run order does not matter: install the layer (viewer menu or script), restart `wivrn-server`, start
`wivrn-monitor-viewer`. The viewer reconnects automatically.

## Viewer (egui)

`wivrn-monitor-viewer` (eframe/glow, Wayland + X11).

- Menu bar: File → "Install layer", "Uninstall layer", "Remove old OpenXR layer" (only if legacy files exist), "Quit";
  View → "FPS overlay"; Help → "About".
- Status bar: Layer (installed / outdated – reinstall / not installed), WiVRn server (running / not running),
  Connection (waiting / connected), Frames (resolution, FPS, or age of the last frame).

Install/uninstall of the server layer (`libwivrn_monitor_server_layer.so` + `wivrn-monitor-server-layer.json`) happens in
`viewer/src/layer_install.rs`. The bundled files are searched in `$WIVRN_MONITOR_LAYER_DIR`, `<exe>/../lib/wivrn-monitor/`,
next to the binary and in `server-layer/`. After (un)installing, `wivrn-server` must be restarted manually.

Installed files (per user, no root):
- `~/.local/share/wivrn-monitor/` – layer `.so`, socket, log
- `~/.local/share/vulkan/implicit_layer.d/wivrn-monitor-server-layer.json` – layer manifest

`XDG_DATA_HOME` is honoured instead of `~/.local/share` (unless the home override is active).

When run from the AppImage, the install menu copies the `.so` out of the (temporary) AppImage mount into
`~/.local/share/wivrn-monitor/`; the status check compares hashes against the bundled `.so` ("outdated" if they differ).

Headless CLI flags of the viewer: `--install-layer`, `--uninstall-layer`, `--layer-status`, `--self-test-install`
(use the latter with `WIVRN_MONITOR_HOME_OVERRIDE=/tmp/h`).

Alternative install without the viewer: `scripts/install-server-layer.sh [--debug|--uninstall]`; restart `wivrn-server` afterwards.

## Server layer (`server-layer/`, Vulkan layer inside `wivrn-server`)

Sees the *finished composed image* (all apps + overlays) instead of one app's swapchain. Implicit Vulkan layer
`VK_LAYER_WESLIE_wivrn_monitor_server` (`libwivrn_monitor_server_layer.so`).

It is loaded by the Vulkan loader in every process but is a pure pass-through unless the executable is `wivrn-server`
(rejecting the negotiation instead makes the loader print ERROR lines in foreign processes).

In `wivrn-server` it:
1. detects WiVRn's foveation compute pass (descriptor-set-layout signature sampler[2] / SSBO / storage image / storage image);
2. appends its own compute dispatch: reads `source[1]` (= unfoveated right eye) through WiVRn's own set 0, takes the source
   rect from the foveation SSBO, centre-crops to 16:9, converts linear → sRGB and downscales into a host-visible buffer;
3. adds a layer-owned timeline-semaphore signal to the same `vkQueueSubmit2`;
4. a consumer thread waits on it, copies into the memfd ring and sends the frame (`Rgba8Srgb`).

If the signature is not found (e.g. a WiVRn version with a changed internal structure), the layer stays inactive.

Shader: `server-layer/shaders/mirror.comp` (compiled by `build.rs`; `mirror.spv` is checked in as fallback).

## Environment variables

Server layer (set in the environment of `wivrn-server`):

| Variable | Meaning |
|---|---|
| `DISABLE_WIVRN_MONITOR_SERVER=1` | Disable the layer completely |
| `WIVRN_MONITOR_LOG` | Log settings |
| `WIVRN_MONITOR_DIAG=1` | Write diagnostics to the log (signature, group sizes, sizes) |
| `WIVRN_MONITOR_FPS` | Target frame rate of the mirror (default `30`) |
| `WIVRN_MONITOR_WIDTH` | Output width in pixels (default `1280`) |
| `WIVRN_MONITOR_ASPECT` | Crop: `16:9` (default), `native` (uncropped) or `a:b` |
| `WIVRN_MONITOR_SERVER_FORCE=1` | Activate in processes other than `wivrn-server` (tests only) |

Viewer / both:

| Variable | Meaning |
|---|---|
| `WIVRN_MONITOR_SOCK` | Socket path (default `~/.local/share/wivrn-monitor/monitor.sock`) |
| `WIVRN_MONITOR_LAYER_DIR` | Directory with the layer files to install |
| `WIVRN_MONITOR_HOME_OVERRIDE` | Redirects all install paths (tests only) |

## Troubleshooting

1. Check the status bar of the window. "Layer: not installed" / "outdated" → File → "Install layer", then restart `wivrn-server`.
2. "WiVRn server: not running" → start WiVRn (e.g. via the WiVRn dashboard).
3. Layer installed and server running, but "Connection: waiting" → `wivrn-server` was probably started before the
   layer was installed. Restart it.
4. Still nothing: look at the log `~/.local/share/wivrn-monitor/server-layer.log`. For more detail, start `wivrn-server`
   with `WIVRN_MONITOR_DIAG=1`. If the log says the foveation pass was not found, your WiVRn version probably changed
   internally – please open an issue with the log and your WiVRn version.
5. If `wivrn-server` crashes and you suspect the layer: start it with `DISABLE_WIVRN_MONITOR_SERVER=1` or uninstall the layer
   (File → "Uninstall layer") and restart WiVRn.
