"""Tests for the isolated HTTP backend process environment."""

import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from codex_layout import SERVER_TESTS_ROOT

sys.path.insert(0, str(SERVER_TESTS_ROOT))
import http_backend_harness
import test_isolation
from server_test_waits import HANG_GUARD_SECONDS


class BackendEnvironmentTests(unittest.TestCase):
    def test_rust_stdin_close_opt_in_matches_harness_environment_contract(self) -> None:
        source = Path(__file__).parents[3] / "runtime-server" / "src" / "main.rs"
        text = source.read_text(encoding="utf-8")
        self.assertIn(
            f'const EXIT_ON_STDIN_CLOSE_ENV: &str = "{http_backend_harness.SERVER_EXIT_ON_STDIN_CLOSE_ENV}";',
            text,
        )

    @unittest.skipIf(os.name == "nt", "process-group cleanup is POSIX-specific")
    def test_runner_sigterm_reaps_an_owned_server_process_group(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-interrupt-") as temporary:
            root = Path(temporary)
            pid_file = root / "child.pid"
            pid_temp_file = root / "child.pid.tmp"
            # The runner blocks TERM before it publishes the PID file and takes
            # TERM with sigwait, then runs the installed harness handler. A
            # handler with signal.pause() loses a TERM that arrives after the
            # last eval-loop check and before pause(2) starts. TERM is blocked
            # only after the child starts, so the child does not inherit the
            # mask, and is unblocked before the handler re-raises TERM.
            runner_code = f"""
import signal, subprocess, sys
from pathlib import Path
sys.path.insert(0, {str(Path(http_backend_harness.__file__).parent)!r})
import http_backend_harness as harness
child = subprocess.Popen(
    [sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True,
)
harness._register_owned_process(child)
signal.pthread_sigmask(signal.SIG_BLOCK, {{signal.SIGTERM}})
pid_temp = Path({str(pid_temp_file)!r})
pid_temp.write_text(str(child.pid), encoding='ascii')
pid_temp.replace({str(pid_file)!r})
signal.sigwait({{signal.SIGTERM}})
signal.pthread_sigmask(signal.SIG_UNBLOCK, {{signal.SIGTERM}})
signal.getsignal(signal.SIGTERM)(signal.SIGTERM, None)
"""
            runner = subprocess.Popen(
                [sys.executable, "-c", runner_code],
                cwd=root,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            try:
                # The runner start and its stop only need hang guards.
                deadline = time.monotonic() + HANG_GUARD_SECONDS
                while not pid_file.exists() and runner.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(pid_file.exists(), "runner started its owned server process")
                child_pid = int(pid_file.read_text(encoding="ascii"))
                runner.send_signal(signal.SIGTERM)
                self.assertEqual(runner.wait(timeout=HANG_GUARD_SECONDS), -signal.SIGTERM)
                with self.assertRaises(ProcessLookupError):
                    os.kill(child_pid, 0)
            finally:
                if runner.poll() is None:
                    # Let the registered owner reap its separate child group even
                    # when the readiness assertion fails.
                    runner.send_signal(signal.SIGTERM)
                    try:
                        runner.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(runner.pid, signal.SIGKILL)
                        runner.wait(timeout=5)
                if runner.stderr is not None:
                    runner.stderr.close()

    @unittest.skipIf(os.name == 'nt', 'POSIX process groups')
    def test_group_permission_error_requires_exited_child_and_closed_pipes(self) -> None:
        with tempfile.TemporaryDirectory(prefix='http-backend-group-') as temporary:
            backend = http_backend_harness.BackendProcess(
                Path(temporary), claude_bin=None, initialize_workspace=False,
            )
            process = mock.Mock(pid=12345)
            backend._stdout_reader_done = mock.Mock()
            backend._stderr_reader_done = mock.Mock()
            try:
                with mock.patch.object(http_backend_harness.os, 'killpg',
                                       side_effect=PermissionError):
                    process.poll.return_value = None
                    with self.assertRaises(PermissionError):
                        backend._signal_group(process, signal.SIGTERM)
                    backend._stdout_reader_done.wait.assert_not_called()
                    process.poll.return_value = -9
                    backend._stdout_reader_done.wait.return_value = True
                    backend._stderr_reader_done.wait.return_value = False
                    with self.assertRaises(PermissionError):
                        backend._signal_group(process, signal.SIGKILL)
                    backend._stderr_reader_done.wait.return_value = True
                    backend._signal_group(process, signal.SIGKILL)
            finally:
                backend.close()

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

    def test_environment_only_harness_does_not_seed_schema_cache(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-seed-") as temporary, mock.patch.object(
            test_isolation, "seed_api_schema_cache_for_fixture",
        ) as seed:
            backend = http_backend_harness.BackendProcess(
                Path(temporary), claude_bin=None, initialize_workspace=False,
            )
            try:
                seed.assert_not_called()
            finally:
                backend.close()

    def test_python_start_seeds_schema_cache_before_spawning(self) -> None:
        with tempfile.TemporaryDirectory(prefix="http-backend-seed-") as temporary:
            scratch = Path(temporary)
            backend = http_backend_harness.BackendProcess(
                scratch, claude_bin=None, initialize_workspace=False,
            )
            codex = scratch / "codex"
            codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            codex.chmod(0o700)
            backend._provider_executable = codex
            order: list[str] = []

            def fixture_which(command: str, path: str | None = None) -> str | None:
                if command == "codex":
                    return str(codex)
                return None

            def record_seed(cache_home: Path) -> str:
                order.append("seed")
                return ""

            def record_spawn(*args: object, **kwargs: object) -> None:
                order.append("spawn")
                raise RuntimeError("spawn reached")

            try:
                with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(
                    test_isolation, "seed_api_schema_cache_for_fixture", side_effect=record_seed,
                ) as seed, mock.patch.object(
                    http_backend_harness.shutil, "which", side_effect=fixture_which,
                ), mock.patch.object(
                    http_backend_harness.subprocess, "Popen", side_effect=record_spawn,
                ), self.assertRaisesRegex(RuntimeError, "spawn reached"):
                    backend.start()
                seed.assert_called_once_with(backend.xdg_cache)
                self.assertEqual(order, ["seed", "spawn"])
            finally:
                backend.close()


if __name__ == "__main__":
    unittest.main()
