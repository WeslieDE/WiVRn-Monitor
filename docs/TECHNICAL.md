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
  cropped by one pixel to even); sound is described under [Sound](#sound). Menu Record / F9; setting `auto_record` in `~/.local/share/wivrn-monitor/viewer.json`.
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
- **Runtime dependency:** the viewer links `libstdc++`/`libgcc_s` dynamically (only `GLIBCXX_3.4`, present on practically every desktop). Minimum glibc stays 2.35.

### Sound

Code: `viewer/src/recorder/audio/` (`capture.rs` devices, `device.rs` choice, `pace.rs` clock handling, `mix.rs`, `engine.rs`), the audio track
in `mp4.rs`, the glue in `worker.rs` / `mod.rs`. Settings `record_audio`, `audio_mic`, `audio_app` (all default `true`) in `viewer.json`.

- **What:** the microphone (mono, the default source) and the output of the default sink (its monitor: everything the apps play)
  are mixed into one stereo AAC-LC track, 48 kHz, 160 kbit/s CBR, in the same fragmented MP4 (track 2). The settings are read when a file is
  created; a running recording keeps its own (the menu is greyed out meanwhile).
- **Capture API:** the PulseAudio client API through `libpulse-simple` (`pa_simple_*`), loaded with `dlopen` at run time (`libloading`;
  no link-time dependency, no `-dev` package, nothing bundled). It works natively on PulseAudio and on PipeWire through `pipewire-pulse`, which
  every current desktop with audio has. Native PipeWire was not chosen because it would need `libpipewire` bindings (bindgen/clang or a
  much larger dependency) and has no advantage for a record-only stream. If the library cannot be loaded or no device opens, the recording
  continues as plain video and the status bar says "No sound in this recording: ..." (about 30 s). Streams are record-only (no
  routing, no volume or default changes); 20 ms fragments, requested as 48 kHz `float32` (the server converts). The microphone is requested as mono.
- **Device choice** (`device.rs`, runs on the audio thread): `WIVRN_MONITOR_AUDIO_SINK=<sink>` records `<sink>.monitor`, `WIVRN_MONITOR_AUDIO_SOURCE=<source>`
  the microphone; otherwise `pactl list short sinks|sources` (1.5 s limit, optional) is searched for a name containing `wivrn` (a source that is
  a `.monitor` is never taken as microphone); otherwise `@DEFAULT_MONITOR@` and the default source (the server resolves both). No sub-menu: the
  environment variables are enough for the rare case. The sink is chosen when the file is created; if the default output changes during
  a recording, the old one keeps being recorded.
- **Threads:** one capture thread per source (blocking reads of 10 ms into a mutex-protected buffer, only a lock and a memcpy), one `rec-audio`
  engine thread (mix + AAC), the existing `rec-encoder` thread muxes. AAC access units travel over an unbounded channel (16-20 kB/s). Nothing blocks the UI or net thread.
  A capture thread stuck inside the device is abandoned after 400 ms at the end of a recording (it ends by itself).
- **Mixer:** sum of microphone (both channels) and application sound, times 0.708 (-3 dB headroom), then a soft limiter: transparent up to 0.85 of full scale,
  tanh curve above, never reaches 1.0. A missing source counts as silence.
- **Sync:** one monotonic clock for both media. Timeline position 0 = arrival time `t0` of the first video frame (that is also video PTS 0); video
  PTS = arrival time of the frame (slots of 1/30 s, existing recorder); audio PTS = sample counter of the engine's output (`access unit k` = `k * 1024` samples, `tfdt`
  in a 48 kHz timescale). The engine renders the timeline 200 ms behind real time, for each 10 ms block reading every source at the position
  that belongs to that time: per source, a minimum filter (`pace::OffsetEstimator`, fast down, slow up) estimates `c0` = capture time of the source's frame 0 from
  (read time - the capture latency reported by `pa_simple_get_latency` - frames read / 48000). A reader (`pace::DriftReader`) follows `m - c0 * 48000` with linear interpolation and a
  proportional step of at most +-0.4 % (inaudible), so a device clock that is a few hundred ppm off is absorbed without clicks; above 100 ms error it jumps. Dropouts give silence, not a
  shifted track. The encoder delay (1024 samples) is declared with an edit list (`elst`, media time 1024), so players skip it; `finish` patches the exact
  length into the `moov`. Measured with synthetic devices (device clock +0.3 %, 45 ms latency): the tone starts within 3 ms of its true time and
  15 ms before the video event it belongs to (test `audio_and_video_start_together_and_stay_locked_with_a_drifting_device`). **Not measured:** the end-to-end offset with a real
  headset (the latency figure of the PulseAudio API is trusted) and the delay between VR rendering and the frame arriving at the viewer (same for the video alone).
- **Pauses:** without sound, pauses > 1 s are cut out of the video timeline (as before). That would shift the picture against the continuous sound, so **when sound is
  recorded the timeline is not cut**: a gap longer than 1 s is a long last frame (no re-encoding; the next picture continues from the previous one). After 5 s without frames
  the file is closed anyway (as before), and the sound ends with the last picture (+1/30 s). While no pictures arrive, audio-only fragments are still written once a second.
- **Container:** `moof` holds one `traf` per track that has data; fragments are written when 30 video samples (or 1 s of video timeline) or 47 audio units (~1 s)
  are pending, so a crash loses about 1 s of both. `mp4a` + `esds` (AAC LC, ASC `0x1190`), `smhd`, `trex` per track.
- **Encoder choice:** `filmcraft-aac` 0.9 (pure Rust, `MIT OR Apache-2.0`, clean-room, no `unsafe`, no C code, no build tools). Considered:
  (1) FDK-AAC (crate `fdk-aac` + bundled source or `libfdk-aac`): the best quality at this bitrate and a good encoder, but its licence
  (Fraunhofer FDK AAC Codec Library licence) is not OSI-approved, is not compatible with the GPL, and expressly grants no patent rights; it needs cmake and g++ in the
  container and CI and a bundled or system C++ library. (2) Pure Rust port of FDK (`fdk-aac-rs`): same licence. (3) Opus in MP4: free, but Safari,
  Windows' default players, older phones and many editors do not play it. (4) Other Rust AAC encoders (`oxideav-aac`) were not chosen after a look at filmcraft's API
  (ready `AudioSpecificConfig`, priming value, CBR, flush). Consequences: AAC-LC plays in browsers, VLC, mpv, ffmpeg, Windows, Android and iOS (checked here with ffmpeg, mpv and
  VLC); no build tools needed; glibc minimum stays 2.35; the AppImage is 5.7 MB in total. Costs: the crate is young and its
  quality at 160 kbit/s is below FDK (the bitrate is on the high side for that reason; music may show slight artefacts, speech and game sound are fine); at 2 % of a core it is cheap.
- **Licence / patents:** AAC is covered by patents that are licensed through a patent pool (Via Licensing); many of the oldest have expired, but not all. As with H.264 above
  this is a factual note, not legal advice: if you distribute the AppImage in a jurisdiction where this matters, check your own situation. The encoder source itself is permissively licensed.
- **Measured (this machine, 1280x720@30 + stereo sound from synthetic devices):** 25 % of one core in the benchmark process in total (23 % without sound, so about 2 % for sound);
  real devices: 8-10 % at 640x360. Size: 160 kbit/s = 1.2 MB/min on top of the video. The AppImage is 5.7 MB; the viewer needs glibc 2.35.
- **Tests:** unit tests (mixer, limiter, clock estimator/reader, device choice, MP4 boxes, counter to time, engine length), `viewer/tests/record_audio_e2e.rs`
  (synthetic 440 Hz "microphone" + 880 Hz "app sound" through the real recorder; `ffprobe`: h264 + aac, 48 kHz, 2 channels, durations; `ffmpeg` decode and
  single-frequency analysis for both tones, levels, A/V onsets, device failures, pauses, truncated files).

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
| `WIVRN_MONITOR_AUDIO_SINK` | Output (sink) whose sound is recorded, e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo` (its `.monitor` is used). Default: a sink with "wivrn" in its name, else the default sink |
| `WIVRN_MONITOR_AUDIO_SOURCE` | Microphone (source) to record. Default: a source with "wivrn" in its name, else the default source |

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
