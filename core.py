from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Iterable, Optional


DEFAULT_PROJECTS_ROOT = Path.home() / ".claude" / "projects"

UNFINISHED_PATTERNS = [
    r"尚未完成", r"还没完成", r"仍未完成", r"需要继续", r"继续完成", r"接下来(?:需要|将|我会)",
    r"下一步(?:是|需要|将)", r"剩余(?:工作|任务|步骤|问题)", r"还需要", r"后续还要", r"我将继续",
    r"未能完成", r"待完成", r"TODO", r"not (?:yet )?(?:finished|complete)", r"still need(?:s)? to",
    r"need(?:s)? to continue", r"I(?:'|’)ll continue", r"I will continue", r"remaining (?:work|tasks?|steps?)",
    r"next steps? (?:are|is|include)",
]

COMPLETION_PATTERNS = [
    r"任务已完成", r"全部完成", r"已全部完成", r"实现完成", r"修复完成", r"开发完成",
    r"所有(?:任务|工作|修改).{0,8}完成", r"测试(?:已)?通过", r"验证(?:已)?通过",
    r"task is complete", r"task has been completed", r"completed successfully", r"fully implemented",
    r"all (?:done|complete)", r"implementation is complete", r"\[\[AUTO_CONTINUE_DONE\]\]",
]


@dataclass(frozen=True)
class SessionInfo:
    path: Path
    project_name: str
    session_id: str
    modified_at: float


@dataclass
class TurnState:
    path: Path
    session_id: str = ""
    cwd: str = ""
    last_user_uuid: str = ""
    last_user_text: str = ""
    last_assistant_uuid: str = ""
    last_assistant_timestamp: str = ""
    stop_reason: Optional[str] = None
    assistant_text: str = ""
    has_unclosed_code_fence: bool = False
    parse_error_count: int = 0

    @property
    def fingerprint(self) -> str:
        return "|".join(
            [
                self.session_id,
                self.last_user_uuid,
                self.last_assistant_uuid,
                self.last_assistant_timestamp,
                self.stop_reason or "",
            ]
        )

    @property
    def is_terminal(self) -> bool:
        return self.stop_reason in {"end_turn", "max_tokens", "stop_sequence"}


@dataclass(frozen=True)
class ContinueDecision:
    should_continue: bool
    reason: str


def _project_display_name(encoded: str) -> str:
    # Claude replaces path separators and punctuation with dashes. Keep the raw name
    # because reverse conversion is ambiguous, but make common drive prefixes readable.
    if re.match(r"^[A-Za-z]--", encoded):
        return encoded[0].upper() + ":\\" + encoded[3:].replace("--", "\\")
    return encoded


def list_sessions(projects_root: Path = DEFAULT_PROJECTS_ROOT, limit: int = 30) -> list[SessionInfo]:
    if not projects_root.exists():
        return []
    sessions: list[SessionInfo] = []
    for project_dir in projects_root.iterdir():
        if not project_dir.is_dir():
            continue
        for path in project_dir.glob("*.jsonl"):
            try:
                stat = path.stat()
            except OSError:
                continue
            sessions.append(
                SessionInfo(
                    path=path,
                    project_name=_project_display_name(project_dir.name),
                    session_id=path.stem,
                    modified_at=stat.st_mtime,
                )
            )
    sessions.sort(key=lambda s: s.modified_at, reverse=True)
    return sessions[:limit]


def format_session(info: SessionInfo) -> str:
    stamp = datetime.fromtimestamp(info.modified_at).strftime("%m-%d %H:%M:%S")
    short_id = info.session_id[:8]
    return f"{stamp}  {info.project_name}  [{short_id}]"


def _read_tail_lines(path: Path, max_bytes: int = 4 * 1024 * 1024) -> list[str]:
    size = path.stat().st_size
    with path.open("rb") as fh:
        if size > max_bytes:
            fh.seek(size - max_bytes)
            fh.readline()  # discard a possibly partial JSON line
        data = fh.read()
    return data.decode("utf-8", errors="replace").splitlines()


def _content_text(content: object) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    chunks: list[str] = []
    for item in content:
        if isinstance(item, dict) and item.get("type") == "text":
            text = item.get("text")
            if isinstance(text, str):
                chunks.append(text)
    return "\n".join(chunks)


def _is_human_user_message(obj: dict) -> bool:
    if obj.get("type") != "user":
        return False
    message = obj.get("message")
    if not isinstance(message, dict) or message.get("role") != "user":
        return False
    content = message.get("content")
    if isinstance(content, str):
        return bool(content.strip())
    if isinstance(content, list):
        return any(
            isinstance(item, dict)
            and item.get("type") == "text"
            and isinstance(item.get("text"), str)
            and item["text"].strip()
            for item in content
        )
    return False


def analyze_transcript(path: os.PathLike[str] | str) -> TurnState:
    target = Path(path)
    state = TurnState(path=target)
    objects: list[dict] = []
    for line in _read_tail_lines(target):
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            state.parse_error_count += 1
            continue
        if isinstance(obj, dict):
            objects.append(obj)

    last_user_index = -1
    for index, obj in enumerate(objects):
        if _is_human_user_message(obj):
            last_user_index = index
            message = obj["message"]
            state.last_user_uuid = str(obj.get("uuid") or obj.get("promptId") or "")
            state.last_user_text = _content_text(message.get("content"))
            state.session_id = str(obj.get("sessionId") or state.session_id)
            state.cwd = str(obj.get("cwd") or state.cwd)

    relevant = objects[last_user_index + 1 :] if last_user_index >= 0 else objects
    assistant_text: list[str] = []
    for obj in relevant:
        message = obj.get("message")
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        state.session_id = str(obj.get("sessionId") or state.session_id)
        state.cwd = str(obj.get("cwd") or state.cwd)
        text = _content_text(message.get("content"))
        if text.strip():
            assistant_text.append(text)
        stop_reason = message.get("stop_reason")
        if stop_reason is not None:
            state.stop_reason = str(stop_reason)
            state.last_assistant_uuid = str(obj.get("uuid") or "")
            state.last_assistant_timestamp = str(obj.get("timestamp") or "")

    state.assistant_text = "\n".join(assistant_text).strip()
    state.has_unclosed_code_fence = state.assistant_text.count("```") % 2 == 1
    return state


def _matches_any(text: str, patterns: Iterable[str]) -> bool:
    return any(re.search(pattern, text, flags=re.IGNORECASE | re.DOTALL) for pattern in patterns)


def decide_continue(state: TurnState, mode: str = "smart") -> ContinueDecision:
    """Decide whether a stopped Claude turn should receive a continuation prompt.

    Modes:
      safe: only definite max-token truncation.
      smart: max-token truncation plus explicit unfinished language/unclosed code blocks.
      strict: every end_turn until a completion marker appears.
    """
    if state.stop_reason == "max_tokens":
        return ContinueDecision(True, "检测到 stop_reason=max_tokens（输出被长度限制截断）")

    if state.stop_reason not in {"end_turn", "stop_sequence"}:
        return ContinueDecision(False, f"当前不是已停止回合（stop_reason={state.stop_reason or '无'}）")

    normalized_mode = mode.lower().strip()
    if normalized_mode == "safe":
        return ContinueDecision(False, "安全模式只处理 max_tokens")

    text = state.assistant_text.strip()
    unfinished = _matches_any(text, UNFINISHED_PATTERNS)
    complete = _matches_any(text, COMPLETION_PATTERNS)

    # Negative/unfinished language takes precedence over broad completion words.
    if normalized_mode == "smart":
        if unfinished:
            return ContinueDecision(True, "回复明确表示仍有未完成工作")
        if state.has_unclosed_code_fence:
            return ContinueDecision(True, "回复末尾存在未闭合代码块，疑似被截断")
        return ContinueDecision(False, "未发现可靠的未完成信号")

    if normalized_mode == "strict":
        if complete and not unfinished:
            return ContinueDecision(False, "检测到完成标记/完成语句")
        return ContinueDecision(True, "严格模式：尚未检测到完成标记")

    return ContinueDecision(False, f"未知检测模式：{mode}")
