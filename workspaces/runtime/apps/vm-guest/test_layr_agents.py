"""Owned line setup rolls back after failures at privileged boundaries."""
from __future__ import annotations

import shutil
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError, atomic_json
from layr_agents import AgentHandlers


class ReviewerLines(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "store/projects/proof"
        (self.project / "lines/main").mkdir(parents=True)
        # Project groups need root; these tests cover the line setup only.
        with patch.object(AgentHandlers, "join_project_groups"):
            self.handlers = AgentHandlers(self.root / "state", self.root / "store")
        self.parent_id, self.agent_id = str(uuid.uuid4()), str(uuid.uuid4())
        self.target = self.project / "lines" / self.agent_id
        self.record = self.handlers.record_path(self.agent_id)
        self.parent = {"agentId": self.parent_id, "projectId": "proof", "line": "main",
                       "owner": "parent", "uid": 1010, "gid": 1010, "home": str(self.root), "readOnly": False}
        atomic_json(self.handlers.record_path(self.parent_id), self.parent)
        self.user = {"owner": "reviewer", "uid": 1011, "gid": 1011, "home": str(self.root)}
        self.calls, self.lines = [], set()
        self.failure = None

        async def command(cwd, arguments, **options):
            self.calls.append(tuple(arguments))
            if arguments[:2] == ["setfacl", "--version"] and self.failure == "preflight":
                raise GuestError("operation_failed", "setfacl is absent")
            if arguments[:2] == ["setfacl", "-RP"] and self.failure in {"acl", "rollback"}:
                raise GuestError("operation_failed", "forced ACL failure after creation")
            if arguments[0] == "btrfs" and arguments[-1] == "true" and self.failure == "readonly":
                raise GuestError("operation_failed", "forced read-only flag failure")
            return ""

        async def layr(context, *arguments, **options):
            self.calls.append(tuple(arguments))
            if arguments[:2] == ("branch", "-D"):
                if self.failure == "rollback":
                    raise GuestError("operation_failed", "forced rollback failure")
                self.lines.remove(self.agent_id)
                shutil.rmtree(self.target)
            elif arguments[0] == "branch":
                self.target.mkdir()
                self.lines.add(self.agent_id)
            elif context["agentId"] == self.agent_id and self.failure == "head":
                raise GuestError("operation_failed", "forced head failure after association publication")
            return "state-0"

        self.handlers.command, self.handlers.layr = command, layr
        self.params = {"agentId": self.agent_id, "parentAgentId": self.parent_id,
                       "projectId": "proof", "stateId": "state-0", "readOnly": True}

    async def asyncTearDown(self):
        self.temp.cleanup()

    async def branch(self):
        with patch("layr_agents._user", return_value=self.user), \
                patch("layr_agents.join_project_group", side_effect=lambda user, project: user):
            return await self.handlers.dispatch("reviewer-create", "line.branch", self.params, None)

    async def test_missing_acl_does_not_create_a_line(self):
        self.failure = "preflight"
        with self.assertRaises(GuestError):
            await self.branch()
        self.assertFalse(self.target.exists())
        self.assertFalse(self.record.exists())
        self.assertFalse(any(call[0] == "branch" for call in self.calls))

    async def test_acl_failure_after_creation_removes_line_and_association(self):
        self.failure = "acl"
        with self.assertRaisesRegex(GuestError, "forced ACL failure"):
            await self.branch()
        self.assertIn(("branch", "-D", self.agent_id), self.calls)
        self.assertFalse(self.target.exists())
        self.assertFalse(self.record.exists())
        self.assertNotIn(self.agent_id, self.lines)

    async def test_later_failures_also_remove_line_and_association(self):
        for failure in ("readonly", "head"):
            with self.subTest(failure=failure):
                self.failure = failure
                with self.assertRaises(GuestError):
                    await self.branch()
                self.assertFalse(self.target.exists())
                self.assertFalse(self.record.exists())
                self.assertNotIn(self.agent_id, self.lines)

    async def test_failed_rollback_reports_uncertainty_without_an_association(self):
        self.failure = "rollback"
        with self.assertRaises(GuestError) as error:
            await self.branch()
        self.assertEqual(error.exception.code, "outcome_unknown")
        self.assertTrue(self.target.exists())
        self.assertFalse(self.record.exists())

    async def test_success_publishes_only_the_complete_read_only_line(self):
        result = await self.branch()
        self.assertTrue(result["readOnly"])
        self.assertEqual(result["stateId"], "state-0")
        self.assertTrue(self.record.is_file())
        self.assertIn(("btrfs", "property", "set", "-ts", str(self.target), "ro", "true"), self.calls)


if __name__ == "__main__":
    unittest.main()
