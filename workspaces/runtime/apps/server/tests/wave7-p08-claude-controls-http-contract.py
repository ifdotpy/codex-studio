#!/usr/bin/env python3
"""Claude session controls use a live retained session and durable receipts."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
from http_backend_harness import (
    TimedHTTPContractTest,
    no_write_snapshot,
    start_contract_backend,
)

_messages_path = Path(__file__).with_name("rust-messages-http-contract.py")
_messages_spec = importlib.util.spec_from_file_location("p08_message_fixture", _messages_path)
assert _messages_spec is not None and _messages_spec.loader is not None
_messages = importlib.util.module_from_spec(_messages_spec)
sys.modules[_messages_spec.name] = _messages
_messages_spec.loader.exec_module(_messages)


class ClaudeControlsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    def headers(self):
        return {
            "Origin": self.backend.origin,
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_missing_agent_is_rejected_without_writes(self):
        # No agents or events exist, so no preparation or dispatch can race the
        # snapshot. Both backends validate the actor before any provider call.
        for action, arguments in (
            ("state", {}),
            ("commands", {}),
            ("command", {"command": "/help", "request_id": "p08-command"}),
            ("settings", {"settings": {}, "request_id": "p08-settings"}),
            ("rollback", {"turn_id": "turn", "request_id": "p08-rollback"}),
            ("stop_task", {"task_id": "task"}),
        ):
            with self.subTest(action=action):
                before = no_write_snapshot(self.backend.state)
                response = self.backend.request(
                    "POST", "/api/claude/session",
                    body={"id": "missing", "action": action, **arguments},
                    headers=self.headers(),
                )
                self.assertEqual(response.status, 400, response.body)
                self.assertEqual(response.json(), {"error": "Unknown managed agent"})
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_invalid_action_is_located_before_the_dependency(self):
        before = no_write_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/claude/session",
            body={"id": "missing", "action": "unknown"}, headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body)
        self.assertIn("location", response.json())
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_threadless_state_and_settings_do_not_need_a_provider_session(self):
        from codex_accounts import AccountStore

        self.backend.stop()
        options = {
            "binaryPath": str(self.backend.root / "unused-claude-fixture"),
            "configDir": str(self.backend.claude_home),
        }
        metadata = {
            "status": "ready",
            "accountId": "claude:p08@example.invalid",
            "email": "p08@example.invalid",
            "plan": "fixture",
            "_credentialIdentity": "claude:p08@example.invalid",
        }
        with patch.dict(os.environ, {
            "HOME": str(self.backend.home),
            "CODEX_HOME": str(self.backend.codex_home),
            "CLAUDE_CONFIG_DIR": str(self.backend.claude_home),
        }), patch("codex_claude.installed", return_value=True):
            accounts = AccountStore(self.backend.state)
            account_key = accounts.register_claude(
                options, label="P08 no-thread fixture", verified_metadata=metadata,
            )
            account = accounts.data["accounts"][account_key]
            account.update(status="signedOut")
            for field in ("accountId", "email", "plan", "_credentialIdentity"):
                account.pop(field, None)
            accounts._save()
        agent_id = _messages.seed_agent(self.backend, {
            "provider": "claude",
            "accountKey": account_key,
            "claudeOptions": {"permissionMode": "default"},
            "threadId": None,
            "autoWake": False,
        })
        self.backend.start()

        before = no_write_snapshot(self.backend.state)
        state = self.backend.request(
            "POST", "/api/claude/session",
            body={"id": agent_id, "action": "state"}, headers=self.headers(),
        )
        self.assertEqual(state.status, 200, state.body)
        self.assertEqual(state.json(), {
            "settings": {"permissionMode": "default"}, "turns": [], "tasks": [],
        })
        self.assertEqual(no_write_snapshot(self.backend.state), before)

        settings = {"permissionMode": "plan", "thinking": True}
        body = {
            "id": agent_id,
            "action": "settings",
            "request_id": "p08-no-thread-settings",
            "settings": settings,
        }
        changed = self.backend.request(
            "POST", "/api/claude/session", body=body, headers=self.headers(),
        )
        self.assertEqual(changed.status, 200, changed.body)
        self.assertEqual(changed.json().get("settings"), settings)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            row = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()
            operation = database.execute(
                "SELECT record FROM runtime_workspace_operations WHERE id=?",
                ("claude-control:p08-no-thread-settings",),
            ).fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(json.loads(row[0])["claudeOptions"], settings)
        self.assertIsNotNone(operation)
        self.assertEqual(json.loads(operation[0])["phase"], "completed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
