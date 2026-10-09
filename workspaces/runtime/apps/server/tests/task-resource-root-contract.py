#!/usr/bin/env python3
"""Task resources select live team members without decoding archived records."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
import inspect
import json
from pathlib import Path
import sqlite3
import sys
import textwrap
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime


class TaskResourceRootContract(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX runtime_agent_root ON runtime_agents(json_extract(record,'$.rootId'));
        """)
        # Exercise the production startup index without starting native processes.
        tree = ast.parse(textwrap.dedent(inspect.getsource(Runtime.__init__)))
        statements = [statement for node in ast.walk(tree)
                      if isinstance(node, ast.Constant) and isinstance(node.value, str)
                      for statement in node.value.split(";")
                      if statement.strip().startswith("CREATE INDEX IF NOT EXISTS runtime_agent_live_root ")]
        self.assertEqual(len(statements), 1)
        self.db.execute(statements[0])
        self.runtime = Runtime.__new__(Runtime)
        self.staged = []
        self.runtime._stage_resource_change = lambda _db, resource: self.staged.append(resource)

    def agent(self, identity, **record):
        self.db.execute("INSERT INTO runtime_agents VALUES (?,?)", (
            identity, json.dumps({"id": identity, **record})))

    def members(self, root):
        self.staged.clear()
        self.runtime._stage_task_resources_for_root(self.db, root)
        return {resource.root.agentId for resource in self.staged}

    def test_live_team_members_keep_root_and_descendants_without_duplicates(self):
        self.agent("root", rootId="root")
        self.agent("child", rootId="root", parentId="root")
        self.agent("grandchild", rootId="root", parentId="child")
        self.agent("deleted", rootId="root", deletedAt=1)
        self.agent("other", rootId="other")
        self.agent("unassigned")
        self.assertEqual(self.members("root"), {"root", "child", "grandchild"})
        self.assertEqual(len(self.staged), 3)
        self.assertEqual(self.members("missing"), set())
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id='root'")
        self.assertEqual(self.members("root"), {"child", "grandchild"})
        self.assertEqual(self.members("unassigned"), {"unassigned"})

    def test_archived_team_history_does_not_consume_the_read_budget(self):
        self.agent("root", rootId="root")
        self.agent("child", rootId="root")
        self.db.executemany("INSERT INTO runtime_agents VALUES (?,?)", (
            (f"archived-{index}", json.dumps({"id": f"archived-{index}", "rootId": "root",
                                              "deletedAt": 1, "prompt": "x" * 2048}))
            for index in range(2000)))
        steps = 0

        def limit_read():
            nonlocal steps
            steps += 100
            return steps > 10000

        self.db.set_progress_handler(limit_read, 100)
        try:
            self.assertEqual(self.members("root"), {"root", "child"})
        finally:
            self.db.set_progress_handler(None, 0)

    def test_team_root_branch_uses_the_live_partial_index(self):
        self.agent("root", rootId="root")
        statements = []
        self.db.set_trace_callback(statements.append)
        self.members("root")
        self.db.set_trace_callback(None)
        query = next(sql for sql in statements if sql.lstrip().upper().startswith("SELECT"))
        plans = [row[3] for row in self.db.execute("EXPLAIN QUERY PLAN " + query)]
        self.assertTrue(any("runtime_agent_live_root" in plan for plan in plans), plans)


if __name__ == "__main__":
    unittest.main()
