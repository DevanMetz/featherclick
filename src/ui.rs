//! The egui interface.
//!
//! The window repaints only when something happens: while the engine is
//! running (to tick the counters) and while a countdown is on screen. An idle
//! window costs nothing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke, Ui};

use crate::config::{
    self, ClickStyle, JitterMode, MouseButton, PositionMode, Settings, Theme, TimingMode,
};
use crate::engine::{Status, Worker};
use crate::hotkeys::{self, Action, Code, Gate, GlobalHotKeyEvent, Hotkeys};

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
    /// Binding currently waiting for the user to press a key.
    capturing: Option<Action>,
    status: Status,
    seen_capture: u64,
    pick_at: Option<Instant>,
    settings_dirty: bool,
    changed_at: Instant,
    applied_theme: Option<Theme>,
    applied_on_top: Option<bool>,
    save_error: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings) -> Self {
        let worker = Worker::spawn();
        let shared = Arc::new(Mutex::new(settings.clone()));

        // Both hotkey backends require the thread that owns the event loop, and
        // `App::new` runs on it.
        let (hotkeys, creation_error) = match Hotkeys::new() {
            Ok(manager) => (Some(manager), None),
            Err(err) => (None, Some(err)),
        };

        apply_style(&cc.egui_ctx);
        install_visuals(&cc.egui_ctx);
        spawn_notifier(cc.egui_ctx.clone(), worker.clone(), Arc::clone(&shared));

        let mut app = Self {
            settings,
            shared,
            worker,
            hotkeys,
            hotkey_error: creation_error,
            capturing: None,
            status: Status::default(),
            seen_capture: 0,
            pick_at: None,
            settings_dirty: false,
            changed_at: Instant::now(),
            applied_theme: None,
            applied_on_top: None,
            save_error: None,
        };
        app.register_hotkeys();
        app
    }

    /// Called once per frame when anything was edited.
    fn touch(&mut self) {
        self.settings_dirty = true;
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

        if ctx.input(|input| input.viewport().close_requested()) {
            let _ = config::save(&self.settings);
            self.worker.shutdown();
        }

        if self.status.running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Reads the next key combination pressed while the window has focus.
    fn capture_key(&mut self, ctx: &egui::Context) {
        let mut chosen: Option<Option<String>> = None;
        ctx.input(|input| {
            for event in &input.events {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                else {
                    continue;
                };
                if *key == egui::Key::Escape {
                    chosen = Some(None);
                    return;
                }
                let Some(code) = key_to_code(*key) else {
                    continue;
                };
                let binding = hotkeys::binding(
                    code,
                    modifiers.ctrl,
                    modifiers.shift,
                    modifiers.alt,
                    modifiers.mac_cmd,
                );
                chosen = Some(Some(binding.to_string()));
                return;
            }
        });
        if let Some(binding) = chosen {
            self.end_capture(binding);
        }
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

    fn flush(&mut self) {
        if self.settings_dirty && self.changed_at.elapsed() >= SAVE_DEBOUNCE {
            self.settings_dirty = false;
            if let Err(err) = config::save(&self.settings) {
                self.save_error = Some(format!("cannot save settings: {err}"));
            }
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("FeatherClick").size(17.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, colour) = self.status_pill();
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
        let elapsed = self.status.elapsed().unwrap_or_default().as_secs_f64();
        let clicks = self.status.clicks;
        let mut line = format!("{clicks} clicks");
        if elapsed > 0.0 {
            line += &format!("  ·  {elapsed:.1} s");
            if elapsed > 0.25 {
                line += &format!("  ·  {:.1}/s", clicks as f64 / elapsed);
            }
        }
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
                } else if !self.settings.hotkey_toggle.trim().is_empty() {
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
        } else if let Some(error) = self.hotkey_error.clone() {
            ui.add_space(6.0);
            banner(ui, &error, WARNING);
        }
    }

    fn status_pill(&self) -> (String, Color32) {
        if self.status.error.is_some() {
            return ("Error".to_string(), DANGER);
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
            let (low, high) = self.settings.rate_range();
            let rate = if (high - low).abs() < 0.05 {
                format!("{low:.1} clicks/s")
            } else {
                format!("{low:.1} – {high:.1} clicks/s")
            };
            hint(ui, &rate);
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
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_window(&ctx);
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
        self.flush();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = config::save(&self.settings);
        self.worker.shutdown();
    }
}

fn ui_muted() -> Color32 {
    Color32::from_gray(140)
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
            let events = GlobalHotKeyEvent::receiver();
            let mut gate = Gate::new();
            while let Ok(event) = events.recv() {
                if !gate.accept(event.id, hotkeys::is_press(&event), Instant::now()) {
                    continue;
                }
                let snapshot = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if hotkeys::matches(&event, &snapshot.hotkey_toggle) {
                    worker.toggle(&snapshot);
                } else if hotkeys::matches(&event, &snapshot.hotkey_capture) {
                    worker.capture_cursor();
                }
                ctx.request_repaint();
            }
        });
    let _ = spawned;
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
