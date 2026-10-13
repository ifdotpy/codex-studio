#!/usr/bin/env python3
"""Account registry routes over the Python and Rust HTTP backends."""

from __future__ import annotations

import base64
import json
import os
from pathlib import Path
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
        cls.backend = BackendProcess(Path(scratch))
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.extra_home = cls.backend.root / "extra-codex-profile"
        write_auth(cls.backend.extra_home, "fixture-account-two")
        cls.backend.register_home = cls.backend.root / "registered-codex-profile"
        write_auth(cls.backend.register_home, "fixture-account-three")
        environment = {
            "HOME": str(cls.backend.home),
            "CODEX_HOME": str(cls.backend.codex_home),
            "CLAUDE_CONFIG_DIR": str(cls.backend.claude_home),
            "CODEX_AGENTS_STATE_DIR": str(cls.backend.state),
        }
        with patch.dict(os.environ, environment):
            cls.accounts = AccountStore(cls.backend.state)
            cls.extra_key = cls.accounts.register(str(cls.backend.extra_home))
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

    def test_default_write_is_durable_and_visible_to_next_accounts_read(self) -> None:
        response = self.request("POST", "/api/accounts/default", {"account_key": self.extra_key})
        assert_status(self, response, 200)
        self.assertEqual(response.json()["defaultAccountKey"], self.extra_key)
        snapshot = self.request("GET", "/api/accounts")
        assert_status(self, snapshot, 200)
        self.assertEqual(snapshot.json()["defaultAccountKey"], self.extra_key)

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

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_interactive_login_routes_remain_explicitly_unavailable(self) -> None:
        response = self.request("POST", "/api/accounts/login", {"login_id": str(uuid.uuid4())})
        assert_status(self, response, 501)
        self.assertEqual(response.json()["error"], "Not implemented: requires provider-session/codex-login")


if __name__ == "__main__":
    unittest.main()
