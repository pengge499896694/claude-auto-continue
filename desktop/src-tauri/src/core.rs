// Session-transcript analysis, ported from the tested Python `core.py`.
// Both the Claude Code VS Code extension and the `claude` CLI write the same
// ~/.claude/projects/<project>/<session>.jsonl files, so this logic is shared.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn projects_root() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

pub fn sessions_root() -> PathBuf {
    home_dir().join(".claude").join("sessions")
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

// Signals that the turn stopped *mid-task*. Kept deliberately strong: pure
// narration words like "接下来 / 下一步 / next steps" were removed because they
// appear constantly in normal, finished replies ("任务完成。接下来你可以…") and
// caused false continues. These are matched against the tail of the reply only
// (see `tail_for_signals`) so a mention early in a long, otherwise-finished
// answer does not trigger a continue.
const UNFINISHED_PATTERNS: &[&str] = &[
    "尚未完成",
    "还没完成",
    "还未完成",
    "仍未完成",
    "需要继续",
    "继续完成",
    "未能完成",
    "待完成",
    "我(?:会|将)继续",
    "让我继续",
    "现在继续",
    "剩余(?:的)?(?:工作|任务|步骤|部分)尚",
    "TODO",
    "not (?:yet )?(?:finished|complete)",
    "still need(?:s)? to",
    "need(?:s)? to continue",
    "I(?:'|’)ll continue",
    "I will continue",
    "let me continue",
    "continuing",
];

const COMPLETION_PATTERNS: &[&str] = &[
    "任务已完成",
    "全部完成",
    "已全部完成",
    "实现完成",
    "修复完成",
    "开发完成",
    "所有(?:任务|工作|修改).{0,8}完成",
    "测试(?:已)?通过",
    "验证(?:已)?通过",
    "task is complete",
    "task has been completed",
    "completed successfully",
    "fully implemented",
    "all (?:done|complete)",
    "implementation is complete",
    r"\[\[AUTO_CONTINUE_DONE\]\]",
];

static UNFINISHED_RE: Lazy<Vec<Regex>> = Lazy::new(|| compile(UNFINISHED_PATTERNS));
static COMPLETION_RE: Lazy<Vec<Regex>> = Lazy::new(|| compile(COMPLETION_PATTERNS));

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|p| Regex::new(&format!("(?is){}", p)).ok())
        .collect()
}

fn matches_any(text: &str, res: &[Regex]) -> bool {
    res.iter().any(|re| re.is_match(text))
}

/// The one explicit "task truly finished" marker. Confirm-completion mode stops
/// ONLY on this, never on the fuzzy completion phrases, so a stray "测试通过" in
/// the middle of ongoing work can't end the loop prematurely.
const DONE_MARKER: &str = "[[AUTO_CONTINUE_DONE]]";

fn has_done_marker(text: &str) -> bool {
    text.contains(DONE_MARKER)
}

/// The trailing portion of a reply, used for "unfinished" detection so that a
/// mid-reply narrative phrase (e.g. "接下来我来实现…") doesn't count once the
/// turn actually ends with a completion statement. Returns roughly the last
/// `chars` characters, snapped to a char boundary.
fn tail_text(text: &str, chars: usize) -> String {
    let all: Vec<char> = text.chars().collect();
    if all.len() <= chars {
        return text.to_string();
    }
    all[all.len() - chars..].iter().collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionItem {
    pub path: String,
    pub project: String,
    pub session_id: String,
    pub modified_at: f64,
    pub display: String,
}

#[derive(Debug, Clone, Default)]
pub struct TurnState {
    #[allow(dead_code)]
    pub path: String,
    pub session_id: String,
    pub cwd: String,
    pub last_user_uuid: String,
    pub last_user_text: String,
    pub last_assistant_uuid: String,
    pub last_assistant_timestamp: String,
    pub stop_reason: Option<String>,
    pub assistant_text: String,
    pub has_unclosed_code_fence: bool,
    pub fingerprint: String,
    /// The client that produced this session, taken from the transcript's
    /// `entrypoint` field: "claude-vscode" | "claude-desktop" | "claude-cli" | ...
    /// Empty when unknown. Used to auto-pick the delivery path.
    pub entrypoint: String,
    /// Set when the *last* event in the transcript is an unrecovered API error
    /// (e.g. 502/524/429/connection error). Carries a human-readable summary
    /// like "502 Upstream service temporarily unavailable". Empty otherwise.
    ///
    /// Only the trailing error matters: if Claude Code retried and recovered,
    /// later events overwrite this back to empty, so a non-empty value that
    /// survives the quiet period means the turn is genuinely stuck.
    pub last_error: String,
}

impl TurnState {
    /// A normally-stopped turn (Claude finished a response and is waiting).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.stop_reason.as_deref(),
            Some("end_turn") | Some("max_tokens") | Some("stop_sequence")
        )
    }

    /// The turn ended on an unrecovered API error rather than a normal stop.
    pub fn is_api_error(&self) -> bool {
        !self.last_error.is_empty()
    }

    /// Claude is mid-flight: it is running a tool / still streaming, so the
    /// absence of a stop reason is expected and must not count as a break.
    pub fn is_working(&self) -> bool {
        self.stop_reason.as_deref() == Some("tool_use")
    }

    /// A silently broken stream: the turn started (there is a user prompt and
    /// some assistant output) but it never reached *any* terminal stop reason
    /// and no API error was recorded — the connection just died mid-answer.
    ///
    /// This is only meaningful once the transcript has been quiet for a while;
    /// the caller enforces that, since an in-progress stream looks identical.
    pub fn is_broken_stream(&self) -> bool {
        !self.last_user_uuid.is_empty()
            && !self.is_terminal()
            && !self.is_api_error()
            && !self.is_working()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContinueDecision {
    pub should_continue: bool,
    pub reason: String,
}

fn project_display_name(encoded: &str) -> String {
    // Claude replaces path separators with dashes; make common drive prefixes readable.
    let bytes = encoded.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'-' && bytes[2] == b'-' {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let rest = encoded[3..].replace("--", "\\");
        return format!("{}:\\{}", drive, rest);
    }
    encoded.to_string()
}

pub fn list_sessions(projects_root: &Path, limit: usize) -> Vec<SessionItem> {
    let mut sessions: Vec<SessionItem> = Vec::new();
    let entries = match fs::read_dir(projects_root) {
        Ok(e) => e,
        Err(_) => return sessions,
    };
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        let project = project_display_name(&entry.file_name().to_string_lossy());
        if let Ok(files) = fs::read_dir(&project_dir) {
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let modified_at = path
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let session_id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let display = format_session(&project, &session_id, modified_at);
                sessions.push(SessionItem {
                    path: path.to_string_lossy().to_string(),
                    project: project.clone(),
                    session_id,
                    modified_at,
                    display,
                });
            }
        }
    }
    sessions.sort_by(|a, b| {
        b.modified_at
            .partial_cmp(&a.modified_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sessions.truncate(limit);
    sessions
}

fn format_session(project: &str, session_id: &str, modified_at: f64) -> String {
    let short_id: String = session_id.chars().take(8).collect();
    let stamp = format_time(modified_at);
    format!("{}  {}  [{}]", stamp, project, short_id)
}

fn format_time(epoch: f64) -> String {
    let secs = epoch.max(0.0) as i64 + crate::automation::utc_offset_seconds();
    let secs = secs.max(0);
    let days = secs / 86400;
    let tod = secs % 86400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (_, month, day) = civil_from_days(days);
    format!("{:02}-{:02} {:02}:{:02}:{:02}", month, day, h, m, s)
}

// Howard Hinnant's civil-from-days algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn read_tail_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size > max_bytes {
        let _ = file.seek(SeekFrom::Start(size - max_bytes));
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if size > max_bytes && !lines.is_empty() {
        lines.remove(0); // drop a possibly partial first line
    }
    lines
}

fn content_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut chunks = Vec::new();
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    chunks.push(t.to_string());
                }
            }
        }
        return chunks.join("\n");
    }
    String::new()
}

fn is_human_user_message(obj: &serde_json::Value) -> bool {
    if obj.get("type").and_then(|t| t.as_str()) != Some("user") {
        return false;
    }
    let message = match obj.get("message") {
        Some(m) if m.is_object() => m,
        _ => return false,
    };
    if message.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let content = match message.get("content") {
        Some(c) => c,
        None => return false,
    };
    if let Some(s) = content.as_str() {
        return !s.trim().is_empty();
    }
    if let Some(arr) = content.as_array() {
        return arr.iter().any(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("text")
                && item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false)
        });
    }
    false
}

pub fn analyze_transcript(path: &Path) -> TurnState {
    let mut state = TurnState {
        path: path.to_string_lossy().to_string(),
        ..Default::default()
    };

    let mut objects: Vec<serde_json::Value> = Vec::new();
    for line in read_tail_lines(path, 4 * 1024 * 1024) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.is_object() {
                objects.push(v);
            }
        }
    }

    let mut last_user_index: i64 = -1;
    for (index, obj) in objects.iter().enumerate() {
        if is_human_user_message(obj) {
            last_user_index = index as i64;
            let message = &obj["message"];
            state.last_user_uuid = obj
                .get("uuid")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("promptId").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            state.last_user_text = content_text(&message["content"]);
            if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
                state.session_id = sid.to_string();
            }
            if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                state.cwd = cwd.to_string();
            }
        }
    }

    // The client that wrote this session (same for every row); grab it once.
    for obj in &objects {
        if let Some(ep) = obj.get("entrypoint").and_then(|v| v.as_str()) {
            if !ep.is_empty() {
                state.entrypoint = ep.to_string();
                break;
            }
        }
    }

    let start = if last_user_index >= 0 {
        (last_user_index + 1) as usize
    } else {
        0
    };
    let mut assistant_text: Vec<String> = Vec::new();
    for obj in &objects[start..] {
        // An unrecovered API error is a trailing `system/api_error` row. We set
        // it here and clear it as soon as a later assistant message appears, so
        // only an error that Claude never recovered from survives to the end.
        if is_api_error_event(obj) {
            state.last_error = api_error_summary(obj);
            continue;
        }

        let message = match obj.get("message") {
            Some(m) if m.is_object() => m,
            _ => continue,
        };
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        // A real assistant message means the request went through — clear any
        // earlier transient error so it does not falsely trigger a continue.
        state.last_error.clear();
        if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
            state.session_id = sid.to_string();
        }
        if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
            state.cwd = cwd.to_string();
        }
        let text = content_text(&message["content"]);
        if !text.trim().is_empty() {
            assistant_text.push(text);
        }
        if let Some(stop) = message.get("stop_reason") {
            if !stop.is_null() {
                state.stop_reason = stop.as_str().map(|s| s.to_string());
                state.last_assistant_uuid = obj
                    .get("uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                state.last_assistant_timestamp = obj
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
            }
        }
    }

    state.assistant_text = assistant_text.join("\n").trim().to_string();
    state.has_unclosed_code_fence = state.assistant_text.matches("```").count() % 2 == 1;
    state.fingerprint = format!(
        "{}|{}|{}|{}|{}|{}",
        state.session_id,
        state.last_user_uuid,
        state.last_assistant_uuid,
        state.last_assistant_timestamp,
        state.stop_reason.clone().unwrap_or_default(),
        state.last_error,
    );
    state
}

/// A `system` row Claude writes when an API request fails (before retrying).
fn is_api_error_event(obj: &serde_json::Value) -> bool {
    obj.get("type").and_then(|v| v.as_str()) == Some("system")
        && obj.get("subtype").and_then(|v| v.as_str()) == Some("api_error")
}

/// A short human-readable summary of an api_error row, e.g.
/// "502 Upstream service temporarily unavailable" or "429 ...".
fn api_error_summary(obj: &serde_json::Value) -> String {
    let error = obj.get("error");
    // Prefer the pre-formatted one-liner Claude provides.
    if let Some(f) = error
        .and_then(|e| e.get("formatted"))
        .and_then(|v| v.as_str())
    {
        if !f.trim().is_empty() {
            return truncate_summary(f);
        }
    }
    // Fall back to status + message.
    let status = error
        .and_then(|e| e.get("status"))
        .and_then(|v| v.as_u64())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("API 错误");
    let combined = if status.is_empty() {
        message.to_string()
    } else {
        format!("{} {}", status, message)
    };
    truncate_summary(&combined)
}

fn truncate_summary(s: &str) -> String {
    let one_line = s.split(['\n', '\r']).next().unwrap_or(s).trim();
    let clipped: String = one_line.chars().take(120).collect();
    clipped
}

/// Case-insensitive substring match of any user-defined keyword against the text.
/// Keywords are plain text (not regex), so non-technical users can list phrases
/// like "已暂停" or "waiting for" without worrying about escaping.
pub fn custom_keyword_hit(text: &str, keywords: &[String]) -> Option<String> {
    let lowered = text.to_lowercase();
    for kw in keywords {
        let k = kw.trim();
        if k.is_empty() {
            continue;
        }
        if lowered.contains(&k.to_lowercase()) {
            return Some(k.to_string());
        }
    }
    None
}

/// True when the reply clearly claims the whole task is done. Used by
/// "confirm completion" mode to decide when to STOP asking.
pub fn looks_complete(state: &TurnState) -> bool {
    let text = state.assistant_text.trim();
    let tail = tail_text(text, 160);
    matches_any(text, &COMPLETION_RE) && !matches_any(&tail, &UNFINISHED_RE)
}

/// Decide whether a stopped/failed turn should receive a continue prompt.
///
/// `custom_keywords` are extra user-defined "not done" signals; pass an empty
/// slice when the feature is disabled.
///
/// `confirm_completion`: when true, ANY normally-stopped turn (in every mode)
/// keeps getting a continue prompt until the reply clearly says it is done
/// (a completion marker/phrase with no trailing "unfinished" signal). This is
/// how the user gets "ask until it confirms it's finished" behavior.
pub fn decide_continue(
    state: &TurnState,
    mode: &str,
    custom_keywords: &[String],
    confirm_completion: bool,
) -> ContinueDecision {
    // An unrecovered API error (502/524/429/connection error/overloaded ...) is
    // an abnormal termination the user explicitly wants retried, in every mode.
    if state.is_api_error() {
        return ContinueDecision {
            should_continue: true,
            reason: format!("检测到未恢复的 API 错误：{}", state.last_error),
        };
    }

    if state.stop_reason.as_deref() == Some("max_tokens") {
        return ContinueDecision {
            should_continue: true,
            reason: "检测到 stop_reason=max_tokens（输出被长度限制截断）".into(),
        };
    }

    // A silently broken stream (no stop reason at all, no API error) means the
    // answer was cut off mid-flight. The caller only reaches this after the
    // transcript has been quiet long enough that "still streaming" is ruled out.
    if state.is_broken_stream() {
        return ContinueDecision {
            should_continue: true,
            reason: "回合被意外中断（无正常结束标记，疑似断流）".into(),
        };
    }

    if !matches!(
        state.stop_reason.as_deref(),
        Some("end_turn") | Some("stop_sequence")
    ) {
        return ContinueDecision {
            should_continue: false,
            reason: format!(
                "当前不是已停止回合（stop_reason={}）",
                state.stop_reason.clone().unwrap_or_else(|| "无".into())
            ),
        };
    }

    let text = state.assistant_text.trim();
    // "Unfinished" signals only count when they appear near the END of the
    // reply. A mid-narration "接下来我来改 X" followed by a wrap-up should not
    // trigger a continue — what matters is how the reply actually ends.
    let tail = tail_text(text, 160);
    let unfinished = matches_any(&tail, &UNFINISHED_RE);
    // Completion markers are checked against the whole reply.
    let complete = matches_any(text, &COMPLETION_RE);

    // "Confirm completion" overrides the per-mode logic in EVERY mode: keep
    // asking until the reply clearly declares the task done. A stray unfinished
    // phrase at the tail also forces another round even if a completion word
    // appeared earlier, so a genuine "done" claim is required to stop.
    if confirm_completion {
        // Stop ONLY on the explicit marker — never on fuzzy completion phrases,
        // which show up mid-work and would end the loop too early. If the reply
        // doesn't carry the marker, ask again regardless of how it's worded.
        if has_done_marker(text) {
            return ContinueDecision {
                should_continue: false,
                reason: "确认完成模式：已收到明确完成标记 [[AUTO_CONTINUE_DONE]]".into(),
            };
        }
        return ContinueDecision {
            should_continue: true,
            reason: "确认完成模式：未见明确完成标记，追问是否已完成".into(),
        };
    }

    // Custom keywords work in every mode: if one matches and the reply does not
    // clearly claim completion, treat the task as unfinished and continue.
    if let Some(hit) = custom_keyword_hit(text, custom_keywords) {
        if !complete {
            return ContinueDecision {
                should_continue: true,
                reason: format!("命中自定义关键字“{}”，且未见完成标记", hit),
            };
        }
    }

    let mode = mode.trim().to_lowercase();
    if mode == "safe" {
        return ContinueDecision {
            should_continue: false,
            reason: "安全模式只处理 max_tokens 与 API 错误".into(),
        };
    }

    if mode == "smart" {
        // A completion claim wins over a stray unfinished phrase: if the reply
        // says it's done, don't continue even if some earlier wording matched.
        if complete {
            return ContinueDecision {
                should_continue: false,
                reason: "回复包含完成标记/完成语句".into(),
            };
        }
        if unfinished {
            return ContinueDecision {
                should_continue: true,
                reason: "回复结尾明确表示仍有未完成工作".into(),
            };
        }
        if state.has_unclosed_code_fence {
            return ContinueDecision {
                should_continue: true,
                reason: "回复末尾存在未闭合代码块，疑似被截断".into(),
            };
        }
        return ContinueDecision {
            should_continue: false,
            reason: "未发现可靠的未完成信号".into(),
        };
    }

    if mode == "strict" {
        if complete && !unfinished {
            return ContinueDecision {
                should_continue: false,
                reason: "检测到完成标记/完成语句".into(),
            };
        }
        return ContinueDecision {
            should_continue: true,
            reason: "严格模式：尚未检测到完成标记".into(),
        };
    }

    ContinueDecision {
        should_continue: false,
        reason: format!("未知检测模式：{}", mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Build a TurnState directly for decision-logic tests.
    fn state(stop: &str, text: &str) -> TurnState {
        TurnState {
            stop_reason: Some(stop.to_string()),
            assistant_text: text.to_string(),
            has_unclosed_code_fence: text.matches("```").count() % 2 == 1,
            ..Default::default()
        }
    }

    #[test]
    fn max_tokens_always_continues() {
        // Even safe mode must continue on a hard length truncation.
        let st = state("max_tokens", "正在实现");
        assert!(decide_continue(&st, "safe", &[], false).should_continue);
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
        assert!(decide_continue(&st, "strict", &[], false).should_continue);
    }

    #[test]
    fn smart_continues_on_explicit_unfinished() {
        let st = state("end_turn", "还需要继续完成剩余工作。");
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn smart_stops_on_normal_completion() {
        let st = state("end_turn", "任务已完成，测试已通过。");
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
        assert!(!decide_continue(&st, "strict", &[], false).should_continue);
    }

    #[test]
    fn safe_ignores_plain_end_turn() {
        let st = state("end_turn", "还需要继续");
        assert!(!decide_continue(&st, "safe", &[], false).should_continue);
    }

    #[test]
    fn unclosed_code_fence_continues_in_smart() {
        let st = state("end_turn", "```python\nprint('x')");
        assert!(st.has_unclosed_code_fence);
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn tool_use_is_not_terminal() {
        let st = state("tool_use", "");
        assert!(!st.is_terminal());
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn broken_stream_continues_in_every_mode() {
        // A turn with a user prompt and some assistant text but NO stop reason
        // and NO API error = the stream was cut off mid-answer. Must retry even
        // in safe mode, since it is an abnormal termination, not a completion.
        let st = TurnState {
            last_user_uuid: "u1".into(),
            stop_reason: None,
            assistant_text: "我先修改这个文件".into(),
            ..Default::default()
        };
        assert!(st.is_broken_stream());
        assert!(decide_continue(&st, "safe", &[], false).should_continue);
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
        assert!(decide_continue(&st, "strict", &[], false).should_continue);
    }

    #[test]
    fn tool_use_is_not_a_broken_stream() {
        // Mid-flight tool execution has no stop reason either, but it is working,
        // not broken — it must not be treated as an interrupted stream.
        let st = TurnState {
            last_user_uuid: "u1".into(),
            stop_reason: Some("tool_use".into()),
            ..Default::default()
        };
        assert!(!st.is_broken_stream());
    }

    #[test]
    fn no_user_prompt_is_not_a_broken_stream() {
        // An empty/fresh transcript with no user turn must not look broken.
        let st = TurnState::default();
        assert!(!st.is_broken_stream());
    }

    #[test]
    fn strict_continues_until_completion_marker() {
        let st = state("end_turn", "这一步做完了，我先看看。");
        assert!(decide_continue(&st, "strict", &[], false).should_continue);
        let done = state("end_turn", "全部完成 [[AUTO_CONTINUE_DONE]]");
        assert!(!decide_continue(&done, "strict", &[], false).should_continue);
    }

    #[test]
    fn smart_completion_claim_beats_stray_unfinished_word() {
        // A reply that narrates "接下来" mid-way but ends with a completion claim
        // must NOT trigger a continue (the old logic wrongly did).
        let st = state(
            "end_turn",
            "我先改了 A，接下来又改了 B。全部完成，测试已通过。",
        );
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn smart_ignores_midtext_narration_when_tail_is_a_wrapup() {
        // "接下来" appears only in the middle; the reply ends on a neutral wrap-up
        // with no trailing unfinished signal -> should not continue.
        let long_middle = "接下来我会实现这个功能。".to_string()
            + &"这里是一大段实现说明和代码解释，用于把前面的叙述推离结尾。".repeat(6);
        let text = format!("{}\n以上就是本次改动的说明。", long_middle);
        let st = state("end_turn", &text);
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn smart_continues_when_tail_says_unfinished() {
        // Unfinished signal genuinely at the end -> continue.
        let st = state("end_turn", "我已经改好了第一部分，尚未完成剩余的部分。");
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
    }

    // ---- transcript parsing on a real temp .jsonl file --------------------
    fn write_jsonl(lines: &[serde_json::Value]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        // A process-unique counter (not a timestamp) guarantees distinct files
        // even when tests run in parallel — Windows SystemTime is too coarse to
        // keep nanosecond names unique, which caused two tests to share a file.
        let unique = format!(
            "cac_test_{}_{}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        path.push(unique);
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
        path
    }

    #[test]
    fn analyze_reads_last_assistant_after_user() {
        let path = write_jsonl(&[
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s1", "cwd": "E:\\demo",
                "message": {"role": "user", "content": "请开发"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "timestamp": "2026-07-27T00:00:00Z",
                "sessionId": "s1",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "还需要继续完成剩余工作。"}],
                    "stop_reason": "end_turn"
                }
            }),
        ]);
        let st = analyze_transcript(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(st.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(st.session_id, "s1");
        assert_eq!(st.cwd, "E:\\demo");
        assert!(st.assistant_text.contains("剩余工作"));
        assert!(!st.fingerprint.is_empty());
        assert!(decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn analyze_detects_max_tokens_truncation() {
        let path = write_jsonl(&[
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s2",
                "message": {"role": "user", "content": "实现功能"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "s2",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "正在实现"}],
                    "stop_reason": "max_tokens"
                }
            }),
        ]);
        let st = analyze_transcript(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(st.stop_reason.as_deref(), Some("max_tokens"));
        assert!(decide_continue(&st, "safe", &[], false).should_continue);
    }

    #[test]
    fn project_display_name_rebuilds_drive_path() {
        assert_eq!(project_display_name("e--ai-claude"), "E:\\ai-claude");
        assert_eq!(project_display_name("plain-name"), "plain-name");
    }

    // ---- API-error detection ---------------------------------------------
    fn api_error_row(status: u64, formatted: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "system", "subtype": "api_error", "level": "error",
            "sessionId": "s3", "entrypoint": "claude-vscode",
            "error": {"status": status, "formatted": formatted, "message": "..."},
            "source": "request_retry"
        })
    }

    #[test]
    fn trailing_api_error_triggers_continue_in_every_mode() {
        // Error is the last event → unrecovered → must continue.
        let path = write_jsonl(&[
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s3",
                "message": {"role": "user", "content": "开发"}
            }),
            api_error_row(502, "502 Upstream service temporarily unavailable"),
        ]);
        let st = analyze_transcript(&path);
        let _ = fs::remove_file(&path);

        assert!(st.is_api_error());
        assert!(st.last_error.contains("502"));
        for mode in ["safe", "smart", "strict"] {
            assert!(decide_continue(&st, mode, &[], false).should_continue, "mode={mode}");
        }
    }

    #[test]
    fn recovered_api_error_does_not_trigger() {
        // Error followed by a real assistant reply → recovered → no trigger.
        let path = write_jsonl(&[
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s3",
                "message": {"role": "user", "content": "开发"}
            }),
            api_error_row(429, "429 Concurrency limit exceeded"),
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "s3",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "任务已完成，测试已通过。"}],
                    "stop_reason": "end_turn"
                }
            }),
        ]);
        let st = analyze_transcript(&path);
        let _ = fs::remove_file(&path);

        assert!(!st.is_api_error(), "error should be cleared by later reply");
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }

    #[test]
    fn entrypoint_is_captured() {
        let path = write_jsonl(&[serde_json::json!({
            "type": "user", "uuid": "u1", "sessionId": "s3", "entrypoint": "claude-desktop",
            "message": {"role": "user", "content": "开发"}
        })]);
        let st = analyze_transcript(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(st.entrypoint, "claude-desktop");
    }

    // ---- custom keywords --------------------------------------------------
    #[test]
    fn custom_keyword_triggers_when_no_completion() {
        let st = state("end_turn", "我先暂停一下，等你确认。");
        let kws = vec!["暂停".to_string()];
        assert!(decide_continue(&st, "smart", &kws, false).should_continue);
    }

    #[test]
    fn custom_keyword_ignored_when_task_complete() {
        // Completion language wins over a keyword hit, so we don't loop forever.
        let st = state("end_turn", "任务已完成。（这里顺便说了暂停）");
        let kws = vec!["暂停".to_string()];
        assert!(!decide_continue(&st, "smart", &kws, false).should_continue);
    }

    #[test]
    fn custom_keyword_empty_list_is_noop() {
        let st = state("end_turn", "随便一段话");
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }

    // ---- confirm-completion mode ------------------------------------------
    #[test]
    fn confirm_completion_keeps_asking_on_plain_stop() {
        // A normally-stopped reply with no completion claim must keep asking,
        // in EVERY mode, when confirm-completion is on.
        let st = state("end_turn", "我先改了这个文件，跑了一下看看效果。");
        for mode in ["safe", "smart", "strict"] {
            assert!(
                decide_continue(&st, mode, &[], true).should_continue,
                "mode={mode} should keep asking"
            );
        }
    }

    #[test]
    fn confirm_completion_stops_only_on_marker() {
        // Only the explicit marker stops it — in every mode.
        let st = state("end_turn", "全部完成 [[AUTO_CONTINUE_DONE]]");
        for mode in ["safe", "smart", "strict"] {
            assert!(
                !decide_continue(&st, mode, &[], true).should_continue,
                "mode={mode} should stop on the explicit marker"
            );
        }
    }

    #[test]
    fn confirm_completion_ignores_fuzzy_completion_phrases() {
        // A fuzzy "任务已完成/测试通过" WITHOUT the marker must NOT stop the loop,
        // because such phrases show up mid-work and would end it prematurely.
        let st = state("end_turn", "任务已完成，全部测试通过。");
        for mode in ["safe", "smart", "strict"] {
            assert!(
                decide_continue(&st, mode, &[], true).should_continue,
                "mode={mode} must keep asking without the explicit marker"
            );
        }
    }

    #[test]
    fn confirm_completion_off_is_unchanged() {
        // With the flag off, a plain stop in smart mode does NOT continue.
        let st = state("end_turn", "我先改了这个文件，跑了一下看看效果。");
        assert!(!decide_continue(&st, "smart", &[], false).should_continue);
    }
}
