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

const UNFINISHED_PATTERNS: &[&str] = &[
    "尚未完成",
    "还没完成",
    "仍未完成",
    "需要继续",
    "继续完成",
    "接下来(?:需要|将|我会)",
    "下一步(?:是|需要|将)",
    "剩余(?:工作|任务|步骤|问题)",
    "还需要",
    "后续还要",
    "我将继续",
    "未能完成",
    "待完成",
    "TODO",
    "not (?:yet )?(?:finished|complete)",
    "still need(?:s)? to",
    "need(?:s)? to continue",
    "I(?:'|’)ll continue",
    "I will continue",
    "remaining (?:work|tasks?|steps?)",
    "next steps? (?:are|is|include)",
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
}

impl TurnState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.stop_reason.as_deref(),
            Some("end_turn") | Some("max_tokens") | Some("stop_sequence")
        )
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

    let start = if last_user_index >= 0 {
        (last_user_index + 1) as usize
    } else {
        0
    };
    let mut assistant_text: Vec<String> = Vec::new();
    for obj in &objects[start..] {
        let message = match obj.get("message") {
            Some(m) if m.is_object() => m,
            _ => continue,
        };
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
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
        "{}|{}|{}|{}|{}",
        state.session_id,
        state.last_user_uuid,
        state.last_assistant_uuid,
        state.last_assistant_timestamp,
        state.stop_reason.clone().unwrap_or_default()
    );
    state
}

pub fn decide_continue(state: &TurnState, mode: &str) -> ContinueDecision {
    if state.stop_reason.as_deref() == Some("max_tokens") {
        return ContinueDecision {
            should_continue: true,
            reason: "检测到 stop_reason=max_tokens（输出被长度限制截断）".into(),
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

    let mode = mode.trim().to_lowercase();
    if mode == "safe" {
        return ContinueDecision {
            should_continue: false,
            reason: "安全模式只处理 max_tokens".into(),
        };
    }

    let text = state.assistant_text.trim();
    let unfinished = matches_any(text, &UNFINISHED_RE);
    let complete = matches_any(text, &COMPLETION_RE);

    if mode == "smart" {
        if unfinished {
            return ContinueDecision {
                should_continue: true,
                reason: "回复明确表示仍有未完成工作".into(),
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
        assert!(decide_continue(&st, "safe").should_continue);
        assert!(decide_continue(&st, "smart").should_continue);
        assert!(decide_continue(&st, "strict").should_continue);
    }

    #[test]
    fn smart_continues_on_explicit_unfinished() {
        let st = state("end_turn", "还需要继续完成剩余工作。");
        assert!(decide_continue(&st, "smart").should_continue);
    }

    #[test]
    fn smart_stops_on_normal_completion() {
        let st = state("end_turn", "任务已完成，测试已通过。");
        assert!(!decide_continue(&st, "smart").should_continue);
        assert!(!decide_continue(&st, "strict").should_continue);
    }

    #[test]
    fn safe_ignores_plain_end_turn() {
        let st = state("end_turn", "还需要继续");
        assert!(!decide_continue(&st, "safe").should_continue);
    }

    #[test]
    fn unclosed_code_fence_continues_in_smart() {
        let st = state("end_turn", "```python\nprint('x')");
        assert!(st.has_unclosed_code_fence);
        assert!(decide_continue(&st, "smart").should_continue);
    }

    #[test]
    fn tool_use_is_not_terminal() {
        let st = state("tool_use", "");
        assert!(!st.is_terminal());
        assert!(!decide_continue(&st, "smart").should_continue);
    }

    #[test]
    fn strict_continues_until_completion_marker() {
        let st = state("end_turn", "这一步做完了，我先看看。");
        assert!(decide_continue(&st, "strict").should_continue);
        let done = state("end_turn", "全部完成 [[AUTO_CONTINUE_DONE]]");
        assert!(!decide_continue(&done, "strict").should_continue);
    }

    // ---- transcript parsing on a real temp .jsonl file --------------------
    fn write_jsonl(lines: &[serde_json::Value]) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "cac_test_{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
        assert!(decide_continue(&st, "smart").should_continue);
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
        assert!(decide_continue(&st, "safe").should_continue);
    }

    #[test]
    fn project_display_name_rebuilds_drive_path() {
        assert_eq!(project_display_name("e--ai-claude"), "E:\\ai-claude");
        assert_eq!(project_display_name("plain-name"), "plain-name");
    }
}
