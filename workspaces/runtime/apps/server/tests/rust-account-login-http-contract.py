#!/usr/bin/env python3
"""Account registry routes over the Python and Rust HTTP backends."""

from __future__ import annotations

import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import stat
import sys
import unittest
import uuid
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

SERVER_SOURCE = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(Path(__file__).parent))

from codex_accounts import AccountStore
from http_backend_harness import BackendProcess, HttpResult


def write_auth(home: Path, account_id: str) -> None:
    claims = {
        "email": "fixture@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": "pro",
        },
    }
    token = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    home.mkdir(parents=True, exist_ok=True)
    (home / "auth.json").write_text(
        json.dumps({
            "tokens": {
                "account_id": account_id,
                "id_token": f"fixture.{token}.signature",
                "access_token": "fixture-access-only",
            },
        }),
        encoding="utf-8",
    )


def assert_status(test: unittest.TestCase, response: HttpResult, expected: int) -> None:
    test.assertEqual(response.status, expected, response.body[:2000])


class AccountLoginHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = BackendProcess(Path(scratch), claude_bin=None)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.extra_home = cls.backend.root / "extra-codex-profile"
        write_auth(cls.backend.extra_home, "fixture-account-two")
        cls.backend.register_home = cls.backend.root / "registered-codex-profile"
        write_auth(cls.backend.register_home, "fixture-account-three")
        cls.z_home = cls.backend.root / "z-first-profile"
        write_auth(cls.z_home, "fixture-account-z")
        cls.a_home = cls.backend.root / "a-second-profile"
        write_auth(cls.a_home, "fixture-account-a")
        cls.discover_home = cls.backend.home / "Projects" / "discovered" / ".codex-profile"
        environment = {
            "HOME": str(cls.backend.home),
            "CODEX_HOME": str(cls.backend.codex_home),
            "CLAUDE_CONFIG_DIR": str(cls.backend.claude_home),
            "CODEX_AGENTS_STATE_DIR": str(cls.backend.state),
        }
        with patch.dict(os.environ, environment):
            cls.accounts = AccountStore(cls.backend.state)
            cls.extra_key = cls.accounts.register(str(cls.backend.extra_home))
            rows = cls.accounts.data["accounts"]
            cls.accounts.data["accounts"] = {
                "default": rows["default"],
                "z-first": {
                    "id": "z-first", "home": str(cls.z_home), "label": "Z first",
                    "source": "Fixture", "accountId": "fixture-account-z",
                    "email": "fixture@example.invalid", "plan": "pro", "status": "ready",
                    "_credentialIdentity": "chatgpt:fixture-account-z",
                },
                "a-second": {
                    "id": "a-second", "home": str(cls.a_home), "label": "A second",
                    "source": "Fixture", "accountId": "fixture-account-a",
                    "email": "fixture@example.invalid", "plan": "pro", "status": "ready",
                    "_credentialIdentity": "chatgpt:fixture-account-a",
                },
                **{key: row for key, row in rows.items() if key != "default"},
            }
            cls.accounts.data["logins"] = {
                request_id: {
                    "requestId": request_id, "accountKey": "default", "status": status,
                    "createdAt": 1.0, "loginId": f"fixture-{status}",
                }
                for status in ("ready", "duplicate", "cancelled", "error")
                for request_id in [str(uuid.uuid5(uuid.NAMESPACE_DNS, f"terminal-{status}"))]
            }
            cls.live_login_id = str(uuid.uuid5(uuid.NAMESPACE_DNS, "live-login"))
            cls.live_login_key = f"login-{cls.live_login_id}"
            cls.live_login_home = cls.backend.state / "accounts" / cls.live_login_key
            cls.accounts.data["accounts"][cls.live_login_key] = {
                "id": cls.live_login_key, "home": str(cls.live_login_home),
                "label": "New account", "source": "Codex Agents", "accountId": None,
                "email": None, "plan": None, "status": "pending",
            }
            cls.accounts.data["logins"][cls.live_login_id] = {
                "requestId": cls.live_login_id, "accountKey": cls.live_login_key, "status": "pending",
                "createdAt": 1.0, "loginId": "fixture-live-login",
            }
            cls.completed_login_id = str(uuid.uuid5(uuid.NAMESPACE_DNS, "completed-local-login"))
            cls.completed_login_key = f"login-{cls.completed_login_id}"
            cls.completed_login_home = cls.backend.state / "accounts" / cls.completed_login_key
            cls.accounts.data["accounts"][cls.completed_login_key] = {
                "id": cls.completed_login_key, "home": str(cls.completed_login_home),
                "label": "New account", "source": "Codex Agents", "accountId": None,
                "email": None, "plan": None, "status": "pending",
            }
            cls.accounts.data["logins"][cls.completed_login_id] = {
                "requestId": cls.completed_login_id, "accountKey": cls.completed_login_key,
                "status": "pending", "createdAt": 1.0, "loginId": "fixture-completed-login",
            }
            cls.terminal_logins = {
                request_id: receipt
                for request_id, receipt in cls.accounts.data["logins"].items()
                if receipt.get("status") in {"ready", "duplicate", "cancelled", "error"}
            }
            cls.accounts._save()
        cls.registry_path = cls.backend.state / "accounts" / "registry.json"
        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.close()

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def request(self, method: str, path: str, body=None) -> HttpResult:
        return self.backend.request(method, path, body=body, headers=self.headers())

    def setUp(self) -> None:
        # Each contract starts from the same disposable registry; mutations in
        # one test must not change the candidate set observed by another.
        self.backend.close()
        type(self).setUpClass()

    def test_default_write_is_durable_and_visible_to_next_accounts_read(self) -> None:
        response = self.request("POST", "/api/accounts/default", {"account_key": self.extra_key})
        assert_status(self, response, 200)
        self.assertEqual(response.json()["defaultAccountKey"], self.extra_key)
        snapshot = self.request("GET", "/api/accounts")
        assert_status(self, snapshot, 200)
        self.assertEqual(snapshot.json()["defaultAccountKey"], self.extra_key)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertTrue(json.loads(self.registry_path.read_text(encoding="utf-8"))["pendingAccountsChange"])

    def test_delete_replay_and_conflict_do_not_write(self) -> None:
        request_id = str(uuid.uuid4())
        body = {"account_key": "a-second", "request_id": request_id}
        first = self.request("POST", "/api/accounts/delete", body)
        assert_status(self, first, 200)
        committed = self.registry_path.read_bytes()
        write_auth(self.a_home, "fixture-account-a-changed")
        replay = self.request("POST", "/api/accounts/delete", body)
        assert_status(self, replay, 200)
        self.assertEqual(self.registry_path.read_bytes(), committed)
        write_auth(self.z_home, "fixture-account-z-changed")
        conflict = self.request(
            "POST", "/api/accounts/delete", {"account_key": "z-first", "request_id": request_id}
        )
        assert_status(self, conflict, 400)
        self.assertEqual(self.registry_path.read_bytes(), committed)
        write_auth(self.a_home, "fixture-account-a")
        write_auth(self.z_home, "fixture-account-z")

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_registry_replacement_preserves_file_mode(self) -> None:
        self.registry_path.chmod(0o640)
        before_directory_mode = stat.S_IMODE(self.registry_path.parent.stat().st_mode)
        response = self.request(
            "POST", "/api/accounts/name",
            {"account_key": "z-first", "label": "permission check", "request_id": str(uuid.uuid4())},
        )
        assert_status(self, response, 200)
        self.assertEqual(stat.S_IMODE(self.registry_path.stat().st_mode), 0o640)
        self.assertEqual(stat.S_IMODE(self.registry_path.parent.stat().st_mode), before_directory_mode)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_refused_operation_does_not_change_directory_mode(self) -> None:
        before = stat.S_IMODE(self.registry_path.parent.stat().st_mode)
        response = self.request(
            "POST", "/api/accounts/delete",
            {"account_key": "missing-account", "request_id": str(uuid.uuid4())},
        )
        assert_status(self, response, 400)
        self.assertEqual(stat.S_IMODE(self.registry_path.parent.stat().st_mode), before)
        restored = self.request("POST", "/api/accounts/register", {"home": str(self.a_home)})
        assert_status(self, restored, 200)

    def test_deleted_account_reconnect_verifies_changed_auth_without_writing(self) -> None:
        deleted = self.request(
            "POST", "/api/accounts/delete", {"account_key": "a-second", "request_id": str(uuid.uuid4())}
        )
        assert_status(self, deleted, 200)
        write_auth(self.a_home, "fixture-account-a-changed")
        before = self.registry_path.read_bytes()
        refusal = self.request("POST", "/api/accounts/reconnect", {"account_key": "a-second"})
        assert_status(self, refusal, 400)
        self.assertEqual(self.registry_path.read_bytes(), before)
        write_auth(self.a_home, "fixture-account-a")
        restored = self.request("POST", "/api/accounts/register", {"home": str(self.a_home)})
        assert_status(self, restored, 200)

    def test_discover_registers_known_codex_profile_from_disposable_home(self) -> None:
        write_auth(self.discover_home, "fixture-account-discovered")
        response = self.request("POST", "/api/accounts/discover", {})
        assert_status(self, response, 200)
        self.assertIn(
            "fixture-account-discovered",
            {row.get("accountId") for row in response.json()["accounts"]},
        )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_discovery_claude_dependency_refusal_does_not_write_registry(self) -> None:
        executable = self.backend.home / ".local" / "bin" / "claude"
        executable.parent.mkdir(parents=True)
        executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        executable.chmod(0o700)
        before = self.registry_path.read_bytes()
        response = self.request("POST", "/api/accounts/discover", {})
        assert_status(self, response, 501)
        self.assertEqual(response.json()["error"], "Not implemented: requires provider-session/claude-discovery")
        self.assertEqual(self.registry_path.read_bytes(), before)

    def test_discovery_follows_disposable_symlink_and_respects_candidate_cap(self) -> None:
        linked_home = self.backend.root / "linked-profile"
        write_auth(linked_home, "fixture-account-linked")
        projects = self.backend.home / "Projects"
        projects.mkdir(exist_ok=True)
        linked = projects / "linked" / ".codex-profile"
        linked.parent.mkdir()
        linked.symlink_to(linked_home, target_is_directory=True)
        for index in range(1001):
            candidate = projects / f"p{index:04d}" / ".codex-profile"
            write_auth(candidate, f"fixture-account-cap-{index:04d}")
        response = self.request("POST", "/api/accounts/discover", {})
        assert_status(self, response, 200)
        account_ids = {row.get("accountId") for row in response.json()["accounts"]}
        self.assertIn("fixture-account-linked", account_ids)
        self.assertIn("fixture-account-cap-0000", account_ids)
        self.assertNotIn("fixture-account-cap-1000", account_ids)

    def test_insertion_order_selects_default_replacement(self) -> None:
        disconnected = self.request("POST", "/api/accounts/disconnect", {"account_key": "default"})
        assert_status(self, disconnected, 200)
        self.assertEqual(disconnected.json()["defaultAccountKey"], "z-first")

    def test_concurrent_name_mutations_both_commit(self) -> None:
        bodies = [
            {"account_key": key, "label": f"parallel {key}", "request_id": str(uuid.uuid4())}
            for key in ("z-first", "a-second")
        ]
        with ThreadPoolExecutor(max_workers=2) as pool:
            responses = list(pool.map(lambda body: self.request("POST", "/api/accounts/name", body), bodies))
        for response in responses:
            assert_status(self, response, 200)
        receipts = json.loads(self.registry_path.read_text(encoding="utf-8"))["nameReceipts"]
        self.assertTrue(all(body["request_id"] in receipts for body in bodies))

    def test_refused_default_after_changed_auth_does_not_write_refresh(self) -> None:
        write_auth(self.a_home, "fixture-account-a-changed")
        before = self.registry_path.read_bytes()
        refusal = self.request("POST", "/api/accounts/default", {"account_key": "a-second"})
        assert_status(self, refusal, 400)
        self.assertEqual(self.registry_path.read_bytes(), before)
        write_auth(self.a_home, "fixture-account-a")

    def test_terminal_login_cancel_returns_saved_receipts_without_provider_calls(self) -> None:
        starts_before = self.backend.provider_start_times()
        for request_id, receipt in self.terminal_logins.items():
            response = self.request("POST", "/api/accounts/login/cancel", {"login_id": request_id})
            assert_status(self, response, 200)
            self.assertEqual(response.json()["status"], receipt["status"])
        self.assertEqual(self.backend.provider_start_times(), starts_before)

    def test_cancel_reconciles_completed_local_login_without_provider_call(self) -> None:
        write_auth(self.completed_login_home, "fixture-account-completed-login")
        starts_before = self.backend.provider_start_times()
        response = self.request(
            "POST", "/api/accounts/login/cancel", {"login_id": self.completed_login_id}
        )
        assert_status(self, response, 200)
        self.assertEqual(response.json()["status"], "ready")
        self.assertEqual(self.backend.provider_start_times(), starts_before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_listing_refresh_reports_changed_auth_without_registry_write(self) -> None:
        write_auth(self.z_home, "fixture-account-z-changed")
        before = self.registry_path.read_bytes()
        response = self.request("GET", "/api/accounts")
        assert_status(self, response, 200)
        account = next(row for row in response.json()["accounts"] if row["id"] == "z-first")
        self.assertEqual(account["status"], "changed")
        self.assertEqual(self.registry_path.read_bytes(), before)

    def test_name_request_replay_and_conflict(self) -> None:
        request_id = str(uuid.uuid4())
        body = {"account_key": self.extra_key, "label": "  renamed fixture  ", "request_id": request_id}
        first = self.request("POST", "/api/accounts/name", body)
        second = self.request("POST", "/api/accounts/name", body)
        assert_status(self, first, 200)
        assert_status(self, second, 200)
        self.assertEqual(
            next(row for row in second.json()["accounts"] if row["id"] == self.extra_key)["label"],
            "renamed fixture",
        )
        conflict = self.request(
            "POST", "/api/accounts/name", {**body, "label": "changed content"}
        )
        assert_status(self, conflict, 400)
        self.assertEqual(conflict.json()["error"], "This name request id has different content")

    def test_name_uses_python_strip_control_separators(self) -> None:
        response = self.request(
            "POST",
            "/api/accounts/name",
            {"account_key": self.extra_key, "label": "\u001cname\u001f", "request_id": str(uuid.uuid4())},
        )
        assert_status(self, response, 200)
        account = next(row for row in response.json()["accounts"] if row["id"] == self.extra_key)
        self.assertEqual(account["label"], "name")

    def test_z_disconnect_refusal_does_not_commit_changed_auth_refresh(self) -> None:
        assert_status(self, self.request("POST", "/api/accounts/default", {"account_key": "z-first"}), 200)
        accounts = self.request("GET", "/api/accounts")
        assert_status(self, accounts, 200)
        disconnected = []
        for row in accounts.json()["accounts"]:
            if row["id"] != "z-first" and row.get("status") == "ready" and not row.get("disconnected"):
                response = self.request("POST", "/api/accounts/disconnect", {"account_key": row["id"]})
                assert_status(self, response, 200)
                disconnected.append(row["id"])
        write_auth(self.z_home, "fixture-account-z-changed")
        before = self.registry_path.read_bytes()
        refusal = self.request("POST", "/api/accounts/disconnect", {"account_key": "z-first"})
        assert_status(self, refusal, 400)
        self.assertEqual(self.registry_path.read_bytes(), before)
        write_auth(self.z_home, "fixture-account-z")
        assert_status(self, self.request("POST", "/api/accounts/reconnect", {"account_key": "z-first"}), 200)
        for account in disconnected:
            assert_status(self, self.request("POST", "/api/accounts/reconnect", {"account_key": account}), 200)

    def test_disconnect_reconnect_and_delete_refusal_do_not_lose_registry_state(self) -> None:
        disconnect = self.request("POST", "/api/accounts/disconnect", {"account_key": self.extra_key})
        assert_status(self, disconnect, 200)
        self.assertTrue(next(row for row in disconnect.json()["accounts"] if row["id"] == self.extra_key)["disconnected"])
        reconnect = self.request("POST", "/api/accounts/reconnect", {"account_key": self.extra_key})
        assert_status(self, reconnect, 200)
        self.assertFalse(next(row for row in reconnect.json()["accounts"] if row["id"] == self.extra_key).get("disconnected", False))

        deleted = self.request(
            "POST", "/api/accounts/delete", {"account_key": self.extra_key, "request_id": str(uuid.uuid4())}
        )
        assert_status(self, deleted, 200)
        for key in ("z-first", "a-second"):
            disconnected = self.request("POST", "/api/accounts/disconnect", {"account_key": key})
            assert_status(self, disconnected, 200)
        before = self.registry_path.read_bytes()
        refusal = self.request(
            "POST", "/api/accounts/delete", {"account_key": "default", "request_id": str(uuid.uuid4())}
        )
        assert_status(self, refusal, 400)
        self.assertEqual(refusal.json()["error"], "Add another connected account before deleting the application default.")
        self.assertEqual(self.registry_path.read_bytes(), before)
        restored = self.request("POST", "/api/accounts/register", {"home": str(self.backend.extra_home)})
        assert_status(self, restored, 200)

    def test_register_reads_python_auth_file_without_copying_credentials(self) -> None:
        response = self.request("POST", "/api/accounts/register", {"home": str(self.backend.register_home)})
        assert_status(self, response, 200)
        account = next(
            row for row in response.json()["accounts"]
            if row["id"].startswith("profile-") and row["accountId"] == "fixture-account-three"
        )
        self.assertEqual(account["home"], str(self.backend.register_home.resolve()))
        serialized = json.dumps(response.json()) + self.registry_path.read_text(encoding="utf-8")
        self.assertNotIn("fixture-access-only", serialized)

    def test_register_api_key_profile_writes_null_identity_fields(self) -> None:
        home = self.backend.root / "api-key-profile"
        home.mkdir()
        (home / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "fixture-api-key"}), encoding="utf-8")
        response = self.request("POST", "/api/accounts/register", {"home": str(home)})
        assert_status(self, response, 200)
        account = next(row for row in response.json()["accounts"] if row["home"] == str(home.resolve()))
        self.assertIsNone(account["accountId"])
        self.assertIsNone(account["email"])
        serialized = json.dumps(response.json()) + self.registry_path.read_text(encoding="utf-8")
        self.assertNotIn("fixture-api-key", serialized)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_interactive_login_routes_remain_explicitly_unavailable(self) -> None:
        response = self.request("POST", "/api/accounts/login", {"login_id": str(uuid.uuid4())})
        assert_status(self, response, 501)
        self.assertEqual(response.json()["error"], "Not implemented: requires provider-session/codex-login")
        cancel = self.request(
            "POST", "/api/accounts/login/cancel", {"login_id": self.live_login_id}
        )
        assert_status(self, cancel, 501)
        self.assertEqual(cancel.json()["error"], "Not implemented: requires provider-session/codex-login")


if __name__ == "__main__":
    unittest.main()
