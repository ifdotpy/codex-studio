#!/usr/bin/env python3
"""Managed command shell contracts. No user state or model service."""

from pathlib import Path
import shlex
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
        with patch("codex_shell.default_shell", return_value="/bin/zsh"), patch("codex_shell.sys.platform", "linux"):
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

    @unittest.skipUnless(sys.platform == "darwin", "macOS SDK selection")
    def test_selected_sdk_compiles_in_each_posix_startup_mode(self):
        selected = subprocess.check_output(
            ["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"], text=True
        ).strip()
        with tempfile.TemporaryDirectory(prefix="studio-monitor-sdk-") as directory:
            root = Path(directory)
            (root / "main.c").write_text("int main(void) { return 0; }\n")
            environment = {"HOME": directory, "ZDOTDIR": directory, "PATH": "/usr/bin:/bin"}
            for shell in ("/bin/zsh", "/bin/bash", "/bin/sh"):
                for config in ({}, {"allow_login_shell": False}, {"features": {"shell_snapshot": False}}):
                    with self.subTest(shell=shell, config=config), patch("codex_shell.default_shell", return_value=shell):
                        argv = monitor_command(ConfigServer(config),
                            'printf "%s\\n" "$SDKROOT"; /usr/bin/cc main.c -o main && ./main', directory)
                        result = subprocess.run(argv, cwd=directory, env=environment,
                                                capture_output=True, text=True, timeout=15)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(result.stdout, selected + "\n")

    @unittest.skipUnless(sys.platform == "darwin", "macOS SDK selection")
    def test_explicit_sdk_values_and_failed_selection(self):
        with tempfile.TemporaryDirectory(prefix="studio-monitor-sdk-") as directory:
            root = Path(directory)
            environment = {"HOME": directory, "ZDOTDIR": directory, "PATH": "/usr/bin:/bin"}
            cases = [
                ({"SDKROOT": ""}, "", {}, ""),
                ({"SDKROOT": "/project/sdk with spaces"}, "", {}, "/project/sdk with spaces"),
                ({}, 'export SDKROOT="/project/startup-sdk"\n', {}, "/project/startup-sdk"),
                ({}, 'export SDKROOT="/project/startup-sdk"\n',
                 {"shell_environment_policy": {"set": {"SDKROOT": "/project/policy-sdk"}}}, "/project/policy-sdk"),
                ({"DEVELOPER_DIR": "/missing/studio-test-developer"}, "", {}, "absent"),
            ]
            with patch("codex_shell.default_shell", return_value="/bin/zsh"):
                for extra, startup, config, expected in cases:
                    with self.subTest(extra=extra, startup=startup, config=config):
                        (root / ".zshrc").write_text(startup)
                        argv = monitor_command(ConfigServer(config), 'printf "%s" "${SDKROOT-absent}"', directory)
                        result = subprocess.run(argv, cwd=directory, env={**environment, **extra},
                                                capture_output=True, text=True, timeout=15)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(result.stdout, expected)
                (root / ".zshrc").write_text("")
                argv = monitor_command(ConfigServer({}),
                    'SDKROOT="/command/sdk" /bin/sh -c ' + shlex.quote('printf "%s" "$SDKROOT"'), directory)
                result = subprocess.run(argv, cwd=directory, env=environment,
                                        capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "/command/sdk")


if __name__ == "__main__":
    unittest.main()
