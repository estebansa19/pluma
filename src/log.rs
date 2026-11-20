//! Minimal log at %APPDATA%\Pluma\pluma.log (trims itself; never grows past ~64 KB).

use crate::config::Config;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX: u64 = 64 * 1024;

pub fn write(msg: &str) {
    let path = Config::dir().join("pluma.log");
    let _ = std::fs::create_dir_all(Config::dir());
    if std::fs::metadata(&path).map_or(false, |m| m.len() > MAX) {
        // Keep the second half.
        if let Ok(old) = std::fs::read(&path) {
            let keep = &old[old.len() / 2..];
            let start = keep.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
            let _ = std::fs::write(&path, &keep[start..]);
        }
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let who = if std::env::args().any(|a| a == "--tray") { "agent" } else { "window" };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{h:02}:{m:02}:{s:02} UTC [{who}] {msg}");
    }
}
