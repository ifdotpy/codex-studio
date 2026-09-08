#!/usr/bin/env python3
"""Regression contracts for durable workspace recovery."""

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "workspace_contract_fixture", HERE / "workspace-contract.py"
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_native_errors import NativeRpcError


class CriticalWorkspaceContract(fixture.WorkspaceContract):
    def test_restore_failure_keeps_durable_recovery_state(self):
        worker, path = self.isolated_worker()
        self.agent_update(worker, threadId="original-thread")
        checkpoint = self.runtime.checkpoint_capture(
            worker["id"], turn_id="completed-turn"
        )
        (path / "tracked.txt").write_text("new edit\n")
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        original_git = self.runtime.git

        def reset_then_fail(agent, args, env=None, input=None):
            result = original_git(agent, args, env, input)
            if args[:3] == ["read-tree", "--reset", "-u"]:
                raise RuntimeError("local reset failed after files changed")
            return result

        with patch.object(self.runtime, "git", side_effect=reset_then_fail):
            with self.assertRaisesRegex(RuntimeError, "local reset failed"):
                self.runtime.restore_checkpoint(
                    worker["id"],
                    {
                        "checkpoint_id": checkpoint["id"],
                        "expectedTree": preview["expectedTree"],
                    },
                )

        agent = self.runtime.agent(worker["id"])
        self.assertIsNotNone(agent.get("workspaceOperation"))
        self.assertFalse(agent["autoWake"])
        self.assertEqual(agent["status"], "interrupted")
        result = self.runtime.restore_checkpoint(
            worker["id"],
            {
                "checkpoint_id": checkpoint["id"],
                "expectedTree": preview["expectedTree"],
            },
        )
        self.assertEqual(result["status"], "restored")
        self.assertIsNone(self.runtime.agent(worker["id"]).get("workspaceOperation"))
        self.assertEqual(
            sum(method == "thread/fork" for method, _ in self.runtime.server.calls),
            1,
        )

    def test_restore_recovery_survives_restart_without_native_retry(self):
        worker, path = self.isolated_worker()
        self.agent_update(worker, threadId="original-thread")
        checkpoint = self.runtime.checkpoint_capture(
            worker["id"], turn_id="completed-turn"
        )
        (path / "tracked.txt").write_text("new edit\n")
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        original_git = self.runtime.git

        def reset_then_fail(agent, args, env=None, input=None):
            result = original_git(agent, args, env, input)
            if args[:3] == ["read-tree", "--reset", "-u"]:
                raise RuntimeError("local reset failed after files changed")
            return result

        with patch.object(self.runtime, "git", side_effect=reset_then_fail):
            with self.assertRaisesRegex(RuntimeError, "local reset failed"):
                self.runtime.restore_checkpoint(
                    worker["id"],
                    {
                        "checkpoint_id": checkpoint["id"],
                        "expectedTree": preview["expectedTree"],
                    },
                )
        self.runtime.close()
        self.runtime = fixture.ControlledRuntime(self.state, fixture.WorkspaceServer)
        recovered_preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        self.assertEqual(recovered_preview["expectedTree"], preview["expectedTree"])
        self.assertTrue(recovered_preview["canRestore"])
        result = self.runtime.restore_checkpoint(
            worker["id"],
            {
                "checkpoint": checkpoint["id"],
                "expectedTree": recovered_preview["expectedTree"],
            },
        )
        self.assertEqual(result["status"], "restored")
        self.runtime.prepare(self.runtime.agent(worker["id"]))
        methods = [
            method
            for server in self.runtime.servers.values()
            for method, _ in server.calls
        ]
        self.assertIn("thread/resume", methods)
        self.assertNotIn("thread/fork", methods)
        self.assertFalse(
            any(
                method == "thread/fork"
                for server in self.runtime.servers.values()
                for method, _ in server.calls
            )
        )

    def test_branch_unknown_provider_result_blocks_exact_retry(self):
        lead = self.start(self.lead())
        self.runtime.notification(
            {
                "method": "item/completed",
                "params": {
                    "threadId": lead["threadId"],
                    "turnId": lead["turnId"],
                    "item": {
                        "id": "branch-answer-unknown",
                        "type": "agentMessage",
                        "text": "Branch point",
                    },
                },
            }
        )
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        message = next(
            item
            for item in self.runtime.transcript(lead["id"])["items"]
            if item["text"] == "Branch point"
        )
        request = {"id": "durable-unknown-branch", "message_id": message["id"]}
        self.runtime.server.fork_error = NativeRpcError(
            {"code": -32000, "message": "Provider rejected fork"}
        )
        with self.assertRaisesRegex(RuntimeError, "Provider rejected"):
            self.runtime.branch_conversation(lead["id"], request)
        self.runtime.server.fork_error = RuntimeError(
            "thread/fork response timed out; outcome unknown"
        )
        with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
            self.runtime.branch_conversation(lead["id"], request)
        with self.assertRaisesRegex(ValueError, "recovery"):
            self.runtime.branch_conversation(lead["id"], request)
        self.assertEqual(
            sum(method == "thread/fork" for method, _ in self.runtime.server.calls),
            2,
        )

    def test_branch_failure_after_provider_fork_does_not_fork_again(self):
        lead = self.start(self.lead())
        self.runtime.notification(
            {
                "method": "item/completed",
                "params": {
                    "threadId": lead["threadId"],
                    "turnId": lead["turnId"],
                    "item": {
                        "id": "branch-answer",
                        "type": "agentMessage",
                        "text": "Branch point",
                    },
                },
            }
        )
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        message = next(
            item
            for item in self.runtime.transcript(lead["id"])["items"]
            if item["text"] == "Branch point"
        )
        request = {"id": "durable-branch", "message_id": message["id"]}
        original_save = self.runtime.save_receipt
        failed = [False]

        def fail_once(db, key, signature, result):
            if not failed[0]:
                failed[0] = True
                raise RuntimeError("receipt write failed")
            return original_save(db, key, signature, result)

        with patch.object(self.runtime, "save_receipt", side_effect=fail_once):
            with self.assertRaisesRegex(RuntimeError, "receipt write failed"):
                self.runtime.branch_conversation(lead["id"], request)

        branch = self.runtime.branch_conversation(lead["id"], request)
        self.assertEqual(branch["forkedFrom"], lead["id"])
        forks = [method for method, _ in self.runtime.server.calls if method == "thread/fork"]
        self.assertEqual(len(forks), 1)


if __name__ == "__main__":
    unittest.main()
