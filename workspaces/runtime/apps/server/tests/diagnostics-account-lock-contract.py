#!/usr/bin/env python3
"""Diagnostics reads saved providers without authentication or nested locks."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
from pathlib import Path
import json
import queue
import sqlite3
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_accounts import AccountStore
from codex_diagnostics import snapshot
from codex_execution import ensure_tables


class NativeServer:
    def __init__(self, provider, runtime_lock):
        self.provider = provider
        self.runtime_lock = runtime_lock
        self.calls = []
        self.callbacks = queue.Queue()
        self.clock_replies = queue.Queue()
        self.tool_requests = queue.Queue()

    def call(self, method, _params, timeout):
        if self.runtime_lock._is_owned():
            raise AssertionError("Native diagnostics runs under Runtime.lock")
        self.calls.append(method)
        if self.provider == "claude":
            if method != "claude/diagnostics":
                raise AssertionError("Claude received a Codex diagnostics request")
            return {"liveQueries": 1, "activeTurns": 0}
        if method != "thread/loaded/list":
            raise AssertionError("Codex received a Claude diagnostics request")
        return {"data": ["thread-1"], "nextCursor": None}


class ObservedRegistryLock:
    def __init__(self, runtime_lock):
        self.lock = threading.RLock()
        self.runtime_lock = runtime_lock
        self.attempted = threading.Event()
        self.nested = False
        self.owner_thread = threading.get_ident()

    def __enter__(self):
        if threading.get_ident() != self.owner_thread:
            self.nested = self.runtime_lock._is_owned()
            self.attempted.set()
        self.lock.acquire()
        return self

    def __exit__(self, *_):
        self.lock.release()


class DiagnosticsAccountLockContract(unittest.TestCase):
    def fixture(self):
        lock = threading.RLock()
        accounts = AccountStore.__new__(AccountStore)
        accounts.lock = ObservedRegistryLock(lock)
        accounts.data = {"accounts": {
            "claude-private": {"provider": "claude", "status": "error",
                               "home": "/private/profile", "claudeOptions": {},
                               "accountId": "private-identity", "email": "private-email"},
            "codex-default": {"home": "/private/codex", "status": "signedOut"},
        }}
        claude = NativeServer("claude", lock)
        codex = NativeServer("codex", lock)
        db = sqlite3.connect(":memory:", check_same_thread=False)
        self.addCleanup(db.close)
        ensure_tables(db)
        db.execute("CREATE TABLE runtime_events (status TEXT)")
        db.execute("INSERT INTO runtime_events VALUES ('uncertain')")

        @contextmanager
        def database():
            if lock._is_owned():
                raise AssertionError("Diagnostics opened SQLite under Runtime.lock")
            yield db

        runtime = SimpleNamespace(
            lock=lock, accounts=accounts,
            servers={"claude-private": claude, "codex-default": codex},
            loaded={"agent"}, recovery_pool=SimpleNamespace(_work_queue=queue.Queue()),
            db=database,
        )
        return runtime, claude, codex

    def read(self, runtime):
        return snapshot(runtime, root_pid=10, ps_output="10 1 1024 0.0 codex-canvas")

    def run_snapshot(self, runtime):
        result = {}
        done = threading.Event()

        def run():
            try:
                result["value"] = self.read(runtime)
            except BaseException as error:
                result["error"] = error
            finally:
                done.set()

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        return thread, done, result

    def assert_finished(self, thread, done, result):
        self.assertTrue(done.wait(2), "Diagnostics did not finish after the fixture released")
        thread.join(2)
        if "error" in result:
            raise result["error"]
        return result["value"]

    def test_saved_provider_routes_without_refreshing_authentication(self):
        runtime, claude, codex = self.fixture()
        with patch("codex_claude.auth_metadata", side_effect=AssertionError("Unexpected Claude auth")), \
                patch("codex_accounts.auth_metadata", side_effect=AssertionError("Unexpected Codex auth")), \
                patch("codex_diagnostics.host_resources", return_value={}):
            result = self.read(runtime)
        self.assertEqual(claude.calls, ["claude/diagnostics"])
        self.assertEqual(codex.calls, ["thread/loaded/list"])
        self.assertEqual(result["nativeAccounts"]["account1"]["provider"], "claude")
        self.assertEqual(result["nativeAccounts"]["account2"]["loadedThreads"], 1)
        self.assertEqual(result["queues"]["durableInputPending"], 1)
        self.assertEqual(result["studioLoadedThreads"], 1)
        for secret in ("claude-private", "codex-default", "private-identity", "private-email", "/private/"):
            self.assertNotIn(secret, json.dumps(result))

    def test_slow_authentication_cannot_hold_runtime_lock(self):
        runtime, _claude, _codex = self.fixture()
        progress = threading.Event()
        release = threading.Event()
        auth_calls = []

        def slow_auth(_options):
            auth_calls.append("claude")
            progress.set()
            if not release.wait(2):
                raise AssertionError("The fixture did not release authentication")
            return {"status": "error", "error": "fixture auth failed"}

        with patch("codex_claude.auth_metadata", side_effect=slow_auth), \
                patch("codex_accounts.auth_metadata", return_value={"status": "signedOut"}), \
                patch("codex_diagnostics.host_resources", return_value={}):
            thread, done, result = self.run_snapshot(runtime)
            try:
                # The old path announces auth; the new path finishes without it.
                self.assertTrue(done.wait(.2) or progress.wait(1))
                acquired = runtime.lock.acquire(timeout=.1)
                try:
                    self.assertTrue(acquired, "Authentication holds Runtime.lock")
                    self.assertEqual(auth_calls, [], "Diagnostics must not authenticate")
                finally:
                    if acquired:
                        runtime.lock.release()
            finally:
                release.set()
                self.assert_finished(thread, done, result)

    def test_registry_lock_wait_does_not_block_runtime_work(self):
        runtime, _claude, _codex = self.fixture()
        with patch("codex_claude.auth_metadata", return_value={"status": "error"}), \
                patch("codex_accounts.auth_metadata", return_value={"status": "signedOut"}), \
                patch("codex_diagnostics.host_resources", return_value={}):
            thread = None
            try:
                with runtime.accounts.lock:
                    thread, done, result = self.run_snapshot(runtime)
                    self.assertTrue(runtime.accounts.lock.attempted.wait(1))
                    self.assertFalse(runtime.accounts.lock.nested,
                                     "Diagnostics nests Accounts.lock under Runtime.lock")
                    acquired = runtime.lock.acquire(timeout=.1)
                    try:
                        self.assertTrue(acquired, "Registry lock blocks runtime work")
                        self.assertFalse(done.is_set(), "The registry lock fixture must hold diagnostics")
                    finally:
                        if acquired:
                            runtime.lock.release()
            finally:
                # Join only after this thread releases the registry lock.
                if thread is not None:
                    self.assert_finished(thread, done, result)


if __name__ == "__main__":
    unittest.main()
