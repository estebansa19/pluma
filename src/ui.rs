//! Settings window (egui). Opened on demand and fully closed afterwards: nothing stays resident.

use crate::config::{Config, Storage};
use crate::device::{Battery, DpiCaps, Link};
use crate::onboard;
use crate::shell;
use crate::sys;
use crate::worker::{self, Cmd, Evt, Snapshot};
use egui::{
    self, pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Layout, Margin, Rect, Response, RichText, Rounding,
    Sense, Stroke, Ui,
};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

mod pal {
    use egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x0E, 0x10, 0x14);
    pub const CARD: Color32 = Color32::from_rgb(0x16, 0x19, 0x20);
    pub const CARD_HI: Color32 = Color32::from_rgb(0x1E, 0x22, 0x2B);
    pub const BORDER: Color32 = Color32::from_rgb(0x24, 0x28, 0x33);
    pub const TRACK: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x3B);
    pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEE);
    pub const MUTED: Color32 = Color32::from_rgb(0x8B, 0x91, 0xA3);
    pub const FAINT: Color32 = Color32::from_rgb(0x5C, 0x62, 0x72);
    pub const ACCENT: Color32 = Color32::from_rgb(0x7A, 0xA2, 0xF7);
    pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0B, 0x10, 0x1C);
    pub const OK: Color32 = Color32::from_rgb(0x9E, 0xCE, 0x6A);
    pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xAF, 0x68);
    pub const BAD: Color32 = Color32::from_rgb(0xF7, 0x76, 0x8E);
    pub const STAGES: [Color32; 5] = [
        Color32::from_rgb(0x7A, 0xA2, 0xF7),
        Color32::from_rgb(0x9E, 0xCE, 0x6A),
        Color32::from_rgb(0xE0, 0xAF, 0x68),
        Color32::from_rgb(0xFF, 0x9E, 0x64),
        Color32::from_rgb(0xF7, 0x76, 0x8E),
    ];
}

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn run() -> Result<(), String> {
    if !sys::single_instance("Local\\PlumaUI") {
        sys::focus_ui_window();
        return Ok(());
    }
    let opts = shell::Options {
        title: sys::UI_TITLE,
        size: (440.0, 700.0),
        min_size: (400.0, 480.0),
        icon: Some((sys::icon_rgba(32), 32)),
        caption: 0x0E1014,
        clear: pal::BG,
    };
    shell::run(opts, App::new)
}

#[derive(Clone, PartialEq, Debug)]
struct Model {
    storage: Storage,
    stages: Vec<u16>,
    default_stage: usize,
    rate_hz: Option<u32>,
}

struct App {
    tx: Sender<Cmd>,
    rx: Receiver<Evt>,
    snap: Option<Snapshot>,
    missing: Option<String>,
    busy: Option<&'static str>,
    model: Option<Model>,
    /// What the mouse actually has (to know whether there are unsaved changes).
    baseline: Option<Model>,
    reload: bool,
    toast: Option<(String, bool, Instant)>,
    cfg: Config,
    background: bool,
    ghub: bool,
    refreshing: bool,
    t_ghub: Instant,
    t_poll: Instant,
    t_battery: Instant,
    confirm_restore: bool,
    has_backup: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        install_style(ctx);
        let (tx, rx) = worker::spawn(ctx.clone());
        let _ = tx.send(Cmd::Refresh);
        let now = Instant::now();
        App {
            tx,
            rx,
            snap: None,
            missing: None,
            busy: None,
            model: None,
            baseline: None,
            reload: true,
            toast: None,
            cfg: Config::load(),
            background: sys::autostart_enabled() || sys::agent_running(),
            ghub: sys::ghub_running(),
            refreshing: true,
            t_ghub: now + Duration::from_secs(4),
            t_poll: now + Duration::from_secs(3),
            t_battery: now + Duration::from_secs(60),
            confirm_restore: false,
            has_backup: !onboard::list_backups(&Config::backup_dir()).is_empty(),
        }
    }

    fn pump(&mut self) {
        while let Ok(e) = self.rx.try_recv() {
            match e {
                Evt::Snapshot(s) => {
                    self.refreshing = false;
                    self.missing = None;
                    if !s.online {
                        match &mut self.snap {
                            Some(cur) => cur.online = false,
                            None => self.snap = Some(*s),
                        }
                        continue;
                    }
                    if self.reload || self.model.is_none() {
                        self.load_model(&s);
                        self.reload = false;
                    }
                    self.snap = Some(*s);
                }
                Evt::Missing(m) => {
                    self.refreshing = false;
                    self.snap = None;
                    self.model = None;
                    self.baseline = None;
                    self.missing = Some(m);
                }
                Evt::Offline => {
                    if let Some(s) = &mut self.snap {
                        s.online = false;
                    }
                }
                Evt::Battery(b) => {
                    if let Some(s) = &mut self.snap {
                        s.battery = Some(b);
                    }
                }
                Evt::Done { ok, msg } => {
                    self.busy = None;
                    self.toast = Some((msg, ok, Instant::now()));
                    if ok {
                        self.cfg = Config::load();
                        self.reload = true;
                        self.confirm_restore = false;
                    }
                    self.has_backup = !onboard::list_backups(&Config::backup_dir()).is_empty();
                }
            }
        }
    }

    fn load_model(&mut self, s: &Snapshot) {
        let ob = s.onboard.as_ref();
        let profile = ob.and_then(|o| o.profile.as_ref());
        let profile_rate = ob.and_then(|o| o.profile_rate_hz);
        let onboard_now = ob.map_or(false, |o| o.mode_onboard);
        let using_ours = ob.map_or(false, |o| !o.needs_activation());
        let writable = ob.map_or(false, |o| o.writable);
        let same_dev = self.cfg.device.as_deref() == Some(s.name.as_str());
        let cfg_soft = same_dev && self.cfg.storage == Storage::Software;

        let baseline = match profile {
            Some(p) if using_ours => Model {
                storage: Storage::Onboard,
                stages: p.dpis.clone(),
                default_stage: p.default_dpi as usize,
                rate_hz: profile_rate.or(s.rate),
            },
            _ if cfg_soft && s.dpi == Some(self.cfg.active_dpi()) => Model {
                storage: Storage::Software,
                stages: self.cfg.dpi_stages.clone(),
                default_stage: self.cfg.default_stage,
                rate_hz: s.rate,
            },
            _ => Model {
                storage: if onboard_now { Storage::Onboard } else { Storage::Software },
                stages: vec![s.dpi.unwrap_or(800)],
                default_stage: 0,
                rate_hz: s.rate,
            },
        };
        let mut model = baseline.clone();
        // If the mouse isn't using our profile (software mode because of G HUB, or it woke up
        // on the factory one) and we can write its memory, suggest activating the user profile.
        if !using_ours && writable && !cfg_soft {
            model.storage = Storage::Onboard;
            if let Some(p) = profile {
                model.stages = p.dpis.clone();
                model.default_stage = p.default_dpi as usize;
                if profile_rate.is_some() {
                    model.rate_hz = profile_rate;
                }
            }
        }
        model.default_stage = model.default_stage.min(model.stages.len().saturating_sub(1));
        self.baseline = Some(baseline);
        self.model = Some(model);
    }

    /// The mouse isn't using the profile Pluma edits, and the user wants the mouse memory.
    fn needs_activation(&self) -> bool {
        let wants_onboard = self.model.as_ref().map_or(false, |m| m.storage == Storage::Onboard);
        let ob = self.snap.as_ref().and_then(|s| s.onboard.as_ref());
        wants_onboard && ob.map_or(false, |o| o.writable && o.needs_activation())
    }

    fn dirty(&self) -> bool {
        self.model.is_some() && (self.model != self.baseline || self.needs_activation())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.t_ghub {
            self.ghub = sys::ghub_running();
            self.t_ghub = now + Duration::from_secs(4);
        }
        let needs_poll = self.snap.as_ref().map_or(true, |s| !s.online);
        if needs_poll && !self.refreshing && self.busy.is_none() && now >= self.t_poll {
            self.refreshing = true;
            let _ = self.tx.send(Cmd::Refresh);
            self.t_poll = now + Duration::from_secs(3);
        }
        if !needs_poll && now >= self.t_battery {
            let _ = self.tx.send(Cmd::Battery);
            self.t_battery = now + Duration::from_secs(60);
        }
        if let Some((_, _, t)) = &self.toast {
            if t.elapsed() > Duration::from_secs(7) {
                self.toast = None;
            }
        }
    }

    fn save(&mut self) {
        let (Some(m), Some(s)) = (self.model.clone(), self.snap.as_ref()) else { return };
        let cfg = Config {
            device: Some(s.name.clone()),
            storage: m.storage,
            default_stage: m.default_stage.min(m.stages.len().saturating_sub(1)),
            dpi_stages: m.stages,
            report_rate_hz: m.rate_hz,
            background: self.background,
            profile_sector: self.cfg.profile_sector,
        };
        self.busy = Some(if cfg.storage == Storage::Onboard { "Saving to the mouse…" } else { "Applying…" });
        self.toast = None;
        let _ = self.tx.send(Cmd::Save(cfg));
    }

    fn set_background(&mut self, on: bool) {
        self.background = on;
        self.cfg.background = on;
        let _ = self.cfg.save();
        sys::set_autostart(on);
        if on {
            if !sys::agent_running() {
                let _ = sys::spawn_agent();
            }
        } else {
            sys::close_agent();
        }
    }

    fn preview(&self, dpi: u16) {
        let _ = self.tx.send(Cmd::Preview(dpi));
    }

    // ---------- sections ----------

    fn empty_state(&mut self, ui: &mut Ui) {
        ui.add_space(110.0);
        ui.vertical_centered(|ui| {
            mouse_glyph(ui, pal::FAINT);
            ui.add_space(18.0);
            let title = if self.missing.is_none() { "Looking for your mouse…" } else { "Can't find your mouse" };
            ui.label(RichText::new(title).font(FontId::new(18.0, semibold())).color(pal::TEXT));
            ui.add_space(6.0);
            if let Some(m) = &self.missing {
                ui.label(RichText::new(m).color(pal::MUTED));
                ui.add_space(4.0);
                ui.label(RichText::new("Plug in the receiver or the cable; Pluma detects it automatically.").size(12.5).color(pal::FAINT));
            }
        });
    }

    fn header(&self, ui: &mut Ui, s: &Snapshot) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&s.name).font(FontId::new(22.0, semibold())).color(pal::TEXT));
                let link = match s.link {
                    Link::Receiver => "LIGHTSPEED",
                    Link::Wired => "USB",
                };
                let ob = s.onboard.as_ref();
                let state = if !s.online {
                    "asleep"
                } else if ob.map_or(false, |o| o.mode_onboard && o.running >= 0x0100) {
                    "factory profile"
                } else if ob.map_or(false, |o| o.mode_onboard) {
                    "using its memory"
                } else {
                    "software mode"
                };
                ui.label(RichText::new(format!("{link}  ·  {state}")).size(12.5).color(pal::MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(b) = &s.battery {
                    battery_widget(ui, b);
                }
            });
        });
    }

    fn ghub_banner(&mut self, ui: &mut Ui) {
        egui::Frame::none()
            .fill(pal::WARN.gamma_multiply(0.10))
            .stroke(Stroke::new(1.0_f32, pal::WARN.gamma_multiply(0.35)))
            .rounding(12.0)
            .inner_margin(Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let w = ui.available_width();
                ui.horizontal(|ui| {
                    let text_w = (w - 120.0).max(120.0);
                    ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(text_w);
                        ui.label(RichText::new("G HUB is running").font(FontId::new(14.0, semibold())).color(pal::WARN));
                        ui.add(
                            egui::Label::new(
                                RichText::new("It can overwrite your settings and take the mouse out of its memory.")
                                    .size(12.5)
                                    .color(pal::MUTED),
                            )
                            .wrap(),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close G HUB").clicked() {
                            sys::kill_ghub();
                            self.t_ghub = Instant::now() + Duration::from_millis(1500);
                        }
                    });
                });
            });
    }

    fn mode_hint(&self, ui: &mut Ui, s: &Snapshot) {
        let Some(ob) = s.onboard.as_ref() else { return };
        if !self.needs_activation() {
            return;
        }
        let text = if !ob.mode_onboard {
            "Your mouse is in software mode (G HUB left it that way): when it wakes up it goes back to \
             its factory DPI. Save so it uses your profile from its own memory."
                .to_string()
        } else {
            format!(
                "It woke up on the factory profile ({} DPI) instead of yours. Save to activate your profile; \
                 with \"Run in the background\", Pluma fixes it automatically every time it wakes up.",
                s.dpi.map_or("?".into(), |d| d.to_string())
            )
        };
        ui.add_space(14.0);
        egui::Frame::none()
            .fill(pal::ACCENT.gamma_multiply(0.09))
            .stroke(Stroke::new(1.0_f32, pal::ACCENT.gamma_multiply(0.30)))
            .rounding(12.0)
            .inner_margin(Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("That's why it loses its DPI when it sleeps").font(FontId::new(14.0, semibold())).color(pal::ACCENT));
                ui.add(egui::Label::new(RichText::new(text).size(12.5).color(pal::MUTED)).wrap());
            });
    }

    fn dpi_section(&mut self, ui: &mut Ui, s: &Snapshot) {
        section_title(ui, "SENSITIVITY", Some("DPI"));
        let caps = s.dpi_caps.clone().unwrap_or_else(DpiCaps::fallback);
        let Some(model) = self.model.as_mut() else { return };
        let mut preview: Option<u16> = None;
        let mut remove: Option<usize> = None;
        let mut add = false;
        card(ui, |ui| {
            let n = model.stages.len();
            for i in 0..n {
                if i > 0 {
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    let selected = model.default_stage == i;
                    let color = pal::STAGES[i % pal::STAGES.len()];
                    if stage_dot(ui, color, selected).clicked() && !selected {
                        model.default_stage = i;
                        preview = Some(model.stages[i]);
                    }
                    let avail = ui.available_width();
                    let slider_w = (avail - 76.0 - 26.0 - 2.0 * ui.spacing().item_spacing.x).max(60.0);
                    let v = &mut model.stages[i];
                    let r = dpi_slider(ui, v, &caps, color, slider_w);
                    if r.drag_stopped() || (r.clicked() && !r.dragged()) {
                        if selected {
                            preview = Some(*v);
                        }
                    }
                    let speed = (*v as f64 / 150.0).max(1.0);
                    let r2 = ui.add_sized(
                        [76.0, 26.0],
                        egui::DragValue::new(v).range(caps.min()..=caps.max()).speed(speed),
                    );
                    if r2.lost_focus() || r2.drag_stopped() {
                        *v = caps.snap(*v);
                        if selected {
                            preview = Some(*v);
                        }
                    }
                    if n > 1 {
                        if close_button(ui).on_hover_text("Remove level").clicked() {
                            remove = Some(i);
                        }
                    } else {
                        ui.add_space(22.0);
                    }
                });
            }
            if n < onboard::DPI_SLOTS {
                ui.add_space(6.0);
                let r = ui.add(egui::Button::new(RichText::new("+  Add level").color(pal::ACCENT)).frame(false));
                if r.clicked() {
                    add = true;
                }
            }
        });
        if add {
            let last = *model.stages.last().unwrap_or(&800);
            let next = caps.snap(((last as u32) * 2).min(caps.max() as u32) as u16);
            model.stages.push(next);
        }
        if let Some(i) = remove {
            model.stages.remove(i);
            if model.default_stage == i {
                model.default_stage = 0;
                preview = model.stages.first().copied();
            } else if model.default_stage > i {
                model.default_stage -= 1;
            }
        }
        let hint = match model.storage {
            Storage::Onboard => "The filled dot marks the level the mouse starts with. The DPI button cycles through the levels.",
            Storage::Software => "In software mode the marked level is used; Pluma reapplies it after wake-up.",
        };
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(hint).size(12.0).color(pal::FAINT)).wrap());
        if let Some(d) = preview {
            self.preview(d);
        }
    }

    fn rate_section(&mut self, ui: &mut Ui, s: &Snapshot) {
        if s.rates.is_empty() {
            return;
        }
        section_title(ui, "POLLING RATE", Some("Hz"));
        let Some(model) = self.model.as_mut() else { return };
        let compact = s.rates.len() > 4;
        let opts: Vec<(u32, String)> = s
            .rates
            .iter()
            .map(|&hz| (hz, if compact && hz >= 1000 { format!("{}K", hz / 1000) } else { hz.to_string() }))
            .collect();
        let mut cur = model.rate_hz.unwrap_or(0);
        if segmented(ui, &mut cur, &opts) {
            model.rate_hz = Some(cur);
        }
        if cur > 1000 {
            ui.add_space(6.0);
            ui.label(RichText::new("Above 1000 Hz the mouse uses a lot more battery and a bit more CPU.").size(12.0).color(pal::FAINT));
        }
    }

    fn storage_section(&mut self, ui: &mut Ui, s: &Snapshot) {
        section_title(ui, "MEMORY AND BACKGROUND", None);
        let ob = s.onboard.as_ref();
        let writable = ob.map_or(false, |o| o.writable);
        let why_not = ob
            .and_then(|o| o.why_not.clone())
            .unwrap_or_else(|| "This mouse has no profile memory.".to_string());
        let storage = self.model.as_ref().map(|m| m.storage).unwrap_or_default();
        let mut bg = self.background;
        let mut storage_changed: Option<Storage> = None;
        card(ui, |ui| {
            let mut on = storage == Storage::Onboard;
            let desc = if writable {
                "Recommended. Settings live in the mouse: they survive sleep and restarts, and work \
                 without any app running."
                    .to_string()
            } else {
                why_not.clone()
            };
            if setting_row(ui, "Save to the mouse memory", &desc, &mut on, writable || storage == Storage::Onboard) {
                storage_changed = Some(if on { Storage::Onboard } else { Storage::Software });
            }
            divider(ui);
            let desc2 = match storage {
                Storage::Onboard => "Tray icon with the battery level. If the mouse wakes up on the factory profile or something (like G HUB) takes it out of its memory, Pluma fixes it. Uses less than 2 MB of RAM.",
                Storage::Software => "Required in software mode: Pluma reapplies your DPI every time the mouse wakes up. Uses less than 2 MB of RAM.",
            };
            setting_row(ui, "Run in the background", desc2, &mut bg, true);
        });
        if let (Some(st), Some(m)) = (storage_changed, self.model.as_mut()) {
            m.storage = st;
        }
        if bg != self.background {
            self.set_background(bg);
        }
        if storage == Storage::Software && !self.background {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new("Without the background agent, the mouse will go back to its default DPI every time it sleeps.")
                        .size(12.0)
                        .color(pal::WARN),
                )
                .wrap(),
            );
        }
    }

    fn advanced_section(&mut self, ui: &mut Ui, s: &Snapshot) {
        ui.add_space(14.0);
        egui::CollapsingHeader::new(RichText::new("Advanced").size(13.0).color(pal::MUTED))
            .id_salt("advanced")
            .default_open(false)
            .show(ui, |ui| {
                if let Some(o) = &s.onboard {
                    let running = if !o.mode_onboard {
                        "none (software mode)".to_string()
                    } else if o.running >= 0x0100 {
                        "factory".to_string()
                    } else {
                        format!("{} of {}", o.running, o.profile_count)
                    };
                    info_row(ui, "Profile in use", &running);
                    info_row(ui, "Profile edited by Pluma", &format!("{} of {}", o.target & 0xFF, o.profile_count));
                    if let Some(p) = &o.profile {
                        let (ps, po) = p.sleep_secs;
                        info_row(ui, "Power timers", &format!("sleep {}  ·  power off {}", secs(ps), secs(po)));
                    }
                }
                if let Some(dpi) = s.dpi {
                    info_row(ui, "Current sensor DPI", &dpi.to_string());
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if self.has_backup {
                        if !self.confirm_restore {
                            if ui.button("Restore original memory").clicked() {
                                self.confirm_restore = true;
                            }
                        } else {
                            ui.label(RichText::new("Go back to how it was before Pluma?").color(pal::WARN));
                            if ui.button("Restore").clicked() && self.busy.is_none() {
                                self.busy = Some("Restoring…");
                                let _ = self.tx.send(Cmd::Restore);
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirm_restore = false;
                            }
                        }
                    }
                    if ui.button("Open data folder").clicked() {
                        sys::open_folder(&Config::dir());
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("Pluma {}  ·  backups in %APPDATA%\\Pluma\\backups", env!("CARGO_PKG_VERSION")))
                        .size(11.5)
                        .color(pal::FAINT),
                );
            });
    }

    fn footer(&mut self, ui: &mut Ui) {
        let online = self.snap.as_ref().map_or(false, |s| s.online);
        let dirty = self.dirty();
        let onboard = self.model.as_ref().map_or(true, |m| m.storage == Storage::Onboard);
        ui.horizontal(|ui| {
            ui.set_min_height(42.0);
            if let Some(b) = self.busy {
                ui.add(egui::Spinner::new().size(14.0).color(pal::ACCENT));
                ui.label(RichText::new(b).color(pal::MUTED));
            } else if let Some((msg, ok, _)) = &self.toast {
                let c = if *ok { pal::OK } else { pal::BAD };
                let w = (ui.available_width() - 190.0).max(80.0);
                ui.allocate_ui_with_layout(vec2(w, 40.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(msg).size(12.5).color(c)).wrap());
                });
            } else if self.snap.is_some() && !online {
                ui.label(RichText::new("Mouse asleep").color(pal::MUTED));
            } else if dirty {
                ui.label(RichText::new("Unsaved changes").color(pal::WARN));
            } else if self.model.is_some() {
                ui.label(RichText::new("All saved").color(pal::FAINT));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if onboard { "Save to mouse" } else { "Apply" };
                let enabled = dirty && online && self.busy.is_none();
                let btn = egui::Button::new(
                    RichText::new(label).font(FontId::new(14.0, semibold())).color(if enabled { pal::ON_ACCENT } else { pal::FAINT }),
                )
                .fill(if enabled { pal::ACCENT } else { pal::CARD })
                .stroke(Stroke::NONE)
                .rounding(10.0)
                .min_size(vec2(170.0, 40.0));
                if ui.add_enabled(enabled, btn).clicked() {
                    self.save();
                }
            });
        });
    }
}

impl shell::AppLogic for App {
    fn update(&mut self, ctx: &egui::Context) {
        self.pump();
        self.tick();
        if self.snap.is_some() {
            egui::TopBottomPanel::bottom("footer")
                .show_separator_line(false)
                .frame(
                    egui::Frame::none()
                        .fill(pal::BG)
                        .inner_margin(Margin { left: 20.0, right: 20.0, top: 8.0, bottom: 14.0 }),
                )
                .show(ctx, |ui| self.footer(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(pal::BG).inner_margin(Margin { left: 20.0, right: 20.0, top: 18.0, bottom: 0.0 }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let Some(snap) = self.snap.clone() else { return self.empty_state(ui) };
                    self.header(ui, &snap);
                    if self.ghub {
                        ui.add_space(14.0);
                        self.ghub_banner(ui);
                    }
                    if !snap.online || self.model.is_none() {
                        ui.add_space(70.0);
                        ui.vertical_centered(|ui| {
                            mouse_glyph(ui, pal::FAINT);
                            ui.add_space(16.0);
                            ui.label(RichText::new("The mouse is asleep").font(FontId::new(17.0, semibold())).color(pal::TEXT));
                            ui.add_space(4.0);
                            ui.label(RichText::new("Move it to wake it up; Pluma detects it automatically.").color(pal::MUTED));
                        });
                        return;
                    }
                    self.mode_hint(ui, &snap);
                    self.dpi_section(ui, &snap);
                    self.rate_section(ui, &snap);
                    self.storage_section(ui, &snap);
                    self.advanced_section(ui, &snap);
                    ui.add_space(12.0);
                });
            });
        ctx.request_repaint_after(Duration::from_millis(1000));
    }
}

// ---------- style ----------

/// Uses the Windows fonts (Segoe UI) instead of embedding fonts in the executable.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let load = |names: &[&str]| names.iter().find_map(|f| std::fs::read(format!(r"{windir}\Fonts\{f}")).ok());
    let mut regular = Vec::new();
    if let Some(b) = load(&["segoeui.ttf", "arial.ttf", "tahoma.ttf"]) {
        fonts.font_data.insert("ui".into(), egui::FontData::from_owned(b));
        regular.push("ui".to_string());
    }
    let mut sb = regular.clone();
    if let Some(b) = load(&["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"]) {
        fonts.font_data.insert("ui-sb".into(), egui::FontData::from_owned(b));
        sb.insert(0, "ui-sb".into());
    }
    fonts.families.insert(FontFamily::Proportional, regular.clone());
    fonts.families.insert(FontFamily::Monospace, regular);
    fonts.families.insert(semibold(), sb);
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = pal::BG;
    v.window_fill = pal::CARD;
    v.extreme_bg_color = pal::BG;
    v.faint_bg_color = pal::CARD;
    v.selection.bg_fill = pal::ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, pal::ACCENT);
    v.hyperlink_color = pal::ACCENT;
    v.window_rounding = Rounding::same(12.0);
    let r = Rounding::same(8.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal::BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal::TEXT);
    v.widgets.inactive.rounding = r;
    v.widgets.inactive.bg_fill = pal::CARD_HI;
    v.widgets.inactive.weak_bg_fill = pal::CARD_HI;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal::TEXT);
    v.widgets.hovered.rounding = r;
    v.widgets.hovered.bg_fill = pal::TRACK;
    v.widgets.hovered.weak_bg_fill = pal::TRACK;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, pal::BORDER);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.widgets.active.rounding = r;
    v.widgets.active.bg_fill = pal::TRACK;
    v.widgets.active.weak_bg_fill = pal::TRACK;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, pal::ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.widgets.open = v.widgets.active;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
        s.interaction.selectable_labels = false;
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::new(20.0, semibold()));
    });
}

// ---------- widgets ----------

fn secs(s: u16) -> String {
    if s >= 60 && s % 60 == 0 {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

fn section_title(ui: &mut Ui, title: &str, unit: Option<&str>) {
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(FontId::new(11.5, semibold())).color(pal::MUTED).extra_letter_spacing(0.8));
        if let Some(u) = unit {
            ui.label(RichText::new(u).size(11.5).color(pal::FAINT));
        }
    });
    ui.add_space(4.0);
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::none()
        .fill(pal::CARD)
        .stroke(Stroke::new(1.0_f32, pal::BORDER))
        .rounding(12.0)
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn divider(ui: &mut Ui) {
    ui.add_space(8.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, pal::BORDER);
    ui.add_space(8.0);
}

fn info_row(ui: &mut Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(k).size(12.5).color(pal::MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(v).size(12.5).color(pal::TEXT));
        });
    });
}

fn setting_row(ui: &mut Ui, title: &str, desc: &str, on: &mut bool, enabled: bool) -> bool {
    let mut changed = false;
    let w = ui.available_width();
    ui.horizontal_top(|ui| {
        let text_w = (w - 56.0).max(120.0);
        ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(text_w);
            ui.label(RichText::new(title).size(14.5).color(if enabled { pal::TEXT } else { pal::MUTED }));
            ui.add_space(1.0);
            ui.add(egui::Label::new(RichText::new(desc).size(12.5).color(pal::MUTED)).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.add_enabled_ui(enabled, |ui| {
                if toggle(ui, on).changed() {
                    changed = true;
                }
            });
        });
    });
    changed
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let enabled = ui.is_enabled();
    let mut track = lerp_color(pal::TRACK, pal::ACCENT, t);
    if !enabled {
        track = track.gamma_multiply(0.45);
    }
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(11.0), track);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    p.circle_filled(pos2(x, rect.center().y), 8.0, if enabled { Color32::WHITE } else { pal::MUTED });
    resp
}

fn stage_dot(ui: &mut Ui, color: Color32, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 26.0), Sense::click());
    let c = rect.center();
    let p = ui.painter();
    if selected {
        p.circle_filled(c, 10.0, color.gamma_multiply(0.18));
        p.circle_filled(c, 6.0, color);
    } else {
        let col = if resp.hovered() { color } else { color.gamma_multiply(0.55) };
        p.circle_stroke(c, 5.5, Stroke::new(1.6_f32, col));
    }
    resp.on_hover_text(if selected { "Default level" } else { "Use as default level" })
}

/// Hand-painted logarithmic DPI slider.
fn dpi_slider(ui: &mut Ui, value: &mut u16, caps: &DpiCaps, color: Color32, width: f32) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click_and_drag());
    let lo = (caps.min().max(1)) as f32;
    let hi = (caps.max().max(caps.min() + 1)) as f32;
    let (llo, lhi) = (lo.ln(), hi.ln());
    let rail = rect.shrink2(vec2(9.0, 0.0));
    if resp.is_pointer_button_down_on() || resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
            let raw = (llo + t * (lhi - llo)).exp();
            // "Friendly" rounding: to 50 above 1000 DPI, to 10 below.
            let nice = if raw >= 1000.0 { (raw / 50.0).round() * 50.0 } else { (raw / 10.0).round() * 10.0 };
            let v = caps.snap(nice.clamp(lo, hi) as u16);
            if v != *value {
                *value = v;
                resp.mark_changed();
            }
        }
    }
    let p = ui.painter();
    let y = rect.center().y;
    let track = Rect::from_min_max(pos2(rail.left(), y - 2.0), pos2(rail.right(), y + 2.0));
    p.rect_filled(track, Rounding::same(2.0), pal::TRACK);
    let t = (((*value).max(1) as f32).ln() - llo) / (lhi - llo);
    let x = rail.left() + t.clamp(0.0, 1.0) * rail.width();
    p.rect_filled(Rect::from_min_max(track.min, pos2(x, track.max.y)), Rounding::same(2.0), color);
    let r = if resp.hovered() || resp.dragged() { 8.0 } else { 7.0 };
    if resp.hovered() || resp.dragged() {
        p.circle_filled(pos2(x, y), r + 5.0, color.gamma_multiply(0.15));
    }
    p.circle_filled(pos2(x, y), r, Color32::WHITE);
    resp
}

fn close_button(ui: &mut Ui) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 26.0), Sense::click());
    let c = rect.center();
    let p = ui.painter();
    if resp.hovered() {
        p.circle_filled(c, 10.0, pal::CARD_HI);
    }
    let col = if resp.hovered() { pal::TEXT } else { pal::FAINT };
    let d = 3.6;
    p.line_segment([c + vec2(-d, -d), c + vec2(d, d)], Stroke::new(1.5_f32, col));
    p.line_segment([c + vec2(-d, d), c + vec2(d, -d)], Stroke::new(1.5_f32, col));
    resp
}

fn segmented(ui: &mut Ui, value: &mut u32, opts: &[(u32, String)]) -> bool {
    let w = ui.available_width();
    let h = 38.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect(rect, Rounding::same(10.0), pal::CARD, Stroke::new(1.0_f32, pal::BORDER));
    let inner = rect.shrink(4.0);
    let seg_w = inner.width() / opts.len().max(1) as f32;
    let mut changed = false;
    for (i, (v, label)) in opts.iter().enumerate() {
        let r = Rect::from_min_size(pos2(inner.left() + i as f32 * seg_w, inner.top()), vec2(seg_w, inner.height()));
        let resp = ui.interact(r, ui.id().with(("rate", i)), Sense::click());
        let sel = *value == *v;
        if sel {
            painter.rect_filled(r, Rounding::same(7.0), pal::ACCENT);
        } else if resp.hovered() {
            painter.rect_filled(r, Rounding::same(7.0), pal::CARD_HI);
        }
        let col = if sel { pal::ON_ACCENT } else if resp.hovered() { pal::TEXT } else { pal::MUTED };
        let font = if sel { FontId::new(13.5, semibold()) } else { FontId::proportional(13.5) };
        painter.text(r.center(), Align2::CENTER_CENTER, label, font, col);
        if resp.clicked() && !sel {
            *value = *v;
            changed = true;
        }
    }
    changed
}

fn battery_widget(ui: &mut Ui, b: &Battery) {
    let color = if b.charging || b.full {
        pal::OK
    } else if b.percent <= 15 {
        pal::BAD
    } else if b.percent <= 30 {
        pal::WARN
    } else {
        pal::TEXT
    };
    let text = if b.charging { format!("{}%  charging", b.percent) } else { format!("{}%", b.percent) };
    ui.label(RichText::new(text).size(14.0).color(color));
    let (rect, _) = ui.allocate_exact_size(vec2(27.0, 14.0), Sense::hover());
    let p = ui.painter();
    let body = Rect::from_min_size(rect.min, vec2(23.0, 13.0));
    p.rect_stroke(body, Rounding::same(3.5), Stroke::new(1.3_f32, pal::MUTED));
    let inner = body.shrink(2.4);
    let w = inner.width() * (b.percent as f32 / 100.0).clamp(0.08, 1.0);
    p.rect_filled(Rect::from_min_size(inner.min, vec2(w, inner.height())), Rounding::same(1.5), color);
    let nub = Rect::from_min_size(pos2(body.right() + 1.2, body.center().y - 2.5), vec2(2.2, 5.0));
    p.rect_filled(nub, Rounding::same(1.0), pal::MUTED);
}

fn mouse_glyph(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(44.0, 64.0), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(rect.shrink(2.0), Rounding::same(20.0), Stroke::new(2.0_f32, color));
    let top = rect.top() + 2.0;
    p.line_segment([pos2(rect.center().x, top + 8.0), pos2(rect.center().x, top + 22.0)], Stroke::new(2.0_f32, color));
}
