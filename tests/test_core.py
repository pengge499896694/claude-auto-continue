import json
import tempfile
import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from core import analyze_transcript, decide_continue  # noqa: E402


def row(kind, role=None, content=None, stop_reason=None, uuid="", timestamp="2026-07-27T00:00:00Z"):
    obj = {"type": kind, "uuid": uuid, "timestamp": timestamp, "sessionId": "session-1", "cwd": r"E:\demo"}
    if role:
        obj["message"] = {"role": role, "content": content, "stop_reason": stop_reason}
    return obj


class CoreTests(unittest.TestCase):
    def write_rows(self, rows):
        temp = tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".jsonl", delete=False)
        with temp:
            for item in rows:
                temp.write(json.dumps(item, ensure_ascii=False) + "\n")
        return Path(temp.name)

    def test_max_tokens_always_continues(self):
        path = self.write_rows(
            [
                row("user", "user", [{"type": "text", "text": "实现功能"}], uuid="u1"),
                row("assistant", "assistant", [{"type": "text", "text": "正在实现"}], "max_tokens", "a1"),
            ]
        )
        state = analyze_transcript(path)
        self.assertEqual(state.stop_reason, "max_tokens")
        self.assertTrue(decide_continue(state, "safe").should_continue)

    def test_smart_continues_explicit_unfinished(self):
        path = self.write_rows(
            [
                row("user", "user", "请开发", uuid="u1"),
                row("assistant", "assistant", [{"type": "text", "text": "还需要继续完成剩余工作。"}], "end_turn", "a1"),
            ]
        )
        state = analyze_transcript(path)
        self.assertTrue(decide_continue(state, "smart").should_continue)

    def test_smart_stops_on_normal_completion(self):
        path = self.write_rows(
            [
                row("user", "user", "请开发", uuid="u1"),
                row("assistant", "assistant", [{"type": "text", "text": "任务已完成，测试已通过。"}], "end_turn", "a1"),
            ]
        )
        state = analyze_transcript(path)
        self.assertFalse(decide_continue(state, "smart").should_continue)
        self.assertFalse(decide_continue(state, "strict").should_continue)

    def test_tool_use_is_not_terminal(self):
        path = self.write_rows(
            [
                row("user", "user", "请开发", uuid="u1"),
                row("assistant", "assistant", [{"type": "tool_use", "name": "Shell"}], "tool_use", "a1"),
            ]
        )
        state = analyze_transcript(path)
        self.assertFalse(state.is_terminal)
        self.assertFalse(decide_continue(state, "smart").should_continue)

    def test_unclosed_code_block_continues_in_smart_mode(self):
        path = self.write_rows(
            [
                row("user", "user", "输出代码", uuid="u1"),
                row("assistant", "assistant", [{"type": "text", "text": "```python\nprint('x')"}], "end_turn", "a1"),
            ]
        )
        state = analyze_transcript(path)
        self.assertTrue(state.has_unclosed_code_fence)
        self.assertTrue(decide_continue(state, "smart").should_continue)


if __name__ == "__main__":
    unittest.main()
