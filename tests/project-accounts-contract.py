#!/usr/bin/env python3
"""Project account defaults and migration. Temporary SQLite, no model requests."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_accounts import AccountStore
from codex_workspace import WorkspaceMixin


class Store(WorkspaceMixin):
    def __init__(self, root):
        self.root = root
        self.lock = threading.RLock()
        self.accounts = AccountStore(root)
        with self.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            self.setup_workspace(db)

    @contextmanager
    def db(self):
        connection = sqlite3.connect(self.root / "test.sqlite")
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    @staticmethod
    def records(db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]

    @staticmethod
    def put(db, table, record):
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES (?, ?)", (record["id"], json.dumps(record)))


class ProjectAccountsContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir()
        (self.home / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "fixture-one"}))
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(self.home)})
        self.env.start()
        self.store = Store(self.root)
        other = self.root / "other"
        other.mkdir()
        (other / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "fixture-two"}))
        self.other = self.store.accounts.register(str(other))
        self.project = self.root / "project"
        self.project.mkdir()

    def tearDown(self):
        self.env.stop()
        self.temp.cleanup()

    def set_account(self, key, revision, path=None):
        return self.store.projects({"action": "set_account", "path": str(path or self.project),
                                    "account_key": key, "expected_revision": revision})

    def test_default_persists_and_most_specific_ancestor_wins(self):
        self.assertEqual(self.store.project_account(self.project), "default")
        parent = self.set_account(self.other, 0)
        self.assertEqual(parent["accountRevision"], 1)
        child = self.project / "child"
        child.mkdir()
        self.assertEqual(self.store.project_account(child), self.other)
        self.set_account("default", 0, child)
        self.assertEqual(self.store.project_account(child / "nested"), "default")
        self.assertEqual(Store(self.root).project_account(self.project), self.other)
        self.assertEqual(self.store.project_account(self.root / "project-other"), "default")
        self.assertEqual(self.store.accounts.home(self.other), self.root / "other")

    def test_revision_conflicts_and_exact_retry_preserve_identity(self):
        first = self.set_account(self.other, 0)
        self.assertEqual(first, self.set_account(self.other, 0))
        with self.assertRaisesRegex(ValueError, "changed"):
            self.set_account("default", 0)
        second = self.set_account("default", 1)
        self.assertEqual(second, self.set_account("default", 1))
        self.assertEqual(second, self.set_account("default", 2))
        for field in ("id", "path", "name", "created"):
            self.assertEqual(first[field], second[field])
        third = self.set_account(self.other, 2)
        with self.assertRaisesRegex(ValueError, "changed"):
            self.set_account(self.other, 0)
        self.assertEqual(third["accountRevision"], 3)
        for revision in (True, -1, None, "3"):
            with self.assertRaises(ValueError):
                self.set_account("default", revision)
        with self.assertRaisesRegex(ValueError, "Unknown"):
            self.set_account("missing", 3)

    def test_register_and_chat_ensure_preserve_saved_choice(self):
        first = self.store.projects({"path": str(self.project), "account_key": self.other})
        self.assertEqual(first, self.store.projects({"path": str(self.project), "account_key": "default"}))
        with self.store.db() as db:
            self.assertEqual(first, self.store.ensure_project(self.project, "default", db))
            unseen = self.store.ensure_project(self.root / "unseen", "default", db)
        self.assertEqual(unseen["accountKey"], "default")
        alias = self.root / "alias"
        alias.symlink_to(self.project, target_is_directory=True)
        self.assertEqual(self.store.project_account(alias), self.other)
        self.assertEqual(self.store.projects({"path": str(alias)}), first)

    def test_competing_writes_have_one_winner(self):
        self.set_account(self.other, 0)
        barrier = threading.Barrier(2)
        results = []
        def change(key):
            barrier.wait()
            try:
                results.append(self.set_account(key, 1))
            except ValueError as error:
                results.append(error)
        # Both mutations differ from the saved account and from each other.
        third_home = self.root / "third"
        third_home.mkdir()
        (third_home / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "fixture-three"}))
        third = self.store.accounts.register(str(third_home))
        threads = [threading.Thread(target=change, args=(key,)) for key in ("default", third)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(sum(isinstance(result, ValueError) for result in results), 1)
        self.assertEqual(self.store.projects()["items"][0]["accountRevision"], 2)

    def test_migration_precedence_and_once_only_registration(self):
        legacy = self.root / "legacy"
        ambiguous = self.root / "ambiguous"
        fallback = self.root / "fallback"
        self.store.accounts.data["accounts"]["default"]["projectRules"] = {
            "allowedProjects": [str(self.project), str(legacy), str(ambiguous)], "revision": 2}
        self.store.accounts.data["accounts"][self.other]["projectRules"] = {
            "allowedProjects": [str(ambiguous)], "revision": 1}
        self.store.accounts._save()
        saved = {"id": str(fallback), "path": str(fallback), "name": "Keep me", "created": 15}
        explicit = {"id": str(self.root / "explicit"), "path": str(self.root / "explicit"),
                    "name": "Explicit", "created": 10, "accountKey": self.other, "accountRevision": 9}
        with self.store.db() as db:
            db.execute("DELETE FROM runtime_workspace_migrations")
            for project in (saved, explicit):
                self.store.put(db, "projects", project)
            for key, account, created, deleted in (("old", "default", 1, False),
                                                   ("new", self.other, 2, False),
                                                   ("deleted", "default", 3, True)):
                self.store.put(db, "agents", {"id": key, "isLead": True, "cwd": str(self.project),
                    "accountKey": account, "created": created, "deletedAt": created if deleted else None})
            self.store.migrate_project_accounts(db)
        projects = {p["path"]: p for p in self.store.projects()["items"]}
        self.assertEqual(projects[str(self.project)]["accountKey"], self.other)
        self.assertEqual(projects[str(legacy)]["accountKey"], "default")
        self.assertNotIn(str(ambiguous), projects)
        self.assertEqual(projects[str(fallback)], dict(saved, accountKey="default", accountRevision=1))
        self.assertEqual(projects[explicit["path"]], explicit)
        self.store.projects({"action": "remove", "path": str(legacy)})
        restarted = Store(self.root)
        self.assertNotIn(str(legacy), {p["path"] for p in restarted.projects()["items"]})
        self.assertNotIn("projectRules", restarted.accounts.get("default"))


if __name__ == "__main__":
    unittest.main()
