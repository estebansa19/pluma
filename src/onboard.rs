//! Onboard memory format (feature 0x8100): CRC-protected sectors, the profile directory and
//! the part of a profile we touch (report rate and DPI levels).
//!
//! Conservative design: when saving, the whole sector is read, only the report rate, default
//! DPI index, DPI-shift index and the 5 DPI levels are modified, the CRC is recomputed and the
//! result is verified by reading it back. The rest of the profile (buttons, lighting, macros)
//! stays exactly as it was.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DPI_SLOTS: usize = 5;

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF), stored big-endian at the end of the sector.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn sector_crc_ok(s: &[u8]) -> bool {
    let n = s.len();
    n >= 4 && crc16(&s[..n - 2]) == u16::from_be_bytes([s[n - 2], s[n - 1]])
}

pub fn seal(s: &mut [u8]) {
    let n = s.len();
    let c = crc16(&s[..n - 2]).to_be_bytes();
    s[n - 2] = c[0];
    s[n - 1] = c[1];
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub sector: u16,
    pub enabled: bool,
}

/// Directory: 4-byte entries [sector BE16, enabled, 0] terminated by 0xFFFF.
pub fn parse_directory(data: &[u8], max: usize) -> Vec<DirEntry> {
    let limit = data.len().saturating_sub(2);
    let mut out = Vec::new();
    for i in 0..max.min(16) {
        let o = i * 4;
        if o + 4 > limit {
            break;
        }
        let sector = u16::from_be_bytes([data[o], data[o + 1]]);
        if sector == 0xFFFF || sector == 0x0000 {
            break;
        }
        out.push(DirEntry { sector, enabled: data[o + 2] != 0 });
    }
    out
}

pub fn build_directory(entries: &[DirEntry], size: usize) -> Vec<u8> {
    let mut d = vec![0xFFu8; size];
    for (i, e) in entries.iter().enumerate() {
        let o = i * 4;
        d[o..o + 2].copy_from_slice(&e.sector.to_be_bytes());
        d[o + 2] = e.enabled as u8;
        d[o + 3] = 0;
    }
    let o = entries.len() * 4;
    d[o..o + 4].copy_from_slice(&[0xFF, 0xFF, 0x00, 0x00]);
    seal(&mut d);
    d
}

/// Layout of the first bytes of a profile, depending on the format the mouse reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Formats 1–4 (G402, G502 HERO, G PRO Wireless…), documented by libratbag/Solaar:
    /// `[0]` report rate, `[1]` default level, `[2]` shift level, `[3..13]` 5 × DPI u16 LE.
    Classic,
    /// Format 7 (PRO X 2 / Superlight 2), inferred from real dumps:
    /// `[0]` report rate (0x8061 index), `[1]` ?, `[2]` default level, `[3]` shift level,
    /// `[4..29]` 5 × (X u16 LE, Y u16 LE, LOD u8).
    Extended,
}

impl Layout {
    pub fn for_format(format: u8) -> Option<Layout> {
        match format {
            1..=4 => Some(Layout::Classic),
            7 => Some(Layout::Extended),
            _ => None,
        }
    }

    fn header(self) -> (usize, usize, usize) {
        match self {
            Layout::Classic => (0, 1, 2),
            Layout::Extended => (0, 2, 3),
        }
    }

    /// Offsets of the power timers (u16 LE, seconds).
    pub fn power_offsets(self) -> (usize, usize) {
        match self {
            Layout::Classic => (28, 30),
            Layout::Extended => (44, 46),
        }
    }
}

/// The parts of a profile we care about.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileCore {
    pub layout: Layout,
    /// Classic: milliseconds between reports (1 = 1000 Hz). Extended: 0x8061 index (3 = 1000 Hz).
    pub report_rate: u8,
    /// Index of the DPI level the profile starts with.
    pub default_dpi: u8,
    /// Index of the level used by the DPI-shift button.
    pub shift_dpi: u8,
    /// Up to 5 levels; a 0 or 0xFFFF marks the end.
    pub dpis: Vec<u16>,
    /// Classic: value this mouse uses for empty slots (0x0000 or 0xFFFF).
    pub filler: u16,
    /// Extended: lift-off distance of each slot.
    pub lods: [u8; DPI_SLOTS],
    /// Power timers (seconds): sleep and power-off.
    pub sleep_secs: (u16, u16),
    pub name: String,
}

pub fn parse_profile(d: &[u8], layout: Layout) -> ProfileCore {
    let mut dpis = Vec::new();
    let mut filler = 0u16;
    let mut lods = [0u8; DPI_SLOTS];
    for i in 0..DPI_SLOTS {
        let v = match layout {
            Layout::Classic => {
                let o = 3 + 2 * i;
                u16::from_le_bytes([d[o], d[o + 1]])
            }
            Layout::Extended => {
                let o = 4 + 5 * i;
                lods[i] = d[o + 4];
                u16::from_le_bytes([d[o], d[o + 1]])
            }
        };
        if v == 0 || v == 0xFFFF {
            if dpis.len() == i {
                filler = v;
            }
            continue;
        }
        if dpis.len() == i {
            dpis.push(v);
        }
    }
    let name = if d.len() >= 208 {
        let units: Vec<u16> = d[160..208]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0 && u != 0xFFFF)
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::new()
    };
    let (r, def, shift) = layout.header();
    let (ps, po) = layout.power_offsets();
    ProfileCore {
        layout,
        report_rate: d[r],
        default_dpi: d[def],
        shift_dpi: d[shift],
        dpis,
        filler,
        lods,
        sleep_secs: (u16::from_le_bytes([d[ps], d[ps + 1]]), u16::from_le_bytes([d[po], d[po + 1]])),
        name,
    }
}

/// Writes report rate, default level, shift level and DPI levels; recomputes the CRC.
pub fn write_core(d: &mut [u8], c: &ProfileCore) {
    let (r, def, shift) = c.layout.header();
    d[r] = c.report_rate;
    d[def] = c.default_dpi;
    d[shift] = c.shift_dpi;
    for i in 0..DPI_SLOTS {
        match c.layout {
            Layout::Classic => {
                let v = c.dpis.get(i).copied().unwrap_or(c.filler);
                d[3 + 2 * i..5 + 2 * i].copy_from_slice(&v.to_le_bytes());
            }
            Layout::Extended => {
                let o = 4 + 5 * i;
                let v = c.dpis.get(i).copied().unwrap_or(0);
                d[o..o + 2].copy_from_slice(&v.to_le_bytes());
                d[o + 2..o + 4].copy_from_slice(&v.to_le_bytes());
                let lod = if c.lods[i] <= 0x10 { c.lods[i] } else { c.lods[0] };
                d[o + 4] = lod;
            }
        }
    }
    seal(d);
}

/// Sanity check: if this fails we don't recognize the format and write nothing.
pub fn looks_valid(c: &ProfileCore) -> bool {
    !c.dpis.is_empty()
        && c.dpis.iter().all(|&v| (50..=50_000).contains(&v))
        && (c.default_dpi as usize) < c.dpis.len()
        && (c.layout == Layout::Classic || c.lods.iter().all(|&l| l <= 0x10))
}

// ---------- backups ----------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Backup {
    pub device: String,
    pub created: u64,
    pub sector_size: u16,
    pub sectors: Vec<BackupSector>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BackupSector {
    pub sector: u16,
    pub hex: String,
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

pub fn write_backup(
    dir: &Path,
    device: &str,
    sector_size: u16,
    sectors: &[(u16, Vec<u8>)],
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let created = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let b = Backup {
        device: device.to_string(),
        created,
        sector_size,
        sectors: sectors.iter().map(|(s, d)| BackupSector { sector: *s, hex: to_hex(d) }).collect(),
    };
    let path = dir.join(format!("{created}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&b)?)?;
    prune_backups(dir);
    Ok(path)
}

/// Backups sorted from oldest to newest.
pub fn list_backups(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map_or(false, |e| e == "json"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Always keeps the first backup (the original state before Pluma) and the 5 most recent ones.
fn prune_backups(dir: &Path) {
    let all = list_backups(dir);
    if all.len() <= 6 {
        return;
    }
    for p in &all[1..all.len() - 5] {
        let _ = std::fs::remove_file(p);
    }
}

pub fn read_backup(path: &Path) -> std::io::Result<Backup> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_ccitt_false() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn directory_roundtrip() {
        let e = vec![DirEntry { sector: 1, enabled: true }, DirEntry { sector: 2, enabled: false }];
        let d = build_directory(&e, 255);
        assert!(sector_crc_ok(&d));
        assert_eq!(parse_directory(&d, 5), e);
    }

    #[test]
    fn profile_edit_keeps_rest() {
        let mut d: Vec<u8> = (0..255u32).map(|i| (i * 7) as u8).collect();
        d[0] = 1;
        d[1] = 1;
        d[2] = 0;
        for (i, v) in [400u16, 800, 1600, 0, 0].iter().enumerate() {
            d[3 + 2 * i..5 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        seal(&mut d);
        let before = d.clone();
        let mut c = parse_profile(&d, Layout::Classic);
        assert_eq!(c.dpis, vec![400, 800, 1600]);
        assert!(looks_valid(&c));
        c.dpis = vec![1200, 3200];
        c.default_dpi = 0;
        write_core(&mut d, &c);
        assert!(sector_crc_ok(&d));
        assert_eq!(&d[13..253], &before[13..253]);
        assert_eq!(parse_profile(&d, Layout::Classic).dpis, vec![1200, 3200]);
    }

    fn unhex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    /// Real sector 0x0001 of a PRO X 2 (format 7) written by G HUB.
    fn pro_x2_profile() -> Vec<u8> {
        unhex(
            "06 03 00 00 60 09 60 09 02 00 00 00 00 02 00 00 00 00 02 00 00 00 00 02 00 00 00 00 02 00 00 00
             00 FF 00 FF FF 00 FF FF FF FF FF FF 3C 00 2C 01 80 01 00 01 80 01 00 02 80 01 00 04 80 01 00 08
             80 01 00 10 FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF
             FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF 80 01 00 01 80 01 00 02 80 01 00 04 80 01 00 08
             80 01 00 10 FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF
             50 00 52 00 4F 00 46 00 49 00 4C 00 45 00 5F 00 4E 00 41 00 4D 00 45 00 5F 00 44 00 45 00 46 00
             41 00 55 00 4C 00 54 00 00 00 00 00 00 00 00 00 03 00 00 00 00 00 1F 40 00 00 00 03 00 00 00 00
             00 1F 40 00 00 00 03 00 00 00 00 00 1F 40 32 00 00 03 00 00 00 00 00 1F 40 32 00 00 03 12 1B",
        )
    }

    #[test]
    fn real_format7_profile() {
        let d = pro_x2_profile();
        assert_eq!(d.len(), 255);
        assert!(sector_crc_ok(&d));
        let c = parse_profile(&d, Layout::Extended);
        assert_eq!(c.dpis, vec![2400]);
        assert_eq!(c.default_dpi, 0);
        assert_eq!(c.report_rate, 6); // 8000 Hz
        assert_eq!(c.sleep_secs, (60, 300));
        assert_eq!(c.name, "PROFILE_NAME_DEFAULT");
        assert!(looks_valid(&c));
    }

    #[test]
    fn format7_edit_only_touches_dpi_block() {
        let mut d = pro_x2_profile();
        let before = d.clone();
        let mut c = parse_profile(&d, Layout::Extended);
        c.dpis = vec![800, 1600, 3200];
        c.default_dpi = 1;
        c.report_rate = 3;
        write_core(&mut d, &c);
        assert!(sector_crc_ok(&d));
        assert_eq!(d[1], before[1]); // unknown byte untouched
        assert_eq!(&d[29..253], &before[29..253]); // buttons, name, lighting: untouched
        assert_eq!(&d[4..9], &[0x20, 0x03, 0x20, 0x03, 0x02]);
        assert_eq!(&d[9..14], &[0x40, 0x06, 0x40, 0x06, 0x02]);
        assert_eq!(&d[24..29], &[0x00, 0x00, 0x00, 0x00, 0x02]);
        let back = parse_profile(&d, Layout::Extended);
        assert_eq!(back.dpis, vec![800, 1600, 3200]);
        assert_eq!(back.default_dpi, 1);
        assert_eq!(back.report_rate, 3);
    }

    #[test]
    fn ghub_directory_without_crc() {
        let mut d = vec![0xFFu8; 255];
        d[..24].copy_from_slice(&unhex("00 01 01 00 00 02 00 00 00 03 00 00 00 04 00 00 00 05 00 00 FF FF 00 00"));
        for b in &mut d[0x70..] {
            *b = 0;
        }
        let e = parse_directory(&d, 5);
        assert_eq!(e.len(), 5);
        assert_eq!(e[0], DirEntry { sector: 1, enabled: true });
        assert!(!e[1].enabled);
    }

    #[test]
    fn hex_roundtrip() {
        let b = vec![0u8, 1, 0xab, 0xff];
        assert_eq!(from_hex(&to_hex(&b)).unwrap(), b);
    }
}
