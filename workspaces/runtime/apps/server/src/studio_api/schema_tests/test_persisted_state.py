"""Producer and drift checks for the persisted formats covered by state-check."""
from __future__ import annotations

import io
import json
import sqlite3
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch
from uuid import uuid4

from runtime_test_environment import make_runtime
from pydantic import ValidationError

from codex_accounts import AccountStore
from codex_runtime import Runtime
from studio_api.generate_state_schemas import generated_schemas
from studio_api.persisted_models import PersistedAgentRecord
from studio_api.state_check import check_state_directory, main as state_check_main


class PersistedStateSchemaTests(unittest.TestCase):
    def test_committed_state_schemas_match_the_models(self) -> None:
        for path, expected in generated_schemas().items():
            with self.subTest(schema=path.name):
                self.assertEqual(json.loads(path.read_text(encoding="utf-8")), expected)
                self.assertNotIn(b'"default"', path.read_bytes())

    def test_persisted_agent_accepts_absent_and_nullable_optional_fields(self) -> None:
        value = {
            "id": "agent-1",
            "rootId": "agent-1",
            "epoch": 0,
            "status": "idle",
        }
        absent = PersistedAgentRecord.model_validate(value)
        self.assertNotIn("sandbox", absent.model_fields_set)
        self.assertNotIn("approvalPolicy", absent.model_fields_set)

        nullable = PersistedAgentRecord.model_validate({
            **value,
            "sandbox": None,
            "approvalPolicy": None,
        })
        self.assertIsNone(nullable.sandbox)
        self.assertIsNone(nullable.approvalPolicy)
        self.assertEqual(
            PersistedAgentRecord.model_validate({**value, "approvalPolicy": "never"}).approvalPolicy,
            "never",
        )
        with self.assertRaises(ValidationError):
            PersistedAgentRecord.model_validate({**value, "sandbox": "workspace-write"})

    def test_real_account_and_usage_writers_produce_valid_disposable_state(self) -> None:
        with TemporaryDirectory(prefix="studio-state-schema-") as directory:
            state = Path(directory)
            home = state / "codex-home"
            home.mkdir()
            with patch("codex_accounts.codex_home", return_value=home):
                accounts = AccountStore(state)
            accounts.discovered = True

            profile_keys: list[str] = []
            for name in ("first", "second"):
                profile = state / name
                profile.mkdir()
                (profile / "auth.json").write_text(
                    json.dumps({"OPENAI_API_KEY": "synthetic-fixture-key"}),
                    encoding="utf-8",
                )
                profile_keys.append(accounts.register(str(profile)))

            accounts.set_name(profile_keys[0], "  Primary  ", str(uuid4()))
            delete_request = str(uuid4())
            accounts.delete(profile_keys[1], delete_request)

            options = {"binaryPath": "claude-fixture", "configDir": str(state / "claude")}
            metadata = {
                "status": "ready",
                "accountId": "claude:fixture@example.invalid",
                "email": "fixture@example.invalid",
                "plan": "fixture",
                "_credentialIdentity": "claude:fixture@example.invalid",
            }
            with patch("codex_claude.installed", return_value=True):
                accounts.register_claude(options, label="Claude Fixture", verified_metadata=metadata)

            class FailedLoginRuntime:
                def connect(self, *_args: object, **_kwargs: object) -> object:
                    raise RuntimeError("fixture provider is unavailable")

            accounts.start_login(FailedLoginRuntime(), str(uuid4()))

            runtime = make_runtime(Runtime, state)
            try:
                runtime.set_rate_limits("default", {
                    "at": 1_800_000_000.0,
                    "data": {"rateLimits": {"primary": {"usedPercent": 25}}},
                })
            finally:
                runtime.close()

            registry_path = state / "accounts" / "registry.json"
            database_path = state / "canvas.sqlite3"
            registry_before = registry_path.read_bytes()
            database_before = database_path.read_bytes()
            self.assertEqual(check_state_directory(state), [])
            output = io.StringIO()
            with patch.object(sys, "stdout", output):
                self.assertEqual(state_check_main(["--state-dir", str(state)]), 0)
            self.assertIn('"violations": []', output.getvalue())
            self.assertEqual(registry_path.read_bytes(), registry_before)
            self.assertEqual(database_path.read_bytes(), database_before)

            persisted = json.loads(registry_before)
            self.assertIn(delete_request, persisted["deleteReceipts"])
            self.assertEqual(next(iter(persisted["nameReceipts"].values()))["label"], "Primary")
            self.assertTrue(persisted["logins"])
            self.assertIn("claudeOptions", next(
                row for row in persisted["accounts"].values() if row.get("provider") == "claude"
            ))

    def test_reports_three_handmade_violations_without_values(self) -> None:
        with TemporaryDirectory(prefix="studio-state-schema-invalid-") as directory:
            state = Path(directory)
            accounts_dir = state / "accounts"
            accounts_dir.mkdir()
            registry = {
                "version": 2,
                "defaultAccountKey": "fixture",
                "accounts": {
                    "fixture": {
                        "id": "fixture", "home": "/fixture", "label": "bad\ud800",
                        "source": "fixture", "status": "ready",
                    }
                },
            }
            (accounts_dir / "registry.json").write_text(
                json.dumps(registry, ensure_ascii=True), encoding="utf-8",
            )
            with sqlite3.connect(state / "canvas.sqlite3") as db:
                db.execute(
                    "CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)"
                )
                db.execute(
                    "INSERT INTO sync_entities VALUES('workspace','current',?,0)",
                    (json.dumps({"value": {"rateLimitsByAccount": {
                        "fixture": {"accountKey": False},
                    }}}),),
                )

            violations = check_state_directory(state)
            self.assertEqual(len(violations), 3)
            self.assertEqual(
                {(item.get("file"), item.get("pointer"), item["keyword"]) for item in violations},
                {
                    ("accounts/registry.json", "/version", "enum"),
                    ("accounts/registry.json", "/accounts/fixture/label", "unicode-scalar"),
                    (None, "/value/rateLimitsByAccount/fixture/accountKey", "type"),
                },
            )
            self.assertTrue(all(set(item) <= {"file", "table", "key", "pointer", "keyword"} for item in violations))
            output = io.StringIO()
            with patch.object(sys, "stdout", output):
                self.assertEqual(state_check_main(["--state-dir", str(state)]), 1)
            self.assertNotIn("bad", output.getvalue())
            self.assertIn("Rust sync-table comparison is not performed", output.getvalue())


if __name__ == "__main__":
    unittest.main()
