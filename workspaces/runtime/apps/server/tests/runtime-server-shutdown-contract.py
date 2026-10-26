"""Exercise SIGTERM with a retained provider session on the Rust backend."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import unittest
import uuid

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS

from runtime_test_environment import make_runtime
from http_backend_harness import BackendProcess

HERE = Path(__file__).resolve().parent
_fixture_spec = importlib.util.spec_from_file_location(
    "runtime_shutdown_fixture", HERE / "runtime-contract.py"
)
assert _fixture_spec is not None and _fixture_spec.loader is not None
_fixture = importlib.util.module_from_spec(_fixture_spec)
_fixture_spec.loader.exec_module(_fixture)


SERVER_SHUTDOWN_TEST_EXIT_GUARD_SECONDS = 25


def process_is_running(pid: int) -> bool:
    """Treat a reaped or exited zombie as stopped; reject a live fixture child."""
    status = Path(f"/proc/{pid}/status")
    try:
        for line in status.read_text(encoding="ascii").splitlines():
            if line.startswith("State:"):
                return not line.split()[1].startswith("Z")
    except FileNotFoundError:
        return False
    return True


def stop_owned_provider_group(pid: int, backend) -> None:
    """Cleanup only this test's fake provider after checking its proc identity."""
    if not process_is_running(pid):
        return
    process_root = Path(f"/proc/{pid}")
    try:
        cwd = Path(os.readlink(process_root / "cwd")).resolve()
        command_line = (process_root / "cmdline").read_bytes()
        stat_fields = (process_root / "stat").read_text(encoding="ascii").split()
    except FileNotFoundError:
        return
    if (
        cwd != backend.cwd.resolve()
        or str(Path(__file__).with_name("provider-replay-server.py")).encode() not in command_line
        or int(stat_fields[4]) != pid
    ):
        raise AssertionError(f"refusing to signal non-fixture process {pid}")
    os.killpg(pid, signal.SIGKILL)


@unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "requires the Rust backend")
class RuntimeServerShutdownContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = Path(os.environ["TMPDIR"])
        cls.backend = BackendProcess(scratch)

        def close_backend() -> None:
            pid_log = cls.backend._provider_pid_log
            if pid_log is not None and pid_log.exists():
                for pid in (int(line) for line in pid_log.read_text().splitlines()):
                    stop_owned_provider_group(pid, cls.backend)
            cls.backend.close()

        cls.addClassCleanup(close_backend)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        (cls.backend.root / "provider-release").unlink()
        subprocess.run(
            [
                "git",
                "-c", "user.name=HTTP fixture",
                "-c", "user.email=http-fixture@example.invalid",
                "-c", "core.hooksPath=/dev/null",
                "commit", "--allow-empty", "-m", "shutdown fixture",
            ],
            cwd=cls.backend.cwd,
            env={
                "HOME": str(cls.backend.home),
                "PATH": os.defpath,
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
            },
            check=True,
            # The timeout only guards against a hang of the fixture commit.
            timeout=HANG_GUARD_SECONDS,
        )
        cls.backend._command()
        cls.backend._bootstrap_python_state(cls.backend._environment())

        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        cls.agent_id = str(uuid.uuid4())
        runtime = make_runtime(QuietRuntime, cls.backend, _fixture.FakeServer)
        try:
            runtime.create(
                {
                    "id": cls.agent_id,
                    "cwd": str(cls.backend.cwd),
                    "prompt": "Hold the fake provider session until shutdown.",
                    "name": "Shutdown worker fixture",
                },
                defer=False,
            )
        finally:
            runtime.close()
        cls.backend.start()
        cls.backend.wait_until(
            lambda: (cls.backend.root / "provider-phase").exists(),
            "fake provider active-turn gate",
        )

    @classmethod
    def tearDownClass(cls) -> None:
        backend = getattr(cls, "backend", None)
        if backend is None:
            return
        pids = []
        if backend._provider_pid_log is not None and backend._provider_pid_log.exists():
            pids = [int(line) for line in backend._provider_pid_log.read_text().splitlines()]
        for pid in pids:
            stop_owned_provider_group(pid, backend)

    def test_sigterm_exits_with_an_active_worker_and_reaps_provider_child(self) -> None:
        process = self.backend._process
        self.assertIsNotNone(process)
        assert process is not None
        pid_log = self.backend._provider_pid_log
        self.assertIsNotNone(pid_log)
        assert pid_log is not None
        provider_pids = [int(line) for line in pid_log.read_text(encoding="ascii").splitlines()]
        self.assertTrue(provider_pids, "the account worker launched the fake provider")
        self.assertTrue((self.backend.root / "provider-phase").exists())

        process.send_signal(signal.SIGTERM)
        process.wait(timeout=SERVER_SHUTDOWN_TEST_EXIT_GUARD_SECONDS)
        self.assertEqual(process.returncode, 0, self.backend.stderr)

        self.assertFalse(
            [pid for pid in provider_pids if process_is_running(pid)],
            "server shutdown must terminate and reap the active fake provider",
        )

        self.backend.stop()
        self.backend.start()
        restarted = self.backend._process
        self.assertIsNotNone(restarted)
        assert restarted is not None and restarted.stdin is not None
        restarted.stdin.close()
        restarted.wait(timeout=SERVER_SHUTDOWN_TEST_EXIT_GUARD_SECONDS)
        self.assertEqual(restarted.returncode, 0, self.backend.stderr)
        self.assertIn("shutdown requested because stdin closed", self.backend.stderr)


if __name__ == "__main__":
    unittest.main()
