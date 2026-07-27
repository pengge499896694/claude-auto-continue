// Cross-platform window automation.
//
// The shared `WindowItem` type and the session-liveness check live here; the
// platform-specific work (enumerating windows, activating one, typing the
// continue prompt) is delegated to a per-OS submodule:
//
//   * Windows -> `automation/windows.rs`  (Win32 API via windows-sys)
//   * macOS   -> `automation/macos.rs`    (System Events via osascript)
//
// Both submodules expose the same function set, so `lib.rs` never needs a
// `#[cfg]`.

use serde::Serialize;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use self::windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

/// A window (or, on macOS, an application) that can host Claude Code / the CLI.
///
/// `hwnd` is a native window handle on Windows. On macOS there is no equivalent
/// public handle, so it carries the process id instead — callers only ever pass
/// it back to this module, so the distinction stays internal.
#[derive(Debug, Clone, Serialize)]
pub struct WindowItem {
    pub hwnd: isize,
    pub title: String,
    pub executable: String,
    pub pid: u32,
    pub kind: String,       // "vscode" | "terminal" | ""
    pub kind_label: String, // "VS Code" | "终端" | "其它"
    pub display: String,
}

/// True when the `claude` process that owns `session_id` is still running.
///
/// Reads the session metadata Claude writes to `~/.claude/sessions/*.json` and
/// checks the recorded pid. Shared across platforms; only the pid probe differs.
pub fn is_claude_session_process_alive(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let entries = match std::fs::read_dir(crate::core::sessions_root()) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&text) {
                if meta.get("sessionId").and_then(|v| v.as_str()) == Some(session_id) {
                    let pid = meta.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    return is_process_running(pid);
                }
            }
        }
    }
    false
}
