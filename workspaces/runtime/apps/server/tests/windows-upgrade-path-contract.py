#!/usr/bin/env python3
"""Upgrade transport and receipt contracts through the installed manager."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

import hashlib
import importlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_windows_server as server
from codex_live_updates import LiveUpdates
from codex_source_inventory import source_files


def wait_until(check, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.1)
    raise AssertionError("The fixture did not become ready")


def runner_fixture(state, port, protocol, source):
    backend = None
    previous = None
    try:
        if protocol == 2:
            server._advertise_control_protocol(state)
        backend = subprocess.Popen([sys.executable, "-B", __file__, "--backend-fixture", str(source), str(port)])
        while not (state / "fixture-stop").exists():
            if protocol == 1:
                request = None
                path = state / "windows-server-control.json"
                claim = state / "windows-server-control-claim.json"
                try:
                    os.replace(path, claim)
                    request = json.loads(claim.read_text(encoding="utf-8"))
                    claim.unlink()
                except FileNotFoundError:
                    pass
            else:
                request = server._read_control_request(state, previous)
            if request:
                previous = request["requestId"]
                backend.terminate()
                backend.wait(timeout=10)
                backend = subprocess.Popen([sys.executable, "-B", __file__, "--backend-fixture",
                                            request["sourceRoot"], str(port)])
                with (state / "fixture-actions.jsonl").open("a", encoding="utf-8") as output:
                    output.write(json.dumps(request) + "\n")
                server._write_control_result(state, previous, "backend-restarted")
                if protocol == 2:
                    Path(request["claimPath"]).unlink()
            time.sleep(.05)
    finally:
        if backend is not None:
            backend.terminate()
            backend.wait(timeout=10)


class UpgradeTransportContract(unittest.TestCase):
    def test_legacy_submission_keeps_one_id_and_refuses_unconfirmed_action(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            identity = server._write_control_request(state, "restart-backend", str(ROOT))
            request = state / "windows-server-control.json"
            value = json.loads(request.read_text(encoding="utf-8"))
            self.assertEqual(value["requestId"], identity)
            self.assertEqual(list(state.glob("windows-server-control-request-*.json")), [])
            request.unlink()  # Old runners remove the claim before they act.
            with self.assertRaisesRegex(RuntimeError, "has no receipt"):
                server._write_control_request(state, "restart-backend", str(ROOT))
            self.assertEqual(json.loads((state / "windows-server-control-legacy-pending.json").read_text())["requestId"], identity)
            server._write_control_result(state, identity, "backend-restarted")
            second = server._write_control_request(state, "stop-backend")
            self.assertNotEqual(second, identity)
            self.assertEqual(json.loads(request.read_text())["requestId"], second)

    def test_untracked_legacy_request_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            for name in ("windows-server-control.json", "windows-server-control-claim.json"):
                with self.subTest(name=name):
                    path = state / name
                    path.write_text('{"requestId":"old"}')
                    with self.assertRaisesRegex(RuntimeError, "pending"):
                        server._write_control_request(state, "restart-backend", str(ROOT))
                    self.assertEqual(path.read_text(), '{"requestId":"old"}')
                    path.unlink()

    def test_verified_runner_uses_per_request_transport_and_stale_identity_refuses_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            server._advertise_control_protocol(state)
            identity = server._write_control_request(state, "restart-backend", str(ROOT))
            self.assertTrue(server._control_request_path(state, identity).exists())
            self.assertFalse((state / "windows-server-control.json").exists())
            path = state / "windows-server-runner.json"
            value = json.loads(path.read_text())
            value["startTime"] = "wrong"
            path.write_text(json.dumps(value))
            with self.assertRaisesRegex(RuntimeError, "identity is not valid"):
                server._write_control_request(state, "restart-backend", str(ROOT))
            self.assertFalse((state / "windows-server-control.json").exists())
            self.assertEqual(len(list(state.glob("windows-server-control-request-*.json"))), 1)


@unittest.skipUnless(os.name == "nt", "Windows manager and ACL contract")
class WindowsUpgradeContract(unittest.TestCase):
    def test_powershell_manager_restarts_old_and_new_protocol_once(self):
        shells = ["powershell.exe"]
        if shutil.which("pwsh"):
            shells.append("pwsh")
        for shell in shells:
            for protocol in (1, 2):
                with self.subTest(shell=shell, protocol=protocol), tempfile.TemporaryDirectory(prefix="studio upgrade Ω ") as temporary:
                    local = Path(temporary)
                    install = local / "CodexStudio"
                    state = install / "state"
                    server.ensure_private_dir(state)
                    old_source = local / "old source Ω"
                    (old_source / "scripts").mkdir(parents=True)
                    (old_source / "scripts" / "codex_canvas.py").touch()
                    shutil.copy2(SERVER_SOURCE_ROOT / "codex_windows_server.py", install)
                    executable = getattr(sys, "_base_executable", sys.executable)
                    with socket.socket() as listener:
                        listener.bind(("127.0.0.1", 0))
                        port = listener.getsockname()[1]
                    config = {"sourceRoot": str(old_source), "stateDir": str(state), "python": executable,
                              "port": port, "publicOrigin": "https://kukuka-win.tailf00fa0.ts.net:8443"}
                    (install / "server.json").write_text(json.dumps(config), encoding="utf-8")
                    env = os.environ.copy()
                    env.update({"LOCALAPPDATA": str(local), "CODEX_STUDIO_SOURCE_DIR": str(ROOT),
                                "PYTHONPATH": str(SERVER_SOURCE_ROOT), "PYTHONIOENCODING": "utf-8"})
                    runner = subprocess.Popen([executable, "-B", __file__, "--runner-fixture",
                                               str(state), str(port), str(protocol), str(old_source)], env=env)
                    try:
                        def ready():
                            try:
                                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                                    return True
                            except OSError:
                                return False
                        wait_until(ready)
                        result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                                                 "-File", str(SERVER_SOURCE_ROOT / "manage-windows-server.ps1"),
                                                 "-Action", "RestartBackend", "-SourceRoot", str(ROOT)],
                                                env=env, capture_output=True, text=True, encoding="utf-8", timeout=170)
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        actions = [json.loads(line) for line in (state / "fixture-actions.jsonl").read_text().splitlines()]
                        self.assertEqual(len(actions), 1, actions)
                        identity = actions[0]["requestId"]
                        self.assertIn(identity, result.stdout)
                        self.assertEqual(json.loads((install / "server.json").read_text(encoding="utf-8-sig"))["sourceRoot"], str(ROOT))
                        receipt = json.loads((state / f"windows-server-control-{identity}.json").read_text())
                        self.assertEqual(receipt, {"requestId": identity, "result": "backend-restarted"})
                    finally:
                        (state / "fixture-stop").touch()
                        runner.wait(timeout=15)
                    self.assertEqual(runner.returncode, 0)

    def test_live_update_tick_and_diagnostic_archives_use_private_windows_receipts(self):
        import codex_sqlite_traces as sqlite_traces
        import codex_http_traces as http_traces
        with tempfile.TemporaryDirectory(prefix="studio receipt Ω ") as temporary:
            root = Path(temporary)
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "codex-canvas").touch()
            name = "codex_fixture_update.py"
            (scripts / name).write_text("def apply(runtime):\n    runtime.applied += 1\n    return {'status':'applied'}\n", encoding="utf-8")
            manifest = {"version": 1, "id": "windows-receipt", "python": [3, 14], "scope": "Receipt fixture",
                        "patch": name, "inputs": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                                                  for key, path in source_files(scripts)}}
            (scripts / "studio-live-update.json").write_text(json.dumps(manifest), encoding="utf-8")
            runtime = SimpleNamespace(root=root, closed=False, lock=threading.RLock(), applied=0)
            importlib.reload(sqlite_traces)
            importlib.reload(http_traces)
            manager = LiveUpdates(runtime, scripts)
            # Native currently uses Python 3.12. Exercise the tick and receipt
            # independently of the manifest's Python 3.14 eligibility rule.
            from unittest.mock import patch
            with patch("codex_live_updates.sys.version_info", (3, 14)):
                manager.tick()
                manager.tick()
            self.assertEqual(runtime.applied, 1)
            self.assertEqual(manager.status()["status"], "applied")
            self.assertNotIn("receiptError", manager.status())
            self.assertEqual(json.loads((root / "live-update.json").read_text()), manager.status())
            journal = root / "diagnostics" / "sqlite-transactions.json"
            value = json.loads(journal.read_text())
            value["pid"] = -1
            value["recent"] = [{"fixture": True}]
            journal.write_text(json.dumps(value))
            importlib.reload(sqlite_traces)
            sqlite_traces.transaction_watchdog(root)
            self.assertEqual(sqlite_traces._JOURNAL["status"], "written")
            self.assertEqual(len(list(journal.parent.glob("sqlite-transactions.history.*.json"))), 1)
            for path in [root / "live-update.json", journal, *journal.parent.glob("sqlite-transactions.history.*.json")]:
                acl = subprocess.check_output(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                                               "(Get-Acl -LiteralPath '" + str(path).replace("'", "''") + "').Access | Select-Object IdentityReference,IsInherited | ConvertTo-Json -Compress"], text=True)
                entries = json.loads(acl)
                entries = entries if isinstance(entries, list) else [entries]
                self.assertEqual(len(entries), 1, acl)
                self.assertFalse(entries[0]["IsInherited"], acl)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--backend-fixture":
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", int(sys.argv[3])))
            listener.listen()
            while True:
                connection, _ = listener.accept()
                connection.close()
    elif len(sys.argv) > 1 and sys.argv[1] == "--runner-fixture":
        runner_fixture(Path(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), Path(sys.argv[5]))
    else:
        unittest.main()
