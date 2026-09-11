#!/usr/bin/env python3
"""Project account defaults at real runtime boundaries. No paid model requests."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location(
    "account_fixture", Path(__file__).with_name("runtime-accounts-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ProjectRuntime(unittest.TestCase):
    setUp = f.AccountContracts.setUp
    tearDown = f.AccountContracts.tearDown

    def legacy_rules(self, allowed, account="default"):
        with self.runtime.accounts.lock:
            self.runtime.accounts.data["accounts"][account]["projectRules"] = {
                "allowedProjects": allowed, "revision": 1}
            self.runtime.accounts._save()

    def lead(self, **data):
        return self.runtime.create(
            {"name": "Lead", "cwd": str(self.root), "prompt": "", **data}, draft=True
        )

    def bind(self, path, account):
        return self.runtime.projects({"action": "set_account", "path": str(path),
            "account_key": account, "expected_revision": 0})

    def test_legacy_rules_allow_create_send_prepare_and_worker(self):
        self.legacy_rules([])
        lead = self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "Go"}, defer=True)
        self.runtime.send(lead["id"], "Continue")
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.agent(lead["id"])["status"] == "running")
        child = self.runtime.create({"name": "Worker", "prompt": "Review", "role": "reviewer"},
                                    parent=lead["id"], defer=True)
        self.assertEqual(self.runtime.prepare(child)["accountKey"], "default")
        params = self.runtime.new_thread_params(lead)
        self.assertNotIn("Project admission policy", params["developerInstructions"])
        self.assertNotIn("Allowed project roots", params["developerInstructions"])
        self.assertEqual(params["sandbox"], "danger-full-access")
        self.assertEqual(params["approvalPolicy"], "never")

    def test_native_permissions_remain_independent_of_project_accounts(self):
        self.legacy_rules([])
        lead = self.lead(yolo_mode=False)
        params = self.runtime.new_thread_params(lead)
        self.assertEqual(params["sandbox"], "workspace-write")
        self.assertEqual(params["approvalPolicy"], "on-request")
        worker = self.runtime.create({"name": "Reviewer", "prompt": "Review", "role": "reviewer"},
                                     parent=lead["id"], defer=True)
        self.assertEqual(self.runtime.new_thread_params(worker)["sandbox"], "read-only")
        self.runtime.prepare(worker)

    def test_new_lead_uses_project_account_and_explicit_override(self):
        first, second = self.root / "first", self.root / "second"
        first.mkdir(); second.mkdir()
        self.bind(first, self.other_key)
        self.bind(second, "default")
        a = self.runtime.new_lead({"cwd": str(first)})
        self.assertEqual(a["accountKey"], self.other_key)
        self.runtime.send(a["id"], "Saved message")
        b = self.runtime.new_lead({"previous": a["id"], "cwd": str(second)})
        self.assertEqual(b["accountKey"], "default")
        override = self.runtime.new_lead({"cwd": str(second), "account_key": self.other_key})
        self.assertEqual(override["accountKey"], self.other_key)
        self.assertEqual(self.runtime.project_account(str(second)), "default")
        self.assertEqual(self.runtime.agent(a["id"])["accountKey"], self.other_key)

    def test_default_change_preserves_creation_retry_and_active_team(self):
        binding = self.bind(self.root, self.other_key)
        body = {"id": str(uuid.uuid4()), "cwd": str(self.root)}
        first = self.runtime.new_lead(body)
        self.runtime.send(first["id"], "Go")
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.agent(first["id"])["status"] == "running")
        child = self.runtime.create({"name": "Reviewer", "prompt": "Review", "role": "reviewer"},
                                     parent=first["id"], defer=True)
        self.runtime.projects({"action": "set_account", "path": str(self.root), "account_key": "default",
                               "expected_revision": binding["accountRevision"]})
        retried = self.runtime.new_lead(body)
        self.assertEqual(retried["id"], first["id"])
        self.assertEqual(retried["accountKey"], self.other_key)
        self.assertEqual(self.runtime.agent(first["id"])["status"], "running")
        self.assertEqual(self.runtime.agent(child["id"])["accountKey"], self.other_key)
        self.assertEqual(self.runtime.new_lead({"cwd": str(self.root)})["accountKey"], "default")

    def test_default_change_during_prepare_keeps_selected_account(self):
        binding = self.bind(self.root, self.other_key)
        a = self.lead()
        self.runtime.send(a["id"], "Go")
        prepare = self.runtime.prepare
        def changed(agent):
            ready = prepare(agent)
            self.runtime.projects({"action": "set_account", "path": str(self.root), "account_key": "default",
                                   "expected_revision": binding["accountRevision"]})
            return ready
        with patch.object(self.runtime, "prepare", side_effect=changed):
            self.runtime.dispatch()
            f.f.eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        self.assertEqual(self.runtime.agent(a["id"])["accountKey"], self.other_key)
        self.assertTrue(any(method == "turn/start" for method, _ in self.runtime.connect(self.other_key).calls))

    def test_account_and_folder_change_is_atomic_for_invalid_directory(self):
        self.legacy_rules([], self.other_key)
        a = self.lead()
        original = self.runtime.agent(a["id"])
        for folder in (str(self.root / "missing"), "", 42):
            with self.subTest(folder=folder), self.assertRaises(ValueError):
                self.runtime.set_account(a["id"], self.other_key, folder)
            self.assertEqual(self.runtime.agent(a["id"]), original)
        changed = self.runtime.set_account(a["id"], self.other_key, str(self.other))
        self.assertEqual((changed["accountKey"], changed["cwd"]), (self.other_key, str(self.other.resolve())))
        self.runtime.send(a["id"], "Start")
        with self.assertRaisesRegex(ValueError, "fixed after"):
            self.runtime.set_account(a["id"], "default", str(self.root))

    def test_loaded_thread_message_retry_remains_once_after_default_change(self):
        binding = self.bind(self.root, "default")
        a = self.runtime.prepare(self.lead())
        key = str(uuid.uuid4())
        self.runtime.send(a["id"], "Go", key)
        self.runtime.projects({"action": "set_account", "path": str(self.root), "account_key": self.other_key,
                               "expected_revision": binding["accountRevision"]})
        self.runtime.send(a["id"], "Go", key)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE id=?", (key,)).fetchone()[0], 1)
        self.assertEqual(self.runtime.agent(a["id"])["accountKey"], "default")

    def test_native_grants_use_actual_approval_without_project_allowlist(self):
        a = self.runtime.prepare(self.lead())
        self.legacy_rules([])
        server = self.runtime.connect()
        for method in ("item/commandExecution/requestApproval", "item/fileChange/requestApproval",
                       "execCommandApproval", "applyPatchApproval", "item/permissions/requestApproval",
                       "mcpServer/elicitation/request"):
            with self.subTest(method=method):
                identity = "conversationId" if method in {"execCommandApproval", "applyPatchApproval"} else "threadId"
                server.request({"id": method, "method": method, "params": {identity: a["threadId"]}})
                request = next(r for r in self.runtime.snapshot()["requests"] if r["rpcId"] == method)
                before = len(server.responses)
                self.runtime.answer(request["id"], {"decision": "accept"})
                self.assertEqual(len(server.responses), before + 1)

    def test_subproject_worker_keeps_relative_directory_in_git_worktree(self):
        import subprocess

        repo = self.root / "repository"
        project = repo / "packages" / "lumina"
        project.mkdir(parents=True)
        (project / "README.md").write_text("Project fixture\n")
        for args in (
            ["init", "--quiet"],
            ["add", "."],
            ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
             "commit", "--quiet", "-m", "Fixture"],
        ):
            subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)
        lead = self.lead(cwd=str(project))
        child = self.runtime.create(
            {"name": "Worker", "prompt": "Implement"}, parent=lead["id"], defer=True
        )
        ready = self.runtime.prepare(child)
        worktree = repo / ".worktrees" / "codex-agents" / child["id"]
        self.assertEqual(Path(ready["cwd"]), (worktree / "packages" / "lumina").resolve())
        self.assertTrue(Path(ready["cwd"], "README.md").is_file())
        self.assertTrue((worktree / ".git").is_file())
        self.assertTrue(ready["worktreeReady"])
        params = next(params for method, params in self.runtime.connect().calls if method == "thread/start")
        self.assertEqual(params["cwd"], ready["cwd"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
