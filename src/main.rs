#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The binary (and so the crate) is called "Pluma" so the file is Pluma.exe.
#![allow(non_snake_case)]

//! Pluma: ultralight configurator for Logitech G mice (a minimal G HUB alternative).
//!
//! - `Pluma`                opens the settings window (fully exits when closed)
//! - `Pluma --tray`         tray agent: battery and reapplying settings after wake-up
//! - `Pluma --status`       one line with the mouse state (read-only)
//! - `Pluma --probe [file]` read-only diagnostics
//! - `Pluma --dump [file]`  onboard memory dump (read-only)
//! - `Pluma --select 0x01`  activates a profile without writing memory (debugging)

mod agent;
mod config;
mod device;
mod hidpp;
mod icon;
mod log;
mod onboard;
mod probe;
mod shell;
mod sys;
mod ui;
mod worker;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--probe") => {
            sys::attach_parent_console();
            probe::run(args.get(1).map(String::as_str));
        }
        Some("--dump") => {
            sys::attach_parent_console();
            probe::dump(args.get(1).map(String::as_str));
        }
        Some("--status") => {
            sys::attach_parent_console();
            probe::status();
        }
        Some("--select") => {
            sys::attach_parent_console();
            probe::select(args.get(1).map(String::as_str));
        }
        Some("--tray") => agent::run(),
        _ => {
            if let Err(e) = ui::run() {
                sys::error_box(&format!("Couldn't open the Pluma window:\n{e}"));
            }
        }
    }
}
