//! The egui interface.
//!
//! The window repaints only when something happens: while the engine is
//! running (to tick the counters), while a countdown is on screen, and when
//! the engine reports that a run started, stopped, or a point was picked. An
//! idle window costs nothing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke, Ui};

use crate::config::{
    self, ClickStyle, JitterMode, MouseButton, PositionMode, Settings, Theme, TimingMode,
};
use crate::engine::{ScreenRect, Status, WindowShield, Worker};
use crate::hotkeys::{self, Action, Code, Gate, Hotkeys};

#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowExtWayland;

const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
const RUNNING: Color32 = Color32::from_rgb(0x4A, 0xD0, 0x8C);
const DANGER: Color32 = Color32::from_rgb(0xE5, 0x6B, 0x6B);
const WARNING: Color32 = Color32::from_rgb(0xE8, 0xB3, 0x39);

/// Quiet period after an edit before settings are written to disk.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
const LABEL_WIDTH: f32 = 104.0;
/// Grace period after pressing "Pick", so the pointer can be moved onto the target.
const PICK_COUNTDOWN: Duration = Duration::from_secs(3);

pub struct App {
    settings: Settings,
    /// Snapshot read by the hotkey notifier thread.
    shared: Arc<Mutex<Settings>>,
    worker: Worker,
    hotkeys: Option<Hotkeys>,
    hotkey_error: Option<String>,
    /// Set when the file on disk could not be used. Cleared after a successful save.
    load_error: Option<String>,
    /// The file on disk is not the settings in memory. Copy it aside before replacing it.
    preserve_original: bool,
    /// A save just failed. Wait for the next edit or for the window to close
    /// instead of retrying on every repaint.
    hold_save: bool,
    /// Binding currently waiting for the user to press a key.
    capturing: Option<Action>,
    status: Status,
    seen_capture: u64,
    pick_at: Option<Instant>,
    settings_dirty: bool,
    changed_at: Instant,
    applied_theme: Option<Theme>,
    applied_on_top: Option<bool>,
    /// Last window bounds sent to the engine. Compared so an idle frame does
    /// not enqueue the same rectangle again.
    sent_shield: Option<WindowShield>,
    save_error: Option<String>,
    /// Keeps the Wayland icon buffers alive for the life of the window.
    #[cfg(target_os = "linux")]
    wayland_icon: Option<crate::wayland_icon::Session>,
    #[cfg(target_os = "linux")]
    wayland_icon_tried: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, loaded: config::LoadOutcome) -> Self {
        let preserve_original = loaded.error.is_some();
        let config::LoadOutcome {
            settings,
            error: load_error,
        } = loaded;
        let shared = Arc::new(Mutex::new(settings.clone()));

        // Windows, macOS, and X11 grabs must be created on the event-loop
        // thread. `App::new` runs on it.
        let (hotkeys, creation_error) = match Hotkeys::new() {
            Ok(manager) => (Some(manager), None),
            Err(err) => (None, Some(err)),
        };

        apply_style(&cc.egui_ctx);
        install_visuals(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let worker = Worker::spawn(move || ctx.request_repaint());
        spawn_notifier(cc.egui_ctx.clone(), worker.clone(), Arc::clone(&shared));

        let mut app = Self {
            settings,
            shared,
            worker,
            hotkeys,
            hotkey_error: creation_error,
            load_error,
            preserve_original,
            hold_save: false,
            capturing: None,
            status: Status::default(),
            seen_capture: 0,
            pick_at: None,
            settings_dirty: false,
            changed_at: Instant::now(),
            applied_theme: None,
            applied_on_top: None,
            sent_shield: None,
            save_error: None,
            #[cfg(target_os = "linux")]
            wayland_icon: None,
            #[cfg(target_os = "linux")]
            wayland_icon_tried: false,
        };
        app.register_hotkeys();
        app
    }

    /// Called once per frame when anything was edited.
    fn touch(&mut self) {
        self.settings_dirty = true;
        self.hold_save = false;
        self.changed_at = Instant::now();
        *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = self.settings.clone();
    }

    fn register_hotkeys(&mut self) {
        let Some(manager) = self.hotkeys.as_mut() else {
            return;
        };
        let toggle = hotkeys::parse(&self.settings.hotkey_toggle);
        let capture = hotkeys::parse(&self.settings.hotkey_capture);
        let mut problem = manager.apply(toggle, capture);
        if problem.is_none() {
            // A hand-edited config can hold text no backend understands; say so
            // instead of silently ignoring the binding.
            let binding = [&self.settings.hotkey_toggle, &self.settings.hotkey_capture]
                .into_iter()
                .find(|text| hotkeys::invalid(text));
            problem = binding.map(|text| format!("not a valid hotkey: {text}"));
        }
        self.hotkey_error = problem;
    }

    fn begin_capture(&mut self, target: Action) {
        self.capturing = Some(target);
        // Keep the current bindings from firing while the user presses their replacement.
        if let Some(manager) = self.hotkeys.as_mut() {
            manager.apply(None, None);
        }
    }

    fn end_capture(&mut self, binding: Option<String>) {
        let Some(target) = self.capturing.take() else {
            return;
        };
        if let Some(binding) = binding {
            match target {
                Action::Toggle => self.settings.hotkey_toggle = binding,
                Action::Capture => self.settings.hotkey_capture = binding,
            }
            self.touch();
        }
        self.register_hotkeys();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.status = self.worker.status();

        // A captured point always becomes the click position.
        if self.status.capture_seq != self.seen_capture {
            self.seen_capture = self.status.capture_seq;
            if let Some(point) = self.status.captured {
                self.settings.point = point;
                self.settings.position = PositionMode::Fixed;
                self.touch();
            }
        }

        if let Some(at) = self.pick_at {
            if Instant::now() >= at {
                self.pick_at = None;
                self.worker.capture_cursor();
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        if self.capturing.is_some() {
            self.capture_key(ctx);
        }

        if self.status.running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Reads the next key combination pressed while the window has focus.
    fn capture_key(&mut self, ctx: &egui::Context) {
        let mut chosen = hotkeys::CaptureResult::Ignore;
        ctx.input(|input| {
            let events: Vec<hotkeys::RawEvent> =
                input.events.iter().filter_map(raw_event).collect();
            let start = hotkeys::sides_at_frame_start(end_sides(input), &events);
            chosen = hotkeys::replay_capture(start, &events);
        });
        match chosen {
            hotkeys::CaptureResult::Cancel => self.end_capture(None),
            hotkeys::CaptureResult::Binding(binding) => self.end_capture(Some(binding)),
            hotkeys::CaptureResult::Ignore => {}
        }
    }

    /// Tells the engine where this window is, once that changes.
    fn publish_window(&mut self, ctx: &egui::Context) {
        let shield = window_shield(ctx);
        if self.sent_shield != Some(shield) {
            self.sent_shield = Some(shield);
            self.worker.set_window(shield);
        }
    }

    /// One attempt. X11 and Windows already get the icon from `with_icon`.
    #[cfg(target_os = "linux")]
    fn publish_wayland_icon(&mut self, frame: &eframe::Frame) {
        if self.wayland_icon_tried {
            return;
        }
        let Some(window) = frame.winit_window() else {
            return;
        };
        let Ok(display) = window.display_handle() else {
            self.wayland_icon_tried = true;
            return;
        };
        let RawDisplayHandle::Wayland(wayland) = display.as_raw() else {
            self.wayland_icon_tried = true;
            return;
        };
        // The toplevel exists once the window does. A missing one means the
        // shell role is not assigned yet, so try again next frame.
        let Some(toplevel) = window.xdg_toplevel() else {
            return;
        };
        let icon = crate::icon::data();
        if let Ok(session) = crate::wayland_icon::publish(
            wayland.display,
            toplevel,
            &icon.rgba,
            icon.width,
            icon.height,
        ) {
            self.wayland_icon = Some(session);
            window.request_redraw();
        }
        self.wayland_icon_tried = true;
    }

    fn apply_window(&mut self, ctx: &egui::Context) {
        if self.applied_theme != Some(self.settings.theme) {
            ctx.set_theme(match self.settings.theme {
                Theme::Dark => egui::ThemePreference::Dark,
                Theme::Light => egui::ThemePreference::Light,
            });
            self.applied_theme = Some(self.settings.theme);
        }
        if self.applied_on_top != Some(self.settings.always_on_top) {
            let level = if self.settings.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            self.applied_on_top = Some(self.settings.always_on_top);
        }
    }

    fn flush(&mut self, ctx: &egui::Context) {
        if self.hold_save {
            return;
        }
        match next_save(self.settings_dirty, self.changed_at, Instant::now()) {
            SaveWhen::Idle => {}
            SaveWhen::After(delay) => ctx.request_repaint_after(delay),
            SaveWhen::Now => {
                // The banner is painted before this runs, so a change in either
                // error has to schedule another frame to become visible.
                match self.write_settings() {
                    Ok(()) => {
                        let painted_error = self.load_error.is_some() || self.save_error.is_some();
                        self.note_saved();
                        if painted_error {
                            ctx.request_repaint();
                        }
                    }
                    Err(err) => {
                        self.save_error = Some(format!("cannot save settings: {err}"));
                        self.hold_save = true;
                        ctx.request_repaint();
                    }
                }
            }
        }
    }

    /// Writes immediately, ignoring the debounce. Used when the window is going away.
    fn persist_now(&mut self) {
        if !self.settings_dirty {
            return;
        }
        if let Err(err) = self.write_settings() {
            self.save_error = Some(format!("cannot save settings: {err}"));
            crate::platform::attach_console();
            eprintln!("featherclick: cannot save settings: {err}");
        } else {
            self.note_saved();
        }
    }

    fn write_settings(&self) -> std::io::Result<()> {
        if self.preserve_original {
            config::save_with_backup(&self.settings)
        } else {
            config::save(&self.settings)
        }
    }

    fn note_saved(&mut self) {
        self.settings_dirty = false;
        self.preserve_original = false;
        self.save_error = None;
        self.load_error = None;
    }

    fn header(&mut self, ui: &mut Ui) {
        let waiting = arming_left(
            self.status.running,
            self.status.clicking.is_some(),
            self.status.arm_until,
            Instant::now(),
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new("FeatherClick").size(17.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, colour) = self.status_pill(waiting.is_some());
                Frame::NONE
                    .fill(colour.gamma_multiply(0.16))
                    .stroke(Stroke::new(1.0, colour.gamma_multiply(0.45)))
                    .corner_radius(CornerRadius::same(20))
                    .inner_margin(Margin::symmetric(9, 3))
                    .show(ui, |ui| {
                        ui.label(RichText::new(text).size(11.0).color(colour).strong());
                    });
            });
        });

        ui.add_space(10.0);

        let running = self.status.running;
        let (label, fill) = if running {
            ("Stop", DANGER)
        } else {
            ("Start", ACCENT)
        };
        let button = egui::Button::new(
            RichText::new(label)
                .size(16.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(ui.available_width(), 40.0));
        if ui.add(button).clicked() {
            if running {
                self.worker.stop();
            } else {
                self.worker.start(&self.settings);
            }
        }

        ui.add_space(6.0);
        let line = stats_line(
            self.status.clicks,
            self.status.elapsed().unwrap_or_default(),
            waiting,
        );
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(line)
                    .monospace()
                    .size(11.5)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut hints = Vec::new();
                if running {
                    hints.push("changes apply on the next start".to_string());
                } else if self.hotkey_error.is_none()
                    && !self.settings.hotkey_toggle.trim().is_empty()
                {
                    hints.push(format!(
                        "{} toggles",
                        hotkeys::pretty(&self.settings.hotkey_toggle)
                    ));
                }
                ui.label(
                    RichText::new(hints.join("  ·  "))
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });

        if let Some(error) = self
            .status
            .error
            .clone()
            .or_else(|| self.save_error.clone())
        {
            ui.add_space(6.0);
            banner(ui, &error, DANGER);
        }
        if let Some(error) = &self.load_error {
            ui.add_space(6.0);
            banner(ui, error, WARNING);
        }
        if self.status.error.is_none()
            && self.save_error.is_none()
            && let Some(error) = &self.hotkey_error
        {
            ui.add_space(6.0);
            banner(ui, error, WARNING);
        }
    }

    fn status_pill(&self, waiting: bool) -> (String, Color32) {
        if self.status.error.is_some() {
            return ("Error".to_string(), DANGER);
        }
        if waiting {
            return ("Waiting".to_string(), ACCENT);
        }
        if self.status.running {
            return ("Running".to_string(), RUNNING);
        }
        match &self.status.stop_reason {
            Some(reason) if reason.is_error() => (reason.describe(), DANGER),
            Some(reason) => (reason.describe(), ACCENT),
            None => ("Idle".to_string(), ui_muted()),
        }
    }

    fn clicking(&mut self, ui: &mut Ui) {
        section(ui, "Clicking", |ui| {
            row(ui, "Button", |ui| {
                egui::ComboBox::from_id_salt("button")
                    .selected_text(self.settings.button.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for option in MouseButton::ALL {
                            ui.selectable_value(&mut self.settings.button, option, option.label());
                        }
                    });
            });
            row(ui, "Style", |ui| {
                egui::ComboBox::from_id_salt("style")
                    .selected_text(self.settings.style.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for option in ClickStyle::ALL {
                            ui.selectable_value(&mut self.settings.style, option, option.label());
                        }
                    });
                if self.settings.style == ClickStyle::Hold {
                    ui.add(
                        egui::DragValue::new(&mut self.settings.hold_ms)
                            .range(1..=60_000)
                            .suffix(" ms hold"),
                    );
                }
            });
        });
    }

    fn timing(&mut self, ui: &mut Ui) {
        section(ui, "Timing", |ui| {
            row(ui, "Mode", |ui| {
                egui::ComboBox::from_id_salt("timing")
                    .selected_text(self.settings.timing.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for option in TimingMode::ALL {
                            ui.selectable_value(&mut self.settings.timing, option, option.label());
                        }
                    });
            });
            match self.settings.timing {
                TimingMode::Fixed => {
                    row(ui, "Interval", |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.settings.interval_ms)
                                .range(1..=600_000)
                                .suffix(" ms"),
                        );
                    });
                }
                TimingMode::Uniform => {
                    row(ui, "Min / max", |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.settings.interval_min_ms)
                                .range(1..=600_000),
                        );
                        ui.add(
                            egui::DragValue::new(&mut self.settings.interval_max_ms)
                                .range(1..=600_000)
                                .suffix(" ms"),
                        );
                    });
                }
                TimingMode::Normal => {
                    row(ui, "Mean / sigma", |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.settings.interval_mean_ms)
                                .range(1..=600_000),
                        );
                        ui.add(
                            egui::DragValue::new(&mut self.settings.interval_std_ms)
                                .range(0..=600_000)
                                .suffix(" ms"),
                        );
                    });
                }
            }
            row(ui, "Start delay", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.settings.start_delay_ms)
                        .range(0..=3_600_000)
                        .suffix(" ms"),
                );
            });
            hint(ui, &rate_text(&self.settings));
        });
    }

    fn position(&mut self, ui: &mut Ui) {
        section(ui, "Position", |ui| {
            row(ui, "Target", |ui| {
                ui.radio_value(&mut self.settings.position, PositionMode::Cursor, "Pointer");
                ui.radio_value(&mut self.settings.position, PositionMode::Fixed, "Fixed");
            });
            if self.settings.position == PositionMode::Fixed {
                row(ui, "X / Y", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.settings.point.0).range(-20_000..=20_000),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.settings.point.1).range(-20_000..=20_000),
                    );
                    if ui
                        .button("Pick")
                        .on_hover_text("Waits 3 s, then takes the pointer position")
                        .clicked()
                    {
                        self.pick_at = Some(Instant::now() + PICK_COUNTDOWN);
                    }
                });
            }
            match self.pick_at {
                Some(at) => hint(
                    ui,
                    &format!(
                        "Capturing in {:.0}…",
                        at.saturating_duration_since(Instant::now()).as_secs_f64()
                    ),
                ),
                None => hint(
                    ui,
                    if self.settings.position == PositionMode::Cursor {
                        "Clicks wherever the pointer is."
                    } else {
                        "Returns to the point before every click."
                    },
                ),
            }

            row(ui, "Jitter", |ui| {
                egui::ComboBox::from_id_salt("jitter")
                    .selected_text(self.settings.jitter.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for option in JitterMode::ALL {
                            ui.selectable_value(&mut self.settings.jitter, option, option.label());
                        }
                    });
            });
            if self.settings.jitter != JitterMode::Off {
                let labels = match self.settings.jitter {
                    JitterMode::Rectangle => ("± X", "± Y"),
                    JitterMode::Ellipse => ("Radius X", "Radius Y"),
                    _ => ("Sigma X", "Sigma Y"),
                };
                row(ui, labels.0, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.settings.jitter_x)
                            .range(0..=10_000)
                            .suffix(" px"),
                    );
                });
                row(ui, labels.1, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.settings.jitter_y)
                            .range(0..=10_000)
                            .suffix(" px"),
                    );
                });
                let note = match (self.settings.position, self.settings.jitter) {
                    (PositionMode::Cursor, _) => {
                        "Measured from where the pointer was when you started."
                    }
                    (_, JitterMode::Ellipse) => "Uniform over the ellipse.",
                    (_, JitterMode::Normal) => "Gaussian; 3 sigma covers the box.",
                    _ => "Uniform over the box.",
                };
                hint(ui, note);
            }
        });
    }

    fn limits(&mut self, ui: &mut Ui) {
        section(ui, "Limits", |ui| {
            row(ui, "Max clicks", |ui| {
                ui.add(egui::DragValue::new(&mut self.settings.max_clicks).range(0..=100_000_000));
                ui.label(
                    RichText::new("0 = unlimited")
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
            row(ui, "Time limit", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.settings.max_seconds)
                        .range(0..=86_400)
                        .suffix(" s"),
                );
                ui.label(
                    RichText::new("0 = unlimited")
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
            row(ui, "Stop on move", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.settings.stop_on_move_px)
                        .range(0..=10_000)
                        .suffix(" px"),
                );
                ui.label(
                    RichText::new("0 = off")
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
    }

    fn hotkey_bindings(&mut self, ui: &mut Ui) {
        section(ui, "Hotkeys", |ui| {
            let capturing = self.capturing;
            for (target, label) in [
                (Action::Toggle, "Start / stop"),
                (Action::Capture, "Pick position"),
            ] {
                let current = match target {
                    Action::Toggle => self.settings.hotkey_toggle.clone(),
                    Action::Capture => self.settings.hotkey_capture.clone(),
                };
                row(ui, label, |ui| {
                    let text = match capturing {
                        Some(active) if active == target => "press a key…".to_string(),
                        _ if current.trim().is_empty() => "not set".to_string(),
                        _ => hotkeys::pretty(&current),
                    };
                    if ui.button(text).clicked() {
                        self.begin_capture(target);
                    }
                    if !current.trim().is_empty()
                        && ui.small_button("×").on_hover_text("Clear").clicked()
                    {
                        match target {
                            Action::Toggle => self.settings.hotkey_toggle.clear(),
                            Action::Capture => self.settings.hotkey_capture.clear(),
                        }
                        self.register_hotkeys();
                    }
                });
            }
            hint(
                ui,
                "Click a binding, then press the combination. Esc cancels.",
            );
        });
    }

    fn footer(&mut self, ui: &mut Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .button(match self.settings.theme {
                    Theme::Dark => "Light theme",
                    Theme::Light => "Dark theme",
                })
                .clicked()
            {
                self.settings.theme = match self.settings.theme {
                    Theme::Dark => Theme::Light,
                    Theme::Light => Theme::Dark,
                };
            }
            ui.checkbox(&mut self.settings.always_on_top, "Always on top");
        });
        ui.add_space(4.0);
        let config = config::config_path();
        ui.label(
            RichText::new(format!(
                "{}  ·  settings: {}",
                crate::version(),
                config.display()
            ))
            .size(10.5)
            .color(ui.visuals().weak_text_color()),
        )
        .on_hover_text("FeatherClick is MIT licensed. Settings are saved automatically.");
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        self.publish_wayland_icon(frame);
        #[cfg(not(target_os = "linux"))]
        let _ = frame;

        let ctx = ui.ctx().clone();
        self.apply_window(&ctx);
        // Before the Start button is handled, so the worker has the rectangle
        // queued ahead of the command that would click it.
        self.publish_window(&ctx);
        self.poll(&ctx);

        let before = self.settings.clone();
        Frame::NONE
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                self.header(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.clicking(ui);
                        self.timing(ui);
                        self.position(ui);
                        self.limits(ui);
                        self.hotkey_bindings(ui);
                        self.footer(ui);
                        ui.add_space(6.0);
                    });
            });

        if self.settings != before {
            self.touch();
        }
        self.flush(&ctx);
        if ctx.input(|input| input.viewport().close_requested()) {
            self.persist_now();
            self.worker.shutdown();
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.persist_now();
        self.worker.shutdown();
    }
}

/// When the debounced settings write should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveWhen {
    Idle,
    Now,
    After(Duration),
}

fn next_save(dirty: bool, changed_at: Instant, now: Instant) -> SaveWhen {
    if !dirty {
        return SaveWhen::Idle;
    }
    let elapsed = now.saturating_duration_since(changed_at);
    if elapsed >= SAVE_DEBOUNCE {
        SaveWhen::Now
    } else {
        SaveWhen::After(SAVE_DEBOUNCE - elapsed)
    }
}

fn ui_muted() -> Color32 {
    Color32::from_gray(140)
}

/// Time left before the first click. Stays `Some` — including zero — until
/// clicking actually starts, so a late wake cannot flash the delay as if it
/// were already part of the run.
fn arming_left(
    running: bool,
    clicking: bool,
    arm_until: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    if !running || clicking {
        return None;
    }
    arm_until.map(|until| until.saturating_duration_since(now))
}

fn stats_line(clicks: u64, elapsed: Duration, arming: Option<Duration>) -> String {
    if let Some(left) = arming {
        return format!(
            "{clicks} clicks  ·  starting in {:.1} s",
            left.as_secs_f64()
        );
    }
    let mut line = format!("{clicks} clicks");
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        line += &format!("  ·  {secs:.1} s");
        if secs > 0.25 {
            line += &format!("  ·  {:.1}/s", clicks as f64 / secs);
        }
    }
    line
}

/// A titled group box.
fn section(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(9.0);
    ui.label(
        RichText::new(title.to_uppercase())
            .size(10.0)
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
    Frame::group(ui.style())
        .inner_margin(Margin::symmetric(10, 8))
        .corner_radius(CornerRadius::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// One `label: controls` line.
fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2(LABEL_WIDTH, 20.0),
            egui::Label::new(RichText::new(label).size(12.0)),
        );
        add(ui);
    });
}

/// Rate line under the timing controls.
///
/// Uniform and fixed modes show the interval bounds. A Gaussian is shown at
/// its mean: the ±4 sigma window is a clamp, and with the default sigma it
/// runs down to the 1 ms floor, which would read as 1000 clicks/s.
fn rate_text(settings: &Settings) -> String {
    let mut settings = settings.clone();
    settings.sanitize();
    if settings.timing == TimingMode::Normal {
        let mean = (settings.interval_mean_ms as f64).max(1.0);
        return format!("{:.1} clicks/s", 1000.0 / mean);
    }
    let (low, high) = settings.rate_range();
    if (high - low).abs() < 0.05 {
        format!("{low:.1} clicks/s")
    } else {
        format!("{low:.1} – {high:.1} clicks/s")
    }
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
    );
}

fn banner(ui: &mut Ui, text: &str, colour: Color32) {
    Frame::NONE
        .fill(colour.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, colour.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).size(11.5).color(colour));
        });
}

fn apply_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style.spacing.interact_size.y = 22.0;
    });
}

/// Installs the palette for both themes; the setting then just picks one.
fn install_visuals(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, styled_visuals(Theme::Dark));
    ctx.set_visuals_of(egui::Theme::Light, styled_visuals(Theme::Light));
}

fn styled_visuals(theme: Theme) -> egui::Visuals {
    let mut visuals = match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    };
    if theme == Theme::Dark {
        visuals.panel_fill = Color32::from_rgb(0x10, 0x12, 0x17);
        visuals.window_fill = visuals.panel_fill;
        visuals.extreme_bg_color = Color32::from_rgb(0x0A, 0x0B, 0x0F);
        visuals.faint_bg_color = Color32::from_rgb(0x18, 0x1B, 0x22);
    }
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.75);
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.9);
    visuals
}

/// Bridges global hotkey events to engine commands. Runs on its own thread so
/// a press is handled even when the window is idle and not painting.
fn spawn_notifier(ctx: egui::Context, worker: Worker, shared: Arc<Mutex<Settings>>) {
    let spawned = std::thread::Builder::new()
        .name("featherclick-hotkeys".to_string())
        .spawn(move || {
            let mut gate = Gate::new();
            while let Ok(event) = hotkeys::recv_event() {
                if !gate.accept(event.id, event.pressed, Instant::now()) {
                    continue;
                }
                let snapshot = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if hotkeys::matches(event.id, &snapshot.hotkey_toggle) {
                    worker.toggle(&snapshot);
                } else if hotkeys::matches(event.id, &snapshot.hotkey_capture) {
                    worker.capture_cursor();
                }
                ctx.request_repaint();
            }
        });
    let _ = spawned;
}

/// Modifier sides at the end of the frame. Super comes from the key itself:
/// off macOS, `modifiers.mac_cmd` stays false while the Windows or Super key
/// is held, and a binding that dropped it would grab the bare key.
fn end_sides(input: &egui::InputState) -> hotkeys::Sides {
    let super_l = input.key_down(egui::Key::SuperLeft);
    let super_r = input.key_down(egui::Key::SuperRight);
    hotkeys::Sides {
        ctrl_l: input.key_down(egui::Key::ControlLeft),
        ctrl_r: input.key_down(egui::Key::ControlRight),
        shift_l: input.key_down(egui::Key::ShiftLeft),
        shift_r: input.key_down(egui::Key::ShiftRight),
        alt_l: input.key_down(egui::Key::AltLeft),
        alt_r: input.key_down(egui::Key::AltRight),
        super_l: super_l || (input.modifiers.mac_cmd && !super_l && !super_r),
        super_r,
    }
}

fn side_of(key: egui::Key) -> Option<hotkeys::Side> {
    Some(match key {
        egui::Key::ControlLeft => hotkeys::Side::CtrlL,
        egui::Key::ControlRight => hotkeys::Side::CtrlR,
        egui::Key::ShiftLeft => hotkeys::Side::ShiftL,
        egui::Key::ShiftRight => hotkeys::Side::ShiftR,
        egui::Key::AltLeft => hotkeys::Side::AltL,
        egui::Key::AltRight => hotkeys::Side::AltR,
        egui::Key::SuperLeft => hotkeys::Side::SuperL,
        egui::Key::SuperRight => hotkeys::Side::SuperR,
        _ => return None,
    })
}

fn raw_event(event: &egui::Event) -> Option<hotkeys::RawEvent> {
    match event {
        egui::Event::Key {
            key,
            pressed,
            repeat,
            modifiers,
            ..
        } => {
            if let Some(side) = side_of(*key) {
                return Some(hotkeys::RawEvent::Side {
                    side,
                    pressed: *pressed,
                    repeat: *repeat,
                });
            }
            Some(hotkeys::RawEvent::Key {
                code: key_to_code(*key),
                escape: *key == egui::Key::Escape,
                pressed: *pressed,
                repeat: *repeat,
                mods: hotkeys::HeldMods {
                    ctrl: modifiers.ctrl,
                    shift: modifiers.shift,
                    alt: modifiers.alt,
                    super_key: modifiers.mac_cmd,
                },
            })
        }
        egui::Event::Copy => Some(hotkeys::RawEvent::Clipboard(hotkeys::Clipboard::Copy)),
        egui::Event::Cut => Some(hotkeys::RawEvent::Clipboard(hotkeys::Clipboard::Cut)),
        egui::Event::Paste(_) => Some(hotkeys::RawEvent::Clipboard(hotkeys::Clipboard::Paste)),
        _ => None,
    }
}

/// Maps an egui key to a physical key code for the global hotkey backends.
/// Modifier keys map to nothing: a binding needs a real key.
fn key_to_code(key: egui::Key) -> Option<Code> {
    use egui::Key as K;
    Some(match key {
        K::A => Code::KeyA,
        K::B => Code::KeyB,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::E => Code::KeyE,
        K::F => Code::KeyF,
        K::G => Code::KeyG,
        K::H => Code::KeyH,
        K::I => Code::KeyI,
        K::J => Code::KeyJ,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::M => Code::KeyM,
        K::N => Code::KeyN,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::Q => Code::KeyQ,
        K::R => Code::KeyR,
        K::S => Code::KeyS,
        K::T => Code::KeyT,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Num0 => Code::Digit0,
        K::Num1 => Code::Digit1,
        K::Num2 => Code::Digit2,
        K::Num3 => Code::Digit3,
        K::Num4 => Code::Digit4,
        K::Num5 => Code::Digit5,
        K::Num6 => Code::Digit6,
        K::Num7 => Code::Digit7,
        K::Num8 => Code::Digit8,
        K::Num9 => Code::Digit9,
        K::F1 => Code::F1,
        K::F2 => Code::F2,
        K::F3 => Code::F3,
        K::F4 => Code::F4,
        K::F5 => Code::F5,
        K::F6 => Code::F6,
        K::F7 => Code::F7,
        K::F8 => Code::F8,
        K::F9 => Code::F9,
        K::F10 => Code::F10,
        K::F11 => Code::F11,
        K::F12 => Code::F12,
        K::F13 => Code::F13,
        K::F14 => Code::F14,
        K::F15 => Code::F15,
        K::F16 => Code::F16,
        K::F17 => Code::F17,
        K::F18 => Code::F18,
        K::F19 => Code::F19,
        K::F20 => Code::F20,
        K::F21 => Code::F21,
        K::F22 => Code::F22,
        K::F23 => Code::F23,
        K::F24 => Code::F24,
        K::ArrowUp => Code::ArrowUp,
        K::ArrowDown => Code::ArrowDown,
        K::ArrowLeft => Code::ArrowLeft,
        K::ArrowRight => Code::ArrowRight,
        K::Escape => Code::Escape,
        K::Tab => Code::Tab,
        K::Backspace => Code::Backspace,
        K::Enter => Code::Enter,
        K::Space => Code::Space,
        K::Insert => Code::Insert,
        K::Delete => Code::Delete,
        K::Home => Code::Home,
        K::End => Code::End,
        K::PageUp => Code::PageUp,
        K::PageDown => Code::PageDown,
        K::Minus => Code::Minus,
        K::Plus | K::Equals => Code::Equal,
        K::Comma => Code::Comma,
        K::Period => Code::Period,
        K::Slash => Code::Slash,
        K::Backslash | K::IntlBackslash => Code::Backslash,
        K::Semicolon | K::Colon => Code::Semicolon,
        K::Quote => Code::Quote,
        K::Backtick => Code::Backquote,
        K::OpenBracket => Code::BracketLeft,
        K::CloseBracket => Code::BracketRight,
        _ => return None,
    })
}

/// Converts a viewport rectangle from ui points into physical pixels, outset
/// by one pixel so rounding cannot leave the real edge clickable.
fn rect_from_points(
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    ppp: f32,
) -> Option<ScreenRect> {
    if ![min_x, min_y, max_x, max_y, ppp]
        .iter()
        .all(|value| value.is_finite())
        || ppp <= 0.0
    {
        return None;
    }
    let x0 = (min_x * ppp).floor() as i32;
    let y0 = (min_y * ppp).floor() as i32;
    let x1 = (max_x * ppp).ceil() as i32;
    let y1 = (max_y * ppp).ceil() as i32;
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some(ScreenRect {
        x: x0.saturating_sub(1),
        y: y0.saturating_sub(1),
        width: x1.saturating_sub(x0).saturating_add(2),
        height: y1.saturating_sub(y0).saturating_add(2),
    })
}

fn window_shield(ctx: &egui::Context) -> WindowShield {
    ctx.input(|input| {
        let rect = input.viewport().outer_rect.and_then(|rect| {
            rect_from_points(
                rect.min.x,
                rect.min.y,
                rect.max.x,
                rect.max.y,
                input.pixels_per_point,
            )
        });
        WindowShield {
            rect,
            pointer_over: input.pointer.hover_pos().is_some(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_from_points_scales_outsets_and_rejects_garbage() {
        let rect = rect_from_points(10.0, 20.0, 110.0, 220.0, 1.0).unwrap();
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (9, 19, 102, 202));

        let scaled = rect_from_points(10.0, 3.0, 14.5, 8.0, 2.0).unwrap();
        assert_eq!(
            (scaled.x, scaled.y, scaled.width, scaled.height),
            (19, 5, 11, 12)
        );

        let negative = rect_from_points(-10.5, -3.25, -1.5, 4.0, 1.0).unwrap();
        assert_eq!(
            (negative.x, negative.y, negative.width, negative.height),
            (-12, -5, 12, 10)
        );

        assert!(rect_from_points(0.0, 0.0, 10.0, 10.0, 0.0).is_none());
        assert!(rect_from_points(0.0, 0.0, 10.0, 10.0, -1.0).is_none());
        assert!(rect_from_points(f32::NAN, 0.0, 10.0, 10.0, 1.0).is_none());
        assert!(rect_from_points(0.0, 0.0, f32::INFINITY, 10.0, 1.0).is_none());
        assert!(rect_from_points(20.0, 0.0, 10.0, 10.0, 1.0).is_none());
    }

    #[test]
    fn the_delay_counts_down_until_clicking_starts() {
        let now = Instant::now();
        let until = now + Duration::from_millis(1_600);
        assert_eq!(
            arming_left(true, false, Some(until), now + Duration::from_millis(400)),
            Some(Duration::from_millis(1_200))
        );
        // The deadline can pass a slice before the engine says clicking began.
        // Keep the countdown, including zero, so the run clock does not jump.
        assert_eq!(
            arming_left(true, false, Some(now), now + Duration::from_millis(20)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            arming_left(true, true, Some(until), now),
            None,
            "clicking replaces the countdown"
        );
        assert_eq!(arming_left(false, false, Some(until), now), None);
        assert_eq!(arming_left(true, false, None, now), None);

        assert_eq!(
            stats_line(
                0,
                Duration::from_millis(400),
                Some(Duration::from_millis(1_200))
            ),
            "0 clicks  ·  starting in 1.2 s"
        );
        assert_eq!(
            stats_line(5, Duration::from_millis(500), None),
            "5 clicks  ·  0.5 s  ·  10.0/s"
        );
        assert_eq!(
            stats_line(0, Duration::from_millis(100), None),
            "0 clicks  ·  0.1 s"
        );
        assert_eq!(stats_line(0, Duration::ZERO, None), "0 clicks");
    }

    #[test]

    fn save_waits_out_the_debounce_then_fires() {
        let start = Instant::now();
        assert_eq!(
            next_save(false, start, start + SAVE_DEBOUNCE),
            SaveWhen::Idle
        );

        assert_eq!(
            next_save(true, start, start),
            SaveWhen::After(SAVE_DEBOUNCE)
        );

        let midway = start + Duration::from_millis(100);
        assert_eq!(
            next_save(true, start, midway),
            SaveWhen::After(SAVE_DEBOUNCE - Duration::from_millis(100))
        );

        assert_eq!(next_save(true, start, start + SAVE_DEBOUNCE), SaveWhen::Now);
        assert_eq!(
            next_save(true, start, start + SAVE_DEBOUNCE + Duration::from_secs(5)),
            SaveWhen::Now
        );
    }

    #[test]

    fn rate_line_uses_the_mean_for_human_like_timing() {
        let mut settings = Settings {
            timing: TimingMode::Normal,
            interval_mean_ms: 100,
            interval_std_ms: 25,
            ..Settings::default()
        };
        assert_eq!(
            rate_text(&settings),
            "10.0 clicks/s",
            "the default sigma reaches the 1 ms floor, which is not the pace"
        );

        settings.interval_mean_ms = 80;
        assert_eq!(rate_text(&settings), "12.5 clicks/s");

        settings.interval_mean_ms = 0;
        assert_eq!(rate_text(&settings), "1000.0 clicks/s");
    }

    #[test]
    fn rate_line_keeps_fixed_and_uniform_bounds() {
        let mut settings = Settings {
            timing: TimingMode::Fixed,
            interval_ms: 100,
            ..Settings::default()
        };
        assert_eq!(rate_text(&settings), "10.0 clicks/s");

        settings.timing = TimingMode::Uniform;
        settings.interval_min_ms = 40;
        settings.interval_max_ms = 120;
        assert_eq!(rate_text(&settings), "8.3 – 25.0 clicks/s");

        settings.interval_min_ms = 200;
        assert_eq!(rate_text(&settings), "5.0 – 8.3 clicks/s");
    }
}
