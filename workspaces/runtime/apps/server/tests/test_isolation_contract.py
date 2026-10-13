"""Verify Python loads the shared environment scrub before contract startup."""

from test_isolation import isolate_supervisor_environment

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT

isolate_supervisor_environment()

import ast
import json
import os
import socket
import subprocess
import tempfile
import threading
import unittest


class TestStartupIsolation(unittest.TestCase):
    def test_shared_fixture_replaces_inherited_workspace_store(self):
        env = os.environ.copy()
        env["CODEX_WORKSPACE_STORE"] = "/default/workspace/store"
        env.pop("CODEX_AGENTS_TEST_WORKSPACE_STORE", None)
        helper = Path(__file__).resolve().with_name("test_isolation.py")
        code = (
            "import json, os, runpy; runpy.run_path(" + repr(str(helper)) + "); "
            "print(json.dumps(os.environ['CODEX_WORKSPACE_STORE']))"
        )
        proc = subprocess.run(
            [sys.executable, "-B", "-c", code],
            cwd=REPOSITORY_ROOT,
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        workspace_store = json.loads(proc.stdout)
        self.assertNotEqual(workspace_store, "/default/workspace/store")
        self.assertEqual(Path(workspace_store).parent, Path(tempfile.gettempdir()))
        self.assertTrue(Path(workspace_store).name.startswith("studio-test-workspaces-"))

    def test_every_python_contract_imports_the_isolation_helper(self):
        tests = Path(__file__).resolve().parent
        missing = []
        for path in tests.glob("*contract.py"):
            tree = ast.parse(path.read_text(), filename=str(path))
            if not any(
                isinstance(node, ast.ImportFrom)
                and node.module == "test_isolation"
                and any(alias.name == "isolate_supervisor_environment" for alias in node.names)
                for node in ast.walk(tree)
            ):
                missing.append(path.name)
        self.assertEqual(missing, [], f"Python contract scripts missing test isolation: {missing}")

    def test_shared_fixture_removes_inherited_supervisor_routing(self):
        names = (
            "CODEX_AGENTS_SUPERVISOR_MODE",
            "CODEX_AGENTS_STATE_DIR",
            "CODEX_AGENTS_SUPERVISOR_FALLBACK",
        )
        env = os.environ.copy()
        env.update({name: "decoy" for name in names})
        helper = Path(__file__).resolve().with_name("test_isolation.py")
        code = (
            "import json, os, runpy; runpy.run_path(" + repr(str(helper)) + "); "
            "print(json.dumps({k: os.environ.get(k) for k in " + repr(names) + "}))"
        )
        proc = subprocess.run(
            [sys.executable, "-B", "-c", code],
            cwd=REPOSITORY_ROOT,
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(json.loads(proc.stdout), {name: None for name in names})

    def test_terminals_contract_does_not_connect_to_decoy_supervisor(self):
        if not os.environ.get("CODEX_BIN"):
            self.skipTest("requires a runner-resolved Codex executable; none is available")
        tests = Path(__file__).resolve().parent
        with tempfile.TemporaryDirectory(prefix="studio-decoy-supervisor-") as root:
            decoy = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            decoy.bind(str(Path(root) / "supervisor.sock"))
            decoy.listen(1)
            decoy.settimeout(0.1)
            connected = []
            stop = threading.Event()

            def observe_connections():
                while not stop.is_set():
                    try:
                        client, _ = decoy.accept()
                    except socket.timeout:
                        continue
                    connected.append(True)
                    client.close()

            observer = threading.Thread(target=observe_connections, daemon=True)
            observer.start()
            env = os.environ.copy()
            env.update(
                {
                    "CODEX_AGENTS_SUPERVISOR_MODE": "1",
                    "CODEX_AGENTS_STATE_DIR": root,
                    "CODEX_AGENTS_SUPERVISOR_FALLBACK": "1",
                }
            )
            try:
                proc = subprocess.run(
                    [
                        sys.executable,
                        "-B",
                        str(tests / "terminals-contract.py"),
                        "-k",
                        "test_real_pty_cwd_output_resize_exit_and_no_model",
                    ],
                    cwd=tests.parent,
                    env=env,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=90,
                )
            finally:
                stop.set()
                observer.join(timeout=1)
                decoy.close()

        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(connected, [], "a test component connected to the decoy supervisor")


class RuntimeEnvironmentSafety(unittest.TestCase):
    def test_guard_blocks_file_metadata_and_execution_with_an_injected_home(self):
        code = r'''import json, os, socket, subprocess, sys, tempfile
from pathlib import Path
from runtime_home_guard import HomeAccessGuard
if True:
    home = Path(sys.argv[1])
    protected = home / ".codex"
    protected.mkdir()
    program = protected / "codex"
    program.write_text("#!/bin/sh\nexit 93\n")
    program.chmod(0o755)
    (protected / "auth.json").write_text("marker")
    (home / ".claude").mkdir()
    (home / ".claude" / "config.json").write_text("marker")
    alias = home / "alias"
    alias.symlink_to(protected, target_is_directory=True)
    guard = HomeAccessGuard(home_resolver=lambda: str(home)).install()
    operations = [
        lambda: open(protected / "auth.json"),
        lambda: open(home / ".claude" / "config.json"),
        lambda: os.listdir(protected),
        lambda: os.scandir(protected),
        lambda: os.stat(protected / "auth.json"),
        lambda: os.lstat(protected / "auth.json"),
        lambda: os.access(protected / "auth.json", os.R_OK),
        lambda: open(alias / "auth.json"),
        lambda: subprocess.run([str(program)], check=True),
        lambda: os.execv(str(program), [str(program)]),
        lambda: socket.create_connection(("192.0.2.1", 443)),
    ]
    if hasattr(os, "posix_spawn"):
        operations.append(lambda: os.posix_spawn(str(program), [str(program)], dict(os.environ)))
    for operation in operations:
        try:
            operation()
        except PermissionError:
            pass
        else:
            raise AssertionError("protected operation was permitted")
    # Legitimate reads and subprocesses still work outside the protected trees.
    allowed = home / "allowed"
    allowed.write_text("safe")
    assert allowed.read_text() == "safe"
    subprocess.run([sys.executable, "-c", "pass"], check=True)
    tools = home / ".cache" / "hostedtoolcache"
    tools.mkdir(parents=True)
    python = tools / "python"
    python.symlink_to(sys.executable)
    subprocess.run([str(python), "-c", "import pathlib"], check=True)
    guard.check_path(home / ".local" / "lib" / "python3.14" / "site-packages")
    print(json.dumps(guard.counters), flush=True)
    # Audit hooks cannot be removed: let the OS reclaim this test subprocess's
    # fixture; the parent TemporaryDirectory owns the surrounding scratch area.
    os._exit(0)
'''
        with tempfile.TemporaryDirectory() as folder:
            result = subprocess.run(
                [sys.executable, "-B", "-c", code, folder],
                env={**os.environ, "PYTHONPATH": str(Path(__file__).parent)},
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertGreaterEqual(json.loads(result.stdout)["blocked"], 9)

    def test_runtime_disables_update_requests_and_restores_the_loopback_opener(self):
        from http.server import BaseHTTPRequestHandler, HTTPServer
        from unittest.mock import patch
        import urllib.request
        import codex_provider_upstream_releases as releases
        from runtime_test_environment import make_runtime

        class RuntimeFixture:
            def __init__(self, root):
                self.root = root

            def close(self):
                pass

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"loopback")

            def log_message(self, *_args):
                pass

        with tempfile.TemporaryDirectory() as folder, HTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                url = f"http://127.0.0.1:{server.server_port}/"
                with patch.dict(os.environ, {}, clear=True), patch.object(urllib.request, "_opener", None), \
                        patch.object(releases, "_registry_request", side_effect=AssertionError("network update attempted")) as request:
                    runtime = make_runtime(RuntimeFixture, Path(folder))
                    try:
                        self.assertFalse(releases.updates_enabled(runtime))
                        self.assertEqual(releases.tick(runtime), {})
                        self.assertEqual(os.environ["HTTPS_PROXY"], "http://127.0.0.1:9")
                        with urllib.request.urlopen(url) as response:
                            self.assertEqual(response.read(), b"loopback")
                        self.assertIsNotNone(urllib.request._opener)
                    finally:
                        runtime.close()
                    self.assertIsNone(urllib.request._opener)
                    with urllib.request.urlopen(url) as response:
                        self.assertEqual(response.read(), b"loopback")
                    request.assert_not_called()
            finally:
                server.shutdown()
                thread.join()

    def test_multiple_runtime_lifetimes_never_restore_inherited_environment_early(self):
        from unittest.mock import patch
        from runtime_test_environment import make_runtime

        class Fixture:
            def __init__(self, root):
                self.home = os.environ["CODEX_HOME"]

            def close(self):
                pass

        class FailedFixture:
            def __init__(self, root):
                raise ValueError("fixture startup failure")

        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"CODEX_UNKNOWN_SELECTOR": "inherited"}):
            original = dict(os.environ)
            first = make_runtime(Fixture, Path(folder) / "first")
            second = make_runtime(Fixture, Path(folder) / "second")
            try:
                first.close()
                self.assertEqual(os.environ["CODEX_HOME"], second.home)
                self.assertNotIn("CODEX_UNKNOWN_SELECTOR", os.environ)
                with self.assertRaisesRegex(ValueError, "fixture startup failure"):
                    make_runtime(FailedFixture, Path(folder) / "failed")
                self.assertEqual(os.environ["CODEX_HOME"], second.home)
            finally:
                second.close()
                first.close()
            self.assertEqual(dict(os.environ), original)

    def test_ci_tool_paths_survive_without_inheriting_the_callers_path(self):
        from unittest.mock import patch
        import shutil
        from codex_runtime import Runtime
        from runtime_test_environment import make_runtime

        node = os.environ.get("CODEX_NODE") or shutil.which("node")
        self.assertIsNotNone(node, "Node is required for Runtime crypto")
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            tools = root / "hostedtoolcache" / "node" / "bin"
            tools.mkdir(parents=True)
            (tools / "node").symlink_to(node)
            for explicit in (False, True):
                with self.subTest(explicit_node=explicit):
                    inherited = {
                        "PATH": str(tools), "HOME": str(root / "runner"),
                        "CODEX_AGENTS_PYTHON": sys.executable,
                        "LD_LIBRARY_PATH": str(root / "hostedtoolcache" / "python" / "lib"),
                        "LC_CTYPE": "C.UTF-8", "UNRECOGNIZED_SETTING": "hostile",
                    }
                    if explicit:
                        inherited.update(CODEX_NODE=str(tools / "node"), PATH="/missing")
                    with patch.dict(os.environ, inherited, clear=True):
                        runtime = make_runtime(Runtime, root / str(explicit))
                        try:
                            self.assertTrue(runtime.paired_access().local_server_id)
                            self.assertEqual(os.environ["CODEX_NODE"], str(tools / "node"))
                            self.assertEqual(os.environ["CODEX_AGENTS_PYTHON"], sys.executable)
                            self.assertEqual(os.environ["LD_LIBRARY_PATH"], inherited["LD_LIBRARY_PATH"])
                            self.assertEqual(os.environ["LC_CTYPE"], inherited["LC_CTYPE"])
                            self.assertNotIn(str(tools), os.environ["PATH"].split(os.pathsep))
                            self.assertNotIn("UNRECOGNIZED_SETTING", os.environ)
                        finally:
                            runtime.close()

    def test_hostile_provider_home_and_path_are_never_read_or_executed(self):
        import shutil
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            decoy = root / "decoy"
            decoy.mkdir()
            auth = decoy / "auth.json"
            auth.write_text('{"marker": "must remain unread"}')
            program = decoy / "codex"
            marker = root / "executed"
            program.write_text("#!/bin/sh\ntouch " + str(marker) + "\n")
            program.chmod(0o755)
            baseline = 1_000_000_000_000_000_000
            for path in (auth, program):
                os.utime(path, ns=(baseline, baseline))
            env = {**os.environ,
                   "CODEX_HOME": str(decoy), "PATH": str(decoy),
                   "CODEX_NODE": os.environ.get("CODEX_NODE") or shutil.which("node"),
                   "STUDIO_TEST_FORBIDDEN_ROOTS": str(decoy),
                   "STUDIO_TEST_GUARD_REPORT": str(root / "guard.jsonl"),
                   "PYTHONPATH": str(Path(__file__).resolve().parents[1] / "src")}
            code = """from pathlib import Path
import sys
from runtime_test_environment import make_runtime
from codex_runtime import Runtime
runtime = make_runtime(Runtime, Path(sys.argv[1]))
try:
    assert runtime.paired_access().local_server_id
finally:
    runtime.close()
"""
            result = subprocess.run(
                [sys.executable, "-B", "-c", code, str(root / "state")],
                env=env, capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(marker.exists())
            self.assertEqual([path.stat().st_atime_ns for path in (auth, program)],
                             [baseline, baseline])
            reports = [json.loads(line) for line in (root / "guard.jsonl").read_text().splitlines()]
            self.assertTrue(reports)
            self.assertEqual(sum(report["blocked"] for report in reports), 0)

    def test_runtime_environment_strips_inherited_provider_settings(self):
        from unittest.mock import patch
        from runtime_test_environment import runtime_environment
        from http_backend_harness import BackendProcess
        with tempfile.TemporaryDirectory() as folder:
            backend = BackendProcess(Path(folder))
            try:
                backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
                hostile = {
                    "CODEX_UNKNOWN_SELECTOR": "hostile", "CLAUDE_UNKNOWN_SELECTOR": "hostile",
                    "CODEX_BIN": "/decoy/codex", "STUDIO_CLAUDE_BIN": "/decoy/claude",
                    "OPENAI_API_KEY": "hostile", "ANTHROPIC_AUTH_TOKEN": "hostile",
                    "AWS_SECRET_ACCESS_KEY": "hostile", "PATH": "/decoy",
                }
                with patch.dict(os.environ, hostile):
                    environment = runtime_environment(backend)
                self.assertFalse(
                    set(hostile) - {"CODEX_BIN", "STUDIO_CLAUDE_BIN", "PATH"} & set(environment)
                )
                self.assertEqual(environment["CODEX_BIN"], str(backend.root / "bin/codex"))
                self.assertEqual(environment["STUDIO_CLAUDE_BIN"], backend._claude_bin)
                self.assertEqual(environment["CODEX_AGENTS_NATIVE_RUNTIME_PIN"], environment["CODEX_BIN"])
                self.assertEqual(environment["PATH"].split(os.pathsep),
                                 [str(backend.root / "bin"), *os.defpath.split(os.pathsep)])
                for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "XDG_CONFIG_HOME",
                            "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
                    self.assertTrue(Path(environment[key]).is_relative_to(Path(folder)))
            finally:
                backend.close()


if __name__ == "__main__":
    unittest.main()
