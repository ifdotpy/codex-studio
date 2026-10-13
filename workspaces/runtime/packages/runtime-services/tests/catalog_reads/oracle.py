"""Python oracle for skill and capability projections using only scripted replies."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

GUARD = install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))

from contextlib import contextmanager
import json
import sqlite3
import threading
from unittest.mock import patch

from codex_runtime import Runtime
from codex_workspace import WorkspaceMixin


class ScriptedProvider:
    def __init__(self, replies):
        self.replies = replies
        self.calls = []
        self.lock = threading.Lock()

    def call(self, method, params, timeout=60):
        with self.lock:
            self.calls.append({"method": method, "params": params, "timeout": timeout})
        return self.replies[method]


class Inventory(WorkspaceMixin):
    def __init__(self, database, agent, replies):
        self.database = database
        self.agent = agent
        self.provider = ScriptedProvider(replies)
        self.capability_cache = {}

    def checked_actor_in_own_db(self, key):
        if key != self.agent["id"]:
            raise ValueError("Unknown agent")
        return self.agent

    def connect_agent(self, _agent):
        return self.provider

    def tool_definitions(self):
        return Runtime.tool_definitions(self)

    @contextmanager
    def db(self):
        connection = sqlite3.connect(self.database)
        try:
            yield connection
        finally:
            connection.close()


def main():
    request = json.load(sys.stdin)
    database = Path(request["database"])
    agent = request["agent"]
    replies = request["replies"]
    with sqlite3.connect(database) as connection:
        connection.executescript(
            "CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT NOT NULL);"
            "CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'),json_extract(record,'$.created'));"
            "CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC,json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';"
        )
        connection.executemany(
            "INSERT INTO runtime_tasks(id,record) VALUES (?,?)",
            [(str(index), json.dumps(record)) for index, record in enumerate(request["records"])],
        )
    runtime = Inventory(database, agent, replies)
    with patch("codex_workspace.time.time", return_value=1234.5):
        skills = runtime.skill_catalog(agent["id"])
        capabilities = runtime.capabilities(agent["id"])
        cached = runtime.capabilities(agent["id"])
    print(json.dumps({
        "skills": skills,
        "capabilities": capabilities,
        "cachedCapabilities": cached,
        "calls": sorted(runtime.provider.calls, key=lambda call: (call["method"], call["timeout"])),
        "skillResponse": replies["skills/list"],
        "mcpResponse": replies["mcpServerStatus/list"],
    }, separators=(",", ":")))


if __name__ == "__main__":
    main()
