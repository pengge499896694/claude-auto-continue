// Win32 window automation, ported from the tested Python `windows_automation.py`.
// Detects VS Code and terminal windows, activates one, and types the continue
// prompt (via VS Code's command palette, or straight into a terminal running
// the `claude` CLI). Uses windows-sys 0.52 (isize handles) for stable signatures.

#![allow(clippy::missing_safety_doc)]

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, GetExitCodeProcess, OpenProcess,
    QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetActiveWindow, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_P, VK_RETURN, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindow,
    SW_RESTORE, SW_SHOWNORMAL,
};

use super::WindowItem;

const STILL_ACTIVE: u32 = 259;
const MAX_PATH: usize = 260;

const VSCODE_EXECUTABLES: &[&str] = &["code.exe", "code - insiders.exe", "codium.exe", "vscodium.exe"];

const TERMINAL_EXECUTABLES: &[&str] = &[
    "windowsterminal.exe",
    "wt.exe",
    "conhost.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "hyper.exe",
    "tabby.exe",
];

fn classify(executable: &str) -> (&'static str, &'static str) {
    let name = Path::new(executable)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if VSCODE_EXECUTABLES.contains(&name.as_str()) {
        ("vscode", "VS Code")
    } else if TERMINAL_EXECUTABLES.contains(&name.as_str()) {
        ("terminal", "终端")
    } else {
        ("", "其它")
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn window_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let read = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if read <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..read as usize])
}

unsafe fn process_path(pid: u32) -> String {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; MAX_PATH];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
    CloseHandle(handle);
    if ok != 0 {
        String::from_utf16_lossy(&buf[..size as usize])
    } else {
        String::new()
    }
}

pub fn get_window_info(hwnd: isize) -> Option<WindowItem> {
    unsafe {
        if hwnd == 0 || IsWindow(hwnd) == 0 {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let executable = process_path(pid);
        let title = window_text(hwnd);
        let (kind, kind_label) = classify(&executable);
        let display = format!("[{}] {}  (PID {})", kind_label, title, pid);
        Some(WindowItem {
            hwnd,
            title,
            executable,
            pid,
            kind: kind.to_string(),
            kind_label: kind_label.to_string(),
            display,
        })
    }
}

fn is_vscode(info: &WindowItem) -> bool {
    info.kind == "vscode"
}
fn is_terminal(info: &WindowItem) -> bool {
    info.kind == "terminal"
}
fn is_supported(info: &WindowItem) -> bool {
    info.kind == "vscode" || info.kind == "terminal"
}

pub fn foreground_window() -> Option<WindowItem> {
    unsafe { get_window_info(GetForegroundWindow()) }
}

/// The current foreground window, but only if it is a supported (VS Code / terminal) window.
pub fn foreground_target() -> Option<WindowItem> {
    foreground_window().filter(is_supported)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let handles = &mut *(lparam as *mut Vec<isize>);
    if IsWindowVisible(hwnd) != 0 && !window_text(hwnd).is_empty() {
        handles.push(hwnd);
    }
    1 // TRUE: keep enumerating
}

fn enum_all_handles() -> Vec<isize> {
    let mut handles: Vec<isize> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_proc), &mut handles as *mut _ as LPARAM);
    }
    handles
}

fn enum_windows_filtered<F: Fn(&WindowItem) -> bool>(pred: F) -> Vec<WindowItem> {
    enum_all_handles()
        .into_iter()
        .filter_map(get_window_info)
        .filter(|info| pred(info))
        .collect()
}

pub fn list_target_windows() -> Vec<WindowItem> {
    enum_windows_filtered(is_supported)
}

pub fn choose_target_window(
    project_hint: &str,
    preferred_hwnd: isize,
    target_kind: &str,
) -> Option<WindowItem> {
    if preferred_hwnd != 0 {
        if let Some(info) = get_window_info(preferred_hwnd) {
            if is_supported(&info) && (target_kind == "auto" || info.kind == target_kind) {
                return Some(info);
            }
        }
    }

    let pred: fn(&WindowItem) -> bool = match target_kind {
        "vscode" => is_vscode,
        "terminal" => is_terminal,
        _ => is_supported,
    };

    if let Some(fg) = foreground_window() {
        if pred(&fg) {
            return Some(fg);
        }
    }

    let mut windows = enum_windows_filtered(pred);
    if windows.is_empty() {
        return None;
    }

    let hint = Path::new(project_hint)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let hint = hint.trim();
    if !hint.is_empty() {
        if let Some(w) = windows.iter().find(|w| w.title.to_lowercase().contains(hint)) {
            return Some(w.clone());
        }
    }

    if target_kind == "auto" {
        windows.sort_by_key(|w| if w.kind == "vscode" { 0 } else { 1 });
    }
    windows.into_iter().next()
}

pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle == 0 {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

fn activate_window(hwnd: isize) -> bool {
    unsafe {
        if IsWindow(hwnd) == 0 {
            return false;
        }
        ShowWindow(hwnd, SW_RESTORE);

        let foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        let target_thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let foreground_thread = if foreground == 0 {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };

        let mut attached_target = false;
        let mut attached_foreground = false;
        if target_thread != 0 && target_thread != current_thread {
            attached_target = AttachThreadInput(current_thread, target_thread, 1) != 0;
        }
        if foreground_thread != 0
            && foreground_thread != current_thread
            && foreground_thread != target_thread
        {
            attached_foreground = AttachThreadInput(current_thread, foreground_thread, 1) != 0;
        }

        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        SetActiveWindow(hwnd);
        SetFocus(hwnd);

        if attached_foreground {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
        if attached_target {
            AttachThreadInput(current_thread, target_thread, 0);
        }

        sleep(Duration::from_millis(250));
        GetForegroundWindow() == hwnd
    }
}

fn keybd_input(vk: VIRTUAL_KEY, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_one(input: INPUT) {
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

fn send_key(vk: VIRTUAL_KEY, up: bool) {
    let flags = if up { KEYEVENTF_KEYUP } else { 0 };
    send_one(keybd_input(vk, 0, flags));
}

fn send_hotkey(keys: &[VIRTUAL_KEY]) {
    for k in keys {
        send_key(*k, false);
    }
    for k in keys.iter().rev() {
        send_key(*k, true);
    }
}

fn type_unicode(text: &str) {
    for unit in text.encode_utf16() {
        send_one(keybd_input(0, unit, KEYEVENTF_UNICODE));
        send_one(keybd_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        sleep(Duration::from_millis(1));
    }
}

fn one_line(prompt: &str) -> String {
    prompt
        .replace('\r', "\n")
        .lines()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn send_via_vscode(hwnd: isize, prompt: &str) {
    if !activate_window(hwnd) {
        sleep(Duration::from_millis(200));
    }
    send_hotkey(&[VK_CONTROL, VK_SHIFT, VK_P]);
    sleep(Duration::from_millis(450));
    type_unicode("Claude Code: Focus input");
    sleep(Duration::from_millis(350));
    send_hotkey(&[VK_RETURN]);
    sleep(Duration::from_millis(650));
    type_unicode(&one_line(prompt));
    sleep(Duration::from_millis(150));
    send_hotkey(&[VK_RETURN]);
}

fn send_via_terminal(hwnd: isize, prompt: &str) {
    if !activate_window(hwnd) {
        sleep(Duration::from_millis(200));
    }
    sleep(Duration::from_millis(400));
    type_unicode(&one_line(prompt));
    sleep(Duration::from_millis(200));
    send_hotkey(&[VK_RETURN]);
}

pub fn send_continue_to_claude(hwnd: isize, prompt: &str, target_kind: &str) -> Result<(), String> {
    let kind = if target_kind == "vscode" || target_kind == "terminal" {
        target_kind.to_string()
    } else {
        get_window_info(hwnd)
            .map(|i| i.kind)
            .filter(|k| k == "vscode" || k == "terminal")
            .unwrap_or_else(|| "vscode".to_string())
    };
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return Err("目标窗口已失效".into());
    }
    if kind == "terminal" {
        send_via_terminal(hwnd, prompt);
    } else {
        send_via_vscode(hwnd, prompt);
    }
    Ok(())
}

pub fn open_path(path: &Path) -> Result<(), String> {
    let wide = to_wide(&path.to_string_lossy());
    let op = to_wide("open");
    let result = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success.
    if (result as isize) > 32 {
        Ok(())
    } else {
        Err(format!("无法打开：{}", path.display()))
    }
}

/// Local UTC offset in seconds (east of UTC positive), from the Windows timezone.
pub fn utc_offset_seconds() -> i64 {
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let result = GetTimeZoneInformation(&mut tzi);
        let active_bias = if result == TIME_ZONE_ID_DAYLIGHT {
            tzi.Bias + tzi.DaylightBias
        } else {
            tzi.Bias + tzi.StandardBias
        };
        // Bias is UTC = local + bias (minutes); local offset = -bias.
        -(active_bias as i64) * 60
    }
}
