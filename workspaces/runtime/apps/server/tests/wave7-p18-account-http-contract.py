#!/usr/bin/env python3
"""P18 account writes and durable login replay on either HTTP backend."""
from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).parent))
from codex_accounts import AccountStore
from http_backend_harness import BackendProcess

LOGIN = "4b154e4b-80bb-4f1f-b07f-dcaa15563190"


class AccountSessionHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = BackendProcess(Path(os.environ["TMPDIR"]), initialize_workspace=False)
        cls.addClassCleanup(cls.backend.close)
        cls.homes = tempfile.TemporaryDirectory(prefix="p18-http-homes-", dir=os.environ["XDG_CACHE_HOME"])
        cls.addClassCleanup(cls.homes.cleanup)
        for field in ("home", "codex_home", "claude_home", "xdg_cache"):
            directory = Path(cls.homes.name) / field
            directory.mkdir()
            setattr(cls.backend, field, directory)
        native = Path(cls.homes.name) / "fixture-claude"
        native.write_text("#!/bin/sh\nprintf '%s\\n' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"fixture@example.invalid\",\"subscriptionType\":\"max\"}'\n")
        native.chmod(0o700)
        cls.backend._claude_bin = str(native)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        with patch.dict(os.environ, {
            "HOME": str(cls.backend.home), "CODEX_HOME": str(cls.backend.codex_home),
            "CLAUDE_CONFIG_DIR": str(cls.backend.claude_home),
        }):
            accounts = AccountStore(cls.backend.state)
            accounts.data["logins"][LOGIN] = {"requestId":LOGIN,"accountKey":"default","status":"cancelled","createdAt":1.0,"emailHint":None,"requestedLabel":None}
            accounts._save()
        cls.registry = cls.backend.state / "accounts/registry.json"
        cls.backend.start()

    def post(self, path, body):
        return self.backend.request("POST", path, body=body, headers={"Origin":self.backend.origin,"X-Canvas-Token":self.backend.session["token"]})

    def test_login_replay_cancel_and_identity_conflict_are_read_only(self):
        before = json.loads(self.registry.read_text())
        for path in ("/api/accounts/login", "/api/accounts/login/cancel"):
            result = self.post(path,{"login_id":LOGIN})
            self.assertEqual(result.status,200,result.body)
            self.assertEqual(result.json()["status"],"cancelled")
            self.assertEqual(json.loads(self.registry.read_text()),before)
        result = self.post("/api/accounts/login",{"login_id":LOGIN,"label":"different"})
        self.assertEqual(result.status,400,result.body)
        self.assertEqual(json.loads(self.registry.read_text()),before)

    def test_discovery_register_update_and_path_change_refusal(self):
        result = self.post("/api/accounts/discover",{})
        self.assertEqual(result.status,200,result.body)
        local = next(row for row in result.json()["accounts"] if row["id"] == "claude-local")
        self.assertEqual(local["email"],"fixture@example.invalid")
        options = {"binaryPath":self.backend._claude_bin,"configDir":str(self.backend.claude_home)}
        result = self.post("/api/claude/profiles",{"options":options,"label":"Fixture profile"})
        self.assertEqual(result.status,200,result.body)
        profile = next(row for row in result.json()["accounts"] if row.get("label") == "Fixture profile")
        updated = {**options,"customModels":[{"id":"fixture-model","label":"fixture-model"}],"launchArgs":"--verbose"}
        result = self.post("/api/claude/profiles",{"account_key":profile["id"],"options":updated,"label":"Updated fixture"})
        self.assertEqual(result.status,200,result.body)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            catalog = self.backend.request("GET", "/api/models?account_key=" + profile["id"])
            self.assertEqual(catalog.status,501,catalog.body)
            self.assertIn("provider-service/claude-model-catalog",catalog.json()["error"])
        stored = json.loads(self.registry.read_text())
        self.assertEqual(stored["accounts"][profile["id"]]["claudeOptions"]["customModels"],[{"id":"fixture-model","label":"fixture-model"}])
        result = self.post("/api/claude/profiles",{"account_key":profile["id"],"options":{**updated,"configDir":str(self.backend.claude_home / "other")}})
        self.assertEqual(result.status,400,result.body)
        self.assertEqual(json.loads(self.registry.read_text()),stored)


if __name__ == "__main__":
    unittest.main()
