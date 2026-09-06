#!/usr/bin/env python3
"""Managed command shell contracts. No user state or model service."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_shell import default_shell, monitor_command


class ConfigServer:
    def __init__(self, config):
        self.config = config
        self.calls = []

    def call(self, method, params):
        self.calls.append((method, params))
        return {"config": self.config}


class MonitorShellContract(unittest.TestCase):
    def test_passwd_shell_wins_over_environment(self):
        with patch("codex_shell.pwd.getpwuid", return_value=SimpleNamespace(pw_shell="/bin/bash")):
            with patch.dict("os.environ", {"SHELL": "/bin/sh"}):
                self.assertEqual(default_shell(), "/bin/bash")

    def test_login_and_snapshot_disabled_preserve_native_flags(self):
        with patch("codex_shell.default_shell", return_value="/bin/zsh"):
            for config, flag in (({"allow_login_shell": False}, "-c"),
                                 ({"features": {"shell_snapshot": False}}, "-lc")):
                server = ConfigServer(config)
                self.assertEqual(monitor_command(server, "echo hello", "/project"),
                                 ["/bin/zsh", flag, "echo hello"])
                self.assertEqual(server.calls, [("config/read", {"cwd": "/project", "includeLayers": False})])

    def test_startup_and_explicit_environment_values(self):
        if not Path("/bin/zsh").exists():
            self.skipTest("zsh is unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".zshrc").write_text('export STUDIO_RC=loaded\nexport STUDIO_POLICY=wrong\nprintf startup-noise\n')
            value = "literal ' $(touch unexpected) `touch unexpected2`"
            server = ConfigServer({"shell_environment_policy": {"set": {"STUDIO_POLICY": value}}})
            with patch("codex_shell.default_shell", return_value="/bin/zsh"):
                argv = monitor_command(server, 'printf "%s\\n%s\\n" "$STUDIO_RC" "$STUDIO_POLICY"; exit 7', directory)
            result = subprocess.run(argv, cwd=directory, env={"HOME": directory, "ZDOTDIR": directory, "PATH": "/usr/bin:/bin"},
                                    text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 7)
            self.assertEqual(result.stdout, "loaded\n" + value + "\n")
            self.assertFalse((root / "unexpected").exists())
            self.assertFalse((root / "unexpected2").exists())


if __name__ == "__main__":
    unittest.main()
