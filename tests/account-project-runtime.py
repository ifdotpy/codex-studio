#!/usr/bin/env python3
"""Project admission at runtime boundaries. Fixtures make no model requests."""

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

    def rules(self, allowed, account="default"):
        old = self.runtime.accounts.get(account)["projectRules"]
        self.runtime.accounts.set_project_rules(account, allowed, old["revision"])

    def lead(self, **data):
        return self.runtime.create(
            {"name": "Lead", "cwd": str(self.root), "prompt": "", **data}, draft=True
        )

    def test_denied_create_has_no_agent_or_native_connection(self):
        self.rules([])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "Go"})
        self.assertEqual(self.runtime.snapshot()["agents"], [])
        self.assertEqual(self.runtime.servers, {})

    def test_empty_lead_exists_but_send_and_prepare_reject(self):
        self.rules([])
        a = self.lead()
        for action in (
            lambda: self.runtime.send(a["id"], "Go"),
            lambda: self.runtime.prepare(a),
        ):
            with self.assertRaisesRegex(ValueError, "cannot use project"):
                action()
        self.assertEqual(self.runtime.servers, {})
        with self.runtime.db() as db:
            self.assertEqual(
                db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0], 0
            )

    def test_override_is_explicit_boolean_and_does_not_change_native_permissions(self):
        self.rules([])
        for value in ("true", "false", 1, None):
            with self.assertRaisesRegex(ValueError, "boolean"):
                self.lead(dangerously_skip_rules=value)
        a = self.lead(dangerously_skip_rules=True)
        a = self.runtime.prepare(a)
        self.assertEqual(a["sandbox"], {"type": "readOnly"})
        self.assertEqual(a["approvalPolicy"], "on-request")
        params = self.runtime.new_thread_params(a)
        self.assertNotIn("approvalPolicy", params)
        self.assertNotIn("dangerouslySkipAccountRules", params)
        self.assertNotIn("dangerously_skip_rules", params)

    def test_child_cannot_grant_override_and_reads_latest_root(self):
        a = self.lead()
        child = self.runtime.create(
            {"name": "Worker", "prompt": "Review", "role": "reviewer"},
            parent=a["id"],
            defer=True,
        )
        for skip in (True, "true"):
            with self.assertRaises(ValueError):
                self.runtime.create(
                    {
                        "name": "Worker",
                        "prompt": "Review",
                        "dangerously_skip_rules": skip,
                    },
                    parent=a["id"],
                    defer=True,
                )
        self.rules([])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.check_account_project(child)
        self.runtime.conversation_settings(a["id"], {"dangerously_skip_rules": True})
        self.runtime.check_account_project(child)
        self.assertFalse(child["dangerouslySkipAccountRules"])
        inherited = self.runtime.create(
            {"name": "Inherited", "prompt": "Review", "role": "reviewer"},
            parent=a["id"],
            defer=True,
        )
        self.assertTrue(inherited["dangerouslySkipAccountRules"])
        self.runtime.conversation_settings(a["id"], {"dangerously_skip_rules": False})
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.check_account_project(inherited)
        with self.assertRaisesRegex(ValueError, "Only a lead"):
            self.runtime.conversation_settings(
                child["id"], {"dangerously_skip_rules": True}
            )

    def test_running_descendant_prevents_override_change(self):
        a = self.lead()
        child = self.runtime.create(
            {"name": "Worker", "prompt": "Review", "role": "reviewer"},
            parent=a["id"],
            defer=True,
        )
        with self.runtime.lock, self.runtime.db() as db:
            child.update(inFlight=True, status="running")
            self.runtime.put(db, "agents", child)
        with self.assertRaisesRegex(ValueError, "every team turn"):
            self.runtime.conversation_settings(
                a["id"], {"dangerously_skip_rules": True}
            )
        self.assertFalse(self.runtime.agent(a["id"])["dangerouslySkipAccountRules"])

    def test_queued_work_checks_changed_rule_before_prepare(self):
        a = self.lead()
        self.runtime.send(a["id"], "Go")
        self.rules([])
        self.runtime.dispatch()
        f.f.eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        self.assertEqual(self.runtime.servers, {})
        self.assertIn("cannot use project", self.runtime.agent(a["id"])["error"])

    def test_turn_rechecks_rule_after_prepare(self):
        a = self.lead()
        self.runtime.send(a["id"], "Go")
        prepare = self.runtime.prepare

        def changed(agent):
            ready = prepare(agent)
            self.rules([])
            return ready

        with patch.object(self.runtime, "prepare", side_effect=changed):
            self.runtime.dispatch()
            f.f.eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        calls = self.runtime.connect().calls
        self.assertTrue(any(method == "thread/start" for method, _ in calls))
        self.assertFalse(any(method == "turn/start" for method, _ in calls))

    def test_loaded_thread_next_send_steer_and_resume_recheck(self):
        a = self.runtime.prepare(self.lead())
        self.rules([])
        for action in (
            lambda: self.runtime.prepare(a),
            lambda: self.runtime.send(a["id"], "Go"),
            lambda: self.runtime.send(a["id"], "Go", delivery="steer"),
            lambda: self.runtime.native_action(a["id"], "review"),
            lambda: self.runtime.native_action(a["id"], "compact"),
        ):
            with self.assertRaisesRegex(ValueError, "cannot use project"):
                action()
        self.assertEqual(len(self.runtime.connect().calls), 1)

    def test_unchanged_message_retry_cannot_replay_after_rule_change(self):
        a = self.lead()
        key = str(uuid.uuid4())
        self.runtime.send(a["id"], "Go", key)
        self.rules([])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.send(a["id"], "Go", key)

    def test_account_and_project_settings_check_candidate_without_partial_save(self):
        a = self.lead()
        self.rules([], self.other_key)
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.set_account(a["id"], self.other_key)
        self.assertEqual(self.runtime.agent(a["id"])["accountKey"], "default")
        allowed = self.root / "allowed"
        allowed.mkdir()
        self.rules([str(allowed)])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.conversation_settings(a["id"], {"cwd": str(self.other)})
        self.runtime.conversation_settings(
            a["id"], {"cwd": str(self.other), "dangerously_skip_rules": True}
        )
        self.assertEqual(self.runtime.agent(a["id"])["cwd"], str(self.other.resolve()))

    def test_new_lead_chooses_allowed_project_and_reuses_empty_identity(self):
        allowed = self.root / "allowed"
        allowed.mkdir()
        self.rules([str(allowed)], self.other_key)
        a = self.lead()
        b = self.runtime.new_lead({"previous": a["id"], "account_key": self.other_key})
        self.assertEqual(a["id"], b["id"])
        self.assertEqual(b["cwd"], str(allowed.resolve()))
        self.runtime.send(b["id"], "Go")
        c = self.runtime.new_lead({"previous": b["id"]})
        self.assertNotEqual(c["id"], b["id"])
        self.assertEqual(c["cwd"], str(allowed.resolve()))

    def test_monitor_files_git_and_capabilities_reject(self):
        a = self.lead()
        (self.root / "file.txt").write_text("fixture")
        self.rules([])
        for action in (
            lambda: self.runtime.monitor(a["id"], {"command": "fixture"}),
            lambda: self.runtime.workspace_path(a["id"], "file.txt"),
            lambda: self.runtime.file_content(a["id"], "file.txt"),
            lambda: self.runtime.git(a, ["status"]),
            lambda: self.runtime.restore_checkpoint(a["id"], {}),
            lambda: self.runtime.capabilities(a["id"]),
        ):
            with self.assertRaisesRegex(ValueError, "cannot use project"):
                action()
        self.assertEqual(self.runtime.servers, {})

    def test_import_rejects_before_reading_messages(self):
        self.rules([])
        server = self.runtime.connect()
        calls = []

        def read(method, params, **kwargs):
            calls.append(method)
            self.assertEqual(method, "thread/read")
            return {"thread": {"cwd": str(self.root)}}

        with patch.object(server, "call", side_effect=read):
            with self.assertRaisesRegex(ValueError, "cannot use project"):
                self.runtime.import_thread({"threadId": "native-thread"})
        self.assertEqual(calls, ["thread/read"])

    def test_branch_does_not_copy_team_exception(self):
        a = self.runtime.prepare(self.lead(dangerously_skip_rules=True))
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(
                db, a["id"], "answer", "assistant", "Done", turnId="finished-turn"
            )
        self.rules([])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.branch_conversation(
                a["id"], {"message_id": a["id"] + ":answer"}
            )
        self.assertFalse(
            any(method == "thread/fork" for method, _ in self.runtime.connect().calls)
        )

    def test_pending_monitor_rechecks_changed_rule_before_execution(self):
        a = self.lead()
        monitor = self.runtime.monitor(a["id"], {"command": "fixture"})
        self.rules([])
        with self.runtime.lock, self.runtime.db() as db:
            monitor["status"] = "starting"
            self.runtime.put(db, "monitors", monitor)
        self.runtime.run_monitor(monitor["id"])
        with self.runtime.db() as db:
            saved = next(
                m
                for m in self.runtime.records(db, "monitors")
                if m["id"] == monitor["id"]
            )
        self.assertEqual(saved["status"], "failed")
        self.assertIn("cannot use project", saved["error"])
        self.assertEqual(self.runtime.servers, {})

    def test_changed_rule_pauses_file_watch_without_reading_file(self):
        a = self.lead()
        (self.root / "watched").write_text("fixture")
        rule = self.runtime.rules(
            {"agent": a["id"], "name": "Watch", "kind": "file", "path": "watched"}
        )
        self.rules([])
        with patch.object(
            self.runtime,
            "file_fingerprint",
            side_effect=AssertionError("Unexpected file read"),
        ):
            self.runtime.rules_tick()
        saved = next(r for r in self.runtime.rules()["rules"] if r["id"] == rule["id"])
        self.assertEqual(saved["status"], "paused")
        self.assertIn("cannot use project", saved["error"])

    def test_account_project_policy_is_in_native_instructions(self):
        self.rules([str(self.root)])
        a = self.lead()
        instructions = self.runtime.new_thread_params(a)["developerInstructions"]
        self.assertIn("Project admission policy", instructions)
        self.assertIn(str(self.root.resolve()), instructions)
        self.runtime.conversation_settings(a["id"], {"dangerously_skip_rules": True})
        instructions = self.runtime.new_thread_params(a)["developerInstructions"]
        self.assertIn("The user enabled Dangerously skip rules", instructions)
        self.assertIn(
            "Native Codex permissions and sandbox rules still apply", instructions
        )

    def test_override_survives_restart(self):
        a = self.lead(dangerously_skip_rules=True)
        self.rules([])
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.AccountServer)
        self.runtime.check_account_project(self.runtime.agent(a["id"]))
        self.assertTrue(self.runtime.agent(a["id"])["dangerouslySkipAccountRules"])

    def test_pending_native_grants_recheck_policy_but_decline_remains_available(self):
        a = self.runtime.prepare(self.lead())
        server = self.runtime.connect()
        for method in (
            "item/commandExecution/requestApproval", "item/fileChange/requestApproval",
            "execCommandApproval", "applyPatchApproval", "item/permissions/requestApproval",
            "mcpServer/elicitation/request",
        ):
            with self.subTest(method=method):
                self.rules(None)
                server.request({"id": method, "method": method, "params": {"threadId": a["threadId"]}})
                request = next(r for r in self.runtime.snapshot()["requests"] if r["rpcId"] == method)
                before = len(server.responses)
                self.rules([])
                with self.assertRaisesRegex(ValueError, "cannot use project"):
                    self.runtime.answer(request["id"], {"decision": "accept"})
                self.assertEqual(len(server.responses), before)
                self.runtime.answer(request["id"], {"decision": "decline"})
                self.assertEqual(len(server.responses), before + 1)

    def test_unscoped_native_grant_cannot_use_restricted_account(self):
        server = self.runtime.connect()
        server.request({"id": "unscoped", "method": "item/permissions/requestApproval", "params": {}})
        with self.runtime.db() as db:
            request = next(r for r in self.runtime.records(db, "requests") if r["rpcId"] == "unscoped")
        self.rules([str(self.root)])
        with self.assertRaisesRegex(ValueError, "project identity"):
            self.runtime.answer(request["id"], {"decision": "accept"})
        self.runtime.answer(request["id"], {"decision": "decline"})


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
        self.rules([str(project)])
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
        self.runtime.check_account_project(ready)
        self.assertFalse(self.runtime.accounts.project_allowed("default", str(worktree)))
        params = next(params for method, params in self.runtime.connect().calls if method == "thread/start")
        self.assertEqual(params["cwd"], ready["cwd"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
