from __future__ import annotations

import json
import os
import sys
import threading
import time
import traceback
from datetime import datetime
from pathlib import Path

import webview

from core import (
    DEFAULT_PROJECTS_ROOT,
    SessionInfo,
    analyze_transcript,
    decide_continue,
    format_session,
    list_sessions,
)
from windows_automation import (
    WindowInfo,
    choose_target_window,
    foreground_window,
    is_claude_session_process_alive,
    is_supported_window,
    list_target_windows,
    send_continue_to_claude,
)


APP_NAME = "Claude Auto Continue"
APP_DIR = Path(os.getenv("APPDATA", Path.home())) / "ClaudeAutoContinue"
CONFIG_PATH = APP_DIR / "config.json"
LOG_PATH = APP_DIR / "monitor.log"
# In a PyInstaller onefile build the bundled webui unpacks to sys._MEIPASS.
_BASE_DIR = Path(getattr(sys, "_MEIPASS", Path(__file__).resolve().parent))
WEBUI_DIR = _BASE_DIR / "webui"

DEFAULT_CONFIG = {
    "quiet_seconds": 7,
    "cooldown_seconds": 15,
    "stalled_seconds": 60,
    "max_continues": 12,
    "mode": "smart",
    "target_kind": "auto",
    "prompt": "请继续完成刚才未完成的任务。不要只汇报进度，请直接继续执行；完成实现与必要验证后再结束。",
    "check_existing": True,
    "follow_latest": False,
}


def _positive_int(value, fallback: int) -> int:
    try:
        return max(1, int(float(value)))
    except (TypeError, ValueError):
        return fallback


def _positive_float(value, fallback: float) -> float:
    try:
        return max(0.5, float(value))
    except (TypeError, ValueError):
        return fallback


class Bridge:
    """JS-facing API plus the background monitor loop.

    The UI lives in an Edge WebView2 window rendered from webui/index.html;
    all analysis and window automation reuse core.py / windows_automation.py.
    """

    def __init__(self) -> None:
        APP_DIR.mkdir(parents=True, exist_ok=True)
        self.window: webview.Window | None = None
        self.config = self.load_config()

        self.session_map: dict[str, SessionInfo] = {}
        self.window_infos: dict[int, WindowInfo] = {}
        self.selected_session: Path | None = None
        self.bound_hwnd = 0

        self.watching = False
        self.sending = False
        self.continue_count = 0
        self.handled_fingerprints: set[str] = set()
        self.last_mtime = 0.0
        self.last_state = None
        self.last_send_at = 0.0
        self.last_switch_check = 0.0
        self.sending_fingerprint = ""

        self._pending_logs: list[dict] = []
        self._lock = threading.RLock()
        self._stop = threading.Event()
        self._poll_thread = threading.Thread(target=self._poll_loop, name="ClaudeMonitorPoll", daemon=True)

    # ---------------------------------------------------------- js -> python
    def get_initial(self) -> dict:
        sessions = self._session_payload(select_latest=True)
        windows = self._window_payload()
        logs, self._pending_logs = self._pending_logs, []
        return {
            "config": self.config,
            "sessions": sessions["sessions"],
            "selected_session": sessions["selected"],
            "windows": windows["windows"],
            "selected_window": windows["selected"],
            "logs": logs,
        }

    def refresh_sessions(self) -> dict:
        payload = self._session_payload()
        self.log(f"会话列表已刷新，共 {len(payload['sessions'])} 个。")
        return payload

    def refresh_windows(self) -> dict:
        payload = self._window_payload()
        if not payload["windows"]:
            self.log("暂未发现 VS Code 或终端窗口。请先打开 Claude Code 或运行 claude CLI，再刷新。", "warn")
        return payload

    def select_session(self, path: str) -> None:
        info = next((i for i in self.session_map.values() if str(i.path) == path), None)
        if info:
            self.selected_session = info.path
            self.last_mtime = 0.0
            self.handled_fingerprints.clear()
            self.continue_count = 0
            self.log(f"已选择会话：{info.project_name} / {info.session_id}")

    def select_window(self, value: str) -> None:
        if value == "__auto__" or not value:
            self.bound_hwnd = 0
            self.log("发送时将自动查找目标窗口。")
            return
        hwnd = int(value)
        info = self.window_infos.get(hwnd)
        if info:
            self.bound_hwnd = hwnd
            self.log(f"已绑定 {info.kind_label} 窗口：{info.title}")

    def open_session_folder(self) -> None:
        if self.selected_session and self.selected_session.exists():
            os.startfile(self.selected_session.parent)

    def open_log(self) -> None:
        try:
            os.startfile(LOG_PATH)
        except OSError:
            pass

    def bind_after_countdown(self) -> None:
        self.log("请在 3 秒内切换到目标窗口（VS Code 或终端）……", "warn")
        self.emit("status", {"text": "3秒后读取当前前台窗口", "kind": "warn"})
        threading.Timer(3.0, self._finish_bind_current).start()

    def _finish_bind_current(self) -> None:
        info = foreground_window()
        if not is_supported_window(info):
            self.emit("status", {"text": "绑定失败：当前前台不是受支持的窗口", "kind": "err"})
            self.log(f"绑定失败，当前窗口：{info.title if info else '无法识别'}", "err")
            self.emit("toast", {"type": "warning", "msg": "当前前台不是 VS Code 或受支持的终端"})
            return
        self.bound_hwnd = info.hwnd
        self.emit("status", {"text": f"已绑定 {info.kind_label} 窗口", "kind": "run"})
        self.log(f"已绑定当前窗口：[{info.kind_label}] {info.title}", "ok")
        self.emit("windows", self._window_payload())

    def save_config(self, cfg: dict) -> None:
        self._apply_config(cfg)
        self._write_config()

    def start_watch(self, cfg: dict) -> bool:
        self._apply_config(cfg)
        if not self.selected_session or not self.selected_session.exists():
            return False
        self._write_config()
        self.watching = True
        self.sending = False
        self.continue_count = 0
        self.handled_fingerprints.clear()
        self.last_mtime = 0.0
        self.last_state = None
        self.emit("watch", {"on": True})
        self.emit("status", {"text": "监听中：等待 Claude 会话变化", "kind": "run"})
        self.log(f"开始监听：{self.selected_session}", "ok")
        self.log(
            f"客户端={self.config['target_kind']}，模式={self.config['mode']}，"
            f"静默={self.config['quiet_seconds']}秒，最大续跑={self.config['max_continues']}次。"
        )
        if not self.config["check_existing"]:
            try:
                state = analyze_transcript(self.selected_session)
                if state.fingerprint:
                    self.handled_fingerprints.add(state.fingerprint)
                    self.last_state = state
            except Exception as exc:
                self.log(f"初始化会话状态失败：{exc}", "err")
        return True

    def stop_watch(self, reason: str = "已手动停止") -> None:
        self.watching = False
        self.emit("watch", {"on": False})
        self.emit("status", {"text": reason, "kind": "idle"})
        self.log(reason)

    def analyze_now(self) -> None:
        if not self.selected_session or not self.selected_session.exists():
            self.emit("toast", {"type": "error", "msg": "没有有效会话"})
            return
        try:
            state = analyze_transcript(self.selected_session)
            decision = decide_continue(state, self.config["mode"])
            self.log(
                f"分析结果：stop_reason={state.stop_reason}，是否续跑={decision.should_continue}，"
                f"原因={decision.reason}。"
            )
            preview = state.assistant_text[-180:].replace("\n", " ")
            if preview:
                self.log(f"末尾摘要：{preview}")
            self.emit("status", {"text": f"分析完成：{decision.reason}", "kind": "info"})
        except Exception as exc:
            self.emit("toast", {"type": "error", "msg": f"分析失败：{exc}"})

    def test_send(self) -> None:
        project_hint = ""
        if self.selected_session and self.selected_session.exists():
            try:
                project_hint = analyze_transcript(self.selected_session).cwd
            except Exception:
                pass
        self._send_prompt("手动测试", project_hint=project_hint, is_test=True)

    # ------------------------------------------------------------- internals
    def load_config(self) -> dict:
        config = dict(DEFAULT_CONFIG)
        try:
            if CONFIG_PATH.exists():
                loaded = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
                if isinstance(loaded, dict):
                    for key in DEFAULT_CONFIG:
                        if key in loaded:
                            config[key] = loaded[key]
        except Exception:
            pass
        return config

    def _apply_config(self, cfg: dict) -> None:
        if not isinstance(cfg, dict):
            return
        self.config["quiet_seconds"] = _positive_float(cfg.get("quiet_seconds"), 7)
        self.config["cooldown_seconds"] = _positive_float(cfg.get("cooldown_seconds"), 15)
        self.config["stalled_seconds"] = _positive_float(cfg.get("stalled_seconds"), 60)
        self.config["max_continues"] = _positive_int(cfg.get("max_continues"), 12)
        self.config["mode"] = cfg.get("mode", "smart") or "smart"
        self.config["target_kind"] = cfg.get("target_kind", "auto") or "auto"
        self.config["prompt"] = (cfg.get("prompt") or "").strip() or DEFAULT_CONFIG["prompt"]
        self.config["check_existing"] = bool(cfg.get("check_existing", True))
        self.config["follow_latest"] = bool(cfg.get("follow_latest", False))

    def _write_config(self) -> None:
        try:
            CONFIG_PATH.write_text(
                json.dumps(self.config, ensure_ascii=False, indent=2), encoding="utf-8"
            )
        except OSError:
            pass

    def _session_payload(self, select_latest: bool = False) -> dict:
        current = self.selected_session
        sessions = list_sessions(DEFAULT_PROJECTS_ROOT, limit=50)
        self.session_map = {format_session(info): info for info in sessions}
        items = [
            {
                "path": str(info.path),
                "display": format_session(info),
                "project": info.project_name,
                "session_id": info.session_id,
            }
            for info in sessions
        ]
        selected = ""
        if current and not select_latest and any(str(i.path) == str(current) for i in sessions):
            selected = str(current)
        elif items:
            selected = items[0]["path"]
        if selected:
            self.selected_session = Path(selected)
            self.last_mtime = 0.0
        return {"sessions": items, "selected": selected}

    def _window_payload(self) -> dict:
        windows = list_target_windows()
        self.window_infos = {w.hwnd: w for w in windows}
        items = [
            {"hwnd": w.hwnd, "display": w.display, "kind": w.kind, "kind_label": w.kind_label, "title": w.title}
            for w in windows
        ]
        selected = str(self.bound_hwnd) if self.bound_hwnd in self.window_infos else "__auto__"
        if self.bound_hwnd and self.bound_hwnd not in self.window_infos:
            self.bound_hwnd = 0
        return {"windows": items, "selected": selected}

    # ------------------------------------------------------------- monitoring
    def _poll_loop(self) -> None:
        while not self._stop.is_set():
            try:
                self._poll_once()
            except Exception as exc:
                self.log(f"监听异常：{exc}", "err")
                self.log(traceback.format_exc().strip(), "err")
            self._stop.wait(1.0)

    def _poll_once(self) -> None:
        if not (self.watching and self.selected_session and self.selected_session.exists()):
            return
        self._maybe_follow_latest()
        stat = self.selected_session.stat()
        if stat.st_mtime != self.last_mtime:
            self.last_mtime = stat.st_mtime
            self.last_state = analyze_transcript(self.selected_session)
            state = self.last_state
            if state.stop_reason == "tool_use":
                self.emit("status", {"text": "Claude 正在执行工具/命令", "kind": "run"})
            elif state.is_terminal:
                self.emit("status", {"text": f"检测到回合停止：{state.stop_reason}，等待静默期", "kind": "warn"})

        quiet = _positive_float(self.config["quiet_seconds"], 7)
        if self.last_state and time.time() - stat.st_mtime >= quiet:
            self._evaluate_state(self.last_state)
        stalled = _positive_float(self.config["stalled_seconds"], 60)
        if self.last_state and time.time() - stat.st_mtime >= stalled:
            self._evaluate_interrupted_state(self.last_state)

    def _maybe_follow_latest(self) -> None:
        if not self.config["follow_latest"] or time.time() - self.last_switch_check < 3:
            return
        self.last_switch_check = time.time()
        sessions = list_sessions(DEFAULT_PROJECTS_ROOT, limit=1)
        if not sessions or not self.selected_session:
            return
        latest = sessions[0]
        try:
            current_mtime = self.selected_session.stat().st_mtime
        except OSError:
            current_mtime = 0
        if latest.path != self.selected_session and latest.modified_at > current_mtime + 1:
            self.selected_session = latest.path
            self.last_mtime = 0.0
            self.handled_fingerprints.clear()
            self.continue_count = 0
            self.log(f"已自动切换到最新会话：{latest.project_name} [{latest.session_id[:8]}]")
            self.emit("sessions", self._session_payload())

    def _evaluate_state(self, state) -> None:
        if self.sending or not state.fingerprint or state.fingerprint in self.handled_fingerprints:
            return
        if not state.is_terminal:
            return
        decision = decide_continue(state, self.config["mode"])
        if not decision.should_continue:
            self.handled_fingerprints.add(state.fingerprint)
            self.emit("status", {"text": f"本回合无需续跑：{decision.reason}", "kind": "info"})
            self.log(f"不续跑：{decision.reason}")
            return
        if time.time() - self.last_send_at < _positive_float(self.config["cooldown_seconds"], 15):
            return
        maximum = _positive_int(self.config["max_continues"], 12)
        if self.continue_count >= maximum:
            self.handled_fingerprints.add(state.fingerprint)
            self.stop_watch(f"已达到最大续跑次数 {maximum}，为防止死循环已停止")
            return
        self.handled_fingerprints.add(state.fingerprint)
        self.sending_fingerprint = state.fingerprint
        self._send_prompt(decision.reason, state.cwd)

    def _evaluate_interrupted_state(self, state) -> None:
        if self.sending or not state.fingerprint or state.fingerprint in self.handled_fingerprints:
            return
        if state.is_terminal or not state.last_user_uuid:
            return
        if is_claude_session_process_alive(state.session_id):
            return
        maximum = _positive_int(self.config["max_continues"], 12)
        if self.continue_count >= maximum:
            self.handled_fingerprints.add(state.fingerprint)
            self.stop_watch(f"已达到最大续跑次数 {maximum}，为防止死循环已停止")
            return
        reason = "Claude 会话进程已退出，且回合没有正常结束记录"
        self.handled_fingerprints.add(state.fingerprint)
        self.sending_fingerprint = state.fingerprint
        self._send_prompt(reason, state.cwd)

    def _send_prompt(self, reason: str, project_hint: str = "", is_test: bool = False) -> None:
        prompt = self.config["prompt"].strip()
        if not prompt:
            self.emit("toast", {"type": "error", "msg": "续跑提示词不能为空"})
            return
        target = choose_target_window(
            project_hint=project_hint,
            preferred_hwnd=self.bound_hwnd,
            target_kind=self.config["target_kind"],
        )
        if not target:
            if self.sending_fingerprint:
                self.handled_fingerprints.discard(self.sending_fingerprint)
                self.sending_fingerprint = ""
            self.emit("status", {"text": "发送失败：未找到目标窗口", "kind": "err"})
            self.log("发送失败：未找到 VS Code 或终端窗口。", "err")
            return

        self.bound_hwnd = target.hwnd
        target_kind = target.kind
        kind_label = target.kind_label
        self.sending = True
        self.emit("status", {"text": f"准备发送继续：{reason}", "kind": "run"})
        self.log(f"触发续跑：{reason}；目标=[{kind_label}] {target.title}")

        def worker() -> None:
            error = None
            try:
                send_continue_to_claude(target.hwnd, prompt, target_kind=target_kind)
            except Exception as exc:
                error = exc
            self._finish_send(error, is_test, kind_label)

        threading.Thread(target=worker, name="ClaudeContinueSender", daemon=True).start()

    def _finish_send(self, error, is_test: bool, kind_label: str) -> None:
        self.sending = False
        if error:
            if self.sending_fingerprint:
                self.handled_fingerprints.discard(self.sending_fingerprint)
                self.sending_fingerprint = ""
            self.emit("status", {"text": f"发送失败：{error}", "kind": "err"})
            self.log(f"发送失败：{error}", "err")
            return
        self.last_send_at = time.time()
        self.sending_fingerprint = ""
        if not is_test:
            self.continue_count += 1
        self.emit("status", {"text": f"已发送继续；累计自动续跑 {self.continue_count} 次", "kind": "run"})
        if kind_label == "终端":
            self.log("已切换到终端窗口并向 Claude CLI 输入续跑提示词。", "ok")
        else:
            self.log("已通过命令面板聚焦 Claude Code 输入框并发送续跑提示词。", "ok")

    # -------------------------------------------------------------- emit/log
    def emit(self, name: str, payload: dict) -> None:
        if not self.window:
            return
        try:
            self.window.evaluate_js(
                f"window.__emit && window.__emit({json.dumps(name)}, {json.dumps(payload, ensure_ascii=False)})"
            )
        except Exception:
            pass

    def log(self, message: str, level: str = "info") -> None:
        now = datetime.now()
        stamp = now.strftime("%H:%M:%S")
        try:
            with LOG_PATH.open("a", encoding="utf-8") as fh:
                fh.write(now.strftime("%Y-%m-%d ") + f"[{stamp}] {message}\n")
        except OSError:
            pass
        entry = {"time": stamp, "msg": message, "level": level}
        if self.window:
            self.emit("log", entry)
        else:
            self._pending_logs.append(entry)

    # ---------------------------------------------------------------- launch
    def on_started(self, window: webview.Window) -> None:
        self.window = window
        self._poll_thread.start()
        self.log("程序已启动。默认使用智能模式；明确的 max_tokens 截断一定会自动继续。")
        self.log("已支持 Claude Code（VS Code）与 Claude CLI（终端）两种客户端。")

    def on_closing(self) -> None:
        self._stop.set()
        self.watching = False


def main() -> None:
    bridge = Bridge()
    index = WEBUI_DIR / "index.html"
    window = webview.create_window(
        f"{APP_NAME} · Claude Code 断点续跑",
        url=str(index),
        js_api=bridge,
        width=980,
        height=820,
        min_size=(860, 680),
        background_color="#0d1117",
    )
    window.events.closing += bridge.on_closing
    webview.start(bridge.on_started, window, gui="edgechromium", private_mode=False)


if __name__ == "__main__":
    main()
