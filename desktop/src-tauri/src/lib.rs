// Claude Auto Continue - Tauri backend.
//
// Ports the previous Python monitor (core.py / windows_automation.py / app.pyw)
// to a native Rust + Tauri application. The webview only renders a lightweight
// hand-written UI; all analysis and window automation happen here.

mod automation;
mod core;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use automation::{
    choose_target_window, list_target_windows, send_continue_to_claude, WindowItem,
};
use core::{analyze_transcript, decide_continue, list_sessions, projects_root, SessionItem};

// --------------------------------------------------------------------------
// Config
// --------------------------------------------------------------------------
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub quiet_seconds: f64,
    pub cooldown_seconds: f64,
    pub stalled_seconds: f64,
    pub max_continues: u32,
    pub mode: String,
    pub target_kind: String,
    pub prompt: String,
    pub check_existing: bool,
    pub follow_latest: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            quiet_seconds: 7.0,
            cooldown_seconds: 15.0,
            stalled_seconds: 60.0,
            max_continues: 12,
            mode: "smart".into(),
            target_kind: "auto".into(),
            prompt: "请继续完成刚才未完成的任务。不要只汇报进度，请直接继续执行；完成实现与必要验证后再结束。"
                .into(),
            check_existing: true,
            follow_latest: false,
        }
    }
}

fn app_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home());
    base.join("ClaudeAutoContinue")
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

fn log_path() -> PathBuf {
    app_dir().join("monitor.log")
}

fn load_config() -> Config {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
            return cfg;
        }
    }
    Config::default()
}

fn save_config_to_disk(cfg: &Config) {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(), text);
    }
}

// --------------------------------------------------------------------------
// Shared monitor state
// --------------------------------------------------------------------------
#[derive(Default)]
struct Watch {
    watching: bool,
    sending: bool,
    continue_count: u32,
    handled: std::collections::HashSet<String>,
    last_mtime: f64,
    last_send_at: f64,
    last_switch_check: f64,
    sending_fingerprint: String,
    selected_session: Option<PathBuf>,
    bound_hwnd: isize,
}

pub struct AppState {
    config: Mutex<Config>,
    watch: Mutex<Watch>,
    session_index: Mutex<Vec<SessionItem>>,
    window_index: Mutex<Vec<WindowItem>>,
    stop_flag: Arc<AtomicBool>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --------------------------------------------------------------------------
// Emit + log helpers
// --------------------------------------------------------------------------
#[derive(Clone, Serialize)]
struct LogEntry {
    time: String,
    msg: String,
    level: String,
}

#[derive(Clone, Serialize)]
struct StatusPayload {
    text: String,
    kind: String,
}

fn local_hms() -> String {
    // Lightweight local HH:MM:SS without pulling chrono.
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Offset applied by the OS is ignored here; show UTC-based clock adjusted
    // by the machine timezone via the `time` crate is overkill. Use local via
    // a simple approach: rely on chrono-free conversion.
    let local = secs as i64 + local_offset_seconds();
    let day = local.rem_euclid(86_400);
    let h = day / 3600;
    let m = (day % 3600) / 60;
    let s = day % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn local_offset_seconds() -> i64 {
    // Query Windows for the current UTC bias.
    #[cfg(windows)]
    {
        automation::utc_offset_seconds()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn emit_log(app: &AppHandle, msg: &str, level: &str) {
    let entry = LogEntry {
        time: local_hms(),
        msg: msg.to_string(),
        level: level.to_string(),
    };
    // Append to log file.
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{} {}", entry.time, entry.msg);
    }
    let _ = app.emit("log", entry);
}

fn emit_status(app: &AppHandle, text: &str, kind: &str) {
    let _ = app.emit(
        "status",
        StatusPayload {
            text: text.to_string(),
            kind: kind.to_string(),
        },
    );
}

// --------------------------------------------------------------------------
// Tauri commands
// --------------------------------------------------------------------------
#[derive(Serialize, Clone)]
struct SessionsPayload {
    sessions: Vec<SessionItem>,
    selected: String,
}

#[derive(Serialize, Clone)]
struct WindowsPayload {
    windows: Vec<WindowItem>,
    selected: String,
}

#[derive(Serialize)]
struct InitialPayload {
    config: Config,
    sessions: Vec<SessionItem>,
    selected_session: String,
    windows: Vec<WindowItem>,
    selected_window: String,
}

fn build_sessions(state: &AppState, select_latest: bool) -> SessionsPayload {
    let sessions = list_sessions(&projects_root(), 50);
    let mut watch = state.watch.lock().unwrap();
    let current = watch.selected_session.clone();

    let mut selected = String::new();
    if !select_latest {
        if let Some(cur) = &current {
            if sessions.iter().any(|s| PathBuf::from(&s.path) == *cur) {
                selected = cur.to_string_lossy().to_string();
            }
        }
    }
    if selected.is_empty() {
        if let Some(first) = sessions.first() {
            selected = first.path.clone();
        }
    }
    if !selected.is_empty() {
        watch.selected_session = Some(PathBuf::from(&selected));
        watch.last_mtime = 0.0;
    }
    drop(watch);
    *state.session_index.lock().unwrap() = sessions.clone();
    SessionsPayload { sessions, selected }
}

fn build_windows(state: &AppState) -> WindowsPayload {
    let windows = list_target_windows();
    let mut watch = state.watch.lock().unwrap();
    let bound = watch.bound_hwnd;
    let selected = if bound != 0 && windows.iter().any(|w| w.hwnd == bound) {
        bound.to_string()
    } else {
        if bound != 0 {
            watch.bound_hwnd = 0;
        }
        "__auto__".to_string()
    };
    drop(watch);
    *state.window_index.lock().unwrap() = windows.clone();
    WindowsPayload { windows, selected }
}

#[tauri::command]
fn get_initial(state: State<AppState>) -> InitialPayload {
    let config = state.config.lock().unwrap().clone();
    let s = build_sessions(&state, true);
    let w = build_windows(&state);
    InitialPayload {
        config,
        sessions: s.sessions,
        selected_session: s.selected,
        windows: w.windows,
        selected_window: w.selected,
    }
}

#[tauri::command]
fn refresh_sessions(app: AppHandle, state: State<AppState>) -> SessionsPayload {
    let payload = build_sessions(&state, false);
    emit_log(&app, &format!("会话列表已刷新，共 {} 个。", payload.sessions.len()), "info");
    payload
}

#[tauri::command]
fn refresh_windows(app: AppHandle, state: State<AppState>) -> WindowsPayload {
    let payload = build_windows(&state);
    if payload.windows.is_empty() {
        emit_log(
            &app,
            "暂未发现 VS Code 或终端窗口。请先打开 Claude Code 或运行 claude CLI，再刷新。",
            "warn",
        );
    }
    payload
}

#[tauri::command]
fn select_session(app: AppHandle, state: State<AppState>, path: String) {
    let sessions = state.session_index.lock().unwrap().clone();
    if let Some(info) = sessions.iter().find(|s| s.path == path) {
        let mut watch = state.watch.lock().unwrap();
        watch.selected_session = Some(PathBuf::from(&path));
        watch.last_mtime = 0.0;
        watch.handled.clear();
        watch.continue_count = 0;
        drop(watch);
        emit_log(
            &app,
            &format!("已选择会话：{} / {}", info.project, info.session_id),
            "info",
        );
    }
}

#[tauri::command]
fn select_window(app: AppHandle, state: State<AppState>, value: String) {
    if value == "__auto__" || value.is_empty() {
        state.watch.lock().unwrap().bound_hwnd = 0;
        emit_log(&app, "发送时将自动查找目标窗口。", "info");
        return;
    }
    if let Ok(hwnd) = value.parse::<isize>() {
        let windows = state.window_index.lock().unwrap().clone();
        if let Some(w) = windows.iter().find(|w| w.hwnd == hwnd) {
            state.watch.lock().unwrap().bound_hwnd = hwnd;
            emit_log(&app, &format!("已绑定 {} 窗口：{}", w.kind_label, w.title), "info");
        }
    }
}

#[tauri::command]
fn open_session_folder(state: State<AppState>) {
    let sel = state.watch.lock().unwrap().selected_session.clone();
    if let Some(path) = sel {
        if let Some(parent) = path.parent() {
            let _ = automation::open_path(parent);
        }
    }
}

#[tauri::command]
fn open_log() {
    let _ = automation::open_path(&log_path());
}

#[tauri::command]
fn save_config(state: State<AppState>, config: Config) {
    let mut cfg = state.config.lock().unwrap();
    *cfg = normalize_config(config);
    save_config_to_disk(&cfg);
}

fn normalize_config(mut c: Config) -> Config {
    c.quiet_seconds = c.quiet_seconds.max(0.5);
    c.cooldown_seconds = c.cooldown_seconds.max(0.5);
    c.stalled_seconds = c.stalled_seconds.max(1.0);
    c.max_continues = c.max_continues.max(1);
    if c.mode.is_empty() {
        c.mode = "smart".into();
    }
    if c.target_kind.is_empty() {
        c.target_kind = "auto".into();
    }
    if c.prompt.trim().is_empty() {
        c.prompt = Config::default().prompt;
    }
    c
}

#[tauri::command]
fn bind_after_countdown(app: AppHandle, _state: State<AppState>) {
    emit_log(&app, "请在 3 秒内切换到目标窗口（VS Code 或终端）……", "warn");
    emit_status(&app, "3秒后读取当前前台窗口", "warn");
    let app2 = app.clone();
    let stateful = app.state::<AppState>().inner() as *const AppState;
    // Safe: AppState lives for the whole app lifetime (managed by Tauri).
    let ptr = stateful as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let state: &AppState = unsafe { &*(ptr as *const AppState) };
        match automation::foreground_target() {
            Some(info) => {
                state.watch.lock().unwrap().bound_hwnd = info.hwnd;
                emit_status(&app2, &format!("已绑定 {} 窗口", info.kind_label), "run");
                emit_log(
                    &app2,
                    &format!("已绑定当前窗口：[{}] {}", info.kind_label, info.title),
                    "ok",
                );
                let payload = build_windows(state);
                let _ = app2.emit("windows", payload);
            }
            None => {
                emit_status(&app2, "绑定失败：当前前台不是受支持的窗口", "err");
                emit_log(&app2, "绑定失败：当前前台不是 VS Code 或受支持的终端。", "err");
                let _ = app2.emit(
                    "toast",
                    serde_json::json!({"type": "warning", "msg": "当前前台不是 VS Code 或受支持的终端"}),
                );
            }
        }
    });
}

#[tauri::command]
fn start_watch(app: AppHandle, state: State<AppState>, config: Config) -> bool {
    {
        let mut cfg = state.config.lock().unwrap();
        *cfg = normalize_config(config);
        save_config_to_disk(&cfg);
    }
    let selected = state.watch.lock().unwrap().selected_session.clone();
    let valid = selected.as_ref().map(|p| p.exists()).unwrap_or(false);
    if !valid {
        return false;
    }
    let cfg = state.config.lock().unwrap().clone();
    {
        let mut watch = state.watch.lock().unwrap();
        watch.watching = true;
        watch.sending = false;
        watch.continue_count = 0;
        watch.handled.clear();
        watch.last_mtime = 0.0;
        // Optionally mark the current state as already handled.
        if !cfg.check_existing {
            if let Some(path) = &watch.selected_session {
                let st = analyze_transcript(path);
                if !st.fingerprint.is_empty() {
                    watch.handled.insert(st.fingerprint);
                }
            }
        }
    }
    let _ = app.emit("watch", serde_json::json!({"on": true}));
    emit_status(&app, "监听中：等待 Claude 会话变化", "run");
    emit_log(
        &app,
        &format!("开始监听：{}", selected.unwrap().to_string_lossy()),
        "ok",
    );
    emit_log(
        &app,
        &format!(
            "客户端={}，模式={}，静默={}秒，最大续跑={}次。",
            cfg.target_kind, cfg.mode, cfg.quiet_seconds, cfg.max_continues
        ),
        "info",
    );
    true
}

#[tauri::command]
fn stop_watch(app: AppHandle, state: State<AppState>, reason: Option<String>) {
    let reason = reason.unwrap_or_else(|| "已手动停止".into());
    state.watch.lock().unwrap().watching = false;
    let _ = app.emit("watch", serde_json::json!({"on": false}));
    emit_status(&app, &reason, "idle");
    emit_log(&app, &reason, "info");
}

#[tauri::command]
fn analyze_now(app: AppHandle, state: State<AppState>) {
    let sel = state.watch.lock().unwrap().selected_session.clone();
    let path = match sel {
        Some(p) if p.exists() => p,
        _ => {
            let _ = app.emit("toast", serde_json::json!({"type": "error", "msg": "没有有效会话"}));
            return;
        }
    };
    let mode = state.config.lock().unwrap().mode.clone();
    let st = analyze_transcript(&path);
    let decision = decide_continue(&st, &mode);
    emit_log(
        &app,
        &format!(
            "分析结果：stop_reason={}，是否续跑={}，原因={}。",
            st.stop_reason.clone().unwrap_or_else(|| "无".into()),
            decision.should_continue,
            decision.reason
        ),
        "info",
    );
    let tail: String = st.assistant_text.chars().rev().take(180).collect::<Vec<_>>().into_iter().rev().collect();
    let preview = tail.replace('\n', " ");
    if !preview.trim().is_empty() {
        emit_log(&app, &format!("末尾摘要：{}", preview), "info");
    }
    emit_status(&app, &format!("分析完成：{}", decision.reason), "info");
}

#[tauri::command]
fn test_send(app: AppHandle, state: State<AppState>) {
    let sel = state.watch.lock().unwrap().selected_session.clone();
    let mut hint = String::new();
    if let Some(p) = &sel {
        if p.exists() {
            hint = analyze_transcript(p).cwd;
        }
    }
    do_send(&app, &state, "手动测试", &hint, true);
}

// --------------------------------------------------------------------------
// Sending
// --------------------------------------------------------------------------
fn do_send(app: &AppHandle, state: &AppState, reason: &str, project_hint: &str, is_test: bool) {
    let cfg = state.config.lock().unwrap().clone();
    let prompt = cfg.prompt.trim().to_string();
    if prompt.is_empty() {
        let _ = app.emit("toast", serde_json::json!({"type": "error", "msg": "续跑提示词不能为空"}));
        return;
    }
    let bound = state.watch.lock().unwrap().bound_hwnd;
    let target = choose_target_window(project_hint, bound, &cfg.target_kind);
    let target = match target {
        Some(t) => t,
        None => {
            let mut watch = state.watch.lock().unwrap();
            if !watch.sending_fingerprint.is_empty() {
                let fp = watch.sending_fingerprint.clone();
                watch.handled.remove(&fp);
                watch.sending_fingerprint.clear();
            }
            drop(watch);
            emit_status(app, "发送失败：未找到目标窗口", "err");
            emit_log(app, "发送失败：未找到 VS Code 或终端窗口。", "err");
            return;
        }
    };
    {
        let mut watch = state.watch.lock().unwrap();
        watch.bound_hwnd = target.hwnd;
        watch.sending = true;
    }
    emit_status(app, &format!("准备发送继续：{}", reason), "run");
    emit_log(
        app,
        &format!("触发续跑：{}；目标=[{}] {}", reason, target.kind_label, target.title),
        "info",
    );

    let app2 = app.clone();
    let ptr = (state as *const AppState) as usize;
    let kind = target.kind.clone();
    let kind_label = target.kind_label.clone();
    let hwnd = target.hwnd;
    std::thread::spawn(move || {
        let result = send_continue_to_claude(hwnd, &prompt, &kind);
        let state: &AppState = unsafe { &*(ptr as *const AppState) };
        let mut watch = state.watch.lock().unwrap();
        watch.sending = false;
        match result {
            Err(e) => {
                if !watch.sending_fingerprint.is_empty() {
                    let fp = watch.sending_fingerprint.clone();
                    watch.handled.remove(&fp);
                    watch.sending_fingerprint.clear();
                }
                drop(watch);
                emit_status(&app2, &format!("发送失败：{}", e), "err");
                emit_log(&app2, &format!("发送失败：{}", e), "err");
            }
            Ok(()) => {
                watch.last_send_at = now_secs();
                watch.sending_fingerprint.clear();
                if !is_test {
                    watch.continue_count += 1;
                }
                let count = watch.continue_count;
                drop(watch);
                emit_status(&app2, &format!("已发送继续；累计自动续跑 {} 次", count), "run");
                if kind_label == "终端" {
                    emit_log(&app2, "已切换到终端窗口并向 Claude CLI 输入续跑提示词。", "ok");
                } else {
                    emit_log(&app2, "已通过命令面板聚焦 Claude Code 输入框并发送续跑提示词。", "ok");
                }
            }
        }
    });
}

// --------------------------------------------------------------------------
// Monitor loop
// --------------------------------------------------------------------------
fn monitor_tick(app: &AppHandle, state: &AppState) {
    let cfg = state.config.lock().unwrap().clone();
    let (watching, selected) = {
        let watch = state.watch.lock().unwrap();
        (watch.watching, watch.selected_session.clone())
    };
    if !watching {
        return;
    }
    let path = match selected {
        Some(p) if p.exists() => p,
        _ => return,
    };

    maybe_follow_latest(app, state, &cfg);

    let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
        Err(_) => return,
    };

    let last_mtime = state.watch.lock().unwrap().last_mtime;
    let st = analyze_transcript(&path);
    if (mtime - last_mtime).abs() > f64::EPSILON {
        state.watch.lock().unwrap().last_mtime = mtime;
        if st.stop_reason.as_deref() == Some("tool_use") {
            emit_status(app, "Claude 正在执行工具/命令", "run");
        } else if st.is_terminal() {
            emit_status(
                app,
                &format!(
                    "检测到回合停止：{}，等待静默期",
                    st.stop_reason.clone().unwrap_or_default()
                ),
                "warn",
            );
        }
    }

    let idle = now_secs() - mtime;
    if idle >= cfg.quiet_seconds {
        evaluate_state(app, state, &cfg, &st);
    }
    if idle >= cfg.stalled_seconds {
        evaluate_interrupted(app, state, &cfg, &st);
    }
}

fn maybe_follow_latest(app: &AppHandle, state: &AppState, cfg: &Config) {
    if !cfg.follow_latest {
        return;
    }
    {
        let mut watch = state.watch.lock().unwrap();
        if now_secs() - watch.last_switch_check < 3.0 {
            return;
        }
        watch.last_switch_check = now_secs();
    }
    let sessions = list_sessions(&projects_root(), 1);
    let latest = match sessions.first() {
        Some(s) => s.clone(),
        None => return,
    };
    let mut watch = state.watch.lock().unwrap();
    let current = watch.selected_session.clone();
    if let Some(cur) = &current {
        let cur_mtime = std::fs::metadata(cur)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if PathBuf::from(&latest.path) != *cur && latest.modified_at > cur_mtime + 1.0 {
            watch.selected_session = Some(PathBuf::from(&latest.path));
            watch.last_mtime = 0.0;
            watch.handled.clear();
            watch.continue_count = 0;
            drop(watch);
            emit_log(
                app,
                &format!("已自动切换到最新会话：{} [{}]", latest.project, &latest.session_id.chars().take(8).collect::<String>()),
                "info",
            );
            let payload = build_sessions(state, false);
            let _ = app.emit("sessions", payload);
        }
    }
}

fn evaluate_state(app: &AppHandle, state: &AppState, cfg: &Config, st: &core::TurnState) {
    {
        let watch = state.watch.lock().unwrap();
        if watch.sending || st.fingerprint.is_empty() || watch.handled.contains(&st.fingerprint) {
            return;
        }
    }
    if !st.is_terminal() {
        return;
    }
    let decision = decide_continue(st, &cfg.mode);
    if !decision.should_continue {
        state.watch.lock().unwrap().handled.insert(st.fingerprint.clone());
        emit_status(app, &format!("本回合无需续跑：{}", decision.reason), "info");
        emit_log(app, &format!("不续跑：{}", decision.reason), "info");
        return;
    }
    {
        let watch = state.watch.lock().unwrap();
        if now_secs() - watch.last_send_at < cfg.cooldown_seconds {
            return;
        }
        if watch.continue_count >= cfg.max_continues {
            drop(watch);
            state.watch.lock().unwrap().handled.insert(st.fingerprint.clone());
            stop_watch_internal(app, state, &format!("已达到最大续跑次数 {}，为防止死循环已停止", cfg.max_continues));
            return;
        }
    }
    {
        let mut watch = state.watch.lock().unwrap();
        watch.handled.insert(st.fingerprint.clone());
        watch.sending_fingerprint = st.fingerprint.clone();
    }
    do_send(app, state, &decision.reason, &st.cwd, false);
}

fn evaluate_interrupted(app: &AppHandle, state: &AppState, cfg: &Config, st: &core::TurnState) {
    {
        let watch = state.watch.lock().unwrap();
        if watch.sending || st.fingerprint.is_empty() || watch.handled.contains(&st.fingerprint) {
            return;
        }
    }
    if st.is_terminal() || st.last_user_uuid.is_empty() {
        return;
    }
    if automation::is_claude_session_process_alive(&st.session_id) {
        return;
    }
    if state.watch.lock().unwrap().continue_count >= cfg.max_continues {
        state.watch.lock().unwrap().handled.insert(st.fingerprint.clone());
        stop_watch_internal(app, state, &format!("已达到最大续跑次数 {}，为防止死循环已停止", cfg.max_continues));
        return;
    }
    {
        let mut watch = state.watch.lock().unwrap();
        watch.handled.insert(st.fingerprint.clone());
        watch.sending_fingerprint = st.fingerprint.clone();
    }
    do_send(app, state, "Claude 会话进程已退出，且回合没有正常结束记录", &st.cwd, false);
}

fn stop_watch_internal(app: &AppHandle, state: &AppState, reason: &str) {
    state.watch.lock().unwrap().watching = false;
    let _ = app.emit("watch", serde_json::json!({"on": false}));
    emit_status(app, reason, "idle");
    emit_log(app, reason, "info");
}

// --------------------------------------------------------------------------
// Entry
// --------------------------------------------------------------------------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let state = AppState {
        config: Mutex::new(load_config()),
        watch: Mutex::new(Watch::default()),
        session_index: Mutex::new(Vec::new()),
        window_index: Mutex::new(Vec::new()),
        stop_flag: stop_flag.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(move |app| {
            let handle = app.handle().clone();
            emit_log(&handle, "程序已启动。默认使用智能模式；明确的 max_tokens 截断一定会自动继续。", "info");
            emit_log(&handle, "已支持 Claude Code（VS Code）与 Claude CLI（终端）两种客户端。", "info");
            let stop = stop_flag.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let state = handle.state::<AppState>();
                    monitor_tick(&handle, state.inner());
                    std::thread::sleep(Duration::from_millis(1000));
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_initial,
            refresh_sessions,
            refresh_windows,
            select_session,
            select_window,
            open_session_folder,
            open_log,
            save_config,
            bind_after_countdown,
            start_watch,
            stop_watch,
            analyze_now,
            test_send
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
