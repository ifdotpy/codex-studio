import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import socket
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
spec = importlib.util.spec_from_file_location("recovery", HERE / "recover_backend.py")
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


def free_port():
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        return server.getsockname()[1]


class RecoveryTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-supervisor-test-")
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name).resolve()
        renderer = self.state / "workspaces/client/apps/web/dist"
        renderer.mkdir(parents=True)
        (renderer / "index.html").write_text("fixture")
        self.config = {"version": 1, "enabled": True, "stateDir": str(self.state),
                       "port": free_port(), "python": sys.executable,
                       "codex": "/usr/bin/true", "resources": str(self.state),
                       "runtimeServer": "/usr/bin/true", "backend": "rust",
                       "processSupervisor": "/usr/bin/true", "environment": {"CODEX_CANVAS_CWD": str(self.state / "workspace")}}
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
                           "supervisorMode": True, "backendKind": "rust"}).encode()
        with patch.object(recovery.urllib.request, "build_opener") as build_opener, \
                patch.object(recovery.os, "kill") as kill:
            build_opener.return_value.open.return_value = io.BytesIO(body)
            with self.assertRaisesRegex(RuntimeError, "not the configured Rust server"):
                recovery.tick(self.config, self.state, supervisor_fallback=True)
            self.assertNotIn(call(foreign_pid, signal.SIGTERM), kill.call_args_list)

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
        argument = str(self.state / "Codex Studio.app" / "scripts" / "studio-runtime-server")
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)", argument])
        try:
            arguments = recovery.backend_process_arguments(process.pid, self.config["resources"])
            self.assertEqual(arguments[-1], argument)
            self.assertIn("import time; time.sleep(30)", arguments)
        finally:
            process.terminate()
            process.wait(timeout=5)

    def test_identity_refuses_the_removed_python_backend(self):
        body = json.dumps({"application": "codex-agents", "protocol": 1,
                           "stateDir": str(self.state), "pid": os.getpid(),
                           "backendKind": "python"}).encode()
        with patch.object(recovery.urllib.request, "build_opener") as opener, patch.object(recovery.subprocess, "Popen") as spawn:
            opener.return_value.open.return_value = io.BytesIO(body)
            with self.assertRaisesRegex(RuntimeError, "identity does not match"):
                recovery.tick(self.config, self.state)
            spawn.assert_not_called()

    def test_supervisor_probe_failure_never_starts_a_replacement(self):
        config = {**self.config, "supervisorEnabled": True}
        with patch.object(recovery, "supervisor_request", side_effect=TimeoutError("probe")), \
                patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.supervisor_tick(config, self.state), (None, "supervisor starting"))
        spawn.assert_not_called()

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
        self.temp = tempfile.TemporaryDirectory(prefix="studio-rust-recovery-")
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name).resolve()
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
            (python_backend, self.config, "attached"),
            (rust_backend, self.config, "attached"),
        ):
            with self.subTest(status=status, kind=existing.get("backendKind")), \
                    patch.object(recovery, "identity", return_value=existing), \
                    patch.object(recovery.subprocess, "Popen") as spawn, patch.object(recovery.os, "kill") as kill:
                self.assertEqual(recovery.tick(config, self.state), (None, status))
                spawn.assert_not_called()
                kill.assert_not_called()

    def test_saved_selection_is_validated_and_rust_is_the_default(self):
        default = {key: value for key, value in self.config.items() if key != "backend"}
        self.assertEqual(recovery.load_config(self.write(default), self.state)["runtimeServer"], str(self.server))
        for changes, message in (
            ({"backend": "python"}, "backend must be rust"),
            ({"backend": 2}, "backend must be rust"),
            ({"runtimeServer": None}, "runtimeServer path must be absolute"),
            ({"runtimeServer": "bin/studio-runtime-server"}, "runtimeServer path must be absolute"),
            ({"supervisorEnabled": True, "processSupervisor": None}, "processSupervisor path must be absolute"),
        ):
            with self.subTest(changes=changes):
                saved = {**self.config, **changes}
                with self.assertRaisesRegex(RuntimeError, message):
                    recovery.load_config(self.write(saved), self.state)

    def write(self, config):
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps(config))
        return filename

    def rust_identity(self, **changes):
        return {"pid": 12345, "backendKind": "rust", "supervisorMode": True,
                "backendBuild": hashlib.sha256(self.server.read_bytes()).hexdigest(), **changes}

    def test_matching_arguments_do_not_replace_the_listener_pid_guard(self):
        with patch.object(recovery, "backend_process_arguments", return_value=[str(self.server)]), \
                patch.object(recovery.subprocess, "check_output", return_value="54321"):
            with self.assertRaisesRegex(RuntimeError, "does not own the configured listener"):
                recovery.verify_backend_process(self.rust_identity(), self.config)

    def test_server_path_with_spaces_requires_exact_native_arguments(self):
        server = self.fake_executable("runtime server", "#!/bin/sh\n")
        config = {**self.config, "runtimeServer": str(server)}
        data = self.rust_identity(backendBuild=hashlib.sha256(server.read_bytes()).hexdigest())
        with patch.object(recovery, "backend_process_arguments", return_value=[str(server)]), \
                patch.object(recovery.subprocess, "check_output", return_value="12345"):
            recovery.verify_backend_process(data, config)
        with patch.object(recovery, "backend_process_arguments", return_value=str(server).split()), \
                patch.object(recovery.subprocess, "check_output") as listener:
            with self.assertRaisesRegex(RuntimeError, "not the configured Rust server"):
                recovery.verify_backend_process(data, config)
        listener.assert_not_called()

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

    def serve_supervisor(self, hello, reply=None):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(self.state / "supervisor.sock"))
        listener.listen(1)
        received = []

        def answer():
            connection, _ = listener.accept()
            with connection, connection.makefile("rwb") as stream:
                received.append(json.loads(stream.readline()))
                stream.write((json.dumps(hello) + "\n").encode())
                stream.flush()
                if reply is not None:
                    received.append(json.loads(stream.readline()))
                    stream.write((json.dumps(reply) + "\n").encode())
                    stream.flush()

        thread = threading.Thread(target=answer, daemon=True)
        thread.start()
        self.addCleanup(listener.close)
        self.addCleanup(lambda: (self.state / "supervisor.sock").unlink(missing_ok=True))
        return received, thread

    def test_supervisor_probe_requires_the_rust_supervisor_protocol(self):
        config = {**self.config, "supervisorEnabled": True}
        received, thread = self.serve_supervisor(
            {"ok": True, "protocol": recovery.SUPERVISOR_PROTOCOL_V2},
            {"requestId": 1, "result": {"recovery": {}}})
        with patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.supervisor_tick(config, self.state), (None, "supervisor ready"))
        thread.join(5)
        self.assertEqual(received, [
            {"protocol": 2, "stateDir": str(self.state), "probe": True},
            {"requestId": 1, "action": "health"}])
        spawn.assert_not_called()

    def test_supervisor_recovery_state_is_preserved(self):
        for health, expected in (
            ({"degraded": True, "fallbackReady": True}, "supervisor degraded"),
            ({"degraded": True, "fallbackReady": False}, "supervisor ready"),
        ):
            with self.subTest(health=health), patch.object(recovery, "supervisor_request", return_value={"recovery": health}):
                self.assertEqual(recovery.supervisor_tick({**self.config, "supervisorEnabled": True}, self.state), (None, expected))
        with patch.object(recovery, "supervisor_request", return_value={"recovery": {"blocked": "uncertain owner"}}):
            with self.assertRaisesRegex(RuntimeError, "uncertain owner"):
                recovery.supervisor_tick({**self.config, "supervisorEnabled": True}, self.state)

    def test_supervisor_reply_errors_and_wrong_request_ids_fail_closed(self):
        for reply in ({"requestId": 2, "result": {}}, {"requestId": 1, "error": "uncertain owner"}):
            received, thread = self.serve_supervisor({"ok": True, "protocol": 2}, reply)
            with self.assertRaises(RuntimeError):
                recovery.supervisor_request(self.state, recovery.SupervisorAction.HEALTH)
            thread.join(5)
            (self.state / "supervisor.sock").unlink()

    def test_finish_fallback_uses_the_same_rust_protocol(self):
        received, thread = self.serve_supervisor(
            {"ok": True, "protocol": recovery.SUPERVISOR_PROTOCOL_V2},
            {"requestId": 1, "result": {"finished": True}})
        self.assertEqual(recovery.supervisor_request(self.state, recovery.SupervisorAction.FINISH_FALLBACK),
                         {"finished": True})
        thread.join(5)
        self.assertEqual(received[-1], {"requestId": 1, "action": "finishFallback"})

    def test_a_python_supervisor_blocks_the_rust_backend(self):
        config = {**self.config, "supervisorEnabled": True}
        received, thread = self.serve_supervisor({"error": "Supervisor protocol or state directory mismatch"})
        with self.assertRaisesRegex(RuntimeError, "does not serve protocol 2"):
            recovery.supervisor_tick(config, self.state)
        thread.join(5)
        self.assertEqual(len(received), 1)

    def test_absent_rust_supervisor_waits_and_a_missing_binary_is_an_error(self):
        config = {**self.config, "supervisorEnabled": True}
        self.assertEqual(recovery.supervisor_tick(config, self.state), (None, "supervisor starting"))
        with self.assertRaisesRegex(RuntimeError, "Rust process supervisor is missing"):
            recovery.supervisor_tick({**config, "processSupervisor": str(self.state / "bin/missing")}, self.state)

    def test_supervisor_protocol_matches_its_rust_owner(self):
        owner = (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-process/src/protocol.rs").read_text()
        self.assertIn(f"pub const SUPERVISOR_PROTOCOL_V2: u64 = {recovery.SUPERVISOR_PROTOCOL_V2};", owner)
        frame_owner = (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-process/src/supervisor_contract.rs").read_text()
        self.assertIn(f"pub const MAX_SUPERVISOR_FRAME_BYTES: usize = {recovery.MAX_SUPERVISOR_FRAME_BYTES};", frame_owner)


if __name__ == "__main__":
    unittest.main()
