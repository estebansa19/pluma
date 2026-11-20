//! Diagnostic commands. Everything here is read-only except `--select`, which only switches
//! the active profile (it never writes the mouse memory).

use crate::device::{self, feat, SW_PROBE};
use crate::hidpp::{self, VID_LOGITECH};
use crate::onboard;
use std::fmt::Write;

/// `pluma --probe [file]`: read-only diagnostics (writes nothing to the mouse).
pub fn run(out_file: Option<&str>) {
    let mut s = String::new();
    if let Err(e) = report(&mut s) {
        let _ = writeln!(s, "\nerror: {e}");
    }
    print!("{s}");
    if let Some(p) = out_file {
        let _ = std::fs::write(p, &s);
    }
}

/// `pluma --dump [file]`: dumps whole onboard memory sectors (read-only).
pub fn dump(out_file: Option<&str>) {
    let mut s = String::new();
    if let Err(e) = dump_inner(&mut s) {
        let _ = writeln!(s, "\nerror: {e}");
    }
    print!("{s}");
    if let Some(p) = out_file {
        let _ = std::fs::write(p, &s);
    }
}

fn crc_variants(d: &[u8]) -> String {
    let n = d.len();
    let mut out = Vec::new();
    for len in [n - 2, n.saturating_sub(3), n.saturating_sub(4)] {
        let c = onboard::crc16(&d[..len]);
        let at_be = u16::from_be_bytes([d[len], d[len + 1]]);
        let at_le = u16::from_le_bytes([d[len], d[len + 1]]);
        if c == at_be {
            out.push(format!("ccitt[..{len}]=BE@{len}"));
        }
        if c == at_le {
            out.push(format!("ccitt[..{len}]=LE@{len}"));
        }
    }
    if out.is_empty() {
        format!("no match (crc[..{}]={:04X}, last={})", n - 2, onboard::crc16(&d[..n - 2]), hex(&d[n - 4..]))
    } else {
        out.join(", ")
    }
}

fn dump_inner(s: &mut String) -> Result<(), Box<dyn std::error::Error>> {
    let api = hidapi::HidApi::new()?;
    let mut d = device::discover(&api, SW_PROBE)?;
    if !d.online {
        writeln!(s, "mouse asleep")?;
        return Ok(());
    }
    let desc = d.onboard_desc()?;
    writeln!(s, "{desc:?}")?;
    writeln!(s, "0x8090 fn0: {:?}", d.call(0x8090, 0, &[]).map(|r| hex(&r)))?;
    writeln!(s, "0x80E0 fn0: {:?}", d.call(0x80E0, 0, &[]).map(|r| hex(&r)))?;
    for sector in [0x0000u16, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0100, 0x0101] {
        match d.read_sector(sector, desc.sector_size) {
            Ok(data) => {
                writeln!(s, "\n== sector {sector:#06X} ({} bytes) crc: {}", data.len(), crc_variants(&data))?;
                for (i, row) in data.chunks(16).enumerate() {
                    writeln!(s, "  {:03X}: {}", i * 16, hex(row))?;
                }
            }
            Err(e) => writeln!(s, "\n== sector {sector:#06X}: {e}")?,
        }
    }
    // Can we read past byte 255?
    let s0 = 0u16.to_be_bytes();
    writeln!(s, "\nread at offset 240 (16 bytes): {:?}", d.call(feat::ONBOARD_PROFILES, 5, &[s0[0], s0[1], 0, 240]).map(|r| hex(&r)))?;
    Ok(())
}

/// `pluma --status`: one line with the mouse state (read-only).
pub fn status() {
    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let api = hidapi::HidApi::new()?;
        let mut d = device::discover(&api, SW_PROBE)?;
        if !d.online {
            return Ok("asleep".into());
        }
        Ok(format!(
            "awake  mode {:?}  profile {:?}  DPI {:?}  battery {:?}",
            d.onboard_mode(),
            d.current_profile().map(|v| format!("{v:#06X}")),
            d.get_dpi(),
            d.battery().map(|b| b.percent)
        ))
    };
    match run() {
        Ok(s) => println!("{s}"),
        Err(e) => println!("error: {e}"),
    }
}

/// `pluma --select 0x0101`: tries to activate a profile (doesn't write memory) and prints the result.
pub fn select(arg: Option<&str>) {
    let sector = arg.and_then(|a| u16::from_str_radix(a.trim_start_matches("0x"), 16).ok());
    let Some(sector) = sector else {
        println!("usage: pluma --select 0x0101");
        return;
    };
    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let api = hidapi::HidApi::new()?;
        let mut d = device::discover(&api, SW_PROBE)?;
        d.check()?;
        let before = d.current_profile()?;
        let mode = d.onboard_mode()?;
        let r = d.set_current_profile(sector);
        let after = d.current_profile()?;
        Ok(format!(
            "mode {mode}  before {before:#06X}  activate {sector:#06X} → {r:?}  after {after:#06X}  DPI {:?}",
            d.get_dpi()
        ))
    };
    match run() {
        Ok(s) => println!("{s}"),
        Err(e) => println!("error: {e}"),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
}

fn report(s: &mut String) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(s, "== Pluma: diagnostics (read-only) ==")?;
    let api = hidapi::HidApi::new()?;

    writeln!(s, "\n-- Logitech HID collections --")?;
    for d in api.device_list().filter(|d| d.vendor_id() == VID_LOGITECH) {
        writeln!(
            s,
            "  PID {:04X} if {:>2} usage {:04X}:{:04X} '{}'",
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string().unwrap_or(""),
        )?;
    }

    writeln!(s, "\n-- HID++ candidates --")?;
    for c in hidpp::candidates(&api) {
        writeln!(s, "  PID {:04X} if {} '{}' short={}", c.product_id, c.interface, c.product, c.short_path.is_some())?;
    }

    writeln!(s, "\n-- Discovery --")?;
    let mut d = match device::discover(&api, SW_PROBE) {
        Ok(d) => d,
        Err(e) => {
            writeln!(s, "  {e}")?;
            return Ok(());
        }
    };
    writeln!(
        s,
        "  name: '{}'  index: {:#04X}  link: {:?}  online: {}  protocol: {}.{}  type: {}",
        d.name, d.index, d.link, d.online, d.protocol.0, d.protocol.1, d.kind
    )?;
    if !d.online {
        writeln!(s, "  (the mouse is asleep: move it and run again)")?;
        return Ok(());
    }
    if let Ok(f) = d.receiver_notification_flags() {
        writeln!(s, "  receiver: notification register = {}", hex(&f))?;
    }

    writeln!(s, "\n-- Features --")?;
    match d.list_features() {
        Ok(list) => {
            for f in list {
                writeln!(s, "  [{:2}] {:04X} v{} type {:02X}  {}", f.index, f.id, f.version, f.kind, device::feature_name(f.id))?;
            }
        }
        Err(e) => writeln!(s, "  error: {e}")?,
    }

    writeln!(s, "\n-- DPI --")?;
    match d.dpi_caps() {
        Ok(c) => writeln!(s, "  range {}..{} step {} ({} values)", c.min(), c.max(), c.step(), c.values.len())?,
        Err(e) => writeln!(s, "  caps: {e}")?,
    }
    writeln!(s, "  current: {:?}", d.get_dpi())?;
    if d.has(feat::ADJUSTABLE_DPI) {
        writeln!(s, "  0x2201 fn1 list: {:?}", d.call(feat::ADJUSTABLE_DPI, 1, &[0]).map(|r| hex(&r)))?;
        writeln!(s, "  0x2201 fn2 current: {:?}", d.call(feat::ADJUSTABLE_DPI, 2, &[0]).map(|r| hex(&r)))?;
    }
    if d.has(feat::EXTENDED_DPI) {
        for (f, p) in [(1u8, vec![0u8]), (2, vec![0, 0, 0]), (2, vec![0, 1, 0]), (5, vec![0])] {
            writeln!(s, "  0x2202 fn{f} {:?}: {:?}", p, d.call(feat::EXTENDED_DPI, f, &p).map(|r| hex(&r)))?;
        }
    }

    writeln!(s, "\n-- Report rate --")?;
    writeln!(s, "  kind {:?}  options {:?}  current {:?}", d.rate_kind(), d.rate_options(), d.get_rate_hz())?;
    if d.has(feat::EXTENDED_REPORT_RATE) {
        for f in 0..3u8 {
            writeln!(s, "  0x8061 fn{f}: {:?}", d.call(feat::EXTENDED_REPORT_RATE, f, &[]).map(|r| hex(&r)))?;
        }
    }

    writeln!(s, "\n-- Battery --")?;
    writeln!(s, "  {:?}", d.battery())?;

    writeln!(s, "\n-- Onboard memory --")?;
    match d.onboard_desc() {
        Ok(desc) => {
            writeln!(s, "  {desc:?}")?;
            writeln!(
                s,
                "  mode {:?} (1=onboard, 2=software)  current profile {:?}  current DPI index {:?}",
                d.onboard_mode(),
                d.current_profile().map(|v| format!("{v:#06X}")),
                d.current_dpi_index()
            )?;
            for (label, sec) in [("user directory", 0x0000u16), ("ROM directory", 0x0100)] {
                match d.read_sector(sec, desc.sector_size) {
                    Ok(data) => writeln!(
                        s,
                        "  {label} {sec:#06X}: crc_ok={}  {}",
                        onboard::sector_crc_ok(&data),
                        hex(&data[..32.min(data.len())])
                    )?,
                    Err(e) => writeln!(s, "  {label} {sec:#06X}: {e}")?,
                }
            }
            let (entries, from_rom) = d.read_directory(&desc)?;
            writeln!(s, "  directory in use: {}  {:?}", if from_rom { "ROM (factory)" } else { "user" }, entries)?;
            let layout = onboard::Layout::for_format(desc.profile_format);
            writeln!(s, "  profile layout: {layout:?}")?;
            for e in &entries {
                let Some(layout) = layout else { break };
                match d.read_sector(e.sector, desc.sector_size) {
                    Ok(data) => {
                        let core = onboard::parse_profile(&data, layout);
                        writeln!(
                            s,
                            "  profile {:#06X} enabled={} crc_ok={} valid={}\n    {:?}",
                            e.sector,
                            e.enabled,
                            onboard::sector_crc_ok(&data),
                            onboard::looks_valid(&core),
                            core
                        )?;
                        writeln!(s, "    {}", hex(&data[..48.min(data.len())]))?;
                    }
                    Err(err) => writeln!(s, "  profile {:#06X}: {err}", e.sector)?,
                }
            }
            writeln!(s, "  summary: {:?}", d.onboard_summary())?;
        }
        Err(e) => writeln!(s, "  {e}")?,
    }
    Ok(())
}
