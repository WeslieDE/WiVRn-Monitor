//! WiVRn-Monitor viewer: shows the right eye mirrored by the Vulkan server layer (inside wivrn-server).
//! Listens on `~/.local/share/wivrn-monitor/monitor.sock`; the layer connects.
//! UI: egui via eframe (glow, Wayland + X11), menu bar on top, status bar at the bottom.

mod layer_install;
mod net;

use eframe::egui::{self, Color32, RichText};
use layer_install::{Bundle, LayerState};
use net::{FrameBuf, Shared, Wake};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wivrn_monitor_protocol as proto;

const STALE_AFTER: Duration = Duration::from_secs(2);
const STATUS_REFRESH: Duration = Duration::from_secs(2);
const RESTART_HINT: &str = "Bitte wivrn-server neu starten, damit die Änderung wirksam wird (wird nicht automatisch gemacht).";

const OK: Color32 = Color32::from_rgb(0x5C, 0xC0, 0x6E);
const WARN: Color32 = Color32::from_rgb(0xE0, 0xA0, 0x30);
const BAD: Color32 = Color32::from_rgb(0xD8, 0x5A, 0x50);
const DIM: Color32 = Color32::from_rgb(0x9A, 0xA0, 0xAA);

struct Status {
    layer: LayerState,
    legacy: bool,
    server_running: bool,
    checked: Instant,
}

struct App {
    shared: Shared,
    bundle: Option<Bundle>,
    status: Status,
    texture: Option<egui::TextureHandle>,
    tex_seq: u64,
    show_fps_overlay: bool,
    /// (is_error, text) shown in a dialog.
    message: Option<(bool, String)>,
    restart_hint: bool,
    show_about: bool,
}

impl App {
    fn new(shared: Shared) -> Self {
        let bundle = layer_install::find_bundle();
        let status = Self::read_status(bundle.as_ref());
        App { shared, bundle, status, texture: None, tex_seq: 0, show_fps_overlay: true, message: None, restart_hint: false, show_about: false }
    }

    fn read_status(bundle: Option<&Bundle>) -> Status {
        Status {
            layer: layer_install::layer_state(bundle),
            legacy: layer_install::legacy_installed(),
            server_running: layer_install::wivrn_server_running(),
            checked: Instant::now(),
        }
    }

    fn refresh_status(&mut self) {
        self.status = Self::read_status(self.bundle.as_ref());
    }

    fn report(&mut self, what: &str, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.restart_hint = true;
                self.message = Some((false, format!("{what}\n\n{RESTART_HINT}")));
            }
            Err(e) => self.message = Some((true, format!("{what} fehlgeschlagen:\n{e}"))),
        }
        self.refresh_status();
    }

    /// Uploads the latest frame as texture if it changed. Returns (size, fps, age).
    fn sync_frame(&mut self, ctx: &egui::Context) -> FrameInfo {
        let f = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let info = FrameInfo {
            connected: f.connected,
            size: (f.seq > 0 && f.width > 0 && f.height > 0).then_some((f.width, f.height)),
            fps: f.fps(),
            age: f.last_frame.map(|t| t.elapsed()),
        };
        if info.size.is_none() {
            self.texture = None;
            self.tex_seq = 0;
        } else if f.seq != self.tex_seq && f.rgba.len() >= (f.width * f.height * 4) as usize {
            // Alpha of the ring is not meaningful: always opaque.
            let rgb: Vec<u8> = f.rgba.chunks(4).take((f.width * f.height) as usize).flat_map(|p| [p[0], p[1], p[2]]).collect();
            let img = egui::ColorImage::from_rgb([f.width as usize, f.height as usize], &rgb);
            match self.texture.as_mut() {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("frame", img, egui::TextureOptions::LINEAR)),
            }
            self.tex_seq = f.seq;
        }
        info
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Datei", |ui| {
                if ui.button("Layer installieren").clicked() {
                    let r = layer_install::install(self.bundle.as_ref());
                    self.report("Layer installiert.", r);
                    ui.close();
                }
                if ui.button("Layer deinstallieren").clicked() {
                    let r = layer_install::uninstall();
                    self.report("Layer deinstalliert.", r);
                    ui.close();
                }
                if self.status.legacy {
                    ui.separator();
                    if ui.button("Alten OpenXR-Layer entfernen").clicked() {
                        let r = layer_install::remove_legacy();
                        self.report("Alter OpenXR-Layer entfernt.", r);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("Beenden").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Ansicht", |ui| {
                ui.checkbox(&mut self.show_fps_overlay, "FPS-Overlay");
            });
            ui.menu_button("Hilfe", |ui| {
                if ui.button("Über").clicked() {
                    self.show_about = true;
                    ui.close();
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui, fi: &FrameInfo) {
        let layer_color = match self.status.layer {
            LayerState::Installed => OK,
            LayerState::Outdated(_) => WARN,
            LayerState::NotInstalled => BAD,
        };
        ui.horizontal(|ui| {
            dot(ui, layer_color, &format!("Layer: {}", self.status.layer.label()));
            ui.separator();
            let (c, t) = if self.status.server_running { (OK, "läuft") } else { (DIM, "läuft nicht") };
            dot(ui, c, &format!("wivrn-server: {t}"));
            ui.separator();
            let (c, t) = if fi.connected { (OK, "verbunden") } else { (WARN, "wartet") };
            dot(ui, c, &format!("Verbindung: {t}"));
            ui.separator();
            match (fi.size, fi.age) {
                (Some((w, h)), Some(age)) => {
                    let stale = age > STALE_AFTER;
                    let txt = if stale {
                        format!("Frames: {w}x{h}, letzter vor {:.0} s", age.as_secs_f32())
                    } else {
                        format!("Frames: {w}x{h}, {} FPS", fi.fps)
                    };
                    dot(ui, if stale { WARN } else { OK }, &txt);
                }
                _ => dot(ui, DIM, "Frames: keine"),
            }
        });
    }

    fn central(&self, ui: &mut egui::Ui, fi: &FrameInfo) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(0x10, 0x12, 0x16));
        let (Some(tex), Some((w, h))) = (&self.texture, fi.size) else {
            let text = if fi.connected { "Verbunden - warte auf Frames..." } else { "Warte auf VR-Session..." };
            let mut lines = text.to_string();
            if !fi.connected {
                lines.push_str(if !self.status.server_running {
                    "\nwivrn-server läuft nicht."
                } else if self.status.layer != LayerState::Installed {
                    "\nLayer ist nicht (aktuell) installiert: Menü Datei - Layer installieren."
                } else {
                    "\nStarte eine VR-Anwendung bzw. verbinde das Headset."
                });
            }
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, lines, egui::FontId::proportional(20.0), Color32::from_rgb(0xC8, 0xCC, 0xD4));
            return;
        };
        let (_, _, rw, rh) = proto::fit_rect(w, h, rect.width().max(1.0) as u32, rect.height().max(1.0) as u32);
        let img_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(rw as f32, rh as f32));
        ui.painter().image(tex.id(), img_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        if fi.age.is_some_and(|a| a > STALE_AFTER) {
            overlay(ui, rect.left_top() + egui::vec2(8.0, 8.0), egui::Align2::LEFT_TOP, "Keine neuen Frames", WARN);
        }
        if self.show_fps_overlay {
            overlay(ui, rect.right_top() + egui::vec2(-8.0, 8.0), egui::Align2::RIGHT_TOP, &format!("{} FPS  {w}x{h}", fi.fps), Color32::from_rgb(0xE6, 0xE8, 0xEC));
        }
    }
}

struct FrameInfo {
    connected: bool,
    size: Option<(u32, u32)>,
    fps: usize,
    age: Option<Duration>,
}

fn dot(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.label(RichText::new("●").color(color));
    ui.label(RichText::new(text).small());
}

fn overlay(ui: &egui::Ui, pos: egui::Pos2, anchor: egui::Align2, text: &str, color: Color32) {
    let p = ui.painter();
    let galley = p.layout_no_wrap(text.to_string(), egui::FontId::monospace(14.0), color);
    let r = anchor.anchor_size(pos, galley.size());
    p.rect_filled(r.expand(4.0), 3.0, Color32::from_black_alpha(150));
    p.galley(r.min, galley, color);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.status.checked.elapsed() >= STATUS_REFRESH {
            self.refresh_status();
        }
        let fi = self.sync_frame(&ctx);
        // Keep age/FPS/status fresh even without incoming frames.
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui));
        egui::Panel::bottom("status").show(ui, |ui| {
            if self.restart_hint {
                ui.label(RichText::new(format!("Hinweis: {RESTART_HINT}")).small().color(WARN));
            }
            self.status_bar(ui, &fi);
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.central(ui, &fi));

        if let Some((is_err, text)) = self.message.clone() {
            let mut open = true;
            egui::Window::new(if is_err { "Fehler" } else { "Layer" })
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(&ctx, |ui| {
                    ui.label(RichText::new(text).color(if is_err { BAD } else { Color32::PLACEHOLDER }));
                    if ui.button("OK").clicked() {
                        self.message = None;
                    }
                });
            if !open {
                self.message = None;
            }
        }
        if self.show_about {
            egui::Window::new("Über WiVRn Monitor").collapsible(false).resizable(false).open(&mut self.show_about).show(&ctx, |ui| {
                ui.label("Zeigt das rechte Auge einer WiVRn-Session.");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label(format!("Socket: {}", proto::socket_path().display()));
            });
        }
    }
}

/// Headless helpers for scripts; returns the exit code if a CLI flag was handled.
fn run_cli(args: &[String]) -> Option<i32> {
    let bundle = layer_install::find_bundle();
    let report = |what: &str, r: Result<(), String>| match r {
        Ok(()) => {
            println!("{what}: ok");
            0
        }
        Err(e) => {
            eprintln!("{what}: {e}");
            1
        }
    };
    match args.first().map(String::as_str)? {
        "--install-layer" => Some(report("install", layer_install::install(bundle.as_ref()))),
        "--uninstall-layer" => Some(report("uninstall", layer_install::uninstall())),
        "--layer-status" => {
            println!("{}", layer_install::layer_state(bundle.as_ref()).label());
            Some(0)
        }
        // install -> status must be "installiert" -> uninstall -> "nicht installiert"; use with WIVRN_MONITOR_HOME_OVERRIDE.
        "--self-test-install" => {
            let ok = (|| -> Result<(), String> {
                let b = bundle.as_ref();
                println!("bundle: {:?}", b.map(|b| &b.so));
                layer_install::install(b)?;
                let s = layer_install::layer_state(b);
                println!("after install: {}", s.label());
                if s != LayerState::Installed {
                    return Err("state after install is not 'installiert'".into());
                }
                layer_install::uninstall()?;
                let s = layer_install::layer_state(b);
                println!("after uninstall: {}", s.label());
                if s != LayerState::NotInstalled {
                    return Err("state after uninstall is not 'nicht installiert'".into());
                }
                Ok(())
            })();
            Some(report("self-test-install", ok))
        }
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = run_cli(&args) {
        std::process::exit(code);
    }
    let path = proto::socket_path();
    let listener = match proto::listen(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wivrn-monitor-viewer: cannot listen on {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let shared: Shared = Arc::new(Mutex::new(FrameBuf::default()));
    // 16:9 image area (1280x720) plus menu bar and status bar.
    let mut viewport = egui::ViewportBuilder::default()
            .with_title("WiVRn Monitor")
            .with_app_id("wivrn-monitor")
            .with_inner_size([1280.0, 776.0])
            .with_min_inner_size([480.0, 300.0])
            .with_visible(true);
    // Window icon (X11 / Wayland protocols that support it); the taskbar icon on Wayland comes from the .desktop file.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let net_shared = shared.clone();
    let r = eframe::run_native(
        "WiVRn Monitor",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let wake: Wake = Arc::new(move || ctx.request_repaint());
            std::thread::Builder::new().name("net".into()).spawn(move || net::run(listener, net_shared, wake)).expect("spawn net thread");
            Ok(Box::new(App::new(shared)))
        }),
    );
    let _ = std::fs::remove_file(&path);
    if let Err(e) = r {
        eprintln!("wivrn-monitor-viewer: {e}");
        std::process::exit(1);
    }
}
