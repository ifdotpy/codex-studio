#!/usr/bin/env python3
"""Windows server entrypoint, backend restart, and native worktree contracts."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_worktree_creation import create_worker_worktree

WINDOWS = os.name == "nt"
skip_posix = unittest.skipUnless(WINDOWS, "Windows server contract")
if WINDOWS:
    tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
    Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)
    os.environ.setdefault("PYTHONIOENCODING", "utf-8")


def _wait_until(check, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.25)
    raise AssertionError("The Windows server did not reach the expected state")


def _base_python() -> tuple[str, dict[str, str]]:
    executable = getattr(sys, "_base_executable", sys.executable) if WINDOWS else sys.executable
    environment = os.environ.copy()
    if WINDOWS:
        site_packages = [path for path in sys.path if "site-packages" in path.casefold()]
        environment["PYTHONPATH"] = os.pathsep.join(
            [str(ROOT / "scripts"), *site_packages, environment.get("PYTHONPATH", "")]
        )
    return executable, environment


def _port_owner(port):
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
         f"(Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue "
         "| Select-Object -First 1).OwningProcess"],
        capture_output=True, text=True, timeout=15,
    )
    if result.returncode:
        raise AssertionError(result.stderr)
    try:
        return int(result.stdout.strip())
    except ValueError:
        return None


def _process_commandline(pid):
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
         f"(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').CommandLine"],
        capture_output=True, text=True, timeout=15,
    )
    if result.returncode:
        raise AssertionError(result.stderr)
    return result.stdout.strip()


def _stop_entrypoint_tree(process):
    command = _process_commandline(process.pid)
    if "codex_windows_server.py" not in command:
        raise AssertionError("Refusing to stop a process with an unexpected command line")
    subprocess.run(["taskkill.exe", "/PID", str(process.pid), "/T", "/F"],
                   capture_output=True, text=True, timeout=15, check=True)
    process.wait(timeout=15)


def _new_port_owner(port, previous_pid):
    pid = _port_owner(port)
    return pid if pid is not None and pid != previous_pid else None


@skip_posix
class WindowsServerContract(unittest.TestCase):
    def test_entrypoint_uses_configured_state_port_and_origin(self):
        with tempfile.TemporaryDirectory(prefix="studio server Ω ") as temporary:
            state = Path(temporary) / "state with spaces Ω"
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts" / "codex_windows_server.py"),
                 "--source-root", str(ROOT), "--state", str(state), "--port", "4630",
                 "--public-origin", "https://kukuka-win.tailf00fa0.ts.net:8443", "--check-config"],
                capture_output=True, text=True, timeout=15,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {
                "sourceRoot": str(ROOT.resolve()), "stateDir": str(state.resolve()),
                "port": 4630, "publicOrigin": "https://kukuka-win.tailf00fa0.ts.net:8443",
            })

    def test_supervisor_pipe_starts_from_the_entrypoint_process(self):
        from codex_process_supervisor import status

        with tempfile.TemporaryDirectory(prefix="studio pipe Ω ") as temporary:
            state = Path(temporary) / "state Ω"
            executable, environment = _base_python()
            environment["CODEX_AGENTS_STATE_DIR"] = str(state)
            log_path = Path(temporary) / "supervisor.log"
            with log_path.open("ab") as output:
                process = subprocess.Popen(
                    [executable, "-B", str(ROOT / "scripts" / "codex_process_supervisor.py"),
                     "--state", str(state)],
                    cwd=ROOT, env=environment, stdin=subprocess.DEVNULL, stdout=output,
                    stderr=subprocess.STDOUT, close_fds=True,
                )
                try:
                    def healthy():
                        try:
                            return status(state)
                        except (OSError, RuntimeError, ValueError):
                            return None

                    result = _wait_until(healthy)
                    self.assertEqual(result["stateDir"], str(state.resolve()))
                except AssertionError as error:
                    output.flush()
                    log_path.seek(0)
                    detail = log_path.read_text(encoding="utf-8", errors="replace")
                    raise AssertionError(f"{error}; supervisor log: {detail}") from error
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)

    def test_backend_restart_keeps_supervisor_process(self):
        with tempfile.TemporaryDirectory(prefix="studio restart Ω ") as temporary:
            root = Path(temporary)
            state = root / "state with spaces Ω"
            profile = root / "Codex profile Ω"
            profile.mkdir()
            port = 4631
            if _port_owner(port):
                self.skipTest("TCP port 4631 is already in use")
            log = (root / "entrypoint.log").open("ab")
            executable, environment = _base_python()
            environment.update({"CODEX_HOME": str(profile), "CODEX_AGENTS_STATE_DIR": str(state),
                                "CODEX_AGENTS_SUPERVISOR_MODE": "1"})
            process = subprocess.Popen(
                [executable, str(ROOT / "scripts" / "codex_windows_server.py"),
                 "--source-root", str(ROOT), "--state", str(state), "--port", str(port),
                 "--public-origin", "https://kukuka-win.tailf00fa0.ts.net:8443"],
                cwd=ROOT, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            )
            self.addCleanup(log.close)
            try:
                first_pid = _wait_until(lambda: _port_owner(port))
                self.assertIn("codex_windows_backend.py", _process_commandline(first_pid))
                lease = json.loads((state / "supervisor.lock").read_text(encoding="utf-8"))
                os.kill(first_pid, signal.SIGTERM)
                second_pid = _wait_until(lambda: _new_port_owner(port, first_pid))
                next_lease = json.loads((state / "supervisor.lock").read_text(encoding="utf-8"))
                self.assertNotEqual(first_pid, second_pid)
                self.assertEqual(next_lease, lease)
                from codex_process_supervisor import status
                self.assertEqual(status(state)["stateDir"], str(state.resolve()))
            finally:
                if process.poll() is None:
                    _stop_entrypoint_tree(process)

    def test_worktree_handles_spaces_unicode_and_fixture_commit(self):
        with tempfile.TemporaryDirectory(prefix="studio git Ω ") as temporary:
            root = Path(temporary) / "repo with spaces Ω"
            root.mkdir()
            subprocess.run(["git", "init", "-b", "main", str(root)], check=True,
                           capture_output=True, timeout=20)
            subprocess.run(["git", "-C", str(root), "config", "user.name", "Fixture Worker"], check=True)
            subprocess.run(["git", "-C", str(root), "config", "user.email", "fixture@example.test"], check=True)
            project_name = "Project with spaces"
            (root / project_name).mkdir()
            (root / project_name / "seed.txt").write_text("seed\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", project_name], check=True)
            subprocess.run(["git", "-C", str(root), "commit", "-m", "seed"], check=True,
                           capture_output=True, timeout=20)
            branch = "codex-agent/fixture-windows"
            worktree = root / ".worktrees" / "codex-agents" / "worker Ω fixture"
            project = worktree / project_name
            created = create_worker_worktree(root, worktree, project, branch)
            self.assertFalse(created)
            output = project / "fixture result Ω.txt"
            output.write_text("fixture provider result\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(worktree), "add", "--", str(output)], check=True)
            subprocess.run(["git", "-C", str(worktree), "commit", "-m", "fixture worker result"],
                           check=True, capture_output=True, timeout=20)
            saved = subprocess.check_output(["git", "-C", str(worktree), "rev-parse", "HEAD"], text=True).strip()
            self.assertTrue(saved)
            subprocess.run(["git", "-C", str(root), "worktree", "remove", "--force", str(worktree)],
                           check=True, capture_output=True, timeout=30)
            subprocess.run(["git", "-C", str(root), "branch", "-D", branch], check=True,
                           capture_output=True, timeout=20)


if __name__ == "__main__":
    unittest.main()
