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
  Record → "Start/Stop recording" (F9), "Record automatically when VR frames arrive", "Open recordings folder"; View → "FPS overlay"; Help → "About".
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

## Recording

Code: `viewer/src/recorder/` (`mod.rs` front end, `worker.rs` encoder thread, `yuv.rs`, `mp4.rs`), `viewer/src/settings.rs`.

- **What is recorded:** exactly the RGBA frames received from the layer (not the UI), at their original size (odd sizes are
  cropped by one pixel to even), no audio. Menu Record / F9; setting `auto_record` in `~/.local/share/wivrn-monitor/viewer.json`.
- **Output:** `~/Videos/VR/VR_YYYY-MM-DD_HH-MM-SS.mp4` (local time; `_2`, `_3` on name collision, files are created with
  `create_new`, never overwritten). A resolution change, a connection end or 5 s without frames closes the file; the next frame opens a new one.
  Auto mode additionally switches itself off then; a manual recording stays armed.
- **Environment:** `WIVRN_MONITOR_VIDEO_DIR` replaces `~/Videos/VR` (tests; `WIVRN_MONITOR_HOME_OVERRIDE` moves `~` as well).
- **Encoder:** Cisco OpenH264 via the `openh264` crate (source feature: C++ sources are compiled into the binary; `nasm` is used for
  x86 assembly if present at build time, otherwise the plain C++ path is used - works, but slower). Constrained Baseline, level
  auto (3.1 at 720p), bitrate mode 8 Mbit/s, frame skipping on, keyframe every 2 s (60 frames), 2 encoder threads, VUI BT.709 limited range.
  Colour conversion RGBA(sRGB values) to I420, BT.709 limited range, fixed-point scalar code writing straight into the planes.
- **Timeline:** constant 30 fps. A frame's slot is computed from its arrival time; surplus frames (source above 30 fps) are skipped
  before any copying, missing slots are filled by re-encoding the previous picture (cheap), gaps longer than 1 s are cut out.
- **Container:** own small fragmented MP4 writer (`ftyp`, `moov` with `mvex`, then `moof`+`mdat` per 30 frames, flushed to the OS
  each time). After a crash everything up to the last full fragment plays. On a clean finish the `moov` is rewritten in place with the real
  duration. Why not a classic MP4: its index is written at the end and a crash would lose the whole file. Why not Matroska: MP4 plays
  in more players, browsers and editors.
- **Threads:** the net thread only does a memcpy into a pooled buffer and a non-blocking `try_send` into a bounded queue (8 frames).
  One `rec-encoder` thread per output file converts, encodes and writes. If the queue is full the frame is dropped and counted (status bar,
  log); the gap is filled by a repeated picture. The UI thread only toggles and polls. Closing the window finalizes the file.
- **Errors:** folder not creatable, disk full or write errors end the recording with an English message dialog; the viewer keeps running.
  Auto mode does not retry until the next session.
- **Measured (1280x720@30, no nasm, this machine):** 23-42 % of one core (simple to very detailed content), up to ~58 MB/min.
- **Licence / patents:** OpenH264 is BSD-2-Clause. Cisco's patent licence for H.264 covers the binaries Cisco itself builds and distributes,
  not binaries compiled from the source by others. This project compiles the source, so that patent grant does not apply here.
  This is a factual note, not legal advice; check your own situation if you distribute the AppImage.
- **Runtime dependency:** the viewer now links `libstdc++`/`libgcc_s` dynamically (only `GLIBCXX_3.4`, present on practically every desktop). Minimum glibc stays 2.35.

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
| `WIVRN_MONITOR_VIDEO_DIR` | Recording folder instead of `~/Videos/VR` (tests) |

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
