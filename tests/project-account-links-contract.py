#!/usr/bin/env python3
"""Multiple accounts retain the project's identity, default, and live chats."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("project_fixture", Path(__file__).with_name("project-accounts-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Links(fixture.ProjectAccountsContract):
    def link(self, keys, default="default", revision=0):
        return self.store.projects({"action": "set_accounts", "path": str(self.project),
                                   "account_keys": keys, "account_key": default,
                                   "expected_revision": revision})

    def test_links_persist_without_changing_default_or_chats(self):
        original = self.set_account("default", 0)
        linked = self.link([self.other, "default"], revision=1)
        self.assertEqual(linked["accountKeys"], sorted([self.other, "default"]))
        self.assertEqual(self.store.project_account(self.project), "default")
        self.assertEqual(self.link(["default", self.other, self.other], revision=1), linked)
        for field in ("id", "path", "name", "created"):
            self.assertEqual(original[field], linked[field])
        restored = fixture.Store(self.root).projects()["items"]
        self.assertIn(linked, restored)
        with self.store.db() as db:
            self.assertEqual(self.store.records(db, "agents"), [])

    def test_invalid_membership_and_stale_write_are_atomic(self):
        original = self.link(["default", self.other])
        for keys, default, revision in [([], "default", 1), (["unknown"], "unknown", 1),
                                         ([self.other], "default", 1), ([self.other], self.other, 0)]:
            with self.assertRaises(ValueError):
                self.link(keys, default, revision)
            self.assertIn(original, self.store.projects()["items"])

    def test_single_default_edit_adds_the_account_to_membership(self):
        first = self.link(["default"])
        second = self.set_account(self.other, first["accountRevision"])
        self.assertEqual(second["accountKeys"], sorted(["default", self.other]))


if __name__ == "__main__":
    unittest.main()
