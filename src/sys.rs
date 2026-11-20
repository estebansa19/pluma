//! Windows helpers: single instance, start with Windows, processes, G HUB and icons.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, INVALID_HANDLE_VALUE, POINT};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, FindWindowW, GetCursorPos, IsIconic, MessageBoxW, PostMessageW, SetForegroundWindow,
    ShowWindow, HICON, ICONINFO, MB_ICONERROR, MB_OK, SW_RESTORE, WM_CLOSE,
};

pub const AGENT_CLASS: &str = "PlumaAgentWnd";
pub const UI_TITLE: &str = "Pluma";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Pluma";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Returns false if another instance already holds the mutex with that name.
pub fn single_instance(name: &str) -> bool {
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr());
        if h.is_null() {
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(h);
            return false;
        }
        true // the handle stays open for the lifetime of the process
    }
}

pub fn focus_ui_window() -> bool {
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), wide(UI_TITLE).as_ptr());
        if hwnd.is_null() {
            return false;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
        true
    }
}

pub fn agent_running() -> bool {
    unsafe { !FindWindowW(wide(AGENT_CLASS).as_ptr(), std::ptr::null()).is_null() }
}

pub fn close_agent() {
    unsafe {
        let hwnd = FindWindowW(wide(AGENT_CLASS).as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

/// Fixed folder for the copy that runs in the background and starts with Windows.
pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Programs")
        .join("Pluma")
}

/// Stable path of the executable: if Pluma was opened from somewhere else (Downloads, a build
/// folder…), it's copied to `install_dir()` so starting with Windows doesn't break.
pub fn stable_exe() -> std::io::Result<PathBuf> {
    let cur = std::env::current_exe()?;
    let dst = install_dir().join("Pluma.exe");
    let same = match (cur.canonicalize(), dst.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        return Ok(dst);
    }
    let up_to_date = match (std::fs::read(&cur), std::fs::read(&dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !up_to_date {
        std::fs::create_dir_all(install_dir())?;
        // The installed agent might be using the file: close it before replacing it.
        if agent_running() {
            close_agent();
            for _ in 0..30 {
                if !agent_running() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        std::fs::copy(&cur, &dst)?;
    }
    Ok(dst)
}

pub fn spawn_agent() -> std::io::Result<()> {
    Command::new(stable_exe()?)
        .arg("--tray")
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

pub fn spawn_ui() -> std::io::Result<()> {
    Command::new(std::env::current_exe()?).creation_flags(DETACHED_PROCESS).spawn().map(|_| ())
}

/// Gives back to Windows the memory pages the process isn't using
/// (the agent spends almost all of its time waiting).
pub fn trim_memory() {
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

pub fn set_autostart(on: bool) -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    unsafe {
        if on {
            let Ok(exe) = stable_exe().map(|p| p.display().to_string()) else { return false };
            let cmd = wide(&format!("\"{exe}\" --tray"));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                cmd.as_ptr() as *const _,
                (cmd.len() * 2) as u32,
            ) == 0
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
            r == 0 || r == ERROR_FILE_NOT_FOUND
        }
    }
}

pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let mut size: u32 = 0;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        ) == 0
    }
}

const GHUB_PROCS: [&str; 3] = ["lghub_agent.exe", "lghub_system_tray.exe", "lghub.exe"];

/// Are there G HUB processes that talk to the mouse?
pub fn ghub_running() -> bool {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
            if GHUB_PROCS.contains(&name.as_str()) {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        found
    }
}

pub fn kill_ghub() {
    let mut cmd = Command::new("taskkill");
    cmd.arg("/F");
    for p in GHUB_PROCS {
        cmd.args(["/IM", p]);
    }
    let _ = cmd.creation_flags(CREATE_NO_WINDOW).output();
}

pub fn attach_parent_console() {
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn set_dpi_aware() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { (GetCursorPos(&mut p) != 0).then_some((p.x, p.y)) }
}

pub fn error_box(text: &str) {
    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("Pluma").as_ptr(), MB_OK | MB_ICONERROR);
    }
}

pub fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    let _ = Command::new("explorer").arg(path).spawn();
}

// ---------- icon ----------

pub use crate::icon::icon_rgba;

pub fn hicon_from_rgba(rgba: &[u8], size: i32) -> HICON {
    let mut bgra = rgba.to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mask_bits = vec![0u8; (((size + 15) / 16) * 2 * size) as usize];
    unsafe {
        let color = CreateBitmap(size, size, 1, 32, bgra.as_ptr() as *const _);
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr() as *const _);
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}
