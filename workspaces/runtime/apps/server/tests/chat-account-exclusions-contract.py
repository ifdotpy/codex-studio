#!/usr/bin/env python3
"""Per-chat account exclusions at worker and account-move selection points."""
import sys
from contextlib import contextmanager
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import sqlite3
import threading
import unittest

from pydantic import ValidationError
from types import SimpleNamespace

from codex_agent_move import AgentMoves
from codex_worker_accounts import account_order, resolve, selected_account
from studio_api.agents.models import ConversationRequest


class FakeAccounts:
    def __init__(self):
        self.rows = [
            {"id": "default", "status": "ready", "provider": "codex"},
            {"id": "second", "status": "ready", "provider": "codex"},
        ]

    def list(self):
        return self.rows

    def default(self):
        return "default"

    def get(self, key):
        return next(row for row in self.rows if row["id"] == key)

    def home(self, key):
        return self.get(key)["id"]

    def allow_native_auth_attempt(self, _key, _row):
        return False


class FakeRuntime:
    def __init__(self, excluded=(), project_accounts=None):
        self.accounts = FakeAccounts()
        self.accounts.data = {"defaultAccountKey": "default"}
        self.root = {
            "id": "lead", "rootId": "lead", "isLead": True,
            "excludedAccountKeys": list(excluded),
            "workerDefaults": {"model": "gpt-test", "effort": None, "fastMode": False},
        }
        self.parent = {"id": "lead", "rootId": "lead", "accountKey": "default"}
        self.project_accounts = project_accounts
        self._project_server_id = "server"
        self.project_db = sqlite3.connect(":memory:")
        self.project_db.execute("CREATE TABLE runtime_projects(id TEXT, record TEXT)")
        if project_accounts is not None:
            self.root.update(projectId="project", projectServerId="server", cwd="/project")
            self.project_db.execute("INSERT INTO runtime_projects VALUES (?, ?)",
                                    ("/project", json.dumps({"accountKeys": list(project_accounts)})))

    def agent(self, _key):
        return self.root

    def worker_defaults(self, root):
        return root["workerDefaults"]

    def catalog(self, key):
        return {"data": [{"model": "gpt-test"}]} if key in {"default", "second"} else {"data": []}

    @contextmanager
    def read_db(self):
        yield self.project_db


class ChatAccountExclusions(unittest.TestCase):
    def test_spawn_selection_skips_excluded_and_explicit_exclusion_is_refused(self):
        runtime = FakeRuntime(["default"])
        self.addCleanup(runtime.project_db.close)
        selected, _catalog = resolve(runtime, runtime.parent, {})
        self.assertEqual(selected, "second")
        with self.assertRaisesRegex(ValueError, "disabled for this chat"):
            selected_account(runtime, {"account_key": "default"}, runtime.root)
        with self.assertRaisesRegex(ValueError, "disabled for this chat"):
            resolve(runtime, runtime.parent, {"account_key": "default"})

    def test_all_excluded_and_project_intersection_have_clear_errors(self):
        runtime = FakeRuntime(["default", "second"])
        self.addCleanup(runtime.project_db.close)
        with self.assertRaisesRegex(ValueError, "chat disables all accounts"):
            resolve(runtime, runtime.parent, {})
        runtime = FakeRuntime(["default"], project_accounts=["default"])
        self.addCleanup(runtime.project_db.close)
        with self.assertRaisesRegex(ValueError, "chat disables all accounts.*project account selection"):
            resolve(runtime, runtime.parent, {})

    def test_other_chat_and_account_move_selection_are_unaffected(self):
        restricted = FakeRuntime(["default"])
        unaffected = FakeRuntime()
        self.addCleanup(restricted.project_db.close)
        self.addCleanup(unaffected.project_db.close)
        self.assertEqual(resolve(unaffected, unaffected.parent, {})[0], "default")
        self.assertEqual(account_order(unaffected, "default"), ["default", "second"])
        moves = AgentMoves(SimpleNamespace(runtime=restricted))
        selected, _warning = moves._select_account({
            "provider": "codex", "sourceAccountKey": "default",
            "excludedAccountKeys": ["default"],
            "accountIdentity": {"email": "same@example.test", "accountId": "same"},
            "accept_cache_loss": True,
        })
        self.assertEqual(selected["id"], "second")
        with self.assertRaisesRegex(ValueError, "disabled for this chat"):
            moves._select_account({
                "provider": "codex", "sourceAccountKey": "default",
                "excludedAccountKeys": ["default"], "account_key": "default",
                "accountIdentity": {"email": "same@example.test", "accountId": "same"},
                "accept_cache_loss": True,
            })
        plain_move, _warning = moves._select_account({
            "provider": "codex", "sourceAccountKey": "default",
            "excludedAccountKeys": [],
            "accountIdentity": {"email": "unmatched@example.test", "accountId": "unmatched"},
            "accept_cache_loss": True,
        })
        self.assertEqual(plain_move["id"], "default")

    def test_transfer_target_is_rejected_and_request_model_rejects_invalid_exclusions(self):
        class Accounts:
            def get(self, key):
                return {"id": key, "status": "ready", "provider": "codex"}

        class TransferRuntime:
            def __init__(self):
                self.lock = threading.RLock()
                self.connection = sqlite3.connect(":memory:")
                self.lead = {"id": "lead", "isLead": True, "accountKey": "default",
                             "excludedAccountKeys": ["second"]}
                self.accounts = Accounts()

            @contextmanager
            def db(self):
                yield self.connection

            def records(self, _db, _collection):
                return []

            def checked_actor(self, _db, _key):
                return self.lead

        runtime = TransferRuntime()
        self.addCleanup(runtime.connection.close)
        from codex_account_transfer import AccountTransfers
        transfers = AccountTransfers(runtime)
        with self.assertRaisesRegex(ValueError, "disabled for this chat"):
            transfers.request("lead", "second", "47c243b6-18d0-49be-a127-72ccfd57a5e8")
        self.assertEqual(
            ConversationRequest(id="lead", excluded_account_keys=["second", "second"])
            .excluded_account_keys,
            {"second"},
        )
        with self.assertRaises(ValidationError) as caught:
            ConversationRequest(id="lead", excluded_account_keys=[""])
        self.assertEqual(caught.exception.errors()[0]["loc"], ("excluded_account_keys", 0))


if __name__ == "__main__":
    unittest.main()
