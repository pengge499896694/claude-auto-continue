// macOS window automation via `osascript` (System Events).
//
// Mirrors the Windows submodule's public API so `lib.rs` needs no `#[cfg]`.
// Instead of low-level FFI we drive AppleScript: it is the most robust way to
// enumerate GUI apps, bring one to the front, and inject keystrokes, and it
// lets us detect the "Accessibility not granted" error and report it clearly.
//
// The user prompt (which may contain Chinese) is delivered through the
// clipboard + Cmd-V rather than `keystroke`, because AppleScript `keystroke`
// is unreliable for non-ASCII text.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::WindowItem;

// On macOS there is no public window handle we can round-trip, so `hwnd`
// carries the process id. Callers only ever hand it back to this module.

const VSCODE_APPS: &[&str] = &["Code", "Code - Insiders", "VSCodium", "Codium"];

const TERMINAL_APPS: &[&str] = &[
    "Terminal",
    "iTerm2",
    "iTerm",
    "Warp",
    "Alacritty",
    "kitty",
    "WezTerm",
    "Hyper",
    "Tabby",
    "Ghostty",
];

// Claude Desktop 独立应用（macOS 上进程名为 "Claude"）。
const DESKTOP_APPS: &[&str] = &["Claude"];

fn classify(app_name: &str) -> (&'static str, &'static str) {
    if VSCODE_APPS.contains(&app_name) {
        ("vscode", "VS Code")
    } else if TERMINAL_APPS.contains(&app_name) {
        ("terminal", "终端")
    } else if DESKTOP_APPS.contains(&app_name) {
        ("desktop", "Claude 桌面版")
    } else {
        ("", "其它")
    }
}

// --------------------------------------------------------------------------
// osascript / shell helpers
// --------------------------------------------------------------------------
fn run_osascript(script: &str) -> Result<String, String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("无法调用 osascript：{}", e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(friendly_osascript_error(&stderr))
    }
}

/// Turn a raw osascript error into actionable Chinese guidance.
fn friendly_osascript_error(stderr: &str) -> String {
    let lowered = stderr.to_lowercase();
    if lowered.contains("-1719")
        || lowered.contains("assistive access")
        || lowered.contains("not allowed")
        || lowered.contains("not authori")
    {
        "缺少辅助功能/自动化授权。请到 系统设置 → 隐私与安全性 → 辅助功能，\
         勾选本应用（ClaudeAutoContinue）；如仍失败，再到 隐私与安全性 → 自动化 中，\
         允许本应用控制“系统事件(System Events)”。授权后重试即可。"
            .to_string()
    } else if stderr.trim().is_empty() {
        "osascript 执行失败（无错误输出）。".to_string()
    } else {
        format!("osascript 执行失败：{}", stderr.trim())
    }
}

fn set_clipboard(text: &str) -> Result<(), String> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法调用 pbcopy：{}", e))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "无法写入剪贴板".to_string())?
        .write_all(text.as_bytes())
        .map_err(|e| format!("写入剪贴板失败：{}", e))?;
    let status = child.wait().map_err(|e| format!("pbcopy 失败：{}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err("pbcopy 返回非零状态".to_string())
    }
}

// --------------------------------------------------------------------------
// Enumeration
// --------------------------------------------------------------------------
// One AppleScript pass returns every foreground GUI app as a tab-separated row:
//   pid \t appName \t frontmost \t frontWindowTitle
const ENUM_SCRIPT: &str = r#"tell application "System Events"
    set out to ""
    repeat with p in (every process whose background only is false)
        try
            set ppid to unix id of p
            set pname to name of p
            set isfront to frontmost of p
            set wtitle to ""
            try
                set wtitle to name of front window of p
            end try
            set out to out & ppid & tab & pname & tab & (isfront as string) & tab & wtitle & linefeed
        end try
    end repeat
    return out
end tell"#;

struct Proc {
    pid: u32,
    name: String,
    frontmost: bool,
    title: String,
}

fn enumerate() -> Result<Vec<Proc>, String> {
    let raw = run_osascript(ENUM_SCRIPT)?;
    let mut procs = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let pid = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
        let name = parts.next().map(|s| s.to_string());
        let front = parts.next().map(|s| s.trim().eq_ignore_ascii_case("true"));
        let title = parts.next().unwrap_or("").to_string();
        if let (Some(pid), Some(name), Some(frontmost)) = (pid, name, front) {
            procs.push(Proc {
                pid,
                name,
                frontmost,
                title,
            });
        }
    }
    Ok(procs)
}

fn to_item(p: &Proc) -> WindowItem {
    let (kind, kind_label) = classify(&p.name);
    let title = if p.title.is_empty() {
        p.name.clone()
    } else {
        p.title.clone()
    };
    let display = format!("[{}] {}  (PID {})", kind_label, title, p.pid);
    WindowItem {
        hwnd: p.pid as isize,
        title,
        executable: p.name.clone(),
        pid: p.pid,
        kind: kind.to_string(),
        kind_label: kind_label.to_string(),
        display,
    }
}

fn is_supported_kind(kind: &str) -> bool {
    kind == "vscode" || kind == "terminal" || kind == "desktop"
}

// --------------------------------------------------------------------------
// Public API (matches windows.rs)
// --------------------------------------------------------------------------
pub fn get_window_info(hwnd: isize) -> Option<WindowItem> {
    if hwnd == 0 {
        return None;
    }
    let pid = hwnd as u32;
    enumerate()
        .ok()?
        .iter()
        .find(|p| p.pid == pid)
        .map(to_item)
}

pub fn foreground_window() -> Option<WindowItem> {
    enumerate()
        .ok()?
        .iter()
        .find(|p| p.frontmost)
        .map(to_item)
}

pub fn foreground_target() -> Option<WindowItem> {
    foreground_window().filter(|w| is_supported_kind(&w.kind))
}

pub fn list_target_windows() -> Vec<WindowItem> {
    match enumerate() {
        Ok(procs) => procs
            .iter()
            .filter(|p| is_supported_kind(classify(&p.name).0))
            .map(to_item)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn choose_target_window(
    project_hint: &str,
    preferred_hwnd: isize,
    target_kind: &str,
) -> Option<WindowItem> {
    if preferred_hwnd != 0 {
        if let Some(info) = get_window_info(preferred_hwnd) {
            if is_supported_kind(&info.kind) && (target_kind == "auto" || info.kind == target_kind) {
                return Some(info);
            }
        }
    }

    let procs = enumerate().ok()?;
    let matches_kind = |kind: &str| -> bool {
        match target_kind {
            "vscode" => kind == "vscode",
            "terminal" => kind == "terminal",
            "desktop" => kind == "desktop",
            _ => is_supported_kind(kind),
        }
    };

    // Prefer the frontmost window if it already matches.
    if let Some(fg) = procs.iter().find(|p| p.frontmost) {
        if matches_kind(classify(&fg.name).0) {
            return Some(to_item(fg));
        }
    }

    let mut items: Vec<WindowItem> = procs
        .iter()
        .filter(|p| matches_kind(classify(&p.name).0))
        .map(to_item)
        .collect();
    if items.is_empty() {
        return None;
    }

    let hint = Path::new(project_hint)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let hint = hint.trim();
    if !hint.is_empty() {
        if let Some(w) = items.iter().find(|w| w.title.to_lowercase().contains(hint)) {
            return Some(w.clone());
        }
    }

    if target_kind == "auto" {
        items.sort_by_key(|w| if w.kind == "vscode" { 0 } else { 1 });
    }
    items.into_iter().next()
}

pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // `ps -p <pid>` exits 0 while the process exists.
    Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

fn send_via_vscode(pid: u32, prompt: &str) -> Result<(), String> {
    set_clipboard(&one_line(prompt))?;
    // key code 36 = Return. Cmd+Shift+P opens the command palette.
    let script = format!(
        r#"tell application "System Events"
    set frontmost of (first process whose unix id is {pid}) to true
    delay 0.4
    keystroke "p" using {{command down, shift down}}
    delay 0.5
    keystroke "Claude Code: Focus input"
    delay 0.4
    key code 36
    delay 0.7
    keystroke "v" using {{command down}}
    delay 0.2
    key code 36
end tell"#,
        pid = pid
    );
    run_osascript(&script).map(|_| ())
}

fn send_via_terminal(pid: u32, prompt: &str) -> Result<(), String> {
    set_clipboard(&one_line(prompt))?;
    let script = format!(
        r#"tell application "System Events"
    set frontmost of (first process whose unix id is {pid}) to true
    delay 0.4
    keystroke "v" using {{command down}}
    delay 0.2
    key code 36
end tell"#,
        pid = pid
    );
    run_osascript(&script).map(|_| ())
}

// Claude Desktop 独立应用：激活后输入框获得焦点，粘贴并回车即可。
fn send_via_desktop(pid: u32, prompt: &str) -> Result<(), String> {
    set_clipboard(&one_line(prompt))?;
    let script = format!(
        r#"tell application "System Events"
    set frontmost of (first process whose unix id is {pid}) to true
    delay 0.5
    keystroke "v" using {{command down}}
    delay 0.2
    key code 36
end tell"#,
        pid = pid
    );
    run_osascript(&script).map(|_| ())
}

pub fn send_continue_to_claude(hwnd: isize, prompt: &str, target_kind: &str) -> Result<(), String> {
    if hwnd == 0 {
        return Err("目标窗口已失效".into());
    }
    let pid = hwnd as u32;
    let kind = if target_kind == "vscode" || target_kind == "terminal" || target_kind == "desktop" {
        target_kind.to_string()
    } else {
        get_window_info(hwnd)
            .map(|i| i.kind)
            .filter(|k| k == "vscode" || k == "terminal" || k == "desktop")
            .unwrap_or_else(|| "vscode".to_string())
    };
    match kind.as_str() {
        "terminal" => send_via_terminal(pid, prompt),
        "desktop" => send_via_desktop(pid, prompt),
        _ => send_via_vscode(pid, prompt),
    }
}

pub fn open_path(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg(path)
        .status()
        .map_err(|e| format!("无法调用 open：{}", e))
        .and_then(|s| {
            if s.success() {
                Ok(())
            } else {
                Err(format!("无法打开：{}", path.display()))
            }
        })
}

/// Local UTC offset in seconds (east of UTC positive), via `date +%z`.
pub fn utc_offset_seconds() -> i64 {
    let out = match Command::new("date").arg("+%z").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return 0,
    };
    // Format: "+0800" / "-0530".
    let bytes = out.as_bytes();
    if bytes.len() < 5 {
        return 0;
    }
    let sign = if bytes[0] == b'-' { -1 } else { 1 };
    let hours: i64 = out[1..3].parse().unwrap_or(0);
    let mins: i64 = out[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + mins * 60)
}
