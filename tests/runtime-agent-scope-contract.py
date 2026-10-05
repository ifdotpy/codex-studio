#!/usr/bin/env python3
"""Compare scoped runtime outputs with snapshots captured from base ca123f6."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
RUNNER = Path(__file__).with_name("runtime-agent-scope-scenario.py")
SCENARIOS = (
    "tree-delete", "repeat-tree-delete", "lead-rename", "worker-rename", "lead-cwd",
    "restore", "single-delete", "rootid-change", "shared-room-change",
    "spawn-then-rename", "spawn-then-unrelated-delete", "peer-team-then-unrelated-rename",
    "direct-message-then-unrelated-rename", "orphan-tree-delete", "cross-root-child",
    "legacy-norootid-delete", "legacy-norootid-budget", "federated-delete",
    "exception-mid-tree-delete",
)


class RuntimeAgentRoomScope(unittest.TestCase):
    def test_scenarios_match_real_base_snapshots(self):
        expected = json.loads((Path(__file__).parent / "fixtures/runtime-agent-room-entities.json").read_text())
        with tempfile.TemporaryDirectory(prefix="runtime-agent-scope-test-",
                                         dir=os.environ.get("TMPDIR")) as scratch:
            env = dict(os.environ, TMPDIR=scratch, PYTHONDONTWRITEBYTECODE="1")
            for scenario in SCENARIOS:
                with self.subTest(scenario=scenario):
                    result = subprocess.run(
                        [sys.executable, str(RUNNER), str(ROOT), scenario],
                        cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=60)
                    actual = json.loads(result.stdout.strip().splitlines()[-1])
                    self.assertEqual(actual, expected[scenario])
                    self.assertEqual(actual["accountKeys"], ["account-2", "default"])
                    self.assertIsNotNone(actual["rooms"]["federated-a"]["value"]["lastMessage"])
                    if scenario == "exception-mid-tree-delete":
                        self.assertEqual(actual["error"], "RuntimeError: injected tree-delete failure")
                        self.assertEqual(actual["deletedAgents"], ["c-worker"])

    def test_scope_query_plans_on_populated_analyzed_roster(self):
        from test_isolation import isolate_supervisor_environment
        sys.path.insert(0, str(ROOT / "scripts"))
        from codex_runtime import Runtime
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "runtime_contract_fixture", Path(__file__).with_name("runtime-contract.py"))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        with tempfile.TemporaryDirectory(prefix="runtime-agent-index-plan-",
                                         dir=os.environ.get("TMPDIR")) as scratch:
            runtime = Runtime(Path(scratch), fixture.FakeServer)
            try:
                with runtime.db() as db:
                    db.executemany("INSERT INTO runtime_agents(id,record) VALUES (?,?)", (
                        (f"agent-{number:04}", json.dumps({
                            "id": f"agent-{number:04}", "rootId": f"lead-{number // 40:02}",
                            "parentId": f"agent-{number - 1:04}" if number % 40 else None,
                            "accountKey": "alternate" if number < 50 else "default",
                            "threadId": f"thread-{number}" if number < 50 else None,
                            "deletedAt": None, "status": "completed"}))
                        for number in range(1200)))
                    db.execute("ANALYZE")
                    queries = (
                        ("runtime_agent_root", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=? ORDER BY rowid", ("lead-01",)),
                        ("runtime_agent_account_scope", "SELECT record FROM runtime_agents WHERE CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' ELSE json_extract(record,'$.accountKey') END=? ORDER BY rowid", ("alternate",)),
                        ("runtime_agent_native_scope", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' ELSE json_extract(record,'$.accountKey') END=? ORDER BY rowid", ("thread-41", "alternate")),
                        ("runtime_agent_parent", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.parentId') IN (?) ORDER BY rowid", ("agent-0041",)),
                    )
                    for index, query, params in queries:
                        plan = " ".join(str(part) for row in db.execute(
                            "EXPLAIN QUERY PLAN " + query, params) for part in row)
                        self.assertIn(index, plan)
                        self.assertIn("SEARCH", plan)
            finally:
                runtime.close()


if __name__ == "__main__":
    unittest.main()
