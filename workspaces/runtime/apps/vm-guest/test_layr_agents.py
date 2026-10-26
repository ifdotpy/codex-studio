"""Owned line setup rolls back after failures at privileged boundaries."""
from __future__ import annotations

import os
import shutil
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError, atomic_json
import layr_agents
from layr_agents import AgentHandlers, PRIVATE_FOLDER, REVIEWER_DENY, REVIEWERS_GROUP


class ReviewerLines(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
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
        self.calls, self.lines, self.users = [], set(), []
        self.failure = None

        async def command(cwd, arguments, **options):
            self.calls.append(tuple(arguments))
            self.users.append((tuple(arguments), options.get("user")))
            if arguments[:2] == ["setfacl", "--version"] and self.failure == "preflight":
                raise GuestError("operation_failed", "setfacl is absent")
            if arguments[:2] == ["setfacl", "-RP"] and self.failure in {"acl", "rollback"}:
                raise GuestError("operation_failed", "forced ACL failure after creation")
            if arguments[0] == "btrfs" and arguments[-1] == "true" and self.failure == "readonly":
                raise GuestError("operation_failed", "forced read-only flag failure")
            return ""

        async def layr(context, *arguments, **options):
            self.calls.append(tuple(arguments))
            if arguments[:3] == ("line", "delete", "-D"):
                if self.failure == "rollback":
                    raise GuestError("operation_failed", "forced rollback failure")
                self.lines.remove(self.agent_id)
                shutil.rmtree(self.target)
            elif arguments[:2] == ("line", "new"):
                self.target.mkdir()
                self.lines.add(self.agent_id)
            elif context["agentId"] == self.agent_id and self.failure == "head":
                raise GuestError("operation_failed", "forced head failure after association publication")
            if arguments[0] == "rev-parse":
                self.assertEqual(arguments[1], "head")
            return "state-0"

        self.handlers.command, self.handlers.layr = command, layr
        self.params = {"agentId": self.agent_id, "parentAgentId": self.parent_id,
                       "projectId": "proof", "stateId": "state-0", "readOnly": True}

    async def asyncTearDown(self):
        self.temp.cleanup()

    async def branch(self):
        with patch("layr_agents._user", return_value=self.user), \
                patch("layr_agents.join_project_group", side_effect=lambda user, project: user), \
                patch("layr_agents.ensure_group"), \
                patch("layr_agents.join_reviewers", side_effect=lambda user: self.calls.append(("join-reviewers",))):
            return await self.handlers.dispatch("reviewer-create", "line.branch", self.params, None)

    async def test_a_reviewer_reads_through_layr_and_writes_only_its_scratch_folder(self):
        result = await self.branch()
        # layr takes the line-changing actions away from reviewers before the user joins the group.
        deny = ("--project", "proof", "access", "deny", "@" + REVIEWERS_GROUP, *REVIEWER_DENY)
        self.assertIn(deny, self.calls)
        self.assertLess(self.calls.index(deny), self.calls.index(("join-reviewers",)))
        self.assertTrue({"line.create", "merge", "line.manage", "push"} <= set(REVIEWER_DENY))
        self.assertNotIn("try", REVIEWER_DENY)
        self.assertNotIn("read", REVIEWER_DENY)
        # The reviewer makes its own private folder, which is its TMPDIR.
        scratch = str(self.root / ".studio/scratch" / self.agent_id)
        self.assertEqual(result["scratch"], scratch)
        self.assertIn((("python3", "-c", PRIVATE_FOLDER, scratch), self.user), self.users)
        context = self.handlers.context(self.agent_id)
        self.assertEqual(self.handlers.agent_env(context)["TMPDIR"], scratch)
        self.assertIn("TMPDIR", layr_agents.LAUNCH_ENV_NAMES)

    async def test_missing_acl_does_not_create_a_line(self):
        self.failure = "preflight"
        with self.assertRaises(GuestError):
            await self.branch()
        self.assertFalse(self.target.exists())
        self.assertFalse(self.record.exists())
        self.assertFalse(any(call[:2] == ("line", "new") for call in self.calls))

    async def test_acl_failure_after_creation_removes_line_and_association(self):
        self.failure = "acl"
        with self.assertRaisesRegex(GuestError, "forced ACL failure"):
            await self.branch()
        self.assertIn(("line", "delete", "-D", self.agent_id), self.calls)
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



class StateEvidence(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        with patch.object(AgentHandlers, "join_project_groups"):
            self.temp = tempfile.TemporaryDirectory()
            self.addCleanup(self.temp.cleanup)
            self.handlers = AgentHandlers(Path(self.temp.name) / "state", Path(self.temp.name) / "store")
        self.lead = {"agentId": "lead", "projectId": "proof", "line": "main", "readOnly": False}
        self.worker = {"agentId": "worker", "projectId": "proof", "line": "worker", "readOnly": False}
        self.handlers.context = lambda agent: self.lead if agent == "lead" else self.worker
        self.state_uuid = "01a17245-dfbb-76b3-850d-c214d39dd76e"
        self.display = "c214d39dd76e76b3850d01a17245dfbb"
        self.stale_uuid = "01a17245-6bbd-735d-83ec-a7df12501445"
        self.stale_display = "a7df12501445735d83ec01a172456bbd"
        self.calls = []

        async def layr(context, *args, **options):
            self.calls.append((context["agentId"], args))
            if args[0] == "rev-parse":
                return {self.state_uuid: self.display, self.display: self.display,
                        self.stale_uuid: self.stale_display, self.stale_display: self.stale_display,
                        "head": self.display}[args[-1]]
            return "merged"
        self.handlers.layr = layr

    async def test_evidence_accepts_both_exact_state_forms_and_preserves_the_request(self):
        for state in (self.state_uuid, self.display):
            with self.subTest(state=state):
                result = await self.handlers.dispatch_locked("evidence", "line.evidence",
                    {"agentId": "worker", "stateId": state}, None)
                self.assertEqual(result["stateId"], state)

    async def test_merge_accepts_both_exact_state_forms_with_an_exact_expect(self):
        for state in (self.state_uuid, self.display):
            with self.subTest(state=state):
                self.calls.clear()
                result = await self.handlers.dispatch_locked("merge", "line.merge",
                    {"agentId": "lead", "sourceAgentId": "worker", "expectedStateId": state}, None)
                self.assertEqual(result["reviewedStateId"], state)
                self.assertIn(("lead", ("merge", "worker", "--expect", self.display)), self.calls)

    async def test_stale_ids_and_moving_refs_never_merge(self):
        for state in (self.stale_uuid, self.stale_display, "head", "worker", "--show-toplevel",
                      None, "", "bad", "{" + self.state_uuid + "}", "urn:uuid:" + self.state_uuid):
            with self.subTest(state=state):
                self.calls.clear()
                with self.assertRaises(GuestError):
                    await self.handlers.dispatch_locked("evidence", "line.evidence",
                        {"agentId": "worker", "stateId": state}, None)
                with self.assertRaises(GuestError):
                    await self.handlers.dispatch_locked("merge", "line.merge",
                        {"agentId": "lead", "sourceAgentId": "worker", "expectedStateId": state}, None)
                self.assertFalse(any(args[0] == "merge" for _, args in self.calls))


class TaskHandoff(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        (self.root / "store/projects/proof/lines/main").mkdir(parents=True)
        with patch.object(AgentHandlers, "join_project_groups"):
            self.handlers = AgentHandlers(self.root / "state", self.root / "store")
        self.lead_id, self.worker_id = str(uuid.uuid4()), str(uuid.uuid4())
        (self.root / "store/projects/proof/lines" / self.worker_id).mkdir()
        ids = {"owner": "lead", "uid": os.getuid(), "gid": os.getgid(), "home": str(self.root), "readOnly": False}
        atomic_json(self.handlers.record_path(self.lead_id),
                    {"agentId": self.lead_id, "projectId": "proof", "line": "main", **ids})
        atomic_json(self.handlers.record_path(self.worker_id),
                    {"agentId": self.worker_id, "projectId": "proof", "line": self.worker_id, **ids})
        self.calls = []

        async def command(cwd, arguments, **options):
            self.calls.append(tuple(arguments))
            return ""

        self.handlers.command = command
        group = type("Group", (), {"gr_gid": os.getgid()})()
        self.group = patch("layr_agents.grp.getgrnam", return_value=group)
        self.group.start()

    async def asyncTearDown(self):
        self.group.stop()
        self.temp.cleanup()

    async def ensure(self, agent, task):
        return await self.handlers.dispatch("handoff-" + task, "handoff.ensure", {"agentId": agent, "taskId": task}, None)

    async def test_the_lead_gets_a_task_folder_that_the_project_group_reads(self):
        task = str(uuid.uuid4())
        result = await self.ensure(self.lead_id, task)
        folder = Path(result["path"])
        self.assertEqual(folder, self.root / "handoff/proof" / task)
        self.assertEqual(result["root"], str(self.root / "handoff/proof"))
        self.assertEqual(stat.S_IMODE(folder.stat().st_mode), 0o2750)
        self.assertEqual(stat.S_IMODE(folder.parent.stat().st_mode), 0o750)
        self.assertEqual(stat.S_IMODE(folder.parent.parent.stat().st_mode), 0o711)
        # New files are readable by the group whatever the umask of the lead.
        self.assertEqual(self.calls, [("setfacl", "-d", "-m", "u::rwX,g::r-X,o::---", str(folder))])
        self.assertEqual(await self.ensure(self.lead_id, task), result)
        self.assertEqual(len(self.calls), 1)

    async def test_only_the_lead_has_task_folders(self):
        with self.assertRaises(GuestError):
            await self.ensure(self.worker_id, str(uuid.uuid4()))
        with self.assertRaises(ValueError):
            await self.ensure(self.lead_id, "../escape")
        self.assertFalse((self.root / "handoff/proof").exists())


if __name__ == "__main__":
    unittest.main()
