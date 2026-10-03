#!/usr/bin/env python3
"""The diagnostics endpoint uses fake accounts and shares no command text."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import http.client
import json
from pathlib import Path
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_canvas import Canvas, make_server
from codex_diagnostics import process_tree, snapshot
from codex_lock_metrics import MeasuredRLock


class Server:
    def __init__(self, provider):
        self.provider = provider
        self.calls = []
        self.callbacks = queue.Queue()
        self.clock_replies = queue.Queue()
        self.tool_requests = queue.Queue()

    def call(self, method, params, timeout=10):
        self.calls.append(method)
        if self.provider == "claude":
            assert method == "claude/diagnostics"
            return {"liveQueries": 1, "activeTurns": 0, "idleQueries": 1}
        assert method == "thread/loaded/list"
        return {"data": ["thread-1", "thread-2"], "nextCursor": None}


class DiagnosticsContract(unittest.TestCase):
    def test_process_tree_counts_only_descendants_and_removes_commands(self):
        ps = """10 1 10240 2.0 /private/secret/codex-canvas --token hidden
11 10 20480 0.5 /secret/codex app-server --listen stdio://
12 11 1024 0.0 /secret/anarlog mcp
13 1 9000 99.0 /unrelated/claude --output-format stream-json
"""
        tree = process_tree(10, ps)
        self.assertEqual(tree["kinds"]["codex_app_server"]["count"], 1)
        self.assertEqual(tree["totalRssMiB"], 31)
        self.assertEqual([row["pid"] for row in tree["processes"]], [10, 11, 12])
        self.assertNotIn("secret", json.dumps(tree))

    def test_snapshot_reads_existing_connections_and_queue_sizes(self):
        codex, claude = Server("codex"), Server("claude")
        codex.callbacks.put(1)
        db = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        db.execute("CREATE TABLE runtime_events (status TEXT)")
        db.execute("INSERT INTO runtime_events VALUES ('pending')")
        lock = MeasuredRLock()
        class UnlockedDb:
            def __enter__(self):
                if lock._is_owned():
                    raise AssertionError("Diagnostics opened SQLite under Runtime.lock")
                return db

            def __exit__(self, *_):
                return False

        runtime = SimpleNamespace(
            lock=lock, servers={"private-account": codex, "other": claude},
            accounts={"private-account": {"provider": "codex"},
                      "other": {"provider": "claude"}},
            loaded={"agent"}, recovery_pool=SimpleNamespace(_work_queue=queue.Queue()),
            db=UnlockedDb,
            sample_dispatch_lock_holder=lambda result, waited: result.append("test"),
        )
        with patch("codex_diagnostics.host_resources", return_value={"cpuCount": 8}):
            result = snapshot(runtime, root_pid=10, ps_output="10 1 1024 0.0 codex-canvas")
        self.assertEqual(result["nativeAccounts"]["account2"]["loadedThreads"], 2)
        self.assertEqual(result["nativeAccounts"]["account1"]["liveQueries"], 1)
        self.assertEqual(result["queues"]["account2.callbacks"], 1)
        self.assertEqual(result["studioLoadedThreads"], 1)
        self.assertEqual(result["queues"]["durableInputPending"], 1)
        self.assertEqual(result["hostResources"]["cpuCount"], 8)
        self.assertTrue(any(row["operation"] == "nativeStatus" for row in result["resourceAttribution"]))
        self.assertEqual(len(result["runtimeLockSamples"]), 5)
        self.assertTrue(result["runtimeLockOperations"])
        self.assertTrue(all("waitMs" in row and "holdMs" in row
                            for row in result["runtimeLockOperations"]))
        self.assertNotIn("private-account", json.dumps(result))

    def test_http_route_returns_diagnostics_without_a_token(self):
        with tempfile.TemporaryDirectory() as folder:
            canvas = Canvas(Path(folder))
            canvas.runtime = SimpleNamespace(lock=threading.RLock())
            server = make_server(canvas)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch("codex_diagnostics.snapshot", return_value={"processTree": {"kinds": {}}}):
                    connection = http.client.HTTPConnection("127.0.0.1", server.server_port)
                    connection.request("GET", "/api/diagnostics")
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    body = json.load(response)
                    # The route adds the process supervisor state to the snapshot.
                    self.assertEqual(body.pop("processTree"), {"kinds": {}})
                    self.assertEqual(set(body), {"supervisor"})
                    self.assertIs(type(body["supervisor"]["mode"]), bool)
                    connection.close()
                    cli = subprocess.run([sys.executable, str(Path(__file__).resolve().parents[1]
                                           / "scripts/codex-diagnostics"), "--port",
                                          str(server.server_port)], capture_output=True,
                                         text=True, timeout=5, check=True)
                    printed = json.loads(cli.stdout)
                    self.assertEqual(printed.pop("processTree"), {"kinds": {}})
                    self.assertEqual(set(printed), {"supervisor"})
            finally:
                server.shutdown()
                server.server_close()
                thread.join()


if __name__ == "__main__":
    unittest.main()
