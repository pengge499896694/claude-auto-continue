from __future__ import annotations

import ctypes
import json
import os
import time
from ctypes import wintypes
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


if os.name != "nt":
    raise RuntimeError("windows_automation.py 仅支持 Windows")

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
SYNCHRONIZE = 0x00100000
STILL_ACTIVE = 259
SW_RESTORE = 9
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004

VK_CONTROL = 0x11
VK_SHIFT = 0x10
VK_MENU = 0x12
VK_RETURN = 0x0D
VK_ESCAPE = 0x1B
VK_P = 0x50

ULONG_PTR = wintypes.WPARAM


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD), ("wParamH", wintypes.WORD)]


class INPUT_UNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("union",)
    _fields_ = [("type", wintypes.DWORD), ("union", INPUT_UNION)]


VSCODE_EXECUTABLES = {"code.exe", "code - insiders.exe", "codium.exe", "vscodium.exe"}
# Terminals that commonly host the `claude` CLI on Windows.
TERMINAL_EXECUTABLES = {
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
}


@dataclass(frozen=True)
class WindowInfo:
    hwnd: int
    title: str
    executable: str
    pid: int

    @property
    def kind(self) -> str:
        name = Path(self.executable).name.lower()
        if name in VSCODE_EXECUTABLES:
            return "vscode"
        if name in TERMINAL_EXECUTABLES:
            return "terminal"
        return ""

    @property
    def kind_label(self) -> str:
        return {"vscode": "VS Code", "terminal": "终端"}.get(self.kind, "其它")

    @property
    def display(self) -> str:
        return f"[{self.kind_label}] {self.title}  (PID {self.pid})"


def _window_text(hwnd: int) -> str:
    length = user32.GetWindowTextLengthW(hwnd)
    if length <= 0:
        return ""
    buf = ctypes.create_unicode_buffer(length + 1)
    user32.GetWindowTextW(hwnd, buf, len(buf))
    return buf.value


def _process_path(pid: int) -> str:
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return ""
    try:
        size = wintypes.DWORD(32768)
        buf = ctypes.create_unicode_buffer(size.value)
        if kernel32.QueryFullProcessImageNameW(handle, 0, buf, ctypes.byref(size)):
            return buf.value
        return ""
    finally:
        kernel32.CloseHandle(handle)


def get_window_info(hwnd: int) -> Optional[WindowInfo]:
    if not hwnd or not user32.IsWindow(hwnd):
        return None
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    executable = _process_path(pid.value)
    return WindowInfo(hwnd=int(hwnd), title=_window_text(hwnd), executable=executable, pid=int(pid.value))


def is_vscode_window(info: Optional[WindowInfo]) -> bool:
    return bool(info) and info.kind == "vscode"  # type: ignore[union-attr]


def is_terminal_window(info: Optional[WindowInfo]) -> bool:
    return bool(info) and info.kind == "terminal"  # type: ignore[union-attr]


def is_supported_window(info: Optional[WindowInfo]) -> bool:
    return bool(info) and info.kind in {"vscode", "terminal"}  # type: ignore[union-attr]


def foreground_window() -> Optional[WindowInfo]:
    return get_window_info(user32.GetForegroundWindow())


def _enum_windows(predicate) -> list[WindowInfo]:
    results: list[WindowInfo] = []
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

    @callback_type
    def callback(hwnd: int, _lparam: int) -> bool:
        if user32.IsWindowVisible(hwnd) and _window_text(hwnd):
            info = get_window_info(hwnd)
            if predicate(info):
                results.append(info)  # type: ignore[arg-type]
        return True

    user32.EnumWindows(callback, 0)
    return results


def list_vscode_windows() -> list[WindowInfo]:
    return _enum_windows(is_vscode_window)


def list_terminal_windows() -> list[WindowInfo]:
    return _enum_windows(is_terminal_window)


def list_target_windows() -> list[WindowInfo]:
    """VS Code and terminal windows that can host Claude Code / the claude CLI."""
    return _enum_windows(is_supported_window)


def choose_target_window(
    project_hint: str = "",
    preferred_hwnd: int = 0,
    target_kind: str = "auto",
) -> Optional[WindowInfo]:
    """Pick the window to deliver the continue prompt to.

    target_kind: "vscode" (Claude Code extension), "terminal" (claude CLI),
    or "auto" (prefer whatever is bound/foreground, else VS Code then terminal).
    """
    if preferred_hwnd:
        preferred = get_window_info(preferred_hwnd)
        if is_supported_window(preferred):
            if target_kind == "auto" or preferred.kind == target_kind:  # type: ignore[union-attr]
                return preferred

    if target_kind == "vscode":
        predicate = is_vscode_window
    elif target_kind == "terminal":
        predicate = is_terminal_window
    else:
        predicate = is_supported_window

    foreground = foreground_window()
    if predicate(foreground):
        return foreground

    windows = _enum_windows(predicate)
    if not windows:
        return None
    hint = Path(project_hint).name.lower().strip() if project_hint else ""
    if hint:
        for window in windows:
            if hint in window.title.lower():
                return window
    # Prefer VS Code over a bare terminal when auto-selecting.
    if target_kind == "auto":
        windows.sort(key=lambda w: 0 if w.kind == "vscode" else 1)
    return windows[0]


# Backwards-compatible alias.
def choose_vscode_window(project_hint: str = "", preferred_hwnd: int = 0) -> Optional[WindowInfo]:
    return choose_target_window(project_hint=project_hint, preferred_hwnd=preferred_hwnd, target_kind="vscode")


def is_process_running(pid: int) -> bool:
    if pid <= 0:
        return False
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, False, pid)
    if not handle:
        return False
    try:
        exit_code = wintypes.DWORD()
        if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
            return False
        return exit_code.value == STILL_ACTIVE
    finally:
        kernel32.CloseHandle(handle)


def is_claude_session_process_alive(session_id: str) -> bool:
    if not session_id:
        return False
    sessions_root = Path.home() / ".claude" / "sessions"
    if not sessions_root.exists():
        return False
    for metadata_path in sessions_root.glob("*.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("sessionId") == session_id:
                return is_process_running(int(metadata.get("pid", 0)))
        except (OSError, ValueError, TypeError, json.JSONDecodeError):
            continue
    return False


def activate_window(hwnd: int) -> bool:
    if not user32.IsWindow(hwnd):
        return False
    user32.ShowWindow(hwnd, SW_RESTORE)

    foreground = user32.GetForegroundWindow()
    current_thread = kernel32.GetCurrentThreadId()
    target_thread = user32.GetWindowThreadProcessId(hwnd, None)
    foreground_thread = user32.GetWindowThreadProcessId(foreground, None) if foreground else 0

    attached_target = False
    attached_foreground = False
    try:
        if target_thread and target_thread != current_thread:
            attached_target = bool(user32.AttachThreadInput(current_thread, target_thread, True))
        if foreground_thread and foreground_thread not in {current_thread, target_thread}:
            attached_foreground = bool(user32.AttachThreadInput(current_thread, foreground_thread, True))
        user32.BringWindowToTop(hwnd)
        user32.SetForegroundWindow(hwnd)
        user32.SetActiveWindow(hwnd)
    finally:
        if attached_foreground:
            user32.AttachThreadInput(current_thread, foreground_thread, False)
        if attached_target:
            user32.AttachThreadInput(current_thread, target_thread, False)

    time.sleep(0.25)
    return user32.GetForegroundWindow() == hwnd


def _send_input(item: INPUT) -> None:
    sent = user32.SendInput(1, ctypes.byref(item), ctypes.sizeof(INPUT))
    if sent != 1:
        raise ctypes.WinError(ctypes.get_last_error())


def _key(vk: int, up: bool = False) -> None:
    flags = KEYEVENTF_KEYUP if up else 0
    item = INPUT(type=INPUT_KEYBOARD, ki=KEYBDINPUT(wVk=vk, wScan=0, dwFlags=flags, time=0, dwExtraInfo=0))
    _send_input(item)


def send_hotkey(*keys: int) -> None:
    for key in keys:
        _key(key, up=False)
    for key in reversed(keys):
        _key(key, up=True)


def type_unicode(text: str, interval: float = 0.001) -> None:
    # KEYEVENTF_UNICODE expects UTF-16 code units, including surrogate pairs.
    encoded = text.encode("utf-16-le")
    for index in range(0, len(encoded), 2):
        unit = int.from_bytes(encoded[index : index + 2], "little")
        down = INPUT(
            type=INPUT_KEYBOARD,
            ki=KEYBDINPUT(wVk=0, wScan=unit, dwFlags=KEYEVENTF_UNICODE, time=0, dwExtraInfo=0),
        )
        up = INPUT(
            type=INPUT_KEYBOARD,
            ki=KEYBDINPUT(
                wVk=0,
                wScan=unit,
                dwFlags=KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                time=0,
                dwExtraInfo=0,
            ),
        )
        _send_input(down)
        _send_input(up)
        if interval:
            time.sleep(interval)


def _one_line(prompt: str) -> str:
    return " ".join(prompt.replace("\r", "\n").splitlines()).strip()


def send_continue_via_vscode(hwnd: int, prompt: str) -> None:
    """Focus Claude Code's composer through VS Code's command palette and submit text."""
    if not activate_window(hwnd):
        # SetForegroundWindow can report false during a race even if subsequent keys work.
        time.sleep(0.2)
    send_hotkey(VK_CONTROL, VK_SHIFT, VK_P)
    time.sleep(0.45)
    type_unicode("Claude Code: Focus input", interval=0.002)
    time.sleep(0.35)
    send_hotkey(VK_RETURN)
    time.sleep(0.65)

    type_unicode(_one_line(prompt), interval=0.001)
    time.sleep(0.15)
    send_hotkey(VK_RETURN)


def send_continue_via_terminal(hwnd: int, prompt: str) -> None:
    """Type the continue prompt straight into the claude CLI's terminal window.

    The terminal is assumed to already have `claude` running and waiting at its
    prompt. We only bring the window forward and type; no command palette exists.
    """
    if not activate_window(hwnd):
        time.sleep(0.2)
    # Give the terminal a moment to take focus before typing.
    time.sleep(0.4)
    type_unicode(_one_line(prompt), interval=0.002)
    time.sleep(0.2)
    send_hotkey(VK_RETURN)


def send_continue_to_claude(hwnd: int, prompt: str, target_kind: str = "") -> None:
    """Deliver the continue prompt to the right client based on window type.

    target_kind may be forced ("vscode"/"terminal"); otherwise it is inferred
    from the window's owning process.
    """
    kind = target_kind
    if kind not in {"vscode", "terminal"}:
        info = get_window_info(hwnd)
        kind = info.kind if info else "vscode"
    if kind == "terminal":
        send_continue_via_terminal(hwnd, prompt)
    else:
        send_continue_via_vscode(hwnd, prompt)
