#!/usr/bin/env python3
"""A guarded timeout update preserves HTTP closures and existing native objects."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_canvas
import codex_runtime
import codex_http_timeout_update as update
from codex_catalog import CatalogPending
from codex_session_costs import SessionCostReader
from codex_source import signature, source_function


class TimeoutUpdateContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-timeout-update-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = codex_runtime.Runtime.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.changed = threading.Event()
        self.runtime.root = self.root
        self.runtime.closed = False
        self.runtime.accounts = None
        self.runtime.servers = {"default": object()}
        self.runtime.connection_ids = {"default": object()}
        def pending(account):
            raise CatalogPending("Metadata is pending")
        self.runtime.catalog = pending
        self.canvas = codex_canvas.Canvas(self.root)
        self.canvas.runtime = self.runtime
        self.server = codex_canvas.make_server(self.canvas)
        self.addCleanup(self.server.server_close)
        self.saved = []
        for name, spec in update.SOURCES.items():
            module = importlib.import_module(name)
            baseline = subprocess.check_output(
                ["git", "show", "c1e557f:scripts/" + spec["source"]], cwd=ROOT)
            for item in spec["functions"]:
                _, current = update.target(module, item["path"])
                old, _ = source_function(baseline, item["path"], vars(module))
                self.replace(current, old)
        current = self.server.RequestHandlerClass.do_GET
        baseline = subprocess.check_output(["git", "show", "c1e557f:scripts/codex_canvas.py"], cwd=ROOT)
        old, _ = source_function(baseline, ("make_server", "Handler", "do_GET"),
                                vars(codex_canvas), closure=current.__closure__)
        self.replace(current, old)
        self.worker_accounts = sys.modules["codex_worker_accounts"]
        self.addCleanup(self.restore)

    def replace(self, current, desired):
        self.saved.append((current, current.__code__, current.__defaults__, current.__kwdefaults__))
        current.__code__, current.__defaults__, current.__kwdefaults__ = (
            desired.__code__, desired.__defaults__, desired.__kwdefaults__)

    def restore(self):
        for function, code, defaults, keywords in reversed(self.saved):
            function.__code__, function.__defaults__, function.__kwdefaults__ = code, defaults, keywords

    def snapshot(self):
        return [(id(function), signature(function)) for function, *_ in self.saved]

    def test_apply_preserves_existing_reader_http_and_native_identity(self):
        db = sqlite3.connect(self.root / "canvas.sqlite3")
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.commit()
        db.close()
        class Pricing:
            def snapshot(self):
                return {"providers": {}}
        reader = SessionCostReader(self.root / "canvas.sqlite3", Pricing())
        handler = self.server.RequestHandlerClass.do_GET
        closure = handler.__closure__
        cache, native = reader.cache, self.runtime.servers
        with patch.object(codex_runtime.subprocess, "Popen", side_effect=AssertionError("An update must not start a native child")):
            self.assertEqual(update.apply(self.runtime), {"status": "applied"})
            self.assertEqual(update.apply(self.runtime), {"status": "already_applied"})
        self.assertIs(handler, self.server.RequestHandlerClass.do_GET)
        self.assertIs(closure, handler.__closure__)
        self.assertIs(cache, reader.cache)
        self.assertIs(native, self.runtime.servers)
        self.assertNotIn("DISPLAY_READ", vars(self.worker_accounts))
        release = threading.Event()
        original = reader._compute
        def slow(agent, root):
            if not release.wait(2):
                raise RuntimeError("The history fixture did not release")
            return original(agent, root)
        with patch.object(reader, "_compute", side_effect=slow):
            try:
                self.assertEqual(reader.snapshot("lead")["pricingState"], "loading")
            finally:
                release.set()
                deadline = time.monotonic() + 2
                while reader.refreshing and time.monotonic() < deadline:
                    time.sleep(0.005)
        self.assertEqual(reader.snapshot("lead")["pricingState"], "ready")
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        try:
            class Accounts:
                def list(self):
                    return [{"id": "default", "status": "ready"}]
                def default(self):
                    return "default"
                def get(self, account):
                    return {"id": account, "status": "ready"}
            self.runtime.accounts = Accounts()
            for suffix in ("", "?workers=1"):
                with self.assertRaises(HTTPError) as failure:
                    urlopen(f"http://127.0.0.1:{self.server.server_port}/api/models{suffix}", timeout=1)
                self.assertEqual(failure.exception.code, 400)
                self.assertTrue(json.load(failure.exception)["catalogPending"])
                failure.exception.close()
        finally:
            self.server.shutdown()
            thread.join()

    def test_unknown_http_code_rejects_before_other_mutations(self):
        current = self.server.RequestHandlerClass.do_GET
        current.__code__ = current.__code__.replace(co_exceptiontable=b"")
        before = self.snapshot()
        with self.assertRaisesRegex(RuntimeError, "running function differs"):
            update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)
        self.assertNotIn("DISPLAY_READ", vars(self.worker_accounts))

    def test_unknown_backend_code_rejects_before_http_mutation(self):
        current = SessionCostReader.snapshot
        current.__code__ = current.__code__.replace(co_consts=current.__code__.co_consts + ("unknown",))
        before = self.snapshot()
        with self.assertRaisesRegex(RuntimeError, "running function differs"):
            update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)

    def test_unreviewed_source_hash_rejects_before_any_mutation(self):
        before = self.snapshot()
        with patch.dict(update.SOURCES["codex_session_costs"], sha256="0" * 64):
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs"):
                update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
