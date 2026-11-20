//! `Pluma --tray`: minimal system tray agent (no graphics engine, a few MB at most).
//!
//! - Shows the battery level in the tooltip.
//! - When the mouse wakes up (receiver notification, 0x1D4B event, the cursor moves after
//!   being still for a while, or Windows resumes from sleep) it verifies and reapplies the
//!   settings: in software mode it sets the DPI again; in onboard mode it makes sure the mouse
//!   is still using its memory and our profile (in case G HUB or another program changed it).

use crate::config::{Config, Storage};
use crate::device::{self, feat, Battery, DevEvent, Device, SW_AGENT};
use crate::hidpp;
use crate::sys;
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, TrackPopupMenu, TranslateMessage, HICON, MF_SEPARATOR,
    MF_STRING, MSG, SM_CXSMICON, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_STATUS: u32 = WM_APP + 2;
const MSG_POWERBROADCAST: u32 = 0x0218;
const MSG_DEVICECHANGE: u32 = 0x0219;
const PBT_RESUMESUSPEND: usize = 0x0007;
const PBT_RESUMEAUTOMATIC: usize = 0x0012;
const DBT_NODES_CHANGED: usize = 0x0007;
const ID_OPEN: usize = 1;
const ID_REAPPLY: usize = 2;
const ID_QUIT: usize = 3;

/// If the cursor was still for at least this long and then moves, assume the mouse may have woken up.
const IDLE_WAKE: Duration = Duration::from_secs(8);

enum Msg {
    Reapply,
    Resume,
    Rescan,
    Quit,
}

static TX: OnceLock<Mutex<Sender<Msg>>> = OnceLock::new();
static TIP: Mutex<String> = Mutex::new(String::new());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
thread_local! {
    static ICON: Cell<HICON> = const { Cell::new(std::ptr::null_mut()) };
}

fn send(m: Msg) {
    if let Some(tx) = TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(m);
        }
    }
}

pub fn run() {
    if !sys::single_instance("Local\\PlumaAgent") {
        return;
    }
    sys::set_dpi_aware();
    let (tx, rx) = mpsc::channel();
    let _ = TX.set(Mutex::new(tx));
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = sys::wide(sys::AGENT_CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let title = sys::wide("PlumaAgent");
        // Hidden window (not "message-only") so it receives power and device broadcasts.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(sys::wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let icon = sys::hicon_from_rgba(&sys::icon_rgba(size as u32), size);
        ICON.with(|i| i.set(icon));
        if let Ok(mut t) = TIP.lock() {
            *t = "Pluma · looking for the mouse…".into();
        }
        tray(hwnd, NIM_ADD);

        let hwnd_val = hwnd as usize;
        let worker = std::thread::spawn(move || hid_loop(rx, hwnd_val));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = worker.join();
    }
}

unsafe fn tray(hwnd: HWND, action: u32) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = ICON.with(|i| i.get());
    let tip = TIP.lock().map(|t| t.clone()).unwrap_or_default();
    let tip = if tip.is_empty() { "Pluma".to_string() } else { tip };
    for (i, c) in tip.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = c;
    }
    Shell_NotifyIconW(action, &nid);
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    let open = sys::wide("Open Pluma");
    let reapply = sys::wide("Reapply settings");
    let quit = sys::wide("Quit");
    AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
    AppendMenuW(menu, MF_STRING, ID_REAPPLY, reapply.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());
    SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, std::ptr::null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp as u32) & 0xFFFF {
                WM_LBUTTONUP => {
                    let _ = sys::spawn_ui();
                }
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_STATUS => {
            tray(hwnd, NIM_MODIFY);
            0
        }
        WM_COMMAND => {
            match wp & 0xFFFF {
                ID_OPEN => {
                    let _ = sys::spawn_ui();
                }
                ID_REAPPLY => send(Msg::Reapply),
                ID_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        MSG_POWERBROADCAST => {
            if wp == PBT_RESUMEAUTOMATIC || wp == PBT_RESUMESUSPEND {
                send(Msg::Resume);
            }
            1
        }
        MSG_DEVICECHANGE => {
            if wp == DBT_NODES_CHANGED {
                send(Msg::Rescan);
            }
            1
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            tray(hwnd, NIM_DELETE);
            send(Msg::Quit);
            PostQuitMessage(0);
            0
        }
        _ => {
            let tc = TASKBAR_CREATED.load(Ordering::Relaxed);
            if tc != 0 && msg == tc {
                // Explorer restarted: add the icon again.
                tray(hwnd, NIM_ADD);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

fn set_status(hwnd: usize, text: String) {
    if let Ok(mut t) = TIP.lock() {
        if *t == text {
            return;
        }
        *t = text;
    }
    unsafe {
        PostMessageW(hwnd as HWND, WM_STATUS, 0, 0);
    }
}

fn status_text(d: Option<&Device>, battery: Option<&Battery>) -> String {
    let Some(d) = d else { return "Pluma · mouse not found".into() };
    let name = if d.name.is_empty() { "Mouse" } else { d.name.as_str() };
    let mut s = format!("Pluma · {name}");
    if !d.online {
        s.push_str(" · asleep");
    } else if let Some(b) = battery {
        s.push_str(&format!(" · {}%", b.percent));
        if b.charging {
            s.push_str(" charging");
        }
    }
    s
}

struct CursorWatch {
    last: Option<(i32, i32)>,
    moved_at: Instant,
    checked_at: Instant,
}

impl CursorWatch {
    fn new() -> Self {
        CursorWatch { last: sys::cursor_pos(), moved_at: Instant::now(), checked_at: Instant::now() }
    }

    /// true when the cursor moves after being still for ≥ IDLE_WAKE.
    fn woke(&mut self) -> bool {
        if self.checked_at.elapsed() < Duration::from_millis(250) {
            return false;
        }
        self.checked_at = Instant::now();
        let Some(p) = sys::cursor_pos() else { return false };
        if self.last == Some(p) {
            return false;
        }
        let was_idle = self.moved_at.elapsed() >= IDLE_WAKE;
        self.last = Some(p);
        self.moved_at = Instant::now();
        was_idle
    }
}

/// Checks that the mouse has what the user saved and fixes it if not.
fn reconcile(d: &mut Device) -> hidpp::Result<()> {
    let cfg = Config::load();
    let Some(owner) = &cfg.device else { return Ok(()) }; // nothing was ever saved
    d.check()?;
    if *owner != d.name {
        return Ok(());
    }
    let has_onboard = d.has(feat::ONBOARD_PROFILES);
    match cfg.storage {
        Storage::Onboard => {
            if has_onboard {
                match cfg.profile_sector {
                    // Some mice (PRO X 2) wake up on the factory profile: switch back to ours.
                    Some(sector) => {
                        let before = d.current_profile().unwrap_or(0);
                        if d.ensure_profile(sector)? {
                            crate::log::write(&format!("profile {before:#06X} → {sector:#06X} (fixed after wake-up)"));
                        }
                    }
                    None => {
                        if d.onboard_mode()? != 1 {
                            d.set_onboard_mode(1)?;
                            crate::log::write("onboard mode restored");
                        }
                    }
                }
            }
        }
        Storage::Software => {
            if has_onboard && d.onboard_mode()? != 2 {
                d.set_onboard_mode(2)?;
            }
            let want = cfg.active_dpi();
            let now = d.get_dpi()?;
            if now != want {
                d.set_dpi(want)?;
                crate::log::write(&format!("DPI {now} → {want} (reapplied after wake-up)"));
            }
            if let Some(hz) = cfg.report_rate_hz {
                if d.get_rate_hz().ok() != Some(hz) {
                    let _ = d.set_rate_hz(hz);
                }
            }
        }
    }
    Ok(())
}

fn hid_loop(rx: Receiver<Msg>, hwnd: usize) {
    let mut api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            set_status(hwnd, format!("Pluma · HID error: {e}"));
            while !matches!(rx.recv(), Ok(Msg::Quit) | Err(_)) {}
            return;
        }
    };
    let mut dev: Option<Device> = None;
    let mut battery: Option<Battery> = None;
    let mut next_scan = Instant::now();
    let mut verify_at: Option<Instant> = None;
    let mut verify_tries = 0u32;
    let mut next_battery = Instant::now();
    let mut cursor = CursorWatch::new();
    // After each burst of work, give back the memory that is no longer used.
    let mut trim_at: Option<Instant> = Some(Instant::now() + Duration::from_secs(3));

    loop {
        if trim_at.is_some_and(|t| Instant::now() >= t) && verify_at.is_none() {
            sys::trim_memory();
            trim_at = None;
        }
        let now = Instant::now();
        let msg = if dev.is_some() {
            rx.try_recv().ok()
        } else {
            match rx.recv_timeout(Duration::from_millis(1000)) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        match msg {
            Some(Msg::Quit) => return,
            Some(Msg::Reapply) => {
                verify_at = Some(now);
                verify_tries = 0;
            }
            Some(Msg::Resume) => {
                // After Windows resumes, handles may be stale: reopen everything.
                dev = None;
                next_scan = now + Duration::from_secs(2);
            }
            Some(Msg::Rescan) => {
                if dev.is_none() {
                    next_scan = now;
                }
            }
            None => {}
        }

        if dev.is_none() {
            if Instant::now() >= next_scan {
                let _ = api.refresh_devices();
                match device::discover(&api, SW_AGENT) {
                    Ok(mut d) => {
                        d.enable_receiver_notifications();
                        verify_at = Some(Instant::now());
                        verify_tries = 0;
                        next_battery = Instant::now();
                        dev = Some(d);
                    }
                    Err(_) => next_scan = Instant::now() + Duration::from_secs(5),
                }
                set_status(hwnd, status_text(dev.as_ref(), battery.as_ref()));
                trim_at = Some(Instant::now() + Duration::from_secs(3));
            }
            continue;
        }
        let d = dev.as_mut().expect("dev");

        match d.poll_event(400) {
            Ok(Some(DevEvent::Link(true))) | Ok(Some(DevEvent::Reconfigure)) => {
                verify_at = Some(Instant::now() + Duration::from_millis(300));
                verify_tries = 0;
                next_battery = Instant::now() + Duration::from_secs(2);
            }
            Ok(Some(DevEvent::Link(false))) => {}
            Ok(Some(DevEvent::Battery(b))) => battery = Some(b),
            Ok(None) => {}
            Err(_) => {
                dev = None;
                next_scan = Instant::now() + Duration::from_secs(1);
                set_status(hwnd, status_text(None, None));
                continue;
            }
        }

        if cursor.woke() && verify_at.is_none() {
            verify_at = Some(Instant::now() + Duration::from_millis(300));
            verify_tries = 0;
        }

        if let Some(t) = verify_at {
            if Instant::now() >= t {
                match reconcile(d) {
                    Ok(()) => verify_at = None,
                    Err(e) if e.is_offline() && verify_tries < 10 => {
                        verify_tries += 1;
                        verify_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                    Err(_) => verify_at = None,
                }
                trim_at = Some(Instant::now() + Duration::from_secs(3));
            }
        }

        if Instant::now() >= next_battery {
            next_battery = Instant::now() + Duration::from_secs(600);
            if d.online {
                if let Ok(b) = d.battery() {
                    battery = Some(b);
                }
            }
            trim_at = Some(Instant::now() + Duration::from_secs(3));
        }
        set_status(hwnd, status_text(Some(d), battery.as_ref()));
    }
}
