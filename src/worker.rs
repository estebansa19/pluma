//! Thread that talks to the mouse on behalf of the window (the UI never blocks waiting on USB).

use crate::config::{Config, Storage};
use crate::device::{self, Battery, Device, DpiCaps, Link, OnboardSummary, SW_UI};
use crate::onboard;
use hidapi::HidApi;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub name: String,
    pub link: Link,
    pub online: bool,
    pub battery: Option<Battery>,
    pub dpi_caps: Option<DpiCaps>,
    pub dpi: Option<u16>,
    pub rates: Vec<u32>,
    pub rate: Option<u32>,
    pub onboard: Option<OnboardSummary>,
}

pub enum Cmd {
    Refresh,
    Preview(u16),
    Save(Config),
    Battery,
    Restore,
}

pub enum Evt {
    Snapshot(Box<Snapshot>),
    Missing(String),
    Offline,
    Battery(Battery),
    Done { ok: bool, msg: String },
}

struct Worker {
    api: Option<HidApi>,
    dev: Option<Device>,
}

pub fn spawn(ctx: egui::Context) -> (Sender<Cmd>, Receiver<Evt>) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<Cmd>();
    let (tx_evt, rx_evt) = mpsc::channel::<Evt>();
    std::thread::Builder::new()
        .name("pluma-hid".into())
        .spawn(move || {
            let mut w = Worker { api: None, dev: None };
            let emit = |e: Evt| {
                let _ = tx_evt.send(e);
                ctx.request_repaint();
            };
            while let Ok(mut cmd) = rx_cmd.recv() {
                // If DPI previews piled up, only the last one matters.
                while let Cmd::Preview(_) = cmd {
                    match rx_cmd.try_recv() {
                        Ok(next) => cmd = next,
                        Err(_) => break,
                    }
                }
                match cmd {
                    Cmd::Refresh => w.refresh(&emit),
                    Cmd::Preview(dpi) => {
                        if let Some(d) = w.dev.as_mut().filter(|d| d.online) {
                            let _ = d.set_dpi(dpi);
                        }
                    }
                    Cmd::Battery => w.battery(&emit),
                    Cmd::Save(cfg) => {
                        crate::log::write(&format!(
                            "save: {:?} levels {:?} default {} rate {:?}",
                            cfg.storage, cfg.dpi_stages, cfg.default_stage, cfg.report_rate_hz
                        ));
                        let r = w.save(&cfg);
                        crate::log::write(&format!("save → {r:?}"));
                        let ok = r.is_ok();
                        emit(Evt::Done { ok, msg: r.unwrap_or_else(|e| e) });
                        w.refresh(&emit);
                    }
                    Cmd::Restore => {
                        let r = w.restore();
                        crate::log::write(&format!("restore → {r:?}"));
                        let ok = r.is_ok();
                        emit(Evt::Done { ok, msg: r.unwrap_or_else(|e| e) });
                        w.refresh(&emit);
                    }
                }
            }
        })
        .expect("couldn't create the HID thread");
    (tx_cmd, rx_evt)
}

fn snapshot(d: &mut Device) -> Snapshot {
    Snapshot {
        battery: d.battery().ok(),
        dpi_caps: d.dpi_caps().ok(),
        dpi: d.get_dpi().ok(),
        rates: d.rate_options().unwrap_or_default(),
        rate: d.get_rate_hz().ok(),
        onboard: d.onboard_summary().ok(),
        name: d.name.clone(),
        link: d.link,
        online: d.online,
    }
}

fn offline_snapshot(d: &Device) -> Snapshot {
    Snapshot {
        name: if d.name.is_empty() { "Wireless mouse".into() } else { d.name.clone() },
        link: d.link,
        online: false,
        battery: None,
        dpi_caps: None,
        dpi: None,
        rates: Vec::new(),
        rate: None,
        onboard: None,
    }
}

impl Worker {
    fn refresh(&mut self, emit: &dyn Fn(Evt)) {
        if self.api.is_none() {
            match HidApi::new() {
                Ok(a) => self.api = Some(a),
                Err(e) => return emit(Evt::Missing(format!("Couldn't initialize HID: {e}"))),
            }
        }
        if let Some(d) = &mut self.dev {
            match d.check() {
                Ok(()) => return emit(Evt::Snapshot(Box::new(snapshot(d)))),
                Err(e) if e.is_offline() => return emit(Evt::Snapshot(Box::new(offline_snapshot(d)))),
                Err(_) => self.dev = None, // unplugged: search again
            }
        }
        let api = self.api.as_mut().expect("api");
        let _ = api.refresh_devices();
        match device::discover(api, SW_UI) {
            Ok(mut d) => {
                let s = if d.online { snapshot(&mut d) } else { offline_snapshot(&d) };
                self.dev = Some(d);
                emit(Evt::Snapshot(Box::new(s)));
            }
            Err(e) => emit(Evt::Missing(e.to_string())),
        }
    }

    fn battery(&mut self, emit: &dyn Fn(Evt)) {
        let Some(d) = &mut self.dev else { return };
        match d.ping() {
            Ok(_) => {
                if let Ok(b) = d.battery() {
                    emit(Evt::Battery(b));
                }
            }
            Err(e) if e.is_offline() => emit(Evt::Offline),
            Err(_) => {
                self.dev = None;
                emit(Evt::Offline);
            }
        }
    }

    fn save(&mut self, cfg: &Config) -> Result<String, String> {
        let d = self.dev.as_mut().ok_or("No mouse is connected.")?;
        d.check().map_err(|e| format!("Couldn't talk to the mouse: {e}"))?;
        let mut c = cfg.clone();
        let msg = match cfg.storage {
            Storage::Onboard => {
                let code = cfg.report_rate_hz.and_then(|hz| d.profile_rate_code(hz));
                let (msg, sector) = d
                    .save_onboard(&cfg.dpi_stages, cfg.default_stage, code, &Config::backup_dir())
                    .map_err(|e| e.to_string())?;
                c.profile_sector = Some(sector);
                msg
            }
            Storage::Software => {
                d.apply_software(cfg.active_dpi(), cfg.report_rate_hz).map_err(|e| e.to_string())?;
                c.profile_sector = None;
                "Settings applied.".to_string()
            }
        };
        c.device = Some(d.name.clone());
        c.save().map_err(|e| format!("Applied to the mouse, but the configuration couldn't be saved: {e}"))?;
        Ok(msg)
    }

    fn restore(&mut self) -> Result<String, String> {
        let d = self.dev.as_mut().ok_or("No mouse is connected.")?;
        d.check().map_err(|e| format!("Couldn't talk to the mouse: {e}"))?;
        let path = onboard::list_backups(&Config::backup_dir())
            .into_iter()
            .next()
            .ok_or("There are no backups.")?;
        let b = onboard::read_backup(&path).map_err(|e| format!("Couldn't read the backup: {e}"))?;
        d.restore_backup(&b).map_err(|e| e.to_string())?;
        Ok("Mouse memory restored to how it was before Pluma.".into())
    }
}
