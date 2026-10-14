"""Tests for the isolated HTTP backend process environment."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from codex_layout import SERVER_TESTS_ROOT

sys.path.insert(0, str(SERVER_TESTS_ROOT))
import http_backend_harness


class BackendEnvironmentTests(unittest.TestCase):
    def test_not_ready_waits_for_child_exit_before_reporting_status(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-exit-") as temporary:
            backend = http_backend_harness.BackendProcess(
                Path(temporary), claude_bin=None, initialize_workspace=False,
            )
            process = mock.Mock()
            process.wait.return_value = 1
            backend._process = process
            try:
                with mock.patch.object(backend, "stop"), self.assertRaisesRegex(
                    RuntimeError, "child exit status=1",
                ):
                    backend._raise_backend_not_ready("stdout closed before ready", None)
                process.wait.assert_called_once_with(
                    timeout=http_backend_harness.STOP_TIMEOUT_SECONDS,
                )
            finally:
                backend._process = None
                backend.close()

    def test_not_ready_reports_when_child_exit_wait_bound_expires(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-exit-") as temporary:
            backend = http_backend_harness.BackendProcess(
                Path(temporary), claude_bin=None, initialize_workspace=False,
            )
            process = mock.Mock()
            process.wait.side_effect = subprocess.TimeoutExpired(
                "backend", http_backend_harness.STOP_TIMEOUT_SECONDS,
            )
            backend._process = process
            try:
                with mock.patch.object(backend, "stop"), self.assertRaisesRegex(
                    RuntimeError, "did not exit within the 4s wait bound",
                ) as raised:
                    backend._raise_backend_not_ready("stdout closed before ready", None)
                self.assertNotIn("child exit status=None", str(raised.exception))
                process.wait.assert_called_once_with(
                    timeout=http_backend_harness.STOP_TIMEOUT_SECONDS,
                )
            finally:
                backend._process = None
                backend.close()

    def test_custom_command_receives_only_explicit_backend_python(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-env-") as temporary:
            scratch = Path(temporary)
            backend = http_backend_harness.BackendProcess(
                scratch, claude_bin=None, initialize_workspace=False,
            )
            codex = scratch / "codex"
            codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            codex.chmod(0o700)
            backend._provider_executable = codex

            def fixture_which(command: str, path: str | None = None) -> str | None:
                if command == "codex":
                    return str(codex)
                return None

            with mock.patch.dict(os.environ, {
                "STUDIO_HTTP_BACKEND_COMMAND": "/fixture/rust-server",
                "STUDIO_HTTP_BACKEND_PYTHON": "/fixture/python",
                "CODEX_AGENTS_PYTHON": "/untrusted/python",
            }, clear=True), mock.patch.object(
                http_backend_harness.shutil, "which", side_effect=fixture_which,
            ):
                environment = backend._environment()

            self.assertEqual(environment["CODEX_AGENTS_PYTHON"], "/fixture/python")
            self.assertNotIn("STUDIO_HTTP_BACKEND_PYTHON", environment)
            self.assertNotIn("STUDIO_HTTP_BACKEND_COMMAND", environment)

    def test_custom_command_does_not_inherit_python_override_without_selection(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-env-") as temporary:
            scratch = Path(temporary)
            backend = http_backend_harness.BackendProcess(
                scratch, claude_bin=None, initialize_workspace=False,
            )
            codex = scratch / "codex"
            codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            codex.chmod(0o700)
            backend._provider_executable = codex

            def fixture_which(command: str, path: str | None = None) -> str | None:
                if command == "codex":
                    return str(codex)
                return None

            with mock.patch.dict(os.environ, {
                "STUDIO_HTTP_BACKEND_COMMAND": "/fixture/rust-server",
                "CODEX_AGENTS_PYTHON": "/untrusted/python",
            }, clear=True), mock.patch.object(
                http_backend_harness.shutil, "which", side_effect=fixture_which,
            ):
                environment = backend._environment()

            self.assertNotIn("CODEX_AGENTS_PYTHON", environment)


if __name__ == "__main__":
    unittest.main()
