//! High-level Logitech mouse: discovery, HID++ 2.0 features, DPI, report rate, battery and
//! onboard memory.

use crate::hidpp::{self, Error, Result, Transport, INDEX_DIRECT};
use crate::onboard::{self, Backup, DirEntry, ProfileCore};
use hidapi::HidApi;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Software IDs (4 bits) so responses aren't mixed up between processes.
pub const SW_UI: u8 = 0x0A;
pub const SW_AGENT: u8 = 0x0B;
pub const SW_PROBE: u8 = 0x0C;

const TIMEOUT: Duration = Duration::from_millis(1500);
const PING_TIMEOUT: Duration = Duration::from_millis(700);

pub mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const WIRELESS_STATUS: u16 = 0x1D4B;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const EXTENDED_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXTENDED_REPORT_RATE: u16 = 0x8061;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
}

pub fn feature_name(id: u16) -> &'static str {
    match id {
        0x0000 => "Root",
        0x0001 => "Feature set",
        0x0002 => "Feature info",
        0x0003 => "Firmware",
        0x0005 => "Device name",
        0x0007 => "Friendly name",
        0x0008 => "Keep alive",
        0x0020 => "Config change",
        0x0021 => "Unique identifier",
        0x00C2 => "DFU control",
        0x00C3 => "Signed DFU control",
        0x00D0 => "DFU",
        0x1000 => "Battery status",
        0x1001 => "Battery voltage",
        0x1004 => "Unified battery",
        0x1010 => "Charging control",
        0x1300 => "LED control",
        0x1802 => "Reset",
        0x1803 => "GPIO",
        0x1806 => "Device properties",
        0x1814 => "Change host",
        0x1815 => "Hosts info",
        0x1830 => "Power modes",
        0x1890 => "RF test",
        0x18A1 => "LED test",
        0x18B0 => "Monitor",
        0x1B04 => "Reprogrammable controls v4",
        0x1B10 => "Control list",
        0x1D4B => "Wireless device status",
        0x1DF3 => "Debug",
        0x1E00 => "Hidden features",
        0x1E02 => "Deactivatable features",
        0x1E22 => "Direct SPI",
        0x1EB0 => "TDE",
        0x1F1F => "Firmware properties",
        0x1F20 => "ADC measurement",
        0x2201 => "Adjustable DPI",
        0x2202 => "Extended DPI",
        0x2230 => "Angle snapping",
        0x2240 => "Surface tuning",
        0x2250 => "XY stats",
        0x2251 => "Wheel stats",
        0x2400 => "Hybrid tracking",
        0x8010 => "G-keys",
        0x8020 => "M-keys",
        0x8030 => "Macro recording",
        0x8040 => "Brightness",
        0x8060 => "Report rate",
        0x8061 => "Extended report rate",
        0x8070 => "LED effects",
        0x8071 => "RGB effects",
        0x8080 => "Per-key lighting",
        0x8081 => "Per-key lighting v2",
        0x8090 => "Mode status",
        0x8100 => "Onboard profiles",
        0x8110 => "Mouse button spy",
        0x8111 => "Latency monitoring",
        0x8120 => "Gaming attachments",
        0x8123 => "Force feedback",
        0x8300 => "Sidetone",
        0x8310 => "Equalizer",
        _ => "",
    }
}

fn feature_label(id: u16) -> &'static str {
    match id {
        feat::ONBOARD_PROFILES => "onboard memory (0x8100)",
        feat::ADJUSTABLE_DPI | feat::EXTENDED_DPI => "DPI adjustment",
        feat::REPORT_RATE | feat::EXTENDED_REPORT_RATE => "report rate",
        feat::UNIFIED_BATTERY | feat::BATTERY_VOLTAGE | feat::BATTERY_STATUS => "battery",
        _ => {
            let n = feature_name(id);
            if n.is_empty() { "unknown feature" } else { n }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Wired,
    Receiver,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    pub full: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DpiCaps {
    /// Supported values, sorted.
    pub values: Vec<u16>,
}

impl DpiCaps {
    pub fn fallback() -> DpiCaps {
        DpiCaps { values: (100..=25600).step_by(50).collect() }
    }
    pub fn min(&self) -> u16 {
        self.values.first().copied().unwrap_or(100)
    }
    pub fn max(&self) -> u16 {
        self.values.last().copied().unwrap_or(25600)
    }
    pub fn step(&self) -> u16 {
        self.values.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(50).max(1)
    }
    pub fn snap(&self, v: u16) -> u16 {
        if self.values.is_empty() {
            return v;
        }
        match self.values.binary_search(&v) {
            Ok(_) => v,
            Err(0) => self.values[0],
            Err(i) if i >= self.values.len() => self.values[self.values.len() - 1],
            Err(i) => {
                let (a, b) = (self.values[i - 1], self.values[i]);
                if v - a <= b - v { a } else { b }
            }
        }
    }
}

/// HID++ DPI list: u16 BE values; 0xE000|step means "from the previous to the next value with this step".
pub fn parse_dpi_list(bytes: &[u8]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let v = u16::from_be_bytes([bytes[i], bytes[i + 1]]);
        if v == 0 {
            break;
        }
        if v >> 13 == 0b111 {
            let step = (v & 0x1FFF).max(1);
            if i + 3 >= bytes.len() {
                break;
            }
            let last = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]);
            let Some(&first) = out.last() else { break };
            let mut cur = first;
            while cur.saturating_add(step) <= last && out.len() < 4096 {
                cur += step;
                out.push(cur);
            }
            i += 4;
        } else {
            out.push(v);
            i += 2;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn voltage_to_percent(mv: u16) -> u8 {
    const T: [(u16, u8); 13] = [
        (4186, 100), (4067, 90), (3989, 80), (3922, 70), (3859, 60), (3811, 50), (3778, 40),
        (3751, 30), (3717, 20), (3671, 10), (3646, 5), (3579, 2), (3500, 0),
    ];
    if mv >= T[0].0 {
        return 100;
    }
    for w in T.windows(2) {
        let (hv, hp) = w[0];
        let (lv, lp) = w[1];
        if mv >= lv {
            let span = (hv - lv) as u32;
            let pos = (mv - lv) as u32;
            return lp + ((pos * (hp - lp) as u32 + span / 2) / span) as u8;
        }
    }
    0
}

fn parse_unified_battery(p: &[u8]) -> Battery {
    let soc = p[0];
    let percent = if soc > 0 {
        soc.min(100)
    } else {
        match p[1] {
            l if l & 0x08 != 0 => 90,
            l if l & 0x04 != 0 => 50,
            l if l & 0x02 != 0 => 20,
            _ => 5,
        }
    };
    Battery { percent, charging: matches!(p[2], 1 | 2), full: p[2] == 3 }
}

fn parse_voltage_battery(p: &[u8]) -> Battery {
    let mv = u16::from_be_bytes([p[0], p[1]]);
    let flags = p[2];
    let external = flags & 0x80 != 0;
    Battery {
        percent: voltage_to_percent(mv),
        charging: external && flags & 0x07 == 0,
        full: external && flags & 0x07 == 1,
    }
}

fn parse_status_battery(p: &[u8]) -> Battery {
    Battery { percent: p[0].min(100), charging: matches!(p[2], 1 | 2 | 4), full: p[2] == 3 }
}

const EXT_RATES_HZ: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateKind {
    /// 0x8060: the value is the number of milliseconds between reports.
    Legacy,
    /// 0x8061: the value is an index 0..6 (125 Hz .. 8000 Hz).
    Extended,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OnboardDesc {
    pub memory_model: u8,
    pub profile_format: u8,
    pub macro_format: u8,
    pub profile_count: u8,
    pub profile_count_oob: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
    pub layout: u8,
    pub various: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OnboardSummary {
    pub mode_onboard: bool,
    pub writable: bool,
    pub why_not: Option<String>,
    /// Profile the mouse is using right now (0x01xx = factory profile).
    pub running: u16,
    /// User profile that Pluma edits and activates.
    pub target: u16,
    pub from_rom: bool,
    /// Contents of the profile being edited.
    pub profile: Option<ProfileCore>,
    pub profile_rate_hz: Option<u32>,
    pub profile_count: u8,
}

impl OnboardSummary {
    /// The mouse isn't using Pluma's profile (software mode, or it woke up with the factory one).
    pub fn needs_activation(&self) -> bool {
        !self.mode_onboard || self.running != self.target
    }
}

#[derive(Debug, Clone)]
pub struct FeatureInfo {
    pub index: u8,
    pub id: u16,
    pub kind: u8,
    pub version: u8,
}

pub enum DevEvent {
    /// Receiver notification: the wireless link was established (true) or lost (false).
    Link(bool),
    /// 0x1D4B event: the mouse reconnected and asks the software to reconfigure it.
    Reconfigure,
    Battery(Battery),
}

pub struct Device {
    pub t: Transport,
    pub index: u8,
    pub link: Link,
    pub online: bool,
    pub name: String,
    pub kind: u8,
    pub protocol: (u8, u8),
    sw: u8,
    features: HashMap<u16, u8>,
}

impl Device {
    fn new(t: Transport, index: u8, link: Link, sw: u8) -> Device {
        Device {
            t,
            index,
            link,
            online: false,
            name: String::new(),
            kind: 0xFF,
            protocol: (0, 0),
            sw,
            features: HashMap::new(),
        }
    }

    // ---------- basic protocol ----------

    pub fn ping(&mut self) -> Result<(u8, u8)> {
        match self.t.request(self.index, 0, 1, self.sw, &[0, 0, 0x5A], PING_TIMEOUT) {
            Ok(p) => Ok((p[0], p[1])),
            Err(e) => {
                if e.is_offline() {
                    self.online = false;
                }
                Err(e)
            }
        }
    }

    pub fn index_of(&mut self, feature: u16) -> Result<u8> {
        if feature == feat::ROOT {
            return Ok(0);
        }
        if let Some(&i) = self.features.get(&feature) {
            return if i == 0 { Err(Error::Unsupported(feature_label(feature))) } else { Ok(i) };
        }
        let id = feature.to_be_bytes();
        let r = self.t.request(self.index, 0, 0, self.sw, &[id[0], id[1]], TIMEOUT).map_err(|e| {
            if e.is_offline() {
                self.online = false;
            }
            e
        })?;
        self.features.insert(feature, r[0]);
        if r[0] == 0 {
            Err(Error::Unsupported(feature_label(feature)))
        } else {
            Ok(r[0])
        }
    }

    fn cached_index(&self, feature: u16) -> Option<u8> {
        self.features.get(&feature).copied().filter(|&i| i != 0)
    }

    pub fn has(&mut self, feature: u16) -> bool {
        self.index_of(feature).is_ok()
    }

    pub fn call(&mut self, feature: u16, func: u8, params: &[u8]) -> Result<[u8; 16]> {
        let idx = self.index_of(feature)?;
        let mut busy = 0;
        loop {
            match self.t.request(self.index, idx, func, self.sw, params, TIMEOUT) {
                Err(Error::Hidpp20(0x08)) if busy < 3 => {
                    busy += 1;
                    std::thread::sleep(Duration::from_millis(40));
                }
                Err(e) => {
                    if e.is_offline() {
                        self.online = false;
                    }
                    return Err(e);
                }
                Ok(r) => return Ok(r),
            }
        }
    }

    fn init(&mut self) -> Result<()> {
        self.features.clear();
        let (maj, min) = self.ping()?;
        if maj < 2 {
            return Err(Error::Invalid("HID++ 1.0 devices are not supported".into()));
        }
        self.protocol = (maj, min);
        self.online = true;
        self.name = self.read_name().unwrap_or_else(|_| "Logitech mouse".into());
        self.kind = self.call(feat::DEVICE_NAME, 2, &[]).map(|r| r[0]).unwrap_or(0xFF);
        for f in [feat::WIRELESS_STATUS, feat::UNIFIED_BATTERY, feat::BATTERY_VOLTAGE, feat::BATTERY_STATUS] {
            let _ = self.index_of(f);
        }
        Ok(())
    }

    /// Checks that the mouse responds; if it was asleep and woke up, re-initializes it.
    pub fn check(&mut self) -> Result<()> {
        let was_online = self.online;
        self.ping()?;
        if !was_online {
            self.init()?;
        }
        self.online = true;
        Ok(())
    }

    fn read_name(&mut self) -> Result<String> {
        let len = self.call(feat::DEVICE_NAME, 0, &[])?[0] as usize;
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            let r = self.call(feat::DEVICE_NAME, 1, &[bytes.len() as u8])?;
            let take = (len - bytes.len()).min(16);
            bytes.extend_from_slice(&r[..take]);
        }
        Ok(String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string())
    }

    pub fn is_mouse(&mut self) -> bool {
        matches!(self.kind, 3 | 4 | 5) || self.has(feat::ADJUSTABLE_DPI) || self.has(feat::EXTENDED_DPI)
    }

    pub fn list_features(&mut self) -> Result<Vec<FeatureInfo>> {
        let count = self.call(feat::FEATURE_SET, 0, &[])?[0];
        let mut v = vec![FeatureInfo { index: 0, id: 0, kind: 0, version: 0 }];
        for i in 1..=count {
            let r = self.call(feat::FEATURE_SET, 1, &[i])?;
            let id = u16::from_be_bytes([r[0], r[1]]);
            self.features.insert(id, i);
            v.push(FeatureInfo { index: i, id, kind: r[2], version: r[3] });
        }
        Ok(v)
    }

    // ---------- receiver ----------

    pub fn receiver_notification_flags(&mut self) -> Result<[u8; 3]> {
        if self.link != Link::Receiver {
            return Err(Error::Unsupported("receiver"));
        }
        self.t.register(INDEX_DIRECT, 0x81, 0x00, [0, 0, 0], TIMEOUT)
    }

    /// Asks the receiver to notify when the mouse connects/disconnects (flag 0x000100).
    pub fn enable_receiver_notifications(&mut self) {
        if self.link != Link::Receiver || !self.t.has_short() {
            return;
        }
        if let Ok(cur) = self.t.register(INDEX_DIRECT, 0x81, 0x00, [0, 0, 0], TIMEOUT) {
            if cur[1] & 0x01 == 0 {
                let _ = self.t.register(INDEX_DIRECT, 0x80, 0x00, [cur[0], cur[1] | 0x01, cur[2]], TIMEOUT);
            }
        }
    }

    /// Waits for a receiver notification or a mouse event.
    pub fn poll_event(&mut self, timeout_ms: i32) -> Result<Option<DevEvent>> {
        let Some(r) = self.t.next_report(timeout_ms)? else { return Ok(None) };
        if r.len() < 7 {
            return Ok(None);
        }
        match r[0] {
            hidpp::REPORT_SHORT if r[2] == 0x41 && self.link == Link::Receiver => {
                let up = r[3] & 0x40 == 0;
                if r[1] == self.index {
                    // After reconnecting, re-initialize on the next check().
                    self.online = false;
                    return Ok(Some(DevEvent::Link(up)));
                }
                if up && !self.online {
                    // Our mouse might be paired in another receiver slot.
                    let prev = self.index;
                    self.index = r[1];
                    if self.init().is_ok() && self.is_mouse() {
                        self.online = false;
                        return Ok(Some(DevEvent::Link(true)));
                    }
                    self.index = prev;
                    self.online = false;
                }
            }
            hidpp::REPORT_LONG | hidpp::REPORT_VERY_LONG
                if r.len() >= 20 && r[1] == self.index && (r[3] & 0x0F) == 0 =>
            {
                let fi = Some(r[2]);
                let p = &r[4..20];
                if fi == self.cached_index(feat::WIRELESS_STATUS) && r[3] >> 4 == 0 {
                    return Ok(Some(DevEvent::Reconfigure));
                }
                if fi == self.cached_index(feat::UNIFIED_BATTERY) {
                    return Ok(Some(DevEvent::Battery(parse_unified_battery(p))));
                }
                if fi == self.cached_index(feat::BATTERY_VOLTAGE) {
                    return Ok(Some(DevEvent::Battery(parse_voltage_battery(p))));
                }
                if fi == self.cached_index(feat::BATTERY_STATUS) {
                    return Ok(Some(DevEvent::Battery(parse_status_battery(p))));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    // ---------- battery ----------

    pub fn battery(&mut self) -> Result<Battery> {
        if self.has(feat::UNIFIED_BATTERY) {
            let r = self.call(feat::UNIFIED_BATTERY, 1, &[])?;
            return Ok(parse_unified_battery(&r));
        }
        if self.has(feat::BATTERY_VOLTAGE) {
            let r = self.call(feat::BATTERY_VOLTAGE, 0, &[])?;
            return Ok(parse_voltage_battery(&r));
        }
        if self.has(feat::BATTERY_STATUS) {
            let r = self.call(feat::BATTERY_STATUS, 0, &[])?;
            return Ok(parse_status_battery(&r));
        }
        Err(Error::Unsupported("battery"))
    }

    // ---------- DPI ----------

    pub fn dpi_caps(&mut self) -> Result<DpiCaps> {
        let values = if self.has(feat::ADJUSTABLE_DPI) {
            let r = self.call(feat::ADJUSTABLE_DPI, 1, &[0])?;
            parse_dpi_list(&r[1..])
        } else if self.has(feat::EXTENDED_DPI) {
            let mut bytes = Vec::new();
            for page in 0..16u8 {
                let r = self.call(feat::EXTENDED_DPI, 2, &[0, 0, page])?;
                bytes.extend_from_slice(&r[3..]);
                // The list ends with a 0x0000 aligned to the start of the full list.
                if bytes.chunks_exact(2).any(|c| c == [0, 0]) {
                    break;
                }
            }
            parse_dpi_list(&bytes)
        } else {
            return Err(Error::Unsupported("DPI adjustment"));
        };
        if values.is_empty() {
            return Err(Error::Invalid("the mouse returned an empty DPI list".into()));
        }
        Ok(DpiCaps { values })
    }

    pub fn get_dpi(&mut self) -> Result<u16> {
        if self.has(feat::ADJUSTABLE_DPI) {
            let r = self.call(feat::ADJUSTABLE_DPI, 2, &[0])?;
            let dpi = u16::from_be_bytes([r[1], r[2]]);
            return Ok(if dpi == 0 { u16::from_be_bytes([r[3], r[4]]) } else { dpi });
        }
        if self.has(feat::EXTENDED_DPI) {
            let r = self.call(feat::EXTENDED_DPI, 5, &[0])?;
            return Ok(u16::from_be_bytes([r[1], r[2]]));
        }
        Err(Error::Unsupported("DPI adjustment"))
    }

    pub fn set_dpi(&mut self, dpi: u16) -> Result<()> {
        let d = dpi.to_be_bytes();
        if self.has(feat::ADJUSTABLE_DPI) {
            self.call(feat::ADJUSTABLE_DPI, 3, &[0, d[0], d[1]])?;
            return Ok(());
        }
        if self.has(feat::EXTENDED_DPI) {
            let cur = self.call(feat::EXTENDED_DPI, 5, &[0])?;
            let lod = cur[9];
            self.call(feat::EXTENDED_DPI, 6, &[0, d[0], d[1], d[0], d[1], lod])?;
            return Ok(());
        }
        Err(Error::Unsupported("DPI adjustment"))
    }

    // ---------- report rate ----------

    pub fn rate_kind(&mut self) -> Option<RateKind> {
        if self.has(feat::EXTENDED_REPORT_RATE) {
            Some(RateKind::Extended)
        } else if self.has(feat::REPORT_RATE) {
            Some(RateKind::Legacy)
        } else {
            None
        }
    }

    pub fn rate_options(&mut self) -> Result<Vec<u32>> {
        match self.rate_kind() {
            Some(RateKind::Legacy) => {
                let mask = self.call(feat::REPORT_RATE, 0, &[])?[0];
                Ok([8u32, 4, 2, 1]
                    .iter()
                    .filter(|&&ms| mask & (1 << (ms - 1)) != 0)
                    .map(|ms| 1000 / ms)
                    .collect())
            }
            Some(RateKind::Extended) => {
                let r = self.call(feat::EXTENDED_REPORT_RATE, 1, &[])?;
                let mask = u16::from_be_bytes([r[0], r[1]]);
                Ok((0..EXT_RATES_HZ.len()).filter(|i| mask & (1 << i) != 0).map(|i| EXT_RATES_HZ[i]).collect())
            }
            None => Err(Error::Unsupported("report rate")),
        }
    }

    pub fn get_rate_hz(&mut self) -> Result<u32> {
        match self.rate_kind() {
            Some(RateKind::Legacy) => {
                let ms = self.call(feat::REPORT_RATE, 1, &[])?[0].max(1) as u32;
                Ok(1000 / ms)
            }
            Some(RateKind::Extended) => {
                let i = self.call(feat::EXTENDED_REPORT_RATE, 2, &[])?[0] as usize;
                EXT_RATES_HZ.get(i).copied().ok_or_else(|| Error::Invalid(format!("unknown report rate ({i})")))
            }
            None => Err(Error::Unsupported("report rate")),
        }
    }

    pub fn set_rate_hz(&mut self, hz: u32) -> Result<()> {
        match self.rate_kind() {
            Some(RateKind::Legacy) => {
                let ms = (1000 / hz.max(1)).clamp(1, 8) as u8;
                self.call(feat::REPORT_RATE, 2, &[ms])?;
                Ok(())
            }
            Some(RateKind::Extended) => {
                let i = EXT_RATES_HZ.iter().position(|&h| h == hz).ok_or(Error::Invalid(format!("{hz} Hz is not valid")))?;
                self.call(feat::EXTENDED_REPORT_RATE, 3, &[i as u8])?;
                Ok(())
            }
            None => Err(Error::Unsupported("report rate")),
        }
    }

    /// Value of byte 0 of the onboard profile for that rate (None = leave it untouched).
    pub fn profile_rate_code(&mut self, hz: u32) -> Option<u8> {
        match self.rate_kind() {
            Some(RateKind::Legacy) if [125, 250, 500, 1000].contains(&hz) => Some((1000 / hz) as u8),
            Some(RateKind::Extended) => EXT_RATES_HZ.iter().position(|&h| h == hz).map(|i| i as u8),
            _ => None,
        }
    }

    /// Rate in Hz encoded in byte 0 of a profile.
    pub fn profile_rate_hz(&mut self, code: u8) -> Option<u32> {
        match self.rate_kind() {
            Some(RateKind::Legacy) if (1..=8).contains(&code) => Some(1000 / code as u32),
            Some(RateKind::Extended) => EXT_RATES_HZ.get(code as usize).copied(),
            _ => None,
        }
    }

    /// Software (host) mode: the PC sets DPI and report rate; the mouse forgets them when it sleeps.
    pub fn apply_software(&mut self, dpi: u16, rate_hz: Option<u32>) -> Result<()> {
        if self.has(feat::ONBOARD_PROFILES) {
            self.set_onboard_mode(2)?;
        }
        self.set_dpi(dpi)?;
        if let Some(hz) = rate_hz {
            if self.get_rate_hz().ok() != Some(hz) {
                self.set_rate_hz(hz)?;
            }
        }
        Ok(())
    }

    // ---------- onboard memory (0x8100) ----------

    pub fn onboard_desc(&mut self) -> Result<OnboardDesc> {
        let r = self.call(feat::ONBOARD_PROFILES, 0, &[])?;
        Ok(OnboardDesc {
            memory_model: r[0],
            profile_format: r[1],
            macro_format: r[2],
            profile_count: r[3],
            profile_count_oob: r[4],
            button_count: r[5],
            sector_count: r[6],
            sector_size: u16::from_be_bytes([r[7], r[8]]),
            layout: r[9],
            various: r[10],
        })
    }

    /// 1 = mouse memory (onboard), 2 = software (host).
    pub fn onboard_mode(&mut self) -> Result<u8> {
        Ok(self.call(feat::ONBOARD_PROFILES, 2, &[])?[0])
    }

    pub fn set_onboard_mode(&mut self, mode: u8) -> Result<()> {
        self.call(feat::ONBOARD_PROFILES, 1, &[mode])?;
        Ok(())
    }

    pub fn current_profile(&mut self) -> Result<u16> {
        let r = self.call(feat::ONBOARD_PROFILES, 4, &[])?;
        Ok(u16::from_be_bytes([r[0], r[1]]))
    }

    pub fn set_current_profile(&mut self, sector: u16) -> Result<()> {
        self.call(feat::ONBOARD_PROFILES, 3, &sector.to_be_bytes())?;
        Ok(())
    }

    pub fn current_dpi_index(&mut self) -> Result<u8> {
        Ok(self.call(feat::ONBOARD_PROFILES, 0x0B, &[])?[0])
    }

    pub fn set_current_dpi_index(&mut self, idx: u8) -> Result<()> {
        self.call(feat::ONBOARD_PROFILES, 0x0C, &[idx])?;
        Ok(())
    }

    pub fn read_sector(&mut self, sector: u16, size: u16) -> Result<Vec<u8>> {
        let size = size as usize;
        if size < 16 {
            return Err(Error::Invalid(format!("invalid sector size ({size})")));
        }
        let mut out = vec![0u8; size];
        let mut off = 0usize;
        while off < size {
            // The mouse rejects reads past the end: the last chunk is read overlapping.
            let read_off = if off + 16 > size { size - 16 } else { off };
            let s = sector.to_be_bytes();
            let o = (read_off as u16).to_be_bytes();
            let chunk = self.call(feat::ONBOARD_PROFILES, 5, &[s[0], s[1], o[0], o[1]])?;
            let skip = off - read_off;
            let n = (size - off).min(16 - skip);
            out[off..off + n].copy_from_slice(&chunk[skip..skip + n]);
            off += n;
        }
        Ok(out)
    }

    fn write_sector(&mut self, sector: u16, data: &[u8]) -> Result<()> {
        let s = sector.to_be_bytes();
        let n = (data.len() as u16).to_be_bytes();
        self.call(feat::ONBOARD_PROFILES, 6, &[s[0], s[1], 0, 0, n[0], n[1]])?;
        let mut result = Ok(());
        for chunk in data.chunks(16) {
            let mut b = [0u8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            if let Err(e) = self.call(feat::ONBOARD_PROFILES, 7, &b) {
                result = Err(e);
                break;
            }
        }
        let end = self.call(feat::ONBOARD_PROFILES, 8, &[]);
        result?;
        end?;
        Ok(())
    }

    /// Writes only if something changed and verifies by reading back. Returns whether it wrote.
    fn write_sector_checked(&mut self, sector: u16, data: &[u8]) -> Result<bool> {
        let size = data.len() as u16;
        if let Ok(cur) = self.read_sector(sector, size) {
            if cur == data {
                return Ok(false);
            }
        }
        self.write_sector(sector, data)?;
        let back = self.read_sector(sector, size)?;
        if back != data {
            return Err(Error::Invalid(format!("verification of sector {sector:#06X} failed")));
        }
        Ok(true)
    }

    /// Profile directory: the user one (sector 0) if valid; otherwise the factory one (ROM).
    /// G HUB writes the format 7 directory without a CRC, so being coherent is enough.
    pub fn read_directory(&mut self, desc: &OnboardDesc) -> Result<(Vec<DirEntry>, bool)> {
        let max = desc.profile_count.max(1) as usize;
        let user = self.read_sector(0x0000, desc.sector_size)?;
        let e = onboard::parse_directory(&user, max);
        let coherent = !e.is_empty() && e.iter().all(|x| x.sector >= 1 && x.sector < desc.sector_count as u16);
        if coherent {
            return Ok((e, false));
        }
        let rom_max = desc.profile_count_oob.max(1) as usize;
        let entries = match self.read_sector(0x0100, desc.sector_size) {
            Ok(rom) if onboard::sector_crc_ok(&rom) => onboard::parse_directory(&rom, rom_max),
            _ => Vec::new(),
        };
        let entries = if entries.is_empty() {
            (1..=rom_max as u16).map(|i| DirEntry { sector: 0x0100 + i, enabled: true }).collect()
        } else {
            entries
        };
        Ok((entries, true))
    }

    /// Reads a profile and checks that we understand it. Factory (ROM) profiles have no CRC.
    fn read_profile(&mut self, sector: u16, desc: &OnboardDesc) -> Result<(Vec<u8>, ProfileCore)> {
        let layout = onboard::Layout::for_format(desc.profile_format).ok_or_else(|| {
            Error::Invalid(format!("Pluma doesn't know this mouse's profile format {} yet.", desc.profile_format))
        })?;
        let data = self.read_sector(sector, desc.sector_size)?;
        if sector < 0x0100 && !onboard::sector_crc_ok(&data) {
            return Err(Error::Invalid(
                "The profile stored in the mouse fails verification (CRC); leaving it untouched.".into(),
            ));
        }
        let core = onboard::parse_profile(&data, layout);
        if !onboard::looks_valid(&core) {
            return Err(Error::Invalid(
                "This mouse's profile contents aren't recognized; not modifying it for safety.".into(),
            ));
        }
        Ok((data, core))
    }

    /// (profile running now, user profile that Pluma edits).
    ///
    /// Note: the running one may be the factory profile (0x01xx) even if user profiles exist.
    /// That's what a PRO X 2 does when it wakes up without G HUB: it starts on the factory profile.
    pub fn profile_slots(&mut self, entries: &[DirEntry], from_rom: bool) -> Option<(u16, u16)> {
        let first = entries.iter().find(|e| e.enabled).or(entries.first())?.sector;
        let running = match self.current_profile() {
            Ok(c) if c != 0 => c,
            _ => first,
        };
        let target = if from_rom {
            running & 0x00FF
        } else if entries.iter().any(|e| e.sector == running) {
            running
        } else {
            first
        };
        Some((running, target))
    }

    pub fn onboard_summary(&mut self) -> Result<OnboardSummary> {
        let desc = self.onboard_desc()?;
        let mode_onboard = self.onboard_mode()? == 1;
        let (entries, from_rom) = self.read_directory(&desc)?;
        let (running, target) = self
            .profile_slots(&entries, from_rom)
            .ok_or_else(|| Error::Invalid("the mouse has no profiles".into()))?;
        let source = if from_rom { running } else { target };
        let (profile, why_not) = match self.read_profile(source, &desc) {
            Err(e) => (None, Some(e.to_string())),
            Ok((_, core)) => {
                let why = if desc.memory_model != 1 {
                    Some(format!("Unsupported onboard memory type ({}).", desc.memory_model))
                } else if target == 0 || target >= desc.sector_count as u16 {
                    Some("The profile isn't in a writable area.".to_string())
                } else if from_rom && core.layout == onboard::Layout::Extended {
                    Some("This mouse has no user profiles in its memory yet.".to_string())
                } else {
                    None
                };
                (Some(core), why)
            }
        };
        let profile_rate_hz = profile.as_ref().and_then(|p| self.profile_rate_hz(p.report_rate));
        Ok(OnboardSummary {
            mode_onboard,
            writable: why_not.is_none(),
            why_not,
            running,
            target,
            from_rom,
            profile,
            profile_rate_hz,
            profile_count: desc.profile_count,
        })
    }

    /// Saves DPI (and report rate) to the user profile in the mouse memory and activates it.
    pub fn save_onboard(
        &mut self,
        dpis: &[u16],
        default_idx: usize,
        rate_code: Option<u8>,
        backup_dir: &Path,
    ) -> Result<(String, u16)> {
        if dpis.is_empty() || dpis.len() > onboard::DPI_SLOTS {
            return Err(Error::Invalid("There must be between 1 and 5 DPI levels.".into()));
        }
        let desc = self.onboard_desc()?;
        if desc.memory_model != 1 {
            return Err(Error::Unsupported("this onboard memory type"));
        }
        let size = desc.sector_size;
        let (entries, from_rom) = self.read_directory(&desc)?;
        let (running, target) = self
            .profile_slots(&entries, from_rom)
            .ok_or_else(|| Error::Invalid("The mouse has no profiles.".into()))?;
        if target == 0 || target >= desc.sector_count as u16 {
            return Err(Error::Invalid(format!("Profile {target:#06X} isn't in a writable area.")));
        }
        // Without user profiles, start from the running factory profile; otherwise from the user profile.
        let active = if from_rom { running } else { target };

        let (original, mut core) = self.read_profile(active, &desc)?;
        if from_rom && core.layout == onboard::Layout::Extended {
            // Creating a format 7 directory from scratch isn't verified: better not risk it.
            return Err(Error::Invalid(
                "This mouse has no user profiles in its memory yet; Pluma doesn't create them from scratch for this format.".into(),
            ));
        }

        // G HUB leaves the profile directory without a CRC, and then the firmware refuses to
        // activate user profiles (that's why the mouse wakes up on the factory profile). Minimal
        // fix: the same directory, with the target profile enabled and its CRC at the end.
        let dir_fix: Option<(Vec<u8>, Vec<u8>)> = if from_rom {
            None
        } else {
            let dir = self.read_sector(0x0000, size)?;
            let mut fixed = dir.clone();
            if let Some(i) = entries.iter().position(|e| e.sector == target) {
                fixed[4 * i + 2] = 1;
            }
            onboard::seal(&mut fixed);
            (fixed != dir).then_some((dir, fixed))
        };

        // Back up everything that is about to be touched.
        let mut backup: Vec<(u16, Vec<u8>)> = Vec::new();
        if let Some((dir, _)) = &dir_fix {
            backup.push((0x0000, dir.clone()));
        }
        if from_rom {
            if let Ok(d) = self.read_sector(0x0000, size) {
                backup.push((0x0000, d));
            }
            for e in &entries {
                let u = e.sector & 0xFF;
                if u != 0 && u < desc.sector_count as u16 {
                    if let Ok(d) = self.read_sector(u, size) {
                        backup.push((u, d));
                    }
                }
            }
        } else {
            backup.push((active, original.clone()));
        }
        onboard::write_backup(backup_dir, &self.name, size, &backup)
            .map_err(|e| Error::Invalid(format!("Couldn't create the backup: {e}")))?;

        core.dpis = dpis.to_vec();
        core.default_dpi = default_idx.min(dpis.len() - 1) as u8;
        if core.shift_dpi != 0xFF && core.shift_dpi as usize >= dpis.len() {
            core.shift_dpi = (dpis.len() - 1) as u8;
        }
        if let Some(r) = rate_code {
            core.report_rate = r;
        }
        let mut updated = original.clone();
        onboard::write_core(&mut updated, &core);

        let mut written = 0;
        if from_rom {
            // First time: copy the factory profiles to the user area and create the directory.
            let mut new_entries = Vec::new();
            for e in &entries {
                let user = e.sector & 0xFF;
                if user == 0 || user >= desc.sector_count as u16 {
                    continue;
                }
                let data = if e.sector == active {
                    updated.clone()
                } else {
                    match self.read_profile(e.sector, &desc) {
                        Ok((mut d, _)) => {
                            onboard::seal(&mut d);
                            d
                        }
                        Err(_) => continue,
                    }
                };
                if self.write_sector_checked(user, &data)? {
                    written += 1;
                }
                new_entries.push(DirEntry { sector: user, enabled: e.enabled || e.sector == active });
            }
            let dir = onboard::build_directory(&new_entries, size as usize);
            if self.write_sector_checked(0x0000, &dir)? {
                written += 1;
            }
        } else {
            if self.write_sector_checked(active, &updated)? {
                written += 1;
            }
            // The directory goes after the profile: if something is interrupted halfway, the
            // mouse keeps booting as before.
            if let Some((_, fixed)) = &dir_fix {
                if self.write_sector_checked(0x0000, fixed)? {
                    written += 1;
                }
            }
        }

        // Switch to onboard mode and load our profile so it picks up the new values.
        let was_inactive = self.onboard_mode().ok() != Some(1) || running != target;
        self.set_onboard_mode(1)?;
        if let Err(e) = self.set_current_profile(target) {
            return Err(Error::Invalid(format!(
                "The profile was saved, but the mouse refuses to activate it ({e}). \
                 You can undo it with \"Restore original memory\"."
            )));
        }
        std::thread::sleep(Duration::from_millis(80));
        let _ = self.set_current_dpi_index(core.default_dpi);
        std::thread::sleep(Duration::from_millis(40));

        // Check that the sensor ended up on the chosen default DPI.
        let want = dpis[core.default_dpi as usize];
        let msg = match (written, was_inactive) {
            (0, true) => "Done: the mouse now uses your profile from its memory.",
            (0, false) => "The mouse already had these settings in its memory.",
            _ => "Saved to the mouse memory.",
        };
        let msg = match self.get_dpi() {
            Ok(now) if now != want => format!("{msg} (the sensor reports {now} DPI instead of {want})"),
            _ => msg.to_string(),
        };
        Ok((msg, target))
    }

    /// Activates a user profile if the mouse isn't using it. Returns whether anything changed.
    pub fn ensure_profile(&mut self, sector: u16) -> Result<bool> {
        let mut changed = false;
        if self.onboard_mode()? != 1 {
            self.set_onboard_mode(1)?;
            changed = true;
        }
        if self.current_profile()? != sector {
            self.set_current_profile(sector)?;
            changed = true;
        }
        Ok(changed)
    }

    pub fn restore_backup(&mut self, b: &Backup) -> Result<usize> {
        if b.device != self.name {
            return Err(Error::Invalid(format!("The backup belongs to another mouse ({}).", b.device)));
        }
        let mut sectors: Vec<(u16, Vec<u8>)> = Vec::new();
        for s in &b.sectors {
            let data = onboard::from_hex(&s.hex)
                .filter(|d| d.len() == b.sector_size as usize)
                .ok_or_else(|| Error::Invalid("The backup is corrupted.".into()))?;
            sectors.push((s.sector, data));
        }
        // Profiles first, directory last.
        sectors.sort_by_key(|(s, _)| *s == 0);
        let mut n = 0;
        for (sector, data) in &sectors {
            if self.write_sector_checked(*sector, data)? {
                n += 1;
            }
        }
        if let Ok(desc) = self.onboard_desc() {
            if let Ok((entries, from_rom)) = self.read_directory(&desc) {
                if let Some((_, target)) = self.profile_slots(&entries, from_rom) {
                    let _ = self.set_current_profile(if from_rom { target | 0x0100 } else { target });
                }
            }
        }
        Ok(n)
    }
}

/// Finds the mouse: first connected by cable, then in every slot of every receiver.
pub fn discover(api: &HidApi, sw: u8) -> Result<Device> {
    let cands = hidpp::candidates(api);
    if cands.is_empty() {
        return Err(Error::Invalid("No Logitech receiver or mouse is connected.".into()));
    }
    let mut sleeping: Option<Device> = None;
    let mut last_err: Option<Error> = None;
    for c in &cands {
        let t = match Transport::open(api, c) {
            Ok(t) => t,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        let mut d = Device::new(t, INDEX_DIRECT, Link::Wired, sw);
        if let Ok((maj, _)) = d.ping() {
            if maj >= 2 && d.init().is_ok() && d.is_mouse() {
                return Ok(d);
            }
        }
        d.link = Link::Receiver;
        let mut asleep_at: Option<u8> = None;
        for idx in 1..=6u8 {
            d.index = idx;
            d.features.clear();
            match d.ping() {
                Ok((maj, _)) if maj >= 2 => {
                    if d.init().is_ok() && d.is_mouse() {
                        return Ok(d);
                    }
                }
                Err(e) if e.is_offline() && asleep_at.is_none() => asleep_at = Some(idx),
                Err(e) => last_err = Some(e),
                _ => {}
            }
        }
        if let (Some(idx), None) = (asleep_at, &sleeping) {
            d.index = idx;
            d.online = false;
            d.features.clear();
            d.name.clear();
            sleeping = Some(d);
        }
    }
    if let Some(d) = sleeping {
        return Ok(d);
    }
    Err(last_err
        .map(|e| Error::Invalid(format!("No compatible Logitech mouse found ({e}).")))
        .unwrap_or_else(|| Error::Invalid("No compatible Logitech mouse found.".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_list_with_range() {
        // 100..=25600 in steps of 50
        let b = [0x00, 0x64, 0xE0, 0x32, 0x64, 0x00, 0x00, 0x00];
        let v = parse_dpi_list(&b);
        assert_eq!(v.first(), Some(&100));
        assert_eq!(v.last(), Some(&25600));
        assert_eq!(v.len(), (25600 - 100) / 50 + 1);
        let caps = DpiCaps { values: v };
        assert_eq!(caps.step(), 50);
        assert_eq!(caps.snap(824), 800);
        assert_eq!(caps.snap(826), 850);
        assert_eq!(caps.snap(10), 100);
    }

    #[test]
    fn voltage_curve() {
        assert_eq!(voltage_to_percent(4200), 100);
        assert_eq!(voltage_to_percent(3811), 50);
        assert_eq!(voltage_to_percent(3400), 0);
    }
}
