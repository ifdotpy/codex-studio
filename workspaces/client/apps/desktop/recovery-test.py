import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import call, patch
import fcntl

HERE = Path(__file__).resolve().parent
REPOSITORY_ROOT = HERE.parents[3]
SERVER_SOURCE = REPOSITORY_ROOT / "workspaces/runtime/apps/server/src"
spec = importlib.util.spec_from_file_location("recovery", HERE / "recover_backend.py")
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


def free_port():
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        return server.getsockname()[1]


def wait_for(check, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError):
            pass
        time.sleep(.1)
    raise AssertionError("Recovery did not reach its expected state.")


class RecoveryTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-supervisor-test-", dir="/tmp")
        self.state = Path(self.temp.name).resolve()
        source_link = self.state / "workspaces/runtime/apps/server/src"
        source_link.parent.mkdir(parents=True)
        source_link.symlink_to(SERVER_SOURCE, target_is_directory=True)
        renderer = self.state / "workspaces/client/apps/web/dist"
        renderer.mkdir(parents=True)
        (renderer / "index.html").write_text("fixture")
        self.config = {"version": 1, "enabled": True, "stateDir": str(self.state),
                       "port": free_port(), "python": sys.executable,
                       "codex": "/usr/bin/true", "resources": str(self.state),
                       "environment": {"CODEX_CANVAS_CWD": str(self.state / "workspace")}}
        self.child = None
        self.supervisors = []
        self.recovered_pid = None

    def tearDown(self):
        for process in self.supervisors:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        if self.child is not None and self.child.poll() is None:
            self.child.terminate()
            self.child.wait(timeout=10)
        if self.recovered_pid is not None:
            try:
                os.kill(self.recovered_pid, signal.SIGTERM)
                wait_for(lambda: recovery.identity(self.config["port"], self.state) is None)
                # The listener can close before this owned process finishes cleanup.
                wait_for(lambda: recovery.process_start_time(self.recovered_pid) is None)
            except ProcessLookupError:
                pass
        self.temp.cleanup()

    def test_free_port_with_occupied_runtime_never_starts(self):
        with (self.state / "runtime.lock").open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(recovery.subprocess, "Popen") as spawn:
                child, status = recovery.tick(self.config, self.state)
            self.assertIsNone(child)
            self.assertEqual(status, "waiting for runtime owner")
            spawn.assert_not_called()

    def test_uncertain_or_incompatible_identity_never_starts(self):
        for error in (TimeoutError("timeout"), ValueError("invalid body"), RuntimeError("wrong state")):
            with patch.object(recovery, "identity", side_effect=error), patch.object(recovery.subprocess, "Popen") as spawn:
                with self.assertRaises(type(error)):
                    recovery.tick(self.config, self.state)
                spawn.assert_not_called()

    def test_supervisor_fallback_never_signals_pid_claimed_by_unrelated_listener(self):
        foreign_pid = os.getpid()
        body = json.dumps({"application": "codex-agents", "protocol": 1,
                           "stateDir": str(self.state), "pid": foreign_pid,
                           "supervisorMode": True}).encode()
        with patch.object(recovery.urllib.request, "build_opener") as build_opener, \
                patch.object(recovery.os, "kill") as kill:
            build_opener.return_value.open.return_value = io.BytesIO(body)
            with self.assertRaisesRegex(RuntimeError, "not the packaged Codex Canvas"):
                recovery.tick(self.config, self.state, supervisor_fallback=True)
            self.assertNotIn(call(foreign_pid, signal.SIGTERM), kill.call_args_list)

    def test_packaged_backend_path_with_spaces_uses_exact_arguments(self):
        resources = self.state / "Codex Studio.app" / "Contents" / "Resources" / "workspace"
        (resources / "scripts").mkdir(parents=True)
        (resources / "scripts/codex-canvas").write_text("fixture")
        (resources / "studio-install.json").write_text('{"mode":"combined"}')
        config = {**self.config, "resources": str(resources)}
        script = str((resources / "scripts/codex-canvas").resolve())
        pid = 12345
        arguments = [sys.executable, "-B", script, "--port", str(config["port"])]
        with patch.object(recovery, "backend_process_arguments", return_value=arguments) as read, \
                patch.object(recovery.subprocess, "check_output", return_value=str(pid)) as output:
            recovery.verify_backend_process({"pid": pid}, config)
        read.assert_called_once_with(pid, str(resources))
        self.assertEqual(output.call_count, 1)
        self.assertEqual(output.call_args.args[0][0], "/usr/sbin/lsof")

    def test_split_display_path_cannot_claim_the_packaged_backend(self):
        resources = self.state / "Codex Studio.app" / "Contents" / "Resources" / "workspace"
        (resources / "scripts").mkdir(parents=True)
        (resources / "scripts/codex-canvas").write_text("fixture")
        (resources / "studio-install.json").write_text('{"mode":"combined"}')
        config = {**self.config, "resources": str(resources)}
        script = str((resources / "scripts/codex-canvas").resolve())
        with patch.object(recovery, "backend_process_arguments", return_value=[sys.executable, *script.split()]), \
                patch.object(recovery.subprocess, "check_output") as output:
            with self.assertRaisesRegex(RuntimeError, "not the packaged Codex Canvas"):
                recovery.verify_backend_process({"pid": 12345}, config)
        output.assert_not_called()

    def test_matching_arguments_do_not_replace_the_listener_pid_guard(self):
        script = str((recovery.backend_scripts(self.config["resources"]) / "codex-canvas").resolve())
        with patch.object(recovery, "backend_process_arguments", return_value=[sys.executable, script]), \
                patch.object(recovery.subprocess, "check_output", return_value="54321"):
            with self.assertRaisesRegex(RuntimeError, "does not own the configured listener"):
                recovery.verify_backend_process({"pid": 12345}, self.config)

    def test_process_argument_read_failure_never_signals_the_reported_pid(self):
        reported = {"pid": 12345, "supervisorMode": True}
        for error in (OSError("unavailable"), ValueError("invalid native data"),
                      ImportError("missing reader"), RuntimeError("unsupported platform")):
            with self.subTest(error=type(error).__name__), \
                    patch.object(recovery, "identity", return_value=reported), \
                    patch.object(recovery, "backend_process_arguments", side_effect=error), \
                    patch.object(recovery.subprocess, "check_output") as output, \
                    patch.object(recovery.os, "kill") as kill:
                with self.assertRaisesRegex(RuntimeError, "Cannot verify fallback backend PID"):
                    recovery.tick(self.config, self.state, supervisor_fallback=True)
                output.assert_not_called()
                kill.assert_not_called()

    @unittest.skipUnless(sys.platform == "darwin" or sys.platform.startswith("linux"), "native process reader")
    def test_native_argument_reader_preserves_spaces(self):
        argument = str(self.state / "Codex Studio.app" / "scripts" / "codex-canvas")
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)", argument])
        try:
            arguments = recovery.backend_process_arguments(process.pid, self.config["resources"])
            self.assertEqual(arguments[-1], argument)
            self.assertIn("import time; time.sleep(30)", arguments)
        finally:
            process.terminate()
            process.wait(timeout=5)

    def test_supervisor_probe_failure_never_starts_a_replacement(self):
        config = {
            "supervisorEnabled": True,
            "python": sys.executable,
            "resources": str(self.state),
            "codex": "/usr/bin/true",
        }
        probe_error = subprocess.TimeoutExpired(["status"], 2)
        with patch.object(recovery.subprocess, "run", side_effect=probe_error), \
                patch.object(recovery.subprocess, "Popen") as spawn:
            child, status = recovery.supervisor_tick(config, self.state)
        self.assertIsNone(child)
        self.assertEqual(status, "supervisor starting")
        spawn.assert_not_called()

    def test_same_owner_survives_supervisor_restart_and_crash_keeps_data(self):
        self.config["supervisorEnabled"] = True
        supervisor_log = self.state / "supervisor-owner.log"
        with supervisor_log.open("w") as output:
            owner = subprocess.Popen(
                [sys.executable, "-B", str(SERVER_SOURCE / "codex_process_supervisor.py"),
                 "--state", str(self.state)],
                stdout=output,
                stderr=output,
            )
        self.supervisors.append(owner)
        wait_for(
            lambda: recovery.supervisor_tick(self.config, self.state)[1]
            == "supervisor ready"
        )
        self.child, status = recovery.tick(self.config, self.state)
        self.assertEqual(status, "started")
        first = wait_for(lambda: recovery.identity(self.config["port"], self.state))
        self.assertEqual(first["pid"], self.child.pid)
        with sqlite3.connect(self.state / "canvas.sqlite3") as db:
            db.execute("CREATE TABLE recovery_test_receipt (id TEXT PRIMARY KEY, value TEXT)")
            db.execute("INSERT INTO recovery_test_receipt VALUES (?, ?)", ("accepted-once", "retained"))
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps(self.config))
        for _ in range(2):
            log = self.state / f"supervisor-{len(self.supervisors)}.log"
            with log.open("w") as output:
                process = subprocess.Popen([sys.executable, "-B", str(HERE / "recover_backend.py"), "--config", str(filename)], stdout=output, stderr=output)
            self.supervisors.append(process)
            wait_for(lambda: 'attached' in log.read_text())
            self.assertEqual(recovery.identity(self.config["port"], self.state)["pid"], first["pid"])
            process.kill()
            process.wait(timeout=10)
            self.assertIsNone(self.child.poll())
        with (self.state / "supervisor-active.log").open("w") as output:
            process = subprocess.Popen([sys.executable, "-B", str(HERE / "recover_backend.py"), "--config", str(filename)], stdout=output, stderr=output)
        self.supervisors.append(process)
        wait_for(lambda: 'attached' in (self.state / "supervisor-active.log").read_text())
        self.child.kill()
        self.child.wait(timeout=10)
        second = wait_for(lambda: recovery.identity(self.config["port"], self.state))
        self.recovered_pid = second["pid"]
        self.assertNotEqual(second["pid"], first["pid"])
        with sqlite3.connect(self.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT * FROM recovery_test_receipt").fetchall(), [("accepted-once", "retained")])

    def test_desktop_restarts_only_after_exact_saved_process_ends(self):
        filename = self.state / "desktop-recovery.json"
        intent = {"version": 1, "desiredOpen": True, "pid": os.getpid(),
                  "startedAt": subprocess.check_output(["/bin/ps", "-p", str(os.getpid()), "-o", "lstart="], text=True, env={**os.environ, "LC_ALL": "C"}).strip(),
                  "executable": "/usr/bin/true", "profile": str(self.state / "profile")}
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "check_output", return_value=intent["startedAt"]), patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.desktop_tick(self.config, self.state)[1], "desktop attached")
            spawn.assert_not_called()
        dead = subprocess.Popen(["/usr/bin/true"])
        dead.wait(timeout=5)
        intent["pid"] = dead.pid
        filename.write_text(json.dumps(intent))
        child, status = recovery.desktop_tick(self.config, self.state)
        self.assertEqual(status, "desktop started")
        self.assertEqual(child.wait(timeout=5), 0)
        intent["desiredOpen"] = False
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.desktop_tick(self.config, self.state)[1], "desktop closed")
            spawn.assert_not_called()

    def test_desktop_unknown_process_and_changed_intent_do_not_restart(self):
        filename = self.state / "desktop-recovery.json"
        intent = {"version": 1, "desiredOpen": True, "pid": os.getpid(),
                  "startedAt": "unknown", "executable": "/usr/bin/true", "profile": str(self.state)}
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "check_output", return_value=""), patch.object(recovery.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "identity is unavailable"):
                recovery.desktop_tick(self.config, self.state)
            spawn.assert_not_called()

    def test_authoritative_absent_environment_stays_absent(self):
        with patch.dict(os.environ, {"CODEX_HOME": "/foreign-account"}):
            env = recovery.launch_environment({**self.config, "unsetEnvironment": ["CODEX_HOME"]}, self.state)
            self.assertNotIn("CODEX_HOME", env)
            self.assertEqual(env["CODEX_CANVAS_CWD"], str(self.state / "workspace"))

    def test_configuration_cannot_switch_database(self):
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps({**self.config, "stateDir": str(self.state / "other")}))
        with self.assertRaisesRegex(RuntimeError, "different state"):
            recovery.load_config(filename, self.state)


class RustSelectionTest(unittest.TestCase):
    """The explicit Rust selection uses fake executables in a private state directory."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-rust-recovery-", dir="/tmp")
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name).resolve()
        source_link = self.state / "workspaces/runtime/apps/server/src"
        source_link.parent.mkdir(parents=True)
        source_link.symlink_to(SERVER_SOURCE, target_is_directory=True)
        self.server = self.fake_executable("studio-runtime-server", (
            '#!/bin/sh\n'
            'printf "%s\\n" "$@" > "$CODEX_AGENTS_STATE_DIR/rust-arguments"\n'
            'env > "$CODEX_AGENTS_STATE_DIR/rust-environment"\n'))
        self.supervisor = self.fake_executable("codex-process-supervisor", "#!/bin/sh\nexit 0\n")
        self.config = {"version": 1, "enabled": True, "stateDir": str(self.state),
                       "port": free_port(), "python": sys.executable,
                       "codex": "/usr/bin/true", "resources": str(self.state),
                       "backend": "rust", "runtimeServer": str(self.server),
                       "processSupervisor": str(self.supervisor), "environment": {}}

    def fake_executable(self, name, content, mode=0o755):
        path = self.state / "bin" / name
        path.parent.mkdir(exist_ok=True)
        path.write_text(content)
        path.chmod(mode)
        return path

    def test_rust_selection_starts_the_configured_server_with_the_runtime_contract(self):
        for supervisor_enabled in (False, True):
            with self.subTest(supervisor_enabled=supervisor_enabled):
                config = {**self.config, "supervisorEnabled": supervisor_enabled}
                child, status = recovery.tick(config, self.state)
                self.assertEqual(status, "started")
                self.assertEqual(child.wait(timeout=10), 0)
                arguments = (self.state / "rust-arguments").read_text().split()
                self.assertEqual(arguments, ["--port", str(config["port"])])
                environment = dict(line.split("=", 1) for line in
                                   (self.state / "rust-environment").read_text().splitlines() if "=" in line)
                self.assertEqual(environment["CODEX_AGENTS_STATE_DIR"], str(self.state))
                self.assertEqual(environment["CODEX_BIN"], "/usr/bin/true")
                self.assertEqual(environment.get("CODEX_AGENTS_SUPERVISOR_MODE"),
                                 "1" if supervisor_enabled else None)
                self.assertEqual((self.state / "canvas.log").stat().st_mode & 0o077, 0)

    def test_rust_selection_refuses_a_missing_or_non_executable_server(self):
        plain = self.fake_executable("not-executable", "#!/bin/sh\n", mode=0o644)
        for server in (self.state / "bin/missing", plain, self.state / "bin"):
            with self.subTest(server=server.name), patch.object(recovery.subprocess, "Popen") as spawn:
                with self.assertRaisesRegex(RuntimeError, "missing or not executable"):
                    recovery.tick({**self.config, "runtimeServer": str(server)}, self.state)
                spawn.assert_not_called()

    def test_rust_selection_never_starts_while_the_runtime_lease_is_held(self):
        with (self.state / "runtime.lock").open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(recovery.subprocess, "Popen") as spawn:
                self.assertEqual(recovery.tick(self.config, self.state), (None, "waiting for runtime owner"))
            spawn.assert_not_called()

    def test_an_occupied_state_is_attached_and_never_replaced(self):
        python_backend = {"pid": os.getpid(), "stateDir": str(self.state)}
        rust_backend = {**python_backend, "backendKind": "rust"}
        for existing, config, status in (
            (python_backend, self.config, "attached to python backend; rust selected"),
            (rust_backend, self.config, "attached"),
            (rust_backend, {**self.config, "backend": "python"}, "attached to rust backend; python selected"),
            (python_backend, {**self.config, "backend": "python"}, "attached"),
        ):
            with self.subTest(status=status, kind=existing.get("backendKind")), \
                    patch.object(recovery, "identity", return_value=existing), \
                    patch.object(recovery.subprocess, "Popen") as spawn, patch.object(recovery.os, "kill") as kill:
                self.assertEqual(recovery.tick(config, self.state), (None, status))
                spawn.assert_not_called()
                kill.assert_not_called()

    def test_saved_selection_is_validated_and_python_stays_the_default(self):
        default = {key: value for key, value in self.config.items()
                   if key not in ("backend", "runtimeServer", "processSupervisor")}
        config = recovery.load_config(self.write(default), self.state)
        self.assertEqual(recovery.backend_selection(config), "python")
        self.assertEqual(recovery.backend_selection(recovery.load_config(self.write(self.config), self.state)), "rust")
        for changes, message in (
            ({"backend": "go"}, "python or rust"),
            ({"backend": 2}, "python or rust"),
            ({"runtimeServer": None}, "runtimeServer path must be absolute"),
            ({"runtimeServer": "bin/studio-runtime-server"}, "runtimeServer path must be absolute"),
            ({"supervisorEnabled": True, "processSupervisor": None}, "processSupervisor path must be absolute"),
            ({"supervisorEnabled": True, "processSupervisor": "codex-process-supervisor"},
             "processSupervisor path must be absolute"),
        ):
            with self.subTest(changes=changes):
                saved = {**self.config, **changes}
                saved = {key: value for key, value in saved.items() if value is not None}
                with self.assertRaisesRegex(RuntimeError, message):
                    recovery.load_config(self.write(saved), self.state)

    def write(self, config):
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps(config))
        return filename

    def test_python_default_launches_the_packaged_backend(self):
        renderer = self.state / "workspaces/client/apps/web/dist"
        renderer.mkdir(parents=True)
        (renderer / "index.html").write_text("fixture")
        config = {key: value for key, value in self.config.items() if key != "backend"}
        with patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.tick(config, self.state)[1], "started")
        command = spawn.call_args.args[0]
        self.assertEqual(command[:2], [sys.executable, "-B"])
        self.assertEqual(Path(command[2]).name, "codex-canvas")
        self.assertNotIn(str(self.server), command)

    def rust_identity(self, **changes):
        return {"pid": 12345, "backendKind": "rust", "supervisorMode": True,
                "backendBuild": hashlib.sha256(self.server.read_bytes()).hexdigest(), **changes}

    def test_fallback_signal_requires_the_configured_rust_executable(self):
        with patch.object(recovery, "backend_process_arguments", return_value=[str(self.server), "--port", "1"]), \
                patch.object(recovery.subprocess, "check_output", return_value="12345\n") as listeners:
            recovery.verify_backend_process(self.rust_identity(), self.config)
        self.assertEqual(listeners.call_args.args[0][0], "/usr/sbin/lsof")
        other = self.fake_executable("other-server", "#!/bin/sh\n")
        for data, arguments in (
            (self.rust_identity(backendBuild="0" * 64), [str(self.server)]),
            (self.rust_identity(), [str(other)]),
            (self.rust_identity(), []),
            (self.rust_identity(backendKind=None), [str(self.server)]),
        ):
            with self.subTest(data=data, arguments=arguments), \
                    patch.object(recovery, "identity", return_value=data), \
                    patch.object(recovery, "backend_process_arguments", return_value=arguments), \
                    patch.object(recovery.subprocess, "check_output") as listeners, \
                    patch.object(recovery.os, "kill") as kill:
                with self.assertRaisesRegex(RuntimeError, "not the configured Rust server"):
                    recovery.tick(self.config, self.state, supervisor_fallback=True)
                listeners.assert_not_called()
                kill.assert_not_called()

    def serve_supervisor(self, reply):
        """A one-connection socket that records the handshake and sends one reply."""
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(self.state / "supervisor.sock"))
        listener.listen(1)
        received = []

        def answer():
            connection, _ = listener.accept()
            with connection:
                received.append(json.loads(connection.makefile("r").readline()))
                connection.sendall((json.dumps(reply) + "\n").encode())

        thread = threading.Thread(target=answer, daemon=True)
        thread.start()
        self.addCleanup(listener.close)
        return received, thread

    def test_supervisor_probe_requires_the_rust_supervisor_protocol(self):
        config = {**self.config, "supervisorEnabled": True}
        received, thread = self.serve_supervisor({"ok": True, "protocol": recovery.SUPERVISOR_PROTOCOL_V2})
        health = subprocess.CompletedProcess([], 0, json.dumps({"protocol": 1, "recovery": {}}), "")
        with patch.object(recovery.subprocess, "run", return_value=health) as probe, \
                patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.supervisor_tick(config, self.state), (None, "supervisor ready"))
        thread.join(5)
        self.assertEqual(received, [{"protocol": 2, "stateDir": str(self.state), "probe": True}])
        self.assertIn("--status-json", probe.call_args.args[0])
        spawn.assert_not_called()

    def test_a_python_supervisor_blocks_the_rust_backend(self):
        config = {**self.config, "supervisorEnabled": True}
        received, thread = self.serve_supervisor({"error": "Supervisor protocol or state directory mismatch"})
        with patch.object(recovery.subprocess, "run") as probe:
            with self.assertRaisesRegex(RuntimeError, "does not serve protocol 2"):
                recovery.supervisor_tick(config, self.state)
        thread.join(5)
        probe.assert_not_called()

    def test_absent_rust_supervisor_waits_and_a_missing_binary_is_an_error(self):
        config = {**self.config, "supervisorEnabled": True}
        with patch.object(recovery.subprocess, "run") as probe:
            self.assertEqual(recovery.supervisor_tick(config, self.state), (None, "supervisor starting"))
            with self.assertRaisesRegex(RuntimeError, "Rust process supervisor is missing"):
                recovery.supervisor_tick({**config, "processSupervisor": str(self.state / "bin/missing")}, self.state)
        probe.assert_not_called()

    def test_supervisor_protocol_matches_its_rust_owner(self):
        owner = (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-process/src/protocol.rs").read_text()
        self.assertIn(f"pub const SUPERVISOR_PROTOCOL_V2: u64 = {recovery.SUPERVISOR_PROTOCOL_V2};", owner)


if __name__ == "__main__":
    unittest.main()
