"""Run the sandbox installer with stub privilege tools in a temporary root."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class InstallCodexSandboxTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.log = self.root / "calls"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, code in {
            "id": "echo 0",
            "chown": 'echo "chown $*" >> "$CALL_LOG"',
            "chmod": 'echo "chmod $*" >> "$CALL_LOG"',
            "install": 'echo "install $*" >> "$CALL_LOG"',
            "apparmor_parser": 'echo "parser $*" >> "$CALL_LOG"; exit "${PARSER_EXIT:-0}"',
            "sysctl": 'echo "Unexpected global policy change" >&2; exit 99',
        }.items():
            path = self.bin / name
            path.write_text("#!/bin/sh\n" + code + "\n")
            path.chmod(0o755)
        self.binary_root = self.root / "opt/codex-studio/codex"
        self.binary_root.mkdir(parents=True)
        self.restriction = self.root / "proc/sys/kernel/apparmor_restrict_unprivileged_userns"
        self.restriction.parent.mkdir(parents=True)
        self.restriction.write_text("1\n")
        source = Path(__file__).with_name("install-codex-sandbox.sh").read_text()
        for prefix in ("/opt/codex-studio", "/proc/sys/kernel", "/etc/apparmor.d"):
            source = source.replace(prefix, str(self.root) + prefix)
        self.script = self.root / "install-codex-sandbox.sh"
        self.script.write_text(source)

    def run_installer(self, **env):
        return subprocess.run(["sh", str(self.script)], capture_output=True, text=True,
                              timeout=5, env={**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                                              "CALL_LOG": str(self.log), **env})

    def test_repairs_trusted_binary_before_loading_only_its_profile(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual([line.split()[0] for line in calls], ["chown", "chmod", "install", "parser"])
        self.assertIn("--no-dereference root:root", calls[0])
        self.assertTrue(calls[-1].endswith("/etc/apparmor.d/studio-codex"))
        self.assertEqual(self.restriction.read_text(), "1\n")

    def test_policy_load_failure_stops_installation(self):
        self.assertEqual(self.run_installer(PARSER_EXIT="42").returncode, 42)

    def test_without_apparmor_only_repairs_binary_ownership(self):
        self.restriction.unlink()
        self.assertEqual(self.run_installer().returncode, 0)
        self.assertEqual(len(self.log.read_text().splitlines()), 2)

    def test_rejects_a_broken_symlink_before_privilege_changes(self):
        self.binary_root.rmdir()
        self.binary_root.symlink_to(self.root / "missing", target_is_directory=True)
        self.assertEqual(self.run_installer().returncode, 1)
        self.assertFalse(self.log.exists())

    def test_rejects_a_symlink_before_privilege_changes(self):
        self.binary_root.rmdir()
        self.binary_root.symlink_to(self.bin, target_is_directory=True)
        self.assertEqual(self.run_installer().returncode, 1)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
