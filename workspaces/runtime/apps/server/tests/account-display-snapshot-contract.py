#!/usr/bin/env python3
"""Account display reads use saved metadata, without native auth probes."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from fastapi import FastAPI
from fastapi.testclient import TestClient
from starlette.responses import JSONResponse

import codex_accounts
import codex_claude
from codex_runtime import Runtime
from codex_usage_resume import AUTH_WAIT_NOTICE_SECONDS, UsageResumeMixin
from studio_api.accounts.models import AccountsResponse
from studio_api.accounts.router import create_router


class DisplayRuntime(UsageResumeMixin):
    def __init__(self, store, root):
        self.accounts = store
        self.lock = threading.RLock()
        self.db_path = root / "runtime.sqlite3"
        self.connection = sqlite3.connect(self.db_path, check_same_thread=False)
        self.connection.row_factory = sqlite3.Row
        self.connection.execute("PRAGMA journal_mode=WAL")
        self.connection.execute("CREATE TABLE runtime_usage_resumes (record TEXT NOT NULL)")
        self.connection.commit()

    @contextmanager
    def db(self):
        yield self.connection

    read_db = Runtime.read_db


class DisplayContext:
    def __init__(self, runtime):
        self.runtime = runtime

    def send(self, request, value, status=200, **_kwargs):
        return JSONResponse(AccountsResponse.model_validate(value).wire_dump(), status_code=status)


class AccountDisplaySnapshotContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.store = codex_accounts.AccountStore.__new__(codex_accounts.AccountStore)
        self.store.lock = threading.RLock()
        self.store.discovered = True
        self.store.data = {
            "version": 1, "defaultAccountKey": "claude-two", "deleteReceipts": {},
            "accounts": {}, "logins": {},
        }
        for name in ("one", "two", "three"):
            key = "claude-" + name
            self.store.data["accounts"][key] = {
                "id": key, "provider": "claude", "home": "/private/profiles/" + name,
                "label": "Claude " + name, "source": "Claude Code", "status": "ready",
                "accountId": "claude:" + name, "email": name + "@example.invalid", "plan": "max",
                "_credentialIdentity": "claude:" + name,
                "claudeOptions": {"configDir": "/private/claude/" + name,
                                  "customModels": [{"id": "model", "label": "Saved model"}]},
            }
        self.runtime = DisplayRuntime(self.store, Path(self.temp.name))
        self.addCleanup(self.runtime.connection.close)

    def client(self):
        app = FastAPI()
        app.include_router(create_router(DisplayContext(self.runtime)))
        return TestClient(app)

    def test_get_returns_while_all_three_native_auth_profiles_are_blocked(self):
        entered, release, done = threading.Event(), threading.Event(), threading.Event()
        result, errors, probes = [], [], []

        def blocked_auth(options):
            probes.append(options["configDir"])
            entered.set()
            if not release.wait(2):
                raise AssertionError("private native auth barrier was not released")
            return {"status": "error", "error": "Cannot read Claude Code sign-in status"}

        with self.client() as client, patch.object(codex_claude, "auth_metadata", side_effect=blocked_auth):
            def read():
                try:
                    result.append(client.get("/api/accounts"))
                except BaseException as error:
                    errors.append(error)
                finally:
                    done.set()

            thread = threading.Thread(target=read, name="private-account-display")
            thread.start()
            try:
                self.assertTrue(done.wait(.3), "GET /api/accounts waits for native auth")
                self.assertFalse(entered.is_set())
            finally:
                release.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(probes, [])
        self.assertEqual(result[0].status_code, 200)
        self.assertEqual([row["id"] for row in result[0].json()["accounts"]],
                         ["claude-one", "claude-two", "claude-three"])

    def test_saved_snapshot_preserves_pins_receipts_and_display_filters(self):
        self.store.data["accounts"]["claude-two"].update(status="changed", error="The account changed")
        self.store.data["accounts"]["claude-three"]["deleted"] = True
        self.store.data["accounts"]["login-pending"] = {
            "id": "login-pending", "home": "/private/login", "label": "Login", "source": "Codex CLI",
            "status": "pending", "_credentialIdentity": "saved-pin",
        }
        self.store.data["logins"] = {
            "pending-id": {"requestId": "pending-id", "accountKey": "login-pending", "status": "pending",
                           "createdAt": 11.0, "loginId": "native-login", "verificationUrl": "https://example.invalid/signin"},
            "ready-id": {"requestId": "ready-id", "accountKey": "claude-one", "status": "ready",
                         "resolvedAccountKey": "claude-one"},
        }
        before = copy.deepcopy(self.store.data)
        with patch.object(self.store, "discover", side_effect=AssertionError("discovery")), \
                patch.object(self.store, "login_receipts", side_effect=AssertionError("login reconciliation")), \
                patch.object(self.store, "refresh", side_effect=AssertionError("auth")), \
                patch.object(self.store, "_save", side_effect=AssertionError("write")):
            snapshot = self.store.snapshot(refresh=False)
        AccountsResponse.model_validate(snapshot)
        self.assertEqual(snapshot["defaultAccountKey"], "claude-two")
        self.assertEqual([row["id"] for row in snapshot["accounts"]], ["claude-one", "claude-two"])
        self.assertEqual(snapshot["archivedAccounts"][0]["id"], "claude-three")
        self.assertEqual(snapshot["logins"], list(before["logins"].values()))
        self.assertEqual(snapshot["accounts"][1]["status"], "changed")
        self.assertEqual(snapshot["accounts"][1]["accountId"], "claude:two")
        self.assertEqual(snapshot["accounts"][1]["home"], "/private/profiles/two")
        self.assertNotIn("_credentialIdentity", json.dumps(snapshot))
        self.assertTrue(snapshot["supportsDisconnect"])
        self.assertTrue(snapshot["supportsDelete"])
        snapshot["accounts"][0]["claudeOptions"]["customModels"][0]["label"] = "Changed copy"
        snapshot["archivedAccounts"][0]["claudeOptions"]["configDir"] = "/other"
        snapshot["logins"][0]["status"] = "cancelled"
        self.assertEqual(self.store.data, before)

    def test_get_does_not_wait_for_runtime_lock_or_use_writer(self):
        done = threading.Event()
        result, errors = [], []
        with self.client() as client, \
                patch.object(self.runtime, "db", side_effect=AssertionError("writer")), \
                patch.object(self.store, "refresh", side_effect=AssertionError("auth")):
            def read():
                try:
                    result.append(client.get("/api/accounts"))
                except BaseException as error:
                    errors.append(error)
                finally:
                    done.set()

            thread = threading.Thread(target=read, name="private-account-runtime-lock")
            try:
                with self.runtime.lock:
                    self.runtime.connection.execute("BEGIN IMMEDIATE")
                    thread.start()
                    self.assertTrue(done.wait(.3), "GET /api/accounts waits for Runtime.lock")
            finally:
                self.runtime.connection.rollback()
                thread.join(1)
            self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(result[0].status_code, 200)

    def test_undiscovered_display_uses_registry_without_discovery_or_filesystem(self):
        self.store.discovered = False
        before = copy.deepcopy(self.store.data)
        with patch.object(self.store, "discover", side_effect=AssertionError("discovery")), \
                patch.object(Path, "exists", side_effect=AssertionError("filesystem")), \
                patch.object(Path, "read_text", side_effect=AssertionError("filesystem")), \
                patch.object(codex_accounts, "auth_metadata", side_effect=AssertionError("auth")):
            snapshot = self.store.snapshot(refresh=False)
        self.assertEqual(len(snapshot["accounts"]), 3)
        self.assertFalse(self.store.discovered)
        self.assertEqual(self.store.data, before)

    def test_legacy_login_request_id_is_added_only_to_display_copy(self):
        self.store.data["logins"]["legacy-request"] = {"accountKey": "claude-one", "status": "pending"}
        snapshot = self.store.snapshot(refresh=False)
        self.assertEqual(snapshot["logins"][0]["requestId"], "legacy-request")
        self.assertNotIn("requestId", self.store.data["logins"]["legacy-request"])
        AccountsResponse.model_validate(snapshot)

    def test_auth_wait_notice_changes_only_the_display_copy(self):
        self.runtime.connection.execute("INSERT INTO runtime_usage_resumes VALUES (?)", (json.dumps({
            "status": "scheduled", "cause": "auth", "accountKey": "claude-two",
            "failedAt": time.time() - AUTH_WAIT_NOTICE_SECONDS - 10,
        }),))
        self.runtime.connection.commit()
        with patch.object(self.store, "refresh", side_effect=AssertionError("auth")):
            snapshot = self.runtime.accounts_snapshot()
        AccountsResponse.model_validate(snapshot)
        self.assertIn("authenticationRecovery", snapshot["accounts"][1])
        self.assertNotIn("authenticationRecovery", self.store.data["accounts"]["claude-two"])

    def test_default_snapshot_keeps_authoritative_refresh_and_discovery(self):
        logins = [{"requestId": "current", "accountKey": "claude-one", "status": "ready"}]
        calls = []

        def refresh(key):
            calls.append(key)
            self.store.data["accounts"][key]["plan"] = "fresh"

        with patch.object(self.store, "login_receipts", return_value=logins) as receipts, \
                patch.object(self.store, "refresh", side_effect=refresh):
            snapshot = self.store.snapshot()
        receipts.assert_called_once_with()
        self.assertEqual(calls, ["claude-one", "claude-two", "claude-three"])
        self.assertEqual([row["plan"] for row in snapshot["accounts"]], ["fresh"] * 3)
        self.assertEqual(snapshot["logins"], logins)
        self.store.discovered = False
        with patch.object(self.store, "discover", return_value={"discovered": True}) as discovery:
            self.assertEqual(self.store.snapshot(), {"discovered": True})
        discovery.assert_called_once_with()

    def test_native_connect_rechecks_auth_instead_of_trusting_display(self):
        runtime = Runtime.__new__(Runtime)
        runtime.closed = False
        runtime.accounts = self.store
        self.store.snapshot(refresh=False)
        with patch.object(codex_claude, "auth_metadata", return_value={
                "status": "error", "error": "Cannot read Claude Code sign-in status"}) as auth:
            with self.assertRaisesRegex(ValueError, "Cannot read Claude Code sign-in status"):
                runtime.connect("claude-one")
        auth.assert_called_once_with(self.store.data["accounts"]["claude-one"]["claudeOptions"])


if __name__ == "__main__":
    unittest.main()
