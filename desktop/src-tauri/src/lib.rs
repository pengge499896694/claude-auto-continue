// Claude Auto Continue - Tauri backend.
//
// Ports the previous Python monitor (core.py / windows_automation.py / app.pyw)
// to a native Rust + Tauri application. The webview only renders a lightweight
// hand-written UI; all analysis and window automation happen here.
//
// Multi-session monitoring: the user can pair several Claude sessions each with
// their own target window and watch them simultaneously. Each pair carries its
// own runtime state (continue count, cooldown, handled fingerprints), so they
// never interfere with one another.

mod automation;
mod core;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use automation::{choose_target_window, get_window_info, list_target_windows, send_continue_to_claude, WindowItem};
use core::{analyze_transcript, decide_continue, list_sessions, projects_root, SessionItem, TurnState};

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
    /// When true, `custom_keywords` are used as extra "not done" signals.
    pub custom_keywords_enabled: bool,
    /// User-defined keywords (plain text, case-insensitive). If any appears in
    /// the last reply and no completion marker is present, a continue is sent.
    pub custom_keywords: Vec<String>,
    /// When true, the monitor prints a periodic "heartbeat" log line per pair
    /// describing the window/session state it sees each poll (throttled).
    pub heartbeat_log_enabled: bool,
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
            prompt: "请继续完成刚才未完成的任务。不要只汇报进度，请直接继续执行。\
                如果任务确实已经全部完成、且验证通过，请只回复“[[AUTO_CONTINUE_DONE]]”以结束；\
                否则请继续执行直到完成。"
                .into(),
            check_existing: true,
            follow_latest: false,
            custom_keywords_enabled: false,
            custom_keywords: Vec::new(),
            heartbeat_log_enabled: false,
        }
    }
}

impl Config {
    /// The keyword list actually applied — empty when the feature is off.
    fn effective_keywords(&self) -> Vec<String> {
        if self.custom_keywords_enabled {
            self.custom_keywords.clone()
        } else {
            Vec::new()
        }
    }
}

fn app_dir() -> PathBuf {
    // Windows -> %APPDATA%\ClaudeAutoContinue
    // macOS   -> ~/Library/Application Support/ClaudeAutoContinue
    // Fall back to home, then the current dir, so this never panics.
    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ClaudeAutoContinue")
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
    c.custom_keywords = c
        .custom_keywords
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    c
}

// --------------------------------------------------------------------------
// Monitor state
// --------------------------------------------------------------------------
/// One monitored session paired with the window its continue prompt goes to.
/// Each pair keeps its own runtime state so multiple pairs run independently.
#[derive(Default, Clone)]
struct Pair {
    session_path: PathBuf,
    session_display: String,
    bound_hwnd: isize,     // 0 = auto-find each time
    window_label: String,  // human label for the bound window (may go stale)
    target_kind: String,   // "auto" | "vscode" | "terminal" | "desktop"
    // Per-pair runtime state.
    sending: bool,
    continue_count: u32,
    handled: HashSet<String>,
    last_mtime: f64,
    last_send_at: f64,
    sending_fingerprint: String,
    status: String,         // last status line, surfaced to the UI
    status_kind: String,
    last_heartbeat: f64,    // last time a heartbeat log line was printed
}

impl Pair {
    fn id(&self) -> String {
        self.session_path.to_string_lossy().to_string()
    }
}

#[derive(Default)]
struct Watch {
    watching: bool,
    // The UI's current single selection, used as input when adding a pair or
    // for the one-off "analyze / test send" actions.
    selected_session: Option<PathBuf>,
    bound_hwnd: isize,
    pairs: Vec<Pair>,
}

pub struct AppState {
    config: Mutex<Config>,
    watch: Mutex<Watch>,
    session_index: Mutex<Vec<SessionItem>>,
    window_index: Mutex<Vec<WindowItem>>,
    // Held to keep the monitor thread's shutdown flag alive for the app's
    // lifetime; not read directly since the thread owns its own clone.
    #[allow(dead_code)]
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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let local = secs as i64 + automation::utc_offset_seconds();
    let day = local.rem_euclid(86_400);
    let h = day / 3600;
    let m = (day % 3600) / 60;
    let s = day % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn emit_log(app: &AppHandle, msg: &str, level: &str) {
    let entry = LogEntry {
        time: local_hms(),
        msg: msg.to_string(),
        level: level.to_string(),
    };
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
// Payload types
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

/// A monitored pair as shown in the UI.
#[derive(Serialize, Clone)]
struct PairView {
    id: String,
    session_display: String,
    window_label: String,
    target_kind: String,
    continue_count: u32,
    status: String,
    status_kind: String,
}

#[derive(Serialize, Clone)]
struct PairsPayload {
    pairs: Vec<PairView>,
    watching: bool,
}

#[derive(Serialize)]
struct InitialPayload {
    config: Config,
    sessions: Vec<SessionItem>,
    selected_session: String,
    windows: Vec<WindowItem>,
    selected_window: String,
    pairs: Vec<PairView>,
    watching: bool,
}

fn pair_view(p: &Pair) -> PairView {
    PairView {
        id: p.id(),
        session_display: p.session_display.clone(),
        window_label: if p.bound_hwnd == 0 {
            "自动查找".to_string()
        } else {
            p.window_label.clone()
        },
        target_kind: p.target_kind.clone(),
        continue_count: p.continue_count,
        status: p.status.clone(),
        status_kind: p.status_kind.clone(),
    }
}

fn build_pairs_payload(state: &AppState) -> PairsPayload {
    let watch = state.watch.lock().unwrap();
    PairsPayload {
        pairs: watch.pairs.iter().map(pair_view).collect(),
        watching: watch.watching,
    }
}

fn emit_pairs(app: &AppHandle, state: &AppState) {
    let _ = app.emit("pairs", build_pairs_payload(state));
}

/// Update one pair's status line (by id) and push the refreshed list to the UI.
fn set_pair_status(app: &AppHandle, state: &AppState, id: &str, text: &str, kind: &str) {
    {
        let mut watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == id) {
            p.status = text.to_string();
            p.status_kind = kind.to_string();
        }
    }
    emit_pairs(app, state);
}

// --------------------------------------------------------------------------
// Session / window listing commands
// --------------------------------------------------------------------------
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
    let pp = build_pairs_payload(&state);
    InitialPayload {
        config,
        sessions: s.sessions,
        selected_session: s.selected,
        windows: w.windows,
        selected_window: w.selected,
        pairs: pp.pairs,
        watching: pp.watching,
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
            "暂未发现 VS Code / 终端 / Claude 桌面应用窗口。请先打开对应客户端，再刷新。",
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

// --------------------------------------------------------------------------
// Window binding
// --------------------------------------------------------------------------
#[tauri::command]
fn bind_after_countdown(app: AppHandle, _state: State<AppState>) {
    emit_log(&app, "请在 3 秒内切换到目标窗口（VS Code / 终端 / Claude 桌面应用）……", "warn");
    emit_status(&app, "3秒后读取当前前台窗口", "warn");
    let app2 = app.clone();
    let ptr = app.state::<AppState>().inner() as *const AppState as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        // Safe: AppState lives for the whole app lifetime (managed by Tauri).
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
                emit_log(&app2, "绑定失败：当前前台不是 VS Code / 终端 / Claude 桌面应用。", "err");
                let _ = app2.emit(
                    "toast",
                    serde_json::json!({"type": "warning", "msg": "当前前台不是受支持的窗口"}),
                );
            }
        }
    });
}

// --------------------------------------------------------------------------
// Pair management
// --------------------------------------------------------------------------
/// Add the current session+window selection as a monitored pair.
#[tauri::command]
fn add_pair(app: AppHandle, state: State<AppState>) -> bool {
    let cfg = state.config.lock().unwrap().clone();
    let (session, bound) = {
        let watch = state.watch.lock().unwrap();
        (watch.selected_session.clone(), watch.bound_hwnd)
    };
    let session = match session {
        Some(p) if p.exists() => p,
        _ => {
            let _ = app.emit("toast", serde_json::json!({"type": "error", "msg": "请先选择有效的会话"}));
            return false;
        }
    };
    let id = session.to_string_lossy().to_string();

    // Resolve a friendly display for the session + window.
    let session_display = state
        .session_index
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.path == id)
        .map(|s| s.display.clone())
        .unwrap_or_else(|| id.clone());
    let window_label = if bound != 0 {
        get_window_info(bound)
            .map(|w| format!("[{}] {}", w.kind_label, w.title))
            .unwrap_or_else(|| "自动查找".to_string())
    } else {
        "自动查找".to_string()
    };

    let mut watch = state.watch.lock().unwrap();
    if watch.pairs.iter().any(|p| p.id() == id) {
        drop(watch);
        let _ = app.emit("toast", serde_json::json!({"type": "warning", "msg": "该会话已在监听列表中"}));
        return false;
    }
    let mut pair = Pair {
        session_path: session.clone(),
        session_display,
        bound_hwnd: bound,
        window_label,
        target_kind: cfg.target_kind.clone(),
        status: "待监听".into(),
        status_kind: "idle".into(),
        ..Default::default()
    };
    // If we should skip the currently-stopped state, pre-mark it handled.
    if !cfg.check_existing {
        let st = analyze_transcript(&session);
        if !st.fingerprint.is_empty() {
            pair.handled.insert(st.fingerprint);
        }
    }
    watch.pairs.push(pair);
    drop(watch);
    emit_log(&app, &format!("已加入监听配对：{}", id), "ok");
    emit_pairs(&app, &state);
    true
}

#[tauri::command]
fn remove_pair(app: AppHandle, state: State<AppState>, id: String) {
    {
        let mut watch = state.watch.lock().unwrap();
        watch.pairs.retain(|p| p.id() != id);
    }
    emit_log(&app, &format!("已移除监听配对：{}", id), "info");
    emit_pairs(&app, &state);
}

#[tauri::command]
fn list_pairs(state: State<AppState>) -> PairsPayload {
    build_pairs_payload(&state)
}

// --------------------------------------------------------------------------
// Start / stop
// --------------------------------------------------------------------------
#[tauri::command]
fn start_watch(app: AppHandle, state: State<AppState>, config: Config) -> bool {
    {
        let mut cfg = state.config.lock().unwrap();
        *cfg = normalize_config(config);
        save_config_to_disk(&cfg);
    }
    let cfg = state.config.lock().unwrap().clone();

    // If no pairs were explicitly added, fall back to a single implicit pair
    // built from the current selection — preserves the simple one-session flow.
    {
        let mut watch = state.watch.lock().unwrap();
        if watch.pairs.is_empty() {
            let session = watch.selected_session.clone();
            let bound = watch.bound_hwnd;
            match session {
                Some(p) if p.exists() => {
                    let id = p.to_string_lossy().to_string();
                    let session_display = state
                        .session_index
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|s| s.path == id)
                        .map(|s| s.display.clone())
                        .unwrap_or_else(|| id.clone());
                    let window_label = if bound != 0 {
                        get_window_info(bound)
                            .map(|w| format!("[{}] {}", w.kind_label, w.title))
                            .unwrap_or_else(|| "自动查找".to_string())
                    } else {
                        "自动查找".to_string()
                    };
                    let mut pair = Pair {
                        session_path: p.clone(),
                        session_display,
                        bound_hwnd: bound,
                        window_label,
                        target_kind: cfg.target_kind.clone(),
                        status: "待监听".into(),
                        status_kind: "idle".into(),
                        ..Default::default()
                    };
                    if !cfg.check_existing {
                        let st = analyze_transcript(&p);
                        if !st.fingerprint.is_empty() {
                            pair.handled.insert(st.fingerprint);
                        }
                    }
                    watch.pairs.push(pair);
                }
                _ => {
                    drop(watch);
                    return false;
                }
            }
        }
        watch.watching = true;
    }

    let _ = app.emit("watch", serde_json::json!({"on": true}));
    let count = state.watch.lock().unwrap().pairs.len();
    emit_status(&app, &format!("监听中：{} 个会话配对", count), "run");
    emit_log(&app, &format!("开始监听，共 {} 个配对。", count), "ok");
    emit_log(
        &app,
        &format!(
            "模式={}，静默={}秒，冷却={}秒，最大续跑={}次{}。",
            cfg.mode,
            cfg.quiet_seconds,
            cfg.cooldown_seconds,
            cfg.max_continues,
            if cfg.custom_keywords_enabled && !cfg.custom_keywords.is_empty() {
                format!("，自定义关键字 {} 个", cfg.custom_keywords.len())
            } else {
                String::new()
            }
        ),
        "info",
    );
    emit_pairs(&app, &state);
    true
}

#[tauri::command]
fn stop_watch(app: AppHandle, state: State<AppState>, reason: Option<String>) {
    let reason = reason.unwrap_or_else(|| "已手动停止".into());
    {
        let mut watch = state.watch.lock().unwrap();
        watch.watching = false;
        for p in watch.pairs.iter_mut() {
            p.status = "已停止".into();
            p.status_kind = "idle".into();
        }
    }
    let _ = app.emit("watch", serde_json::json!({"on": false}));
    emit_status(&app, &reason, "idle");
    emit_log(&app, &reason, "info");
    emit_pairs(&app, &state);
}

// --------------------------------------------------------------------------
// One-off analyze / test (operate on the single UI selection)
// --------------------------------------------------------------------------
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
    let cfg = state.config.lock().unwrap().clone();
    let st = analyze_transcript(&path);
    let decision = decide_continue(&st, &cfg.mode, &cfg.effective_keywords());
    if st.is_api_error() {
        emit_log(&app, &format!("检测到 API 错误：{}", st.last_error), "warn");
    }
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
    let (sel, bound) = {
        let watch = state.watch.lock().unwrap();
        (watch.selected_session.clone(), watch.bound_hwnd)
    };
    let cfg = state.config.lock().unwrap().clone();
    let mut hint = String::new();
    if let Some(p) = &sel {
        if p.exists() {
            hint = analyze_transcript(p).cwd;
        }
    }
    // Fire-and-forget test send to the current selection's target.
    let prompt = cfg.prompt.trim().to_string();
    if prompt.is_empty() {
        let _ = app.emit("toast", serde_json::json!({"type": "error", "msg": "续跑提示词不能为空"}));
        return;
    }
    let target = match choose_target_window(&hint, bound, &cfg.target_kind) {
        Some(t) => t,
        None => {
            emit_status(&app, "发送失败：未找到目标窗口", "err");
            emit_log(&app, "发送失败：未找到 VS Code / 终端 / Claude 桌面应用窗口。", "err");
            return;
        }
    };
    emit_log(&app, &format!("测试发送；目标=[{}] {}", target.kind_label, target.title), "info");
    let app2 = app.clone();
    let (hwnd, kind, kind_label) = (target.hwnd, target.kind.clone(), target.kind_label.clone());
    std::thread::spawn(move || match send_continue_to_claude(hwnd, &prompt, &kind) {
        Ok(()) => {
            emit_status(&app2, "测试发送完成", "run");
            emit_log(&app2, &format!("测试发送成功（{}）。", kind_label), "ok");
        }
        Err(e) => {
            emit_status(&app2, &format!("测试发送失败：{}", e), "err");
            emit_log(&app2, &format!("测试发送失败：{}", e), "err");
        }
    });
}

// --------------------------------------------------------------------------
// Sending for a monitored pair
// --------------------------------------------------------------------------
/// Deliver a continue prompt for one pair. Runs the actual keystrokes on a
/// background thread so the monitor loop is never blocked.
fn do_send_for_pair(app: &AppHandle, state: &AppState, pair_id: &str, reason: &str, project_hint: &str) {
    let cfg = state.config.lock().unwrap().clone();
    let prompt = cfg.prompt.trim().to_string();
    if prompt.is_empty() {
        return;
    }

    // Resolve target from the pair's own binding + kind.
    let (bound, target_kind) = {
        let watch = state.watch.lock().unwrap();
        match watch.pairs.iter().find(|p| p.id() == pair_id) {
            Some(p) => (p.bound_hwnd, p.target_kind.clone()),
            None => return,
        }
    };
    let kind = if target_kind.is_empty() { cfg.target_kind.clone() } else { target_kind };
    let target = choose_target_window(project_hint, bound, &kind);
    let target = match target {
        Some(t) => t,
        None => {
            // Release the fingerprint so we retry next tick, and report it.
            let mut watch = state.watch.lock().unwrap();
            if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == pair_id) {
                if !p.sending_fingerprint.is_empty() {
                    let fp = p.sending_fingerprint.clone();
                    p.handled.remove(&fp);
                    p.sending_fingerprint.clear();
                }
                p.sending = false;
                p.status = "发送失败：未找到目标窗口".into();
                p.status_kind = "err".into();
            }
            drop(watch);
            emit_log(app, &format!("[{}] 发送失败：未找到目标窗口。", short_id(pair_id)), "err");
            emit_pairs(app, state);
            return;
        }
    };

    {
        let mut watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == pair_id) {
            p.bound_hwnd = target.hwnd;
            p.window_label = format!("[{}] {}", target.kind_label, target.title);
            p.sending = true;
            p.status = format!("准备发送：{}", reason);
            p.status_kind = "run".into();
        }
    }
    emit_log(
        app,
        &format!("[{}] 触发续跑：{}；目标=[{}] {}", short_id(pair_id), reason, target.kind_label, target.title),
        "info",
    );
    emit_pairs(app, state);

    let app2 = app.clone();
    let ptr = (state as *const AppState) as usize;
    let (hwnd, tkind, kind_label) = (target.hwnd, target.kind.clone(), target.kind_label.clone());
    let pid = pair_id.to_string();
    // Session file mtime just before sending; used to verify the prompt really
    // landed (Claude writes a new message → the file changes shortly after).
    let session_path = PathBuf::from(&pid);
    let mtime_before = file_mtime(&session_path);
    std::thread::spawn(move || {
        let result = send_continue_to_claude(hwnd, &prompt, &tkind);
        // Safe: AppState lives for the whole app lifetime (managed by Tauri).
        let state: &AppState = unsafe { &*(ptr as *const AppState) };

        // On a successful key-injection, confirm delivery by watching the
        // session file for a change over the next few seconds. A change means
        // Claude actually received the prompt and started a new turn.
        let delivered = if result.is_ok() {
            wait_for_session_change(&session_path, mtime_before, 6.0)
        } else {
            false
        };

        let mut watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == pid) {
            p.sending = false;
            match &result {
                Err(e) => {
                    if !p.sending_fingerprint.is_empty() {
                        let fp = p.sending_fingerprint.clone();
                        p.handled.remove(&fp);
                        p.sending_fingerprint.clear();
                    }
                    p.status = format!("发送失败：{}", e);
                    p.status_kind = "err".into();
                }
                Ok(()) => {
                    p.last_send_at = now_secs();
                    p.sending_fingerprint.clear();
                    if delivered {
                        p.continue_count += 1;
                        p.status = format!("已确认送达（第 {} 次续跑）", p.continue_count);
                        p.status_kind = "run".into();
                    } else {
                        // The keys were injected but the session did not change.
                        // Release the fingerprint so the next tick can retry, and
                        // do NOT count it as a real continue.
                        let fp = p.sending_fingerprint.clone();
                        if !fp.is_empty() {
                            p.handled.remove(&fp);
                        }
                        p.status = "已执行发送动作，但未检测到会话更新".into();
                        p.status_kind = "warn".into();
                    }
                }
            }
        }
        drop(watch);
        match result {
            Err(e) => emit_log(&app2, &format!("[{}] 发送失败：{}", short_id(&pid), e), "err"),
            Ok(()) => {
                if delivered {
                    let how = if kind_label == "终端" {
                        "已切换到终端并输入续跑提示词，已确认会话更新"
                    } else if kind_label == "Claude 桌面应用" {
                        "已切换到 Claude 桌面应用并输入续跑提示词，已确认会话更新"
                    } else {
                        "已通过命令面板聚焦输入框并发送续跑提示词，已确认会话更新"
                    };
                    emit_log(&app2, &format!("[{}] {}。", short_id(&pid), how), "ok");
                } else {
                    emit_log(
                        &app2,
                        &format!(
                            "[{}] 已执行发送动作，但 6 秒内未检测到会话更新——提示词可能未真正进入输入框（请检查目标窗口/命令名是否正确）。稍后会重试。",
                            short_id(&pid)
                        ),
                        "warn",
                    );
                }
            }
        }
        emit_pairs(&app2, state);
    });
}

/// The session file's mtime as a float epoch, or 0.0 if unavailable.
fn file_mtime(path: &std::path::Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Poll the session file for up to `timeout` seconds, returning true as soon as
/// its mtime advances past `baseline` — evidence the prompt actually landed.
fn wait_for_session_change(path: &std::path::Path, baseline: f64, timeout: f64) -> bool {
    let deadline = now_secs() + timeout;
    while now_secs() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        if file_mtime(path) > baseline + 0.001 {
            return true;
        }
    }
    false
}

fn short_id(id: &str) -> String {
    // Show the session file stem's first 8 chars for compact log lines.
    std::path::Path::new(id)
        .file_stem()
        .map(|s| s.to_string_lossy().chars().take(8).collect())
        .unwrap_or_else(|| id.chars().take(8).collect())
}

// --------------------------------------------------------------------------
// Monitor loop
// --------------------------------------------------------------------------
fn monitor_tick(app: &AppHandle, state: &AppState) {
    let cfg = state.config.lock().unwrap().clone();
    let watching = state.watch.lock().unwrap().watching;
    if !watching {
        return;
    }

    // Snapshot the pair ids so we don't hold the lock while doing file IO.
    let pair_ids: Vec<String> = state
        .watch
        .lock()
        .unwrap()
        .pairs
        .iter()
        .map(|p| p.id())
        .collect();

    for id in pair_ids {
        tick_pair(app, state, &cfg, &id);
    }
}

fn tick_pair(app: &AppHandle, state: &AppState, cfg: &Config, id: &str) {
    let path = PathBuf::from(id);
    if !path.exists() {
        return;
    }

    let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
        Err(_) => return,
    };

    let last_mtime = {
        let watch = state.watch.lock().unwrap();
        match watch.pairs.iter().find(|p| p.id() == id) {
            Some(p) => p.last_mtime,
            None => return, // removed while iterating
        }
    };

    let st = analyze_transcript(&path);
    if (mtime - last_mtime).abs() > f64::EPSILON {
        {
            let mut watch = state.watch.lock().unwrap();
            if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == id) {
                p.last_mtime = mtime;
            }
        }
        let (text, kind) = if st.is_api_error() {
            (format!("检测到 API 错误：{}，等待静默期", st.last_error), "warn")
        } else if st.stop_reason.as_deref() == Some("tool_use") {
            ("正在执行工具/命令".to_string(), "run")
        } else if st.is_terminal() {
            (format!("回合停止：{}，等待静默期", st.stop_reason.clone().unwrap_or_default()), "warn")
        } else {
            ("监听中".to_string(), "run")
        };
        set_pair_status(app, state, id, &text, kind);
    }

    let idle = now_secs() - mtime;

    // Optional heartbeat: a periodic log line describing what the monitor sees
    // for this pair (session state + whether its target window is found).
    // Throttled to ~once every 3s per pair so it never floods the log.
    if cfg.heartbeat_log_enabled {
        let should_log = {
            let mut watch = state.watch.lock().unwrap();
            match watch.pairs.iter_mut().find(|p| p.id() == id) {
                Some(p) if now_secs() - p.last_heartbeat >= 3.0 => {
                    p.last_heartbeat = now_secs();
                    true
                }
                _ => false,
            }
        };
        if should_log {
            let (bound, target_kind) = {
                let watch = state.watch.lock().unwrap();
                match watch.pairs.iter().find(|p| p.id() == id) {
                    Some(p) => (p.bound_hwnd, p.target_kind.clone()),
                    None => return,
                }
            };
            let kind = if target_kind.is_empty() { cfg.target_kind.clone() } else { target_kind };
            let window_desc = match choose_target_window(&st.cwd, bound, &kind) {
                Some(w) => format!("目标窗口=[{}] {}", w.kind_label, w.title),
                None => "目标窗口=未找到".to_string(),
            };
            let session_state = if st.is_api_error() {
                format!("API错误({})", st.last_error)
            } else if st.stop_reason.as_deref() == Some("tool_use") {
                "执行工具中".to_string()
            } else if st.is_terminal() {
                format!("已停止({})", st.stop_reason.clone().unwrap_or_default())
            } else if st.stop_reason.is_none() {
                "生成中/无停止标记".to_string()
            } else {
                format!("stop_reason={}", st.stop_reason.clone().unwrap_or_default())
            };
            emit_log(
                app,
                &format!(
                    "[心跳][{}] 会话状态={}；静默={:.0}秒；{}",
                    short_id(id),
                    session_state,
                    idle,
                    window_desc
                ),
                "info",
            );
        }
    }

    if idle >= cfg.quiet_seconds {
        evaluate_pair(app, state, cfg, id, &st);
    }
    if idle >= cfg.stalled_seconds {
        evaluate_pair_interrupted(app, state, cfg, id, &st);
    }
}

/// Shared pre-checks for a pair before deciding to continue. Returns false if
/// the pair should be skipped this tick.
fn pair_ready(state: &AppState, id: &str, fingerprint: &str) -> bool {
    let watch = state.watch.lock().unwrap();
    match watch.pairs.iter().find(|p| p.id() == id) {
        Some(p) => !p.sending && !fingerprint.is_empty() && !p.handled.contains(fingerprint),
        None => false,
    }
}

fn evaluate_pair(app: &AppHandle, state: &AppState, cfg: &Config, id: &str, st: &TurnState) {
    if !pair_ready(state, id, &st.fingerprint) {
        return;
    }
    // Normal stop or unrecovered API error only.
    if !st.is_terminal() && !st.is_api_error() {
        return;
    }
    let decision = decide_continue(st, &cfg.mode, &cfg.effective_keywords());
    if !decision.should_continue {
        mark_handled(state, id, &st.fingerprint);
        set_pair_status(app, state, id, &format!("无需续跑：{}", decision.reason), "info");
        emit_log(app, &format!("[{}] 不续跑：{}", short_id(id), decision.reason), "info");
        return;
    }

    // Cooldown + cap, per pair.
    {
        let watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter().find(|p| p.id() == id) {
            if now_secs() - p.last_send_at < cfg.cooldown_seconds {
                return;
            }
            if p.continue_count >= cfg.max_continues {
                drop(watch);
                mark_handled(state, id, &st.fingerprint);
                set_pair_status(app, state, id, &format!("已达最大续跑次数 {}，已停止该配对", cfg.max_continues), "warn");
                emit_log(app, &format!("[{}] 已达最大续跑次数 {}，停止该配对。", short_id(id), cfg.max_continues), "warn");
                return;
            }
        } else {
            return;
        }
    }

    // Reserve the fingerprint, then send.
    {
        let mut watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == id) {
            p.handled.insert(st.fingerprint.clone());
            p.sending_fingerprint = st.fingerprint.clone();
        }
    }
    do_send_for_pair(app, state, id, &decision.reason, &st.cwd);
}

fn evaluate_pair_interrupted(app: &AppHandle, state: &AppState, cfg: &Config, id: &str, st: &TurnState) {
    if !pair_ready(state, id, &st.fingerprint) {
        return;
    }
    // This runs only after `stalled_seconds` of total silence. Terminal stops
    // and API errors are already handled earlier by evaluate_pair; here we catch
    // the silent case: a turn that produced output but never reached any stop
    // reason (the stream was cut mid-answer). After this long a silence it is
    // genuinely stuck, whether or not the `claude` process is still alive — so
    // we no longer require the process to have exited.
    if !st.is_broken_stream() {
        return;
    }
    let process_gone = !automation::is_claude_session_process_alive(&st.session_id);
    let reason = if process_gone {
        "Claude 进程已退出且回合无正常结束记录（疑似意外中断），自动续跑"
    } else {
        "回合长时间无输出且无正常结束记录（疑似断流/意外中断），自动续跑"
    };
    {
        let watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter().find(|p| p.id() == id) {
            if p.continue_count >= cfg.max_continues {
                drop(watch);
                mark_handled(state, id, &st.fingerprint);
                set_pair_status(app, state, id, &format!("已达最大续跑次数 {}，已停止该配对", cfg.max_continues), "warn");
                return;
            }
        } else {
            return;
        }
    }
    {
        let mut watch = state.watch.lock().unwrap();
        if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == id) {
            p.handled.insert(st.fingerprint.clone());
            p.sending_fingerprint = st.fingerprint.clone();
        }
    }
    do_send_for_pair(app, state, id, reason, &st.cwd);
}

fn mark_handled(state: &AppState, id: &str, fingerprint: &str) {
    let mut watch = state.watch.lock().unwrap();
    if let Some(p) = watch.pairs.iter_mut().find(|p| p.id() == id) {
        p.handled.insert(fingerprint.to_string());
    }
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
            emit_log(&handle, "程序已启动。默认使用智能模式；max_tokens 截断与未恢复的 API 错误都会自动续跑。", "info");
            emit_log(&handle, "支持 Claude Code（VS Code）、Claude CLI（终端）、Claude Desktop（桌面应用）三种客户端，并可多会话同时监听。", "info");
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
            add_pair,
            remove_pair,
            list_pairs,
            start_watch,
            stop_watch,
            analyze_now,
            test_send
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
