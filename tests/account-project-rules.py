#!/usr/bin/env python3
"""Account/project admission, canonical paths, and durable policy edits."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_accounts import AccountStore


class ProjectRules(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="account-project-rules-")
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "lumina"
        self.other = self.root / "lumina-private"
        self.project.mkdir()
        self.other.mkdir()
        home = self.root / "profile"
        home.mkdir()
        (home / "auth.json").write_text(
            json.dumps({"tokens": {"account_id": "fixture", "access_token": "fixture"}})
        )
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(home)})
        self.env.start()
        self.store = AccountStore(self.root / "state")

    def tearDown(self):
        self.env.stop()
        self.temp.cleanup()

    def rule(self, paths):
        revision = self.store.get("default")["projectRules"]["revision"]
        return self.store.set_project_rules("default", paths, revision)

    def test_defaults_and_explicit_deny_all(self):
        self.assertTrue(self.store.project_allowed("default", self.other))
        self.rule([])
        self.assertFalse(self.store.project_allowed("default", self.project))
        with self.assertRaisesRegex(ValueError, "Allowed projects: none"):
            self.store.check_project("default", self.project)
        self.store.check_project("default", self.project, skip=True)
        with self.assertRaisesRegex(ValueError, "true or false"):
            self.store.check_project("default", self.project, skip="true")
        self.rule(None)
        self.assertTrue(self.store.project_allowed("default", self.project))

    def test_exact_project_and_subdirectories_not_sibling_prefix(self):
        subdir = self.project / "src"
        subdir.mkdir()
        self.rule([str(self.project)])
        self.assertTrue(self.store.project_allowed("default", subdir))
        self.assertFalse(self.store.project_allowed("default", self.other))
        self.assertFalse(
            self.store.project_allowed("default", self.project / ".." / self.other.name)
        )

    def test_symlink_escape_and_replaced_allowed_root(self):
        self.rule([str(self.project)])
        escape = self.project / "escape"
        escape.symlink_to(self.other, target_is_directory=True)
        self.assertFalse(self.store.project_allowed("default", escape))
        self.project.rename(self.root / "original")
        self.project.symlink_to(self.other, target_is_directory=True)
        self.assertFalse(self.store.project_allowed("default", self.project))

    def test_canonical_edit_persistence_conflict_and_lost_reply_retry(self):
        alias = self.root / "project-link"
        alias.symlink_to(self.project, target_is_directory=True)
        first = self.rule([str(alias), str(self.project)])
        self.assertEqual(first["allowedProjects"], [str(self.project)])
        restarted = AccountStore(self.root / "state")
        self.assertEqual(restarted.get("default")["projectRules"], first)
        self.assertEqual(
            restarted.set_project_rules("default", [str(self.project)], 0), first
        )
        with self.assertRaisesRegex(ValueError, "changed"):
            restarted.set_project_rules("default", [str(self.other)], 0)
        self.assertEqual(restarted.get("default")["projectRules"], first)

    def test_invalid_inputs_and_failed_save_preserve_policy(self):
        for allowed, version in [
            ("/tmp", 0),
            ([""], 0),
            ([str(self.root / "missing")], 0),
            ([], True),
        ]:
            with self.assertRaises(ValueError):
                self.store.set_project_rules("default", allowed, version)
        before = self.store.get("default")["projectRules"]
        with patch.object(self.store, "_save", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.rule([])
        self.assertEqual(self.store.get("default")["projectRules"], before)

    def test_second_login_of_same_account_keeps_its_rules(self):
        self.rule([str(self.project)])
        import uuid

        with self.store.lock:
            duplicate = self.store._login_profile(str(uuid.uuid4()))
            home = Path(self.store.data["accounts"][duplicate]["home"])
            (home / "auth.json").write_text(
                json.dumps(
                    {"tokens": {"account_id": "fixture", "access_token": "other-token"}}
                )
            )
        self.store.login_completed(duplicate, {"success": True})
        self.assertFalse(self.store.project_allowed(duplicate, self.other))
        self.assertEqual(
            self.store.get(duplicate)["projectRules"],
            self.store.get("default")["projectRules"],
        )
        revision = self.store.get(duplicate)["projectRules"]["revision"]
        self.store.set_project_rules(duplicate, [], revision)
        self.assertFalse(self.store.project_allowed("default", self.project))

    def test_linked_worktree_belongs_to_project_and_subtree_stays_scoped(self):
        def git(*args):
            subprocess.run(
                ["git", "-C", str(self.project), *args], check=True, capture_output=True
            )

        git("init", "-q")
        (self.project / "src").mkdir()
        (self.project / "src/example.txt").write_text("fixture\n")
        git("add", "src/example.txt")
        git(
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        )
        linked = self.root / "linked"
        git("worktree", "add", "--detach", str(linked), "HEAD")
        self.rule([str(self.project)])
        self.assertTrue(self.store.project_allowed("default", linked))
        self.rule([str(self.project / "src")])
        self.assertTrue(self.store.project_allowed("default", linked / "src"))
        self.assertFalse(self.store.project_allowed("default", linked))
        # Merely pointing .git at a permitted repository does not register a worktree.
        (self.other / ".git").write_text("gitdir: " + str(self.project / ".git") + "\n")
        self.assertFalse(self.store.project_allowed("default", self.other))


if __name__ == "__main__":
    unittest.main()
