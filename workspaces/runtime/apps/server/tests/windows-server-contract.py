#!/usr/bin/env python3
"""Windows server entrypoint, backend restart, and native worktree contracts."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_worktree_creation import create_worker_worktree
from codex_windows_server import _read_control_request, _write_control_request
from codex_windows_server import _advertise_control_protocol, _recover_control_claims, _write_control_result

WINDOWS = os.name == "nt"
skip_posix = unittest.skipUnless(WINDOWS, "Windows server contract")
if WINDOWS:
    tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
    Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)
    os.environ.setdefault("PYTHONIOENCODING", "utf-8")


class ControlRequestPosixContract(unittest.TestCase):
    def setUp(self):
        protocol = patch("codex_windows_server._runner_control_protocol", return_value=2)
        protocol.start()
        self.addCleanup(protocol.stop)

    def test_sequence_lock_covers_request_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            reached_publish = threading.Event()
            allow_publish = threading.Event()
            original_replace = os.replace
            published = []
            errors = []

            def replace(source, destination):
                if Path(destination).name.startswith("windows-server-control-request-"):
                    if not reached_publish.is_set():
                        reached_publish.set()
                        if not allow_publish.wait(3):
                            raise TimeoutError("publication barrier timed out")
                    published.append(Path(destination).name)
                original_replace(source, destination)

            def submit(action):
                try:
                    _write_control_request(state, action)
                except BaseException as error:
                    errors.append(error)

            with patch("codex_windows_server.os.replace", side_effect=replace):
                first = threading.Thread(target=submit, args=("stop-backend",))
                second = threading.Thread(target=submit, args=("restart-backend",))
                first.start()
                self.assertTrue(reached_publish.wait(2), "first request did not reach publication")
                second.start()
                second.join(.1)
                self.assertTrue(second.is_alive(), "second request passed the sequence lock")
                allow_publish.set()
                first.join(2)
                second.join(2)
            self.assertFalse(first.is_alive())
            self.assertFalse(second.is_alive())
            self.assertEqual(errors, [])
            requests = [_read_control_request(state, None)]
            requests.append(_read_control_request(state, requests[0]["requestId"]))
            self.assertEqual([int(row["sequence"]) for row in requests], [1, 2])
            self.assertEqual([row["action"] for row in requests], ["stop-backend", "restart-backend"])

    def test_manager_wait_exceeds_backend_stop_deadline(self):
        script = (SERVER_SOURCE_ROOT / "manage-windows-server.ps1").read_text(encoding="utf-8")
        self.assertIn("AddSeconds(90)", script)

    def test_claim_remains_until_receipt_and_unreceipted_claim_recovers_as_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            request_id = _write_control_request(state, "stop-backend")
            request = _read_control_request(state, None)
            self.assertEqual(request["requestId"], request_id)
            claim = Path(request["claimPath"])
            self.assertTrue(claim.exists())
            _write_control_result(state, request_id, "backend-stopped")
            claim.unlink()

            second_id = _write_control_request(state, "restart-backend")
            second = _read_control_request(state, request_id)
            _recover_control_claims(state)
            self.assertFalse(Path(second["claimPath"]).exists())
            receipt = json.loads((state / f"windows-server-control-{second_id}.json").read_text())
            self.assertEqual(receipt["result"], "unknown")
            self.assertIn("not replayed", receipt["detail"])

    def test_requests_are_claimed_in_monotonic_sequence_order(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            first_id = _write_control_request(state, "stop-backend")
            second_id = _write_control_request(state, "restart-backend")
            stop_id, restart_id = "f" * 32, "0" * 32
            for old_id, new_id in ((first_id, stop_id), (second_id, restart_id)):
                request_path = state / f"windows-server-control-request-{old_id}.json"
                payload = json.loads(request_path.read_text())
                payload["requestId"] = new_id
                request_path.unlink()
                (state / f"windows-server-control-request-{new_id}.json").write_text(json.dumps(payload))
            first = _read_control_request(state, None)
            second = _read_control_request(state, first["requestId"])
            self.assertEqual([first["requestId"], second["requestId"]], [stop_id, restart_id])


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
            [str(SERVER_SOURCE_ROOT), *site_packages, environment.get("PYTHONPATH", "")]
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
    def test_control_requests_have_distinct_files_and_receipts(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            _advertise_control_protocol(state)
            first = _write_control_request(state, "stop-backend")
            second = _write_control_request(state, "restart-backend")
            self.assertNotEqual(first, second)
            self.assertTrue((state / f"windows-server-control-request-{first}.json").is_file())
            self.assertTrue((state / f"windows-server-control-request-{second}.json").is_file())
            request_one = _read_control_request(state, None)
            request_two = _read_control_request(state, request_one["requestId"] if request_one else None)
            requests = [request_one, request_two]
            self.assertEqual({row["requestId"] for row in requests if row}, {first, second})

    def test_config_replace_keeps_old_file_when_target_is_locked(self):
        script = (SERVER_SOURCE_ROOT / "manage-windows-server.ps1").read_text(encoding="utf-8")
        self.assertIn("[System.IO.File]::Replace", script)
        self.assertNotIn("Move-Item -Force", script)
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "server.json"
            replacement = Path(temporary) / "server.json.tmp"
            destination.write_text("old", encoding="utf-8")
            replacement.write_text("new", encoding="utf-8")
            command = (
                "$p='" + str(destination).replace("'", "''") + "'; "
                "$t='" + str(replacement).replace("'", "''") + "'; "
                "$s=[IO.File]::Open($p,'Open','ReadWrite','None'); "
                "try { [IO.File]::Replace($t,$p,[NullString]::Value,$true) } catch {}; $s.Dispose()"
            )
            subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                           check=True, capture_output=True, text=True, timeout=20)
            self.assertEqual(destination.read_text(encoding="utf-8"), "old")

    def test_entrypoint_uses_configured_state_port_and_origin(self):
        with tempfile.TemporaryDirectory(prefix="studio server Ω ") as temporary:
            state = Path(temporary) / "state with spaces Ω"
            result = subprocess.run(
                [sys.executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
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
                    [executable, "-B", str(SERVER_SOURCE_ROOT / "codex_process_supervisor.py"),
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

    def test_multi_server_identity_uses_owner_only_windows_acl(self):
        from codex_multi_server import MultiServerService
        from codex_multi_server_crypto import CryptoProcess

        with tempfile.TemporaryDirectory(prefix="studio identity Ω ") as temporary:
            state = Path(temporary) / "state with spaces Ω"
            state.mkdir()
            service = object.__new__(MultiServerService)
            service.lock = threading.RLock()
            service._identity = None
            service.runtime = SimpleNamespace(root=state)
            service.crypto = CryptoProcess()
            try:
                service._keys()
            finally:
                service.crypto.shutdown()

            identity = subprocess.run(["whoami.exe", "/user", "/fo", "csv", "/nh"],
                                      capture_output=True, text=True, check=True).stdout
            owner = identity.split(",", 1)[0].strip().strip('"').casefold()
            for path in (state / "multi-server", state / "multi-server" / "identity.json"):
                output = subprocess.run(["icacls.exe", str(path)], capture_output=True,
                                        text=True, check=True, timeout=10).stdout.casefold()
                self.assertNotIn("everyone", output)
                self.assertNotIn("\\builtin\\users", output)
                principals = [line.strip().casefold() for line in output.splitlines()
                              if ":(" in line]
                self.assertEqual(len(principals), 1, output)
                self.assertIn(owner, principals[0], output)

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
            node = shutil.which("node")
            self.assertIsNotNone(node, "The backend requires Node.js for its Ed25519 service")
            fixture_path = os.pathsep.join([
                str(Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32"),
                str(Path(node).parent),
            ])
            environment.update({"CODEX_HOME": str(profile), "CODEX_AGENTS_STATE_DIR": str(state),
                                "CODEX_AGENTS_SUPERVISOR_MODE": "1",
                                "CODEX_BIN": "",
                                "PATH": fixture_path})
            process = subprocess.Popen(
                [executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
                 "--source-root", str(ROOT), "--state", str(state), "--port", str(port),
                 "--public-origin", "https://kukuka-win.tailf00fa0.ts.net:8443"],
                cwd=ROOT, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            )
            try:
                try:
                    first_pid = _wait_until(lambda: _port_owner(port))
                except AssertionError as error:
                    log.flush()
                    details = (root / "entrypoint.log").read_text(encoding="utf-8", errors="replace")
                    backend_log = state / "backend.log"
                    if backend_log.exists():
                        details += backend_log.read_text(encoding="utf-8", errors="replace")
                    raise AssertionError(f"{error}; server logs: {details}") from error
                self.assertIn("codex_windows_backend.py", _process_commandline(first_pid))
                lease = json.loads((state / "supervisor.lock").read_text(encoding="utf-8"))
                stop = subprocess.run(
                    [executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
                     "--state", str(state), "--request-action", "stop-backend"],
                    cwd=ROOT, env=environment, capture_output=True, text=True, timeout=15,
                )
                self.assertEqual(stop.returncode, 0, stop.stderr)
                stop_id = json.loads(stop.stdout)["requestId"]
                stopped_path = state / f"windows-server-control-{stop_id}.json"
                _wait_until(lambda: stopped_path.exists())
                self.assertEqual(json.loads(stopped_path.read_text(encoding="utf-8"))["result"],
                                 "backend-stopped")
                _wait_until(lambda: _port_owner(port) is None)
                exit_events = []
                for line in (state / "backend.log").read_text(encoding="utf-8", errors="replace").splitlines():
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if event.get("event") == "backend_exit":
                        exit_events.append(event)
                self.assertTrue(exit_events, "The entrypoint did not record backend exit")
                self.assertEqual(exit_events[-1]["returnCode"], 0,
                                 "The backend did not exit through its graceful signal handler")
                from codex_process_supervisor import status
                self.assertEqual(status(state)["stateDir"], str(state.resolve()))
                request = subprocess.run(
                    [executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
                     "--state", str(state), "--request-action", "restart-backend"],
                    cwd=ROOT, env=environment, capture_output=True, text=True, timeout=15,
                )
                self.assertEqual(request.returncode, 0, request.stderr)
                request_id = json.loads(request.stdout)["requestId"]
                second_pid = _wait_until(lambda: _new_port_owner(port, first_pid))
                result_path = state / f"windows-server-control-{request_id}.json"
                _wait_until(lambda: result_path.exists())
                self.assertEqual(json.loads(result_path.read_text(encoding="utf-8"))["result"],
                                 "backend-restarted")
                next_lease = json.loads((state / "supervisor.lock").read_text(encoding="utf-8"))
                self.assertNotEqual(first_pid, second_pid)
                self.assertEqual(next_lease, lease)
                self.assertEqual(status(state)["stateDir"], str(state.resolve()))
            finally:
                if process.poll() is None:
                    _stop_entrypoint_tree(process)
                log.close()

    def test_stop_all_control_action_stops_the_server_tree(self):
        with tempfile.TemporaryDirectory(prefix="studio stop Ω ") as temporary:
            root = Path(temporary)
            state = root / "state"
            port = 4632
            if _port_owner(port):
                self.skipTest("TCP port 4632 is already in use")
            log = (root / "entrypoint.log").open("ab")
            executable, environment = _base_python()
            environment["CODEX_AGENTS_STATE_DIR"] = str(state)
            process = subprocess.Popen(
                [executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
                 "--source-root", str(ROOT), "--state", str(state), "--port", str(port),
                 "--public-origin", "https://kukuka-win.tailf00fa0.ts.net:8443"],
                cwd=ROOT, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            )
            try:
                _wait_until(lambda: _port_owner(port))
                request = subprocess.run(
                    [executable, str(SERVER_SOURCE_ROOT / "codex_windows_server.py"),
                     "--state", str(state), "--request-action", "stop-all"],
                    cwd=ROOT, env=environment, capture_output=True, text=True, timeout=15,
                )
                self.assertEqual(request.returncode, 0, request.stderr)
                request_id = json.loads(request.stdout)["requestId"]
                self.assertEqual(process.wait(timeout=45), 0)
                result_path = state / f"windows-server-control-{request_id}.json"
                self.assertEqual(json.loads(result_path.read_text(encoding="utf-8"))["result"],
                                 "stopped-all")
                self.assertIsNone(_port_owner(port))
                from codex_process_supervisor import process_start_time

                lease = json.loads((state / "supervisor.lock").read_text(encoding="utf-8"))
                self.assertIsNone(process_start_time(lease["pid"]))
            finally:
                if process.poll() is None:
                    _stop_entrypoint_tree(process)
                log.close()

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
            saved = subprocess.check_output(["git", "-C", str(worktree), "rev-parse", "HEAD"],
                                            text=True).strip()
            self.assertTrue(saved)
            subprocess.run(["git", "-C", str(root), "worktree", "remove", "--force", str(worktree)],
                           check=True, capture_output=True, timeout=30)
            subprocess.run(["git", "-C", str(root), "branch", "-D", branch], check=True,
                           capture_output=True, timeout=20)

    def test_checkout_timeout_terminates_descendants_that_hold_output_pipes(self):
        from codex_worktree_creation import _run_checkout

        with tempfile.TemporaryDirectory(prefix="studio checkout timeout Ω ") as temporary:
            child_pid = Path(temporary) / "child.pid"
            child = "import time; time.sleep(60)"
            parent = (
                "import pathlib,subprocess,sys,time; "
                f"p=subprocess.Popen([sys.executable,'-c',{child!r}]); "
                f"pathlib.Path({str(child_pid)!r}).write_text(str(p.pid)); time.sleep(60)"
            )
            started = time.monotonic()
            with self.assertRaises(subprocess.TimeoutExpired):
                _run_checkout([sys.executable, "-c", parent], timeout=.5,
                              check=False, capture_output=True, text=True)
            self.assertLess(time.monotonic() - started, 4)
            pid = int(child_pid.read_text(encoding="ascii"))
            self.assertFalse(_process_commandline(pid))


if __name__ == "__main__":
    unittest.main()
