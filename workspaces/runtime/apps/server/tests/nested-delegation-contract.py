#!/usr/bin/env python3
"""Nested delegation policy, root budgets and restart persistence without providers."""

from test_isolation import isolate_supervisor_environment

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

# Support direct execution without runner-provided PYTHONPATH.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
isolate_supervisor_environment()
from runtime_test_environment import make_runtime

from server_test_waits import wait_for_condition

spec = importlib.util.spec_from_file_location(
    "nested_delegation_fixture", Path(__file__).with_name("worker-defaults-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class NestedDelegationContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="nested-delegation-")
        self.root = Path(self.temp.name).resolve()
        self.state = self.root / "state"
        self.runtime = self.open_runtime()
        self.lead = self.runtime.new_lead({"cwd": str(self.root)})

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def open_runtime(self):
        runtime = make_runtime(fixture.ControlledRuntime, self.state, fixture.f.FakeServer)
        runtime.catalog = lambda account="default": fixture.copy.deepcopy(fixture.CATALOG)
        return runtime

    def set_depth(self, depth):
        defaults = self.runtime.agent(self.lead["id"])["workerDefaults"]
        return self.runtime.conversation_settings(self.lead["id"], {
            "worker_defaults": {
                "model": defaults["model"],
                "effort": defaults["effort"],
                "fast_mode": defaults["fastMode"],
                "delegation_depth": depth,
            },
        })

    def spawn(self, actor, key):
        result = self.runtime.spawn_agents(actor, {"agents": [{
            "name": key,
            "prompt": "Do the assigned check",
            "role": "implementer",
        }]}, key)["agents"][0]
        return self.runtime.agent(result["id"])

    def tool_names(self, actor):
        return {tool["name"] for tool in self.runtime.tool_definitions(actor)}

    def freeze_tools(self, actor, provider="codex"):
        actor = self.runtime.agent(actor["id"])
        actor["provider"] = provider
        actor["frozenNativeParams"] = {"dynamicTools": self.runtime.tool_definitions(actor)}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", actor)
        return self.runtime.agent(actor["id"])

    def test_default_is_lead_only_and_depth_two_stops_at_grandchild(self):
        worker = self.spawn(self.lead, "depth-one-worker")
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        worker = self.freeze_tools(worker)
        self.assertEqual(self.runtime.agent(self.lead["id"])["workerDefaults"]["delegationDepth"], 1)
        with self.assertRaisesRegex(ValueError, "delegation depth"):
            self.spawn(worker, "blocked-child-default")

        self.set_depth(2)
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        self.assertNotIn("orchestration_spawn", self.tool_names(self.freeze_tools(worker, "claude")))
        new_worker = self.spawn(self.lead, "depth-two-new-session")
        self.assertIn("orchestration_spawn", self.tool_names(new_worker))
        grandchild = self.spawn(worker, "depth-two-grandchild")
        self.assertEqual(grandchild["parentId"], worker["id"])
        self.assertEqual(grandchild["rootId"], self.lead["id"])
        self.assertNotIn("orchestration_spawn", self.tool_names(grandchild))
        grandchild = self.freeze_tools(grandchild, "claude")
        self.assertNotIn("orchestration_spawn", self.tool_names(grandchild))
        with self.assertRaisesRegex(ValueError, "delegation depth"):
            self.spawn(grandchild, "blocked-great-grandchild")

    def test_depth_three_and_worker_task_assignment_rules(self):
        self.set_depth(3)
        worker = self.spawn(self.lead, "level-one")
        grandchild = self.spawn(worker, "level-two")
        self.assertIn("orchestration_spawn", self.tool_names(grandchild))
        great_grandchild = self.spawn(grandchild, "level-three")
        self.assertNotIn("orchestration_spawn", self.tool_names(great_grandchild))
        with self.assertRaisesRegex(ValueError, "Only the lead can delegate an existing task"):
            self.runtime.spawn_agents(worker, {"agents": [{
                "name": "assigned-task",
                "prompt": "Do not change task ownership",
                "task_id": "lead-task",
            }]}, "worker-task-assignment")

    def test_lowering_depth_preserves_existing_descendants(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "existing-worker")
        grandchild = self.spawn(worker, "existing-grandchild")
        worker = self.freeze_tools(worker, "claude")
        grandchild = self.freeze_tools(grandchild, "claude")
        before = self.runtime.agent(grandchild["id"])
        self.set_depth(1)
        self.assertEqual(self.runtime.agent(grandchild["id"]), before)
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        self.assertNotIn("orchestration_spawn", self.tool_names(grandchild))
        with self.assertRaisesRegex(ValueError, "delegation depth"):
            self.spawn(worker, "new-child-after-lowering")

    def test_worker_defaults_update_without_depth_preserves_chat_policy(self):
        self.set_depth(2)
        defaults = self.runtime.agent(self.lead["id"])["workerDefaults"]
        self.runtime.conversation_settings(self.lead["id"], {
            "worker_defaults": {
                "model": defaults["model"],
                "effort": defaults["effort"],
                "fast_mode": defaults["fastMode"],
            },
        })
        self.assertEqual(self.runtime.agent(self.lead["id"])["workerDefaults"]["delegationDepth"], 2)
        worker = self.spawn(self.lead, "after-settings-update")
        child = self.spawn(worker, "after-settings-update-child")
        self.assertEqual(child["parentId"], worker["id"])

    def test_root_team_size_limit_counts_descendants_at_every_level(self):
        self.set_depth(2)
        self.runtime.configure(self.lead["id"], {"maxAgents": 2})
        worker = self.spawn(self.lead, "capacity-worker")
        with self.assertRaisesRegex(ValueError, "active agent limit"):
            self.spawn(worker, "over-capacity-grandchild")
        with self.runtime.read_db() as db:
            descendants = self.runtime.team_agents(db, self.lead["id"])
        self.assertEqual(sum(agent["rootId"] == self.lead["id"] for agent in descendants), 2)

    def test_grandchild_result_is_routed_to_direct_parent(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "result-parent")
        grandchild = self.spawn(worker, "result-child")
        with self.runtime.lock, self.runtime.db() as db:
            child = self.runtime.agent(grandchild["id"], db)
            child.update(status="completed", autoWake=True)
            self.runtime.put(db, "agents", child)
            self.runtime.parent_event(db, child, "completed", "grandchild result")
            events = db.execute(
                "SELECT agent,text FROM runtime_events WHERE kind='child_result'"
            ).fetchall()
        self.assertEqual([row["agent"] for row in events], [worker["id"]])
        self.assertIn("grandchild result", events[0]["text"])

    def test_interrupting_worker_stops_its_nested_descendants(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "stop-parent")
        grandchild = self.spawn(worker, "stop-child")
        stopped = self.runtime.stop(worker["id"])
        self.assertEqual(set(stopped["stopped"]), {worker["id"], grandchild["id"]})
        self.assertEqual(self.runtime.agent(worker["id"])["status"], "paused")
        self.assertEqual(self.runtime.agent(grandchild["id"])["status"], "paused")

    def test_lead_can_archive_a_nested_descendant(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "archive-parent")
        grandchild = self.spawn(worker, "archive-child")
        with self.runtime.lock, self.runtime.db() as db:
            for agent_id in (worker["id"], grandchild["id"]):
                target = self.runtime.agent(agent_id, db)
                target.update(status="completed", autoWake=False, inFlight=False)
                self.runtime.put(db, "agents", target)
            db.execute(
                "UPDATE runtime_events SET status='delivered' WHERE agent IN (?, ?)",
                (worker["id"], grandchild["id"]),
            )
        from codex_agent_management import manage_agent

        result = manage_agent(self.runtime, self.lead["id"], {
            "action": "archive_finished",
        }, self.lead["epoch"])
        self.assertEqual(result["archived"], 2, result)
        self.assertFalse(result["kept"])
        self.assertTrue(self.runtime.agent(worker["id"])["deletedAt"])
        self.assertTrue(self.runtime.agent(grandchild["id"])["deletedAt"])

    def test_root_concurrency_is_shared_by_direct_and_nested_workers(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "busy-parent")
        grandchild = self.spawn(worker, "queued-grandchild")
        self.runtime.configure(self.lead["id"], {
            "concurrency": 1,
            "expected_mode_revision": self.runtime.agent(self.lead["id"])["agentModeRevision"],
            "request_id": "one-team-slot",
        })
        self.runtime.send(grandchild["id"], "Queued nested work")
        with self.runtime.lock, self.runtime.db() as db:
            active = self.runtime.agent(worker["id"], db)
            active.update(status="running", inFlight=True, threadId="fixture-thread", turnId="fixture-turn")
            self.runtime.put(db, "agents", active)
            slots = self.runtime.dispatch_active_slots(db)
        self.assertEqual(
            sum(slot["rootId"] == self.lead["id"] and slot["id"] != self.lead["id"] for slot in slots),
            1,
        )
        self.assertEqual(self.runtime.dispatch_candidates(grandchild["id"]), 0)

    def test_depth_setting_survives_restart_and_rejects_invalid_values(self):
        self.set_depth(3)
        lead_id = self.lead["id"]
        worker = self.spawn(self.lead, "restart-level-one")
        grandchild = self.spawn(worker, "restart-level-two")
        great_grandchild = self.spawn(grandchild, "restart-level-three")
        child_ids = (worker["id"], grandchild["id"], great_grandchild["id"])
        self.runtime.close()
        self.runtime = self.open_runtime()
        self.lead = self.runtime.agent(lead_id)
        self.assertEqual(self.lead["workerDefaults"]["delegationDepth"], 3)
        worker, grandchild, great_grandchild = (self.runtime.agent(key) for key in child_ids)
        self.assertEqual(worker["parentId"], lead_id)
        self.assertEqual(grandchild["parentId"], worker["id"])
        self.assertEqual(great_grandchild["parentId"], grandchild["id"])
        self.assertIn("orchestration_spawn", self.tool_names(worker))
        self.assertIn("orchestration_spawn", self.tool_names(grandchild))
        self.assertNotIn("orchestration_spawn", self.tool_names(great_grandchild))

    def test_corrupt_stored_depth_falls_back_to_lead_only(self):
        worker = self.spawn(self.lead, "corrupt-default-worker")
        lead = self.runtime.agent(self.lead["id"])
        lead["workerDefaults"]["delegationDepth"] = True
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", lead)
        self.assertIn("orchestration_spawn", self.tool_names(lead))
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        with self.assertRaisesRegex(ValueError, "delegation depth reached"):
            self.spawn(worker, "corrupt-depth-blocked")

    def test_request_model_rejects_depth_outside_the_supported_range(self):
        from studio_api.agents.models import WorkerDefaults

        value = {"model": "gpt-6-luna", "effort": "high", "fast_mode": False}
        for invalid in (0, 4, True, 1.5, "2"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                WorkerDefaults.model_validate({**value, "delegation_depth": invalid})

    def test_delegation_denial_reports_policy_cause(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "reason-worker")
        grandchild = self.spawn(worker, "reason-grandchild")
        with self.assertRaisesRegex(ValueError, "delegation depth reached"):
            self.spawn(grandchild, "depth-denial")
        with patch("codex_nested_delegation.concurrency", return_value=0):
            with self.assertRaisesRegex(ValueError, "no subagent concurrency"):
                self.spawn(worker, "concurrency-denial")

    def test_broken_parent_chain_is_reported_as_broken_ancestry(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "broken-ancestry-worker")
        worker["parentId"] = "missing-parent"
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", worker)
        with self.assertRaisesRegex(ValueError, "broken ancestry"):
            self.spawn(worker, "broken-ancestry-child")

    def test_reviewer_denial_has_a_specific_reason(self):
        from codex_nested_delegation import require_delegation

        self.set_depth(2)
        reviewer = self.runtime.spawn_agents(self.lead, {"agents": [{
            "name": "reviewer-actor", "prompt": "Review", "role": "reviewer",
        }]}, "reviewer-actor")["agents"][0]
        with self.assertRaisesRegex(ValueError, "reviewer"):
            require_delegation(self.runtime, reviewer)

    def test_level_two_parent_result_targets_worker_and_not_lead(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "wake-parent")
        grandchild = self.spawn(worker, "wake-child")
        with self.runtime.lock, self.runtime.db() as db:
            child = self.runtime.agent(grandchild["id"], db)
            child.update(status="completed", autoWake=True)
            self.runtime.put(db, "agents", child)
            self.runtime.parent_event(db, child, "result-wake", "reported by nested worker")
            targets = [row[0] for row in db.execute(
                "SELECT agent FROM runtime_events WHERE kind='child_result'"
            )]
        self.assertEqual(targets, [worker["id"]])
        self.assertNotIn(self.lead["id"], targets)

    def test_lead_interrupts_and_archives_level_two_agent(self):
        self.set_depth(2)
        worker = self.spawn(self.lead, "management-parent")
        grandchild = self.spawn(worker, "management-grandchild")
        lead = self.runtime.agent(self.lead["id"])
        lead["threadId"] = "nested-lead-thread"
        lead["turnId"] = "nested-lead-turn"
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", lead)
        with self.runtime.lock, self.runtime.db() as db:
            target = self.runtime.agent(grandchild["id"], db)
            target.update(status="running", inFlight=True, threadId="grandchild-thread", turnId="grandchild-turn")
            self.runtime.put(db, "agents", target)
        message = {"id": 100, "method": "item/tool/call", "params": {
            "threadId": lead["threadId"], "callId": "interrupt-level-two", "turnId": lead.get("turnId"),
            "tool": "orchestration_interrupt", "arguments": {"agent_id": grandchild["id"]}}}
        request_key = self.runtime.tool_request_key(message)
        self.runtime.dynamic(message)
        wait_for_condition(lambda: (self.runtime.tool_request(request_key) or {}).get("outcome") == "applied")
        self.assertEqual(self.runtime.agent(grandchild["id"])["status"], "paused")
        with self.runtime.lock, self.runtime.db() as db:
            target = self.runtime.agent(grandchild["id"], db)
            target.update(status="completed", inFlight=False, threadId=None, turnId=None)
            self.runtime.put(db, "agents", target)
        from codex_agent_management import manage_agent

        archived = manage_agent(self.runtime, lead["id"], {
            "action": "archive", "agent_id": grandchild["id"], "reason": "Retire nested test worker",
        }, lead["epoch"])
        self.assertEqual(archived["status"], "archived")
        self.assertTrue(self.runtime.agent(grandchild["id"])["deletedAt"])
        self.assertFalse(self.runtime.agent(worker["id"]).get("deletedAt"))


if __name__ == "__main__":
    unittest.main()
