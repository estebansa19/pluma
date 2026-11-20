//! HID++ (1.0 and 2.0) transport and messaging over hidapi.
//!
//! On Windows every HID top-level collection shows up as a separate device: short reports
//! (0x10, 7 bytes) travel over the FF00:0001 collection and long reports (0x11, 20 bytes)
//! over FF00:0002. Every HID++ 2.0 request is sent as a long report; the short collection is
//! used for receiver registers (HID++ 1.0), their errors and notifications.

use hidapi::{HidApi, HidDevice};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt;
use std::time::{Duration, Instant};

pub const VID_LOGITECH: u16 = 0x046D;
const USAGE_PAGE_HIDPP: u16 = 0xFF00;
const USAGE_SHORT: u16 = 0x0001;
const USAGE_LONG: u16 = 0x0002;

pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const REPORT_VERY_LONG: u8 = 0x12;
const LONG_LEN: usize = 20;

/// Device index used to talk to a wired mouse (or to the receiver itself).
pub const INDEX_DIRECT: u8 = 0xFF;
const SUB_ERROR_10: u8 = 0x8F;
const SUB_ERROR_20: u8 = 0xFF;
const MAX_PENDING: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Hid(String),
    Timeout,
    Hidpp10(u8),
    Hidpp20(u8),
    Unsupported(&'static str),
    Invalid(String),
}

impl Error {
    /// The mouse exists but can't be reached right now (asleep, off or out of range).
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Timeout | Error::Hidpp10(0x04) | Error::Hidpp10(0x09))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "USB/HID error: {e}"),
            Error::Timeout => write!(f, "the mouse did not respond in time"),
            Error::Hidpp10(c) => write!(f, "{} (HID++ 1.0, code 0x{c:02X})", hidpp10_msg(*c)),
            Error::Hidpp20(c) => write!(f, "{} (HID++ 2.0, code 0x{c:02X})", hidpp20_msg(*c)),
            Error::Unsupported(what) => write!(f, "this mouse does not support: {what}"),
            Error::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

fn hidpp10_msg(c: u8) -> &'static str {
    match c {
        0x01 => "invalid sub-command",
        0x02 => "invalid address",
        0x03 => "invalid value",
        0x04 => "could not connect to the mouse",
        0x05 => "too many devices",
        0x06 => "already exists",
        0x07 => "receiver busy",
        0x08 => "no device paired in that slot",
        0x09 => "the mouse is asleep or turned off",
        0x0A => "request unavailable",
        0x0B => "invalid parameter",
        _ => "receiver error",
    }
}

fn hidpp20_msg(c: u8) -> &'static str {
    match c {
        0x01 => "unknown error",
        0x02 => "invalid argument",
        0x03 => "value out of range",
        0x04 => "hardware error",
        0x05 => "internal error",
        0x06 => "invalid feature index",
        0x07 => "invalid function",
        0x08 => "the mouse is busy",
        0x09 => "unsupported",
        _ => "mouse error",
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn hid_err(e: hidapi::HidError) -> Error {
    Error::Hid(e.to_string())
}

/// A pair of HID++ collections (long + optional short) belonging to the same receiver or mouse.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub product_id: u16,
    pub product: String,
    pub interface: i32,
    pub long_path: CString,
    pub short_path: Option<CString>,
}

/// Key used to pair the COL01/COL02 collections of the same interface:
/// `\\?\hid#vid_046d&pid_c54d&mi_02&col02#a&1eb22274&0&0001#{guid}` → `vid_046d&pid_c54d&mi_02#a&1eb22274&0`
fn collection_key(path: &[u8]) -> String {
    let s = String::from_utf8_lossy(path).to_ascii_lowercase();
    let parts: Vec<&str> = s.split('#').collect();
    if parts.len() < 3 {
        return s;
    }
    let ids: Vec<&str> = parts[1].split('&').filter(|p| !p.starts_with("col")).collect();
    let inst = match parts[2].rfind('&') {
        Some(i) => &parts[2][..i],
        None => parts[2],
    };
    format!("{}#{}", ids.join("&"), inst)
}

pub fn candidates(api: &HidApi) -> Vec<Candidate> {
    let mut longs = Vec::new();
    let mut shorts = Vec::new();
    for d in api.device_list() {
        if d.vendor_id() != VID_LOGITECH || d.usage_page() != USAGE_PAGE_HIDPP {
            continue;
        }
        let key = collection_key(d.path().to_bytes());
        match d.usage() {
            USAGE_LONG => longs.push((key, d)),
            USAGE_SHORT => shorts.push((key, d)),
            _ => {}
        }
    }
    longs
        .into_iter()
        .map(|(key, l)| Candidate {
            product_id: l.product_id(),
            product: l.product_string().unwrap_or_default().to_string(),
            interface: l.interface_number(),
            long_path: l.path().to_owned(),
            short_path: shorts.iter().find(|(k, _)| *k == key).map(|(_, s)| s.path().to_owned()),
        })
        .collect()
}

fn is_notification(r: &[u8]) -> bool {
    match r.first() {
        Some(&REPORT_SHORT) => r.len() >= 3 && r[2] < 0x80,
        Some(&REPORT_LONG) | Some(&REPORT_VERY_LONG) => {
            r.len() >= 4 && r[2] != SUB_ERROR_20 && (r[3] & 0x0F) == 0
        }
        _ => false,
    }
}

pub struct Transport {
    long: HidDevice,
    short: Option<HidDevice>,
    #[allow(dead_code)]
    pub info: Candidate,
    pending: VecDeque<Vec<u8>>,
}

impl Transport {
    pub fn open(api: &HidApi, c: &Candidate) -> Result<Transport> {
        let long = api.open_path(&c.long_path).map_err(hid_err)?;
        let short = c.short_path.as_ref().and_then(|p| api.open_path(p).ok());
        Ok(Transport { long, short, info: c.clone(), pending: VecDeque::new() })
    }

    pub fn has_short(&self) -> bool {
        self.short.is_some()
    }

    fn read_raw(&mut self, timeout_ms: i32) -> Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 64];
        if let Some(s) = &self.short {
            let n = s.read_timeout(&mut buf, 0).map_err(hid_err)?;
            if n > 0 {
                return Ok(Some(buf[..n].to_vec()));
            }
        }
        let n = self.long.read_timeout(&mut buf, timeout_ms).map_err(hid_err)?;
        Ok((n > 0).then(|| buf[..n].to_vec()))
    }

    fn stash(&mut self, r: Vec<u8>) {
        if is_notification(&r) {
            if self.pending.len() >= MAX_PENDING {
                self.pending.pop_front();
            }
            self.pending.push_back(r);
        }
    }

    /// Next notification/event (the ones that arrived while waiting for responses come first).
    pub fn next_report(&mut self, timeout_ms: i32) -> Result<Option<Vec<u8>>> {
        if let Some(r) = self.pending.pop_front() {
            return Ok(Some(r));
        }
        self.read_raw(timeout_ms)
    }

    /// HID++ 2.0 request: function `func` of the feature at index `feature`; 16-byte response.
    pub fn request(
        &mut self,
        dev: u8,
        feature: u8,
        func: u8,
        sw: u8,
        params: &[u8],
        timeout: Duration,
    ) -> Result<[u8; 16]> {
        let mut out = [0u8; LONG_LEN];
        out[0] = REPORT_LONG;
        out[1] = dev;
        out[2] = feature;
        out[3] = (func << 4) | (sw & 0x0F);
        let n = params.len().min(16);
        out[4..4 + n].copy_from_slice(&params[..n]);
        self.long.write(&out).map_err(hid_err)?;

        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            let wait = (deadline - now).as_millis().clamp(1, 20) as i32;
            let Some(r) = self.read_raw(wait)? else { continue };
            if r.len() >= 7 && r[1] == dev {
                match r[0] {
                    REPORT_LONG | REPORT_VERY_LONG if r.len() >= LONG_LEN => {
                        if r[2] == SUB_ERROR_20 && r[3] == feature && r[4] == out[3] {
                            return Err(Error::Hidpp20(r[5]));
                        }
                        if r[2] == feature && r[3] == out[3] {
                            let mut p = [0u8; 16];
                            p.copy_from_slice(&r[4..20]);
                            return Ok(p);
                        }
                    }
                    REPORT_SHORT => {
                        if r[2] == SUB_ERROR_10 && r[3] == feature && r[4] == out[3] {
                            return Err(Error::Hidpp10(r[5]));
                        }
                    }
                    _ => {}
                }
            }
            self.stash(r);
        }
    }

    /// HID++ 1.0 register access (receiver). `sub_id` 0x80 = write, 0x81 = read.
    pub fn register(
        &mut self,
        dev: u8,
        sub_id: u8,
        address: u8,
        params: [u8; 3],
        timeout: Duration,
    ) -> Result<[u8; 3]> {
        let Some(short) = &self.short else {
            return Err(Error::Unsupported("short HID++ messages"));
        };
        let out = [REPORT_SHORT, dev, sub_id, address, params[0], params[1], params[2]];
        short.write(&out).map_err(hid_err)?;

        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            let wait = (deadline - now).as_millis().clamp(1, 20) as i32;
            let Some(r) = self.read_raw(wait)? else { continue };
            if r[0] == REPORT_SHORT && r.len() >= 7 && r[1] == dev {
                if r[2] == SUB_ERROR_10 && r[3] == sub_id && r[4] == address {
                    return Err(Error::Hidpp10(r[5]));
                }
                if r[2] == sub_id && r[3] == address {
                    return Ok([r[4], r[5], r[6]]);
                }
            }
            self.stash(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_collections() {
        let a = collection_key(br"\\?\hid#vid_046d&pid_c54d&mi_02&col01#a&1eb22274&0&0000#{4d1e55b4-f16f-11cf-88cb-001111000030}");
        let b = collection_key(br"\\?\HID#VID_046D&PID_C54D&MI_02&COL02#a&1eb22274&0&0001#{4d1e55b4-f16f-11cf-88cb-001111000030}");
        assert_eq!(a, b);
    }
}
