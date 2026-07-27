from __future__ import annotations

import json
import os
import threading
import time
import traceback
from datetime import datetime
from pathlib import Path
import tkinter as tk
from tkinter import messagebox, ttk

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

MODE_LABELS = {
    "安全：只处理 max_tokens": "safe",
    "智能：截断 + 明确未完成语句": "smart",
    "严格：直到检测到完成标记": "strict",
}
MODE_BY_VALUE = {value: label for label, value in MODE_LABELS.items()}

# Which Claude client should receive the continue prompt.
TARGET_LABELS = {
    "自动（VS Code 或终端）": "auto",
    "Claude Code（VS Code 插件）": "vscode",
    "Claude CLI（终端）": "terminal",
}
TARGET_BY_VALUE = {value: label for label, value in TARGET_LABELS.items()}

DEFAULT_CONFIG = {
    "projects_root": str(DEFAULT_PROJECTS_ROOT),
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


# ---------------------------------------------------------------------------
# Clean light theme palette
# ---------------------------------------------------------------------------
class Palette:
    bg = "#f4f6fb"           # window background
    surface = "#ffffff"      # card background
    surface_alt = "#f0f2f7"  # inputs / raised
    border = "#dfe3ec"
    text = "#1f2733"
    muted = "#6b7688"
    accent = "#3b6fe0"       # primary action
    accent_hover = "#5384ea"
    accent_active = "#2f5bc0"
    success = "#1f9d63"
    warning = "#c07d0a"
    danger = "#d94b4b"
    log_bg = "#ffffff"


UI_FONT = "Microsoft YaHei UI"
MONO_FONT = "Cascadia Code"


class MonitorApp:
    def __init__(self, root: tk.Tk) -> None:
        self.root = root
        self.root.title(f"{APP_NAME} · Claude Code 断点续跑")
        self.root.geometry("940x740")
        self.root.minsize(820, 640)
        self.root.configure(bg=Palette.bg)

        APP_DIR.mkdir(parents=True, exist_ok=True)
        self.config = self.load_config()

        self.session_map: dict[str, SessionInfo] = {}
        self.window_map: dict[str, WindowInfo] = {}
        self.selected_session: Path | None = None
        self.bound_hwnd: int = 0
        self.bound_window_title = "未绑定（发送时自动查找）"
        self.watching = False
        self.sending = False
        self.continue_count = 0
        self.handled_fingerprints: set[str] = set()
        self.last_mtime = 0.0
        self.last_state = None
        self.last_send_at = 0.0
        self.last_switch_check = 0.0
        self.sending_fingerprint = ""

        self.status_var = tk.StringVar(value="未启动")
        self.status_kind = "idle"
        self.session_var = tk.StringVar()
        self.window_var = tk.StringVar(value=self.bound_window_title)
        self.mode_var = tk.StringVar(value=MODE_BY_VALUE.get(self.config["mode"], MODE_BY_VALUE["smart"]))
        self.target_var = tk.StringVar(
            value=TARGET_BY_VALUE.get(self.config.get("target_kind", "auto"), TARGET_BY_VALUE["auto"])
        )
        self.quiet_var = tk.StringVar(value=str(self.config["quiet_seconds"]))
        self.cooldown_var = tk.StringVar(value=str(self.config["cooldown_seconds"]))
        self.stalled_var = tk.StringVar(value=str(self.config["stalled_seconds"]))
        self.max_var = tk.StringVar(value=str(self.config["max_continues"]))
        self.prompt_var = tk.StringVar(value=self.config["prompt"])
        self.check_existing_var = tk.BooleanVar(value=bool(self.config["check_existing"]))
        self.follow_latest_var = tk.BooleanVar(value=bool(self.config["follow_latest"]))

        self.setup_style()
        self.build_ui()
        self.refresh_sessions(select_latest=True)
        self.refresh_windows()
        self.root.protocol("WM_DELETE_WINDOW", self.on_close)
        self.root.after(1000, self.poll)

        self.log("程序已启动。默认使用智能模式；明确的 max_tokens 截断一定会自动继续。")
        self.log("已支持 Claude Code（VS Code）与 Claude CLI（终端）两种客户端。")

    # ------------------------------------------------------------------ style
    def setup_style(self) -> None:
        style = ttk.Style(self.root)
        try:
            style.theme_use("clam")
        except tk.TclError:
            pass

        p = Palette
        style.configure(".", background=p.bg, foreground=p.text, font=(UI_FONT, 10))

        style.configure("App.TFrame", background=p.bg)
        style.configure("Card.TFrame", background=p.surface)
        style.configure("Header.TFrame", background=p.bg)

        style.configure("TLabel", background=p.surface, foreground=p.text, font=(UI_FONT, 10))
        style.configure("App.TLabel", background=p.bg, foreground=p.text)
        style.configure("Title.TLabel", background=p.bg, foreground=p.text, font=(UI_FONT, 19, "bold"))
        style.configure("Subtitle.TLabel", background=p.bg, foreground=p.muted, font=(UI_FONT, 10))
        style.configure("CardTitle.TLabel", background=p.surface, foreground=p.accent, font=(UI_FONT, 11, "bold"))
        style.configure("Muted.TLabel", background=p.surface, foreground=p.muted, font=(UI_FONT, 9))
        style.configure("FieldLabel.TLabel", background=p.surface, foreground=p.muted, font=(UI_FONT, 9))

        # Buttons -----------------------------------------------------------
        style.configure(
            "TButton",
            background=p.surface_alt,
            foreground=p.text,
            bordercolor=p.border,
            focuscolor=p.surface,
            relief="flat",
            padding=(12, 7),
            font=(UI_FONT, 10),
        )
        style.map(
            "TButton",
            background=[("active", p.border), ("pressed", p.border)],
            foreground=[("disabled", p.muted)],
        )
        style.configure(
            "Accent.TButton",
            background=p.accent,
            foreground="#ffffff",
            relief="flat",
            padding=(16, 8),
            font=(UI_FONT, 10, "bold"),
        )
        style.map(
            "Accent.TButton",
            background=[("active", p.accent_hover), ("pressed", p.accent_active), ("disabled", p.surface_alt)],
            foreground=[("disabled", p.muted)],
        )
        style.configure(
            "Stop.TButton",
            background=p.danger,
            foreground="#ffffff",
            relief="flat",
            padding=(16, 8),
            font=(UI_FONT, 10, "bold"),
        )
        style.map("Stop.TButton", background=[("active", "#ff8585"), ("pressed", "#e65a5a")])

        # Entries & combos --------------------------------------------------
        style.configure(
            "TEntry",
            fieldbackground=p.surface_alt,
            background=p.surface_alt,
            foreground=p.text,
            bordercolor=p.border,
            insertcolor=p.text,
            relief="flat",
            padding=6,
        )
        style.map("TEntry", bordercolor=[("focus", p.accent)])
        style.configure(
            "TCombobox",
            fieldbackground=p.surface_alt,
            background=p.surface_alt,
            foreground=p.text,
            arrowcolor=p.muted,
            bordercolor=p.border,
            relief="flat",
            padding=6,
        )
        style.map(
            "TCombobox",
            fieldbackground=[("readonly", p.surface_alt)],
            foreground=[("readonly", p.text)],
            bordercolor=[("focus", p.accent)],
            arrowcolor=[("active", p.text)],
        )
        # Dropdown list colors (applies to the popdown listbox).
        self.root.option_add("*TCombobox*Listbox.background", p.surface_alt)
        self.root.option_add("*TCombobox*Listbox.foreground", p.text)
        self.root.option_add("*TCombobox*Listbox.selectBackground", p.accent)
        self.root.option_add("*TCombobox*Listbox.selectForeground", "#ffffff")
        self.root.option_add("*TCombobox*Listbox.font", (UI_FONT, 10))

        # Checkbuttons ------------------------------------------------------
        style.configure(
            "TCheckbutton",
            background=p.surface,
            foreground=p.text,
            focuscolor=p.surface,
            font=(UI_FONT, 9),
        )
        style.map(
            "TCheckbutton",
            background=[("active", p.surface)],
            indicatorcolor=[("selected", p.accent), ("!selected", p.surface_alt)],
        )

        # Scrollbar ---------------------------------------------------------
        style.configure(
            "Vertical.TScrollbar",
            background=p.surface_alt,
            troughcolor=p.surface,
            bordercolor=p.surface,
            arrowcolor=p.muted,
            relief="flat",
        )
        style.map("Vertical.TScrollbar", background=[("active", p.border)])

        style.configure("Sep.TFrame", background=p.border)

    def _card(self, parent, title: str) -> ttk.Frame:
        """A rounded-look card: a bordered surface frame with a title row."""
        wrapper = tk.Frame(parent, bg=Palette.border)  # 1px border via padding
        wrapper.configure(highlightthickness=0)
        card = ttk.Frame(wrapper, style="Card.TFrame", padding=16)
        card.pack(fill="both", expand=True, padx=1, pady=1)
        card.columnconfigure(0, weight=1)
        header = ttk.Frame(card, style="Card.TFrame")
        header.grid(row=0, column=0, sticky="ew", pady=(0, 12))
        header.columnconfigure(1, weight=1)
        dot = tk.Canvas(header, width=10, height=10, bg=Palette.surface, highlightthickness=0)
        dot.create_oval(1, 1, 9, 9, fill=Palette.accent, outline="")
        dot.grid(row=0, column=0, padx=(0, 8))
        ttk.Label(header, text=title, style="CardTitle.TLabel").grid(row=0, column=1, sticky="w")
        body = ttk.Frame(card, style="Card.TFrame")
        body.grid(row=1, column=0, sticky="nsew")
        body.columnconfigure(0, weight=1)
        card.rowconfigure(1, weight=1)
        return wrapper, body

    # -------------------------------------------------------------------- ui
    def build_ui(self) -> None:
        p = Palette
        outer = ttk.Frame(self.root, style="App.TFrame", padding=18)
        outer.pack(fill="both", expand=True)
        outer.columnconfigure(0, weight=1)
        outer.rowconfigure(4, weight=1)

        # Header -----------------------------------------------------------
        header = ttk.Frame(outer, style="Header.TFrame")
        header.grid(row=0, column=0, sticky="ew")
        header.columnconfigure(1, weight=1)

        logo = tk.Canvas(header, width=44, height=44, bg=p.bg, highlightthickness=0)
        logo.create_oval(2, 2, 42, 42, fill=p.accent, outline="")
        logo.create_text(22, 22, text="⟳", fill="#ffffff", font=(UI_FONT, 20, "bold"))
        logo.grid(row=0, column=0, rowspan=2, padx=(0, 14))
        ttk.Label(header, text="Claude 自动续跑监听器", style="Title.TLabel").grid(row=0, column=1, sticky="w")
        ttk.Label(
            header,
            text="监听 ~/.claude/projects 会话记录；发现截断或明确未完成后，自动向 Claude Code 或 Claude CLI 发送继续。",
            style="Subtitle.TLabel",
        ).grid(row=1, column=1, sticky="w", pady=(3, 0))

        # Status pill (top-right)
        self.status_dot = tk.Canvas(header, width=12, height=12, bg=p.bg, highlightthickness=0)
        self.status_dot.grid(row=0, column=2, rowspan=2, sticky="e", padx=(0, 8))
        self._draw_status_dot(p.muted)
        self.header_status = ttk.Label(header, textvariable=self.status_var, style="Subtitle.TLabel")
        self.header_status.grid(row=0, column=3, rowspan=2, sticky="e")

        ttk.Frame(outer, style="Sep.TFrame", height=1).grid(row=1, column=0, sticky="ew", pady=(14, 14))

        # Session card -----------------------------------------------------
        session_wrap, session_box = self._card(outer, "1 · Claude 会话")
        session_wrap.grid(row=2, column=0, sticky="ew", pady=(0, 12))
        session_box.columnconfigure(0, weight=1)

        row0 = ttk.Frame(session_box, style="Card.TFrame")
        row0.grid(row=0, column=0, sticky="ew")
        row0.columnconfigure(0, weight=1)
        self.session_combo = ttk.Combobox(row0, textvariable=self.session_var, state="readonly")
        self.session_combo.grid(row=0, column=0, sticky="ew", padx=(0, 8))
        self.session_combo.bind("<<ComboboxSelected>>", self.on_session_selected)
        ttk.Button(row0, text="刷新会话", command=self.refresh_sessions).grid(row=0, column=1, padx=(0, 6))
        ttk.Button(row0, text="打开目录", command=self.open_session_folder).grid(row=0, column=2)

        window_row = ttk.Frame(session_box, style="Card.TFrame")
        window_row.grid(row=1, column=0, sticky="ew", pady=(12, 0))
        window_row.columnconfigure(1, weight=1)
        ttk.Label(window_row, text="目标窗口", style="FieldLabel.TLabel").grid(row=0, column=0, sticky="w")
        self.window_combo = ttk.Combobox(window_row, textvariable=self.window_var, state="readonly")
        self.window_combo.grid(row=0, column=1, sticky="ew", padx=(10, 8))
        self.window_combo.bind("<<ComboboxSelected>>", self.on_window_selected)
        ttk.Button(window_row, text="刷新窗口", command=self.refresh_windows).grid(row=0, column=2, padx=(0, 6))
        ttk.Button(window_row, text="3秒后绑定当前窗口", command=self.bind_after_countdown).grid(row=0, column=3)

        # Settings card ----------------------------------------------------
        settings_wrap, settings = self._card(outer, "2 · 监听设置")
        settings_wrap.grid(row=3, column=0, sticky="ew", pady=(0, 12))
        for column in range(4):
            settings.columnconfigure(column, weight=1 if column % 2 == 1 else 0)

        ttk.Label(settings, text="客户端类型", style="FieldLabel.TLabel").grid(row=0, column=0, sticky="w", pady=(0, 4))
        ttk.Combobox(
            settings, textvariable=self.target_var, values=list(TARGET_LABELS), state="readonly"
        ).grid(row=0, column=1, sticky="ew", padx=(10, 20), pady=(0, 4))
        ttk.Label(settings, text="判断模式", style="FieldLabel.TLabel").grid(row=0, column=2, sticky="w", pady=(0, 4))
        ttk.Combobox(
            settings, textvariable=self.mode_var, values=list(MODE_LABELS), state="readonly"
        ).grid(row=0, column=3, sticky="ew", padx=(10, 0), pady=(0, 4))

        ttk.Label(settings, text="静默等待(秒)", style="FieldLabel.TLabel").grid(row=1, column=0, sticky="w", pady=(10, 4))
        ttk.Entry(settings, textvariable=self.quiet_var, width=8).grid(row=1, column=1, sticky="w", padx=(10, 20), pady=(10, 4))
        ttk.Label(settings, text="最大续跑次数", style="FieldLabel.TLabel").grid(row=1, column=2, sticky="w", pady=(10, 4))
        ttk.Entry(settings, textvariable=self.max_var, width=8).grid(row=1, column=3, sticky="w", padx=(10, 0), pady=(10, 4))

        ttk.Label(settings, text="发送冷却(秒)", style="FieldLabel.TLabel").grid(row=2, column=0, sticky="w", pady=(10, 4))
        ttk.Entry(settings, textvariable=self.cooldown_var, width=8).grid(row=2, column=1, sticky="w", padx=(10, 20), pady=(10, 4))
        ttk.Label(settings, text="断线判定(秒)", style="FieldLabel.TLabel").grid(row=2, column=2, sticky="w", pady=(10, 4))
        ttk.Entry(settings, textvariable=self.stalled_var, width=8).grid(row=2, column=3, sticky="w", padx=(10, 0), pady=(10, 4))

        ttk.Label(settings, text="续跑提示词", style="FieldLabel.TLabel").grid(row=3, column=0, sticky="w", pady=(12, 4))
        ttk.Entry(settings, textvariable=self.prompt_var).grid(
            row=3, column=1, columnspan=3, sticky="ew", padx=(10, 0), pady=(12, 4)
        )

        checks = ttk.Frame(settings, style="Card.TFrame")
        checks.grid(row=4, column=0, columnspan=4, sticky="w", pady=(12, 0))
        ttk.Checkbutton(checks, text="启动时检查当前已停止状态", variable=self.check_existing_var).pack(side="left")
        ttk.Checkbutton(
            checks, text="自动跟随最新会话（多窗口时慎用）", variable=self.follow_latest_var
        ).pack(side="left", padx=(20, 0))

        # Control + log ----------------------------------------------------
        control = ttk.Frame(outer, style="App.TFrame")
        control.grid(row=4, column=0, sticky="nsew")
        control.columnconfigure(0, weight=1)
        control.rowconfigure(2, weight=1)

        buttons = ttk.Frame(control, style="App.TFrame")
        buttons.grid(row=0, column=0, sticky="ew", pady=(0, 10))
        self.start_button = ttk.Button(buttons, text="▶  开始监听", style="Accent.TButton", command=self.toggle_watch)
        self.start_button.pack(side="left")
        ttk.Button(buttons, text="立即分析一次", command=self.analyze_now).pack(side="left", padx=(8, 0))
        ttk.Button(buttons, text="测试发送", command=self.test_send).pack(side="left", padx=(8, 0))
        ttk.Button(buttons, text="打开日志", command=lambda: os.startfile(LOG_PATH)).pack(side="left", padx=(8, 0))

        ttk.Label(control, textvariable=self.status_var, style="App.TLabel", font=(UI_FONT, 10, "bold")).grid(
            row=1, column=0, sticky="w", pady=(0, 8)
        )

        log_wrap, log_body = self._card(control, "运行日志")
        log_wrap.grid(row=2, column=0, sticky="nsew")
        log_body.rowconfigure(0, weight=1)
        log_body.columnconfigure(0, weight=1)
        self.log_text = tk.Text(
            log_body,
            wrap="word",
            height=13,
            state="disabled",
            font=(MONO_FONT, 9),
            bg=Palette.log_bg,
            fg=Palette.text,
            insertbackground=Palette.text,
            selectbackground=Palette.accent,
            relief="flat",
            borderwidth=0,
            padx=10,
            pady=8,
        )
        self.log_text.grid(row=0, column=0, sticky="nsew")
        self.log_text.tag_configure("time", foreground=Palette.muted)
        self.log_text.tag_configure("info", foreground=Palette.text)
        self.log_text.tag_configure("ok", foreground=Palette.success)
        self.log_text.tag_configure("warn", foreground=Palette.warning)
        self.log_text.tag_configure("err", foreground=Palette.danger)
        scrollbar = ttk.Scrollbar(log_body, command=self.log_text.yview, style="Vertical.TScrollbar")
        scrollbar.grid(row=0, column=1, sticky="ns")
        self.log_text.configure(yscrollcommand=scrollbar.set)

        footer = ttk.Label(
            outer,
            text="安全提示：程序最多续跑指定次数；不会自动批准权限弹窗，也不会绕过 Claude Code 的安全确认。",
            style="Subtitle.TLabel",
        )
        footer.grid(row=5, column=0, sticky="w", pady=(12, 0))

    def _draw_status_dot(self, color: str) -> None:
        self.status_dot.delete("all")
        self.status_dot.create_oval(2, 2, 10, 10, fill=color, outline="")

    def set_status(self, text: str, kind: str = "info") -> None:
        self.status_var.set(text)
        color = {
            "idle": Palette.muted,
            "info": Palette.accent,
            "run": Palette.success,
            "warn": Palette.warning,
            "err": Palette.danger,
        }.get(kind, Palette.accent)
        self._draw_status_dot(color)

    # ---------------------------------------------------------------- config
    def load_config(self) -> dict:
        config = dict(DEFAULT_CONFIG)
        try:
            if CONFIG_PATH.exists():
                loaded = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
                if isinstance(loaded, dict):
                    config.update(loaded)
        except Exception:
            pass
        return config

    def save_config(self) -> None:
        data = {
            "projects_root": str(DEFAULT_PROJECTS_ROOT),
            "quiet_seconds": self._positive_float(self.quiet_var.get(), 7),
            "cooldown_seconds": self._positive_float(self.cooldown_var.get(), 15),
            "stalled_seconds": self._positive_float(self.stalled_var.get(), 60),
            "max_continues": self._positive_int(self.max_var.get(), 12),
            "mode": MODE_LABELS.get(self.mode_var.get(), "smart"),
            "target_kind": TARGET_LABELS.get(self.target_var.get(), "auto"),
            "prompt": self.prompt_var.get().strip() or DEFAULT_CONFIG["prompt"],
            "check_existing": self.check_existing_var.get(),
            "follow_latest": self.follow_latest_var.get(),
        }
        CONFIG_PATH.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")

    @staticmethod
    def _positive_int(value: str, fallback: int) -> int:
        try:
            return max(1, int(value))
        except (TypeError, ValueError):
            return fallback

    @staticmethod
    def _positive_float(value: str, fallback: float) -> float:
        try:
            return max(0.5, float(value))
        except (TypeError, ValueError):
            return fallback

    def current_target_kind(self) -> str:
        return TARGET_LABELS.get(self.target_var.get(), "auto")

    def log(self, message: str, level: str = "info") -> None:
        stamp = datetime.now().strftime("%H:%M:%S")
        try:
            with LOG_PATH.open("a", encoding="utf-8") as fh:
                fh.write(datetime.now().strftime("%Y-%m-%d ") + f"[{stamp}] {message}\n")
        except OSError:
            pass
        self.log_text.configure(state="normal")
        self.log_text.insert("end", f"{stamp}  ", ("time",))
        self.log_text.insert("end", message + "\n", (level,))
        self.log_text.see("end")
        self.log_text.configure(state="disabled")

    # -------------------------------------------------------------- sessions
    def refresh_sessions(self, select_latest: bool = False) -> None:
        current_path = self.selected_session
        sessions = list_sessions(Path(self.config.get("projects_root", DEFAULT_PROJECTS_ROOT)), limit=50)
        self.session_map = {format_session(info): info for info in sessions}
        values = list(self.session_map)
        self.session_combo["values"] = values
        if not values:
            self.session_var.set("未找到 Claude Code 会话")
            self.selected_session = None
            return

        selected_display = None
        if current_path and not select_latest:
            selected_display = next(
                (display for display, info in self.session_map.items() if info.path == current_path), None
            )
        selected_display = selected_display or values[0]
        self.session_var.set(selected_display)
        self.selected_session = self.session_map[selected_display].path
        self.last_mtime = 0.0
        if not select_latest:
            self.log(f"会话列表已刷新，共 {len(values)} 个。")

    def on_session_selected(self, _event=None) -> None:
        info = self.session_map.get(self.session_var.get())
        if info:
            self.selected_session = info.path
            self.last_mtime = 0.0
            self.handled_fingerprints.clear()
            self.continue_count = 0
            self.log(f"已选择会话：{info.project_name} / {info.session_id}")

    def refresh_windows(self) -> None:
        windows = list_target_windows()
        self.window_map = {window.display: window for window in windows}
        auto_label = "自动查找（优先匹配会话项目）"
        values = [auto_label, *self.window_map.keys()]
        self.window_combo["values"] = values

        if self.bound_hwnd:
            match = next((window for window in windows if window.hwnd == self.bound_hwnd), None)
            if match:
                self.window_var.set(match.display)
                self.bound_window_title = match.title
                return
            self.bound_hwnd = 0
        self.window_var.set(auto_label)
        if not windows:
            self.log("暂未发现 VS Code 或终端窗口。请先打开 Claude Code 或运行 claude CLI，再刷新。", "warn")

    def on_window_selected(self, _event=None) -> None:
        window = self.window_map.get(self.window_var.get())
        if window:
            self.bound_hwnd = window.hwnd
            self.bound_window_title = window.title
            self.log(f"已绑定 {window.kind_label} 窗口：{window.title}")
        else:
            self.bound_hwnd = 0
            self.bound_window_title = "自动查找"
            self.log("发送时将自动查找目标窗口。")

    def bind_after_countdown(self) -> None:
        self.log("请在 3 秒内切换到目标窗口（VS Code 或终端）……", "warn")
        self.set_status("3秒后读取当前前台窗口", "warn")
        self.root.after(3000, self.finish_bind_current)

    def finish_bind_current(self) -> None:
        info = foreground_window()
        if not is_supported_window(info):
            self.set_status("绑定失败：当前前台不是受支持的窗口", "err")
            self.log(f"绑定失败，当前窗口：{info.title if info else '无法识别'}", "err")
            messagebox.showwarning(APP_NAME, "当前前台窗口不是 VS Code 或受支持的终端，请重试。")
            return
        assert info is not None
        self.bound_hwnd = info.hwnd
        self.bound_window_title = info.title
        self.refresh_windows()
        self.set_status(f"已绑定 {info.kind_label} 窗口", "run")
        self.log(f"已绑定当前窗口：[{info.kind_label}] {info.title}", "ok")

    def open_session_folder(self) -> None:
        if self.selected_session and self.selected_session.exists():
            os.startfile(self.selected_session.parent)

    # ----------------------------------------------------------------- watch
    def toggle_watch(self) -> None:
        if self.watching:
            self.stop_watch("已手动停止")
        else:
            self.start_watch()

    def start_watch(self) -> None:
        if not self.selected_session or not self.selected_session.exists():
            messagebox.showerror(APP_NAME, "请先选择有效的 Claude Code 会话。")
            return
        self.save_config()
        self.watching = True
        self.sending = False
        self.continue_count = 0
        self.handled_fingerprints.clear()
        self.last_mtime = 0.0
        self.last_state = None
        self.start_button.configure(text="■  停止监听", style="Stop.TButton")
        self.set_status("监听中：等待 Claude 会话变化", "run")
        self.log(f"开始监听：{self.selected_session}", "ok")
        self.log(
            f"客户端={TARGET_LABELS.get(self.target_var.get(), 'auto')}，"
            f"模式={MODE_LABELS.get(self.mode_var.get(), 'smart')}，"
            f"静默={self.quiet_var.get()}秒，最大续跑={self.max_var.get()}次。"
        )
        if not self.check_existing_var.get():
            try:
                state = analyze_transcript(self.selected_session)
                if state.fingerprint:
                    self.handled_fingerprints.add(state.fingerprint)
                    self.last_state = state
            except Exception as exc:
                self.log(f"初始化会话状态失败：{exc}", "err")

    def stop_watch(self, reason: str) -> None:
        self.watching = False
        self.start_button.configure(text="▶  开始监听", style="Accent.TButton")
        self.set_status(reason, "idle")
        self.log(reason)

    def maybe_follow_latest(self) -> None:
        if not self.follow_latest_var.get() or time.time() - self.last_switch_check < 3:
            return
        self.last_switch_check = time.time()
        sessions = list_sessions(Path(self.config.get("projects_root", DEFAULT_PROJECTS_ROOT)), limit=1)
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
            self.refresh_sessions()

    def poll(self) -> None:
        try:
            if self.watching and self.selected_session and self.selected_session.exists():
                self.maybe_follow_latest()
                stat = self.selected_session.stat()
                if stat.st_mtime != self.last_mtime:
                    self.last_mtime = stat.st_mtime
                    self.last_state = analyze_transcript(self.selected_session)
                    state = self.last_state
                    if state.stop_reason == "tool_use":
                        self.set_status("Claude 正在执行工具/命令", "run")
                    elif state.is_terminal:
                        self.set_status(f"检测到回合停止：{state.stop_reason}，等待静默期", "warn")

                quiet_seconds = self._positive_float(self.quiet_var.get(), 7)
                if self.last_state and time.time() - stat.st_mtime >= quiet_seconds:
                    self.evaluate_state(self.last_state)
                stalled_seconds = self._positive_float(self.stalled_var.get(), 60)
                if self.last_state and time.time() - stat.st_mtime >= stalled_seconds:
                    self.evaluate_interrupted_state(self.last_state)
        except Exception as exc:
            self.log(f"监听异常：{exc}", "err")
            self.log(traceback.format_exc().strip(), "err")
        finally:
            self.root.after(1000, self.poll)

    def evaluate_state(self, state) -> None:
        if self.sending or not state.fingerprint or state.fingerprint in self.handled_fingerprints:
            return
        if not state.is_terminal:
            return

        mode = MODE_LABELS.get(self.mode_var.get(), "smart")
        decision = decide_continue(state, mode)
        if not decision.should_continue:
            self.handled_fingerprints.add(state.fingerprint)
            self.set_status(f"本回合无需续跑：{decision.reason}", "info")
            self.log(f"不续跑：{decision.reason}")
            return

        cooldown = self._positive_float(self.cooldown_var.get(), 15)
        if time.time() - self.last_send_at < cooldown:
            return

        maximum = self._positive_int(self.max_var.get(), 12)
        if self.continue_count >= maximum:
            self.handled_fingerprints.add(state.fingerprint)
            self.stop_watch(f"已达到最大续跑次数 {maximum}，为防止死循环已停止")
            return

        self.handled_fingerprints.add(state.fingerprint)
        self.sending_fingerprint = state.fingerprint
        self.send_prompt(decision.reason, state.cwd)

    def evaluate_interrupted_state(self, state) -> None:
        if self.sending or not state.fingerprint or state.fingerprint in self.handled_fingerprints:
            return
        if state.is_terminal or not state.last_user_uuid:
            return
        if is_claude_session_process_alive(state.session_id):
            return

        maximum = self._positive_int(self.max_var.get(), 12)
        if self.continue_count >= maximum:
            self.handled_fingerprints.add(state.fingerprint)
            self.stop_watch(f"已达到最大续跑次数 {maximum}，为防止死循环已停止")
            return

        reason = "Claude 会话进程已退出，且回合没有正常结束记录"
        self.handled_fingerprints.add(state.fingerprint)
        self.sending_fingerprint = state.fingerprint
        self.send_prompt(reason, state.cwd)

    # ------------------------------------------------------------ send logic
    def resolve_target_window(self, project_hint: str = "") -> WindowInfo | None:
        return choose_target_window(
            project_hint=project_hint,
            preferred_hwnd=self.bound_hwnd,
            target_kind=self.current_target_kind(),
        )

    def send_prompt(self, reason: str, project_hint: str = "", is_test: bool = False) -> None:
        prompt = self.prompt_var.get().strip()
        if not prompt:
            messagebox.showerror(APP_NAME, "续跑提示词不能为空。")
            return
        target = self.resolve_target_window(project_hint)
        if not target:
            if self.sending_fingerprint:
                self.handled_fingerprints.discard(self.sending_fingerprint)
                self.sending_fingerprint = ""
            self.set_status("发送失败：未找到目标窗口", "err")
            self.log("发送失败：未找到 VS Code 或终端窗口。", "err")
            return

        self.bound_hwnd = target.hwnd
        target_kind = target.kind
        self.sending = True
        self.set_status(f"准备发送继续：{reason}", "run")
        self.log(f"触发续跑：{reason}；目标=[{target.kind_label}] {target.title}")

        def worker() -> None:
            error = None
            try:
                send_continue_to_claude(target.hwnd, prompt, target_kind=target_kind)
            except Exception as exc:  # UI automation errors need to return to Tk main thread.
                error = exc
            self.root.after(0, lambda: self.finish_send(error, is_test, target.kind_label))

        threading.Thread(target=worker, name="ClaudeContinueSender", daemon=True).start()

    def finish_send(self, error: Exception | None, is_test: bool, kind_label: str = "") -> None:
        self.sending = False
        if error:
            if self.sending_fingerprint:
                self.handled_fingerprints.discard(self.sending_fingerprint)
                self.sending_fingerprint = ""
            self.set_status(f"发送失败：{error}", "err")
            self.log(f"发送失败：{error}", "err")
            return
        self.last_send_at = time.time()
        self.sending_fingerprint = ""
        if not is_test:
            self.continue_count += 1
        self.set_status(f"已发送继续；累计自动续跑 {self.continue_count} 次", "run")
        if kind_label == "终端":
            self.log("已切换到终端窗口并向 Claude CLI 输入续跑提示词。", "ok")
        else:
            self.log("已通过命令面板聚焦 Claude Code 输入框并发送续跑提示词。", "ok")

    def analyze_now(self) -> None:
        if not self.selected_session or not self.selected_session.exists():
            messagebox.showerror(APP_NAME, "没有有效会话。")
            return
        try:
            state = analyze_transcript(self.selected_session)
            decision = decide_continue(state, MODE_LABELS.get(self.mode_var.get(), "smart"))
            preview = state.assistant_text[-180:].replace("\n", " ")
            self.log(
                f"分析结果：stop_reason={state.stop_reason}，是否续跑={decision.should_continue}，原因={decision.reason}。"
            )
            if preview:
                self.log(f"末尾摘要：{preview}")
            self.set_status(f"分析完成：{decision.reason}", "info")
        except Exception as exc:
            messagebox.showerror(APP_NAME, f"分析失败：{exc}")

    def test_send(self) -> None:
        if not messagebox.askyesno(
            APP_NAME,
            "测试会真实切换到目标窗口（VS Code 或终端），并发送当前续跑提示词。是否继续？",
        ):
            return
        project_hint = ""
        if self.selected_session and self.selected_session.exists():
            try:
                project_hint = analyze_transcript(self.selected_session).cwd
            except Exception:
                pass
        self.send_prompt("手动测试", project_hint=project_hint, is_test=True)

    def on_close(self) -> None:
        try:
            self.save_config()
        finally:
            self.root.destroy()


def main() -> None:
    root = tk.Tk()
    MonitorApp(root)
    root.mainloop()


if __name__ == "__main__":
    main()
