import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

try:
    from windows_automation import WindowInfo  # noqa: E402
except RuntimeError:
    WindowInfo = None  # module refuses to import off Windows


@unittest.skipIf(WindowInfo is None, "windows_automation only imports on Windows")
class WindowKindTests(unittest.TestCase):
    def make(self, exe: str) -> "WindowInfo":
        return WindowInfo(hwnd=1, title="demo", executable=exe, pid=100)

    def test_vscode_variants_are_vscode(self):
        for exe in (r"C:\a\Code.exe", r"C:\a\Code - Insiders.exe", r"C:\a\VSCodium.exe"):
            self.assertEqual(self.make(exe).kind, "vscode")

    def test_common_terminals_are_terminal(self):
        for exe in (
            r"C:\a\WindowsTerminal.exe",
            r"C:\Windows\System32\cmd.exe",
            r"C:\a\pwsh.exe",
            r"C:\a\powershell.exe",
        ):
            self.assertEqual(self.make(exe).kind, "terminal")

    def test_unknown_process_has_no_kind(self):
        self.assertEqual(self.make(r"C:\a\explorer.exe").kind, "")

    def test_kind_label_is_human_readable(self):
        self.assertEqual(self.make(r"C:\a\Code.exe").kind_label, "VS Code")
        self.assertEqual(self.make(r"C:\a\wt.exe").kind_label, "终端")


if __name__ == "__main__":
    unittest.main()
