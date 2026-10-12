#!/usr/bin/env python3
"""Nested delegation policy, root budgets and restart persistence without providers."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

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
        runtime = fixture.ControlledRuntime(self.state, fixture.f.FakeServer)
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

    def test_default_is_lead_only_and_depth_two_stops_at_grandchild(self):
        worker = self.spawn(self.lead, "depth-one-worker")
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        self.assertEqual(self.runtime.agent(self.lead["id"])["workerDefaults"]["delegationDepth"], 1)
        with self.assertRaisesRegex(ValueError, "delegation depth"):
            self.spawn(worker, "blocked-child-default")

        self.set_depth(2)
        self.assertIn("orchestration_spawn", self.tool_names(worker))
        grandchild = self.spawn(worker, "depth-two-grandchild")
        self.assertEqual(grandchild["parentId"], worker["id"])
        self.assertEqual(grandchild["rootId"], self.lead["id"])
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
        before = self.runtime.agent(grandchild["id"])
        self.set_depth(1)
        self.assertEqual(self.runtime.agent(grandchild["id"]), before)
        self.assertNotIn("orchestration_spawn", self.tool_names(worker))
        with self.assertRaisesRegex(ValueError, "delegation depth"):
            self.spawn(worker, "new-child-after-lowering")

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
        self.set_depth(2)
        lead_id = self.lead["id"]
        for invalid in (0, 4, True, 1.5, "2"):
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, "delegation_depth"):
                self.set_depth(invalid)
        self.runtime.close()
        self.runtime = self.open_runtime()
        self.lead = self.runtime.agent(lead_id)
        self.assertEqual(self.lead["workerDefaults"]["delegationDepth"], 2)
        worker = self.spawn(self.lead, "after-restart")
        self.assertIn("orchestration_spawn", self.tool_names(worker))


if __name__ == "__main__":
    unittest.main()
