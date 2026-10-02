"""Verify Python loads the shared environment scrub before contract startup."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest


class TestStartupIsolation(unittest.TestCase):
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
            cwd=Path(__file__).resolve().parents[1],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(json.loads(proc.stdout), {name: None for name in names})

    def test_terminals_contract_does_not_connect_to_decoy_supervisor(self):
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


if __name__ == "__main__":
    unittest.main()
