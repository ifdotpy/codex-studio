"""Focused tests for native notification routing and hook item updates."""
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import threading
import unittest

SCRIPT_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPT_ROOT))

from native_notifications.dispatch import consume_native_notification, matching_agents

INDEX_SQL = """CREATE INDEX runtime_agent_native_scope ON runtime_agents(
    json_extract(record,'$.threadId'),
    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
         ELSE json_extract(record,'$.accountKey') END)"""
CURRENT_CONNECTION = "connection-current"
SHARED_THREAD_AGENT_COUNT = 137


class FakeRuntime:
    def __init__(self, agents=()):
        self.lock = threading.RLock()
        self.connection_ids = {"default": CURRENT_CONNECTION, "other": "other-connection"}
        self.connection = sqlite3.connect(":memory:")
        self.connection.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_items (id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL)")
        self.connection.execute("CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute(INDEX_SQL)
        for agent in agents:
            self.put(self.connection, "agents", agent)

    @contextmanager
    def db(self):
        try:
            yield self.connection
            self.connection.commit()
        except BaseException:
            self.connection.rollback()
            raise

    def connection_current(self, account_key, connection_id):
        return self.connection_ids.get(account_key) == connection_id

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]

    def put(self, db, table, record):
        db.execute(f"INSERT INTO runtime_{table}(id,record) VALUES (?,?) "
                   "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (record["id"], json.dumps(record)))

    def item(self, db, agent_id, item_id, role, text, author, **metadata):
        record = {"id": item_id, "role": role, "text": text, "author": author, **metadata}
        db.execute("INSERT INTO runtime_items VALUES (?,?,?,?) "
                   "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (item_id, agent_id, json.dumps(record), 1))

    def touch_ui(self, _agent_id):
        pass

    def items(self, agent_id):
        return [json.loads(row[0]) for row in self.connection.execute(
            "SELECT record FROM runtime_items WHERE agent=?", (agent_id,))]


def agent(agent_id, thread_id="thread-a", account_key="default", **fields):
    record = {"id": agent_id, "threadId": thread_id, "accountKey": account_key,
              "inFlight": True, "turnId": "turn-a", "status": "running", "autoWake": True}
    record.update(fields)
    return record


def hook(status="running", entries=None, run_id="hook-a", scope="turn"):
    return {"id": run_id, "eventName": "preToolUse", "status": status,
            "scope": scope, "statusMessage": "Inspecting command", "entries": entries or []}


class NativeNotificationDispatchTests(unittest.TestCase):
    def test_index_query_returns_all_live_matches_and_honors_default_account(self):
        runtime = FakeRuntime([
            *(agent(f"default-{number}", account_key="default")
              for number in range(SHARED_THREAD_AGENT_COUNT)),
            {"id": "legacy-default", "threadId": "thread-a", "inFlight": True},
            agent("default-deleted", deletedAt=1),
            agent("other-account", account_key="other"),
            agent("null-account", account_key=None),
            agent("empty-account", account_key=""),
            agent("other-thread", thread_id="thread-b"),
        ])
        with runtime.db() as db:
            selected = matching_agents(runtime, db, "default", "thread-a")
            null_account = matching_agents(runtime, db, None, "thread-a")
            empty_account = matching_agents(runtime, db, "", "thread-a")
            plan = [row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT record FROM runtime_agents "
                "WHERE json_extract(record,'$.threadId')=? "
                "AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
                "ELSE json_extract(record,'$.accountKey') END=?", ("thread-a", "default"))]
            null_plan = [row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT record FROM runtime_agents "
                "WHERE json_extract(record,'$.threadId')=? "
                "AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
                "ELSE json_extract(record,'$.accountKey') END IS NULL", ("thread-a",))]
        self.assertEqual(len(selected), SHARED_THREAD_AGENT_COUNT + 1)
        self.assertIn("legacy-default", {row["id"] for row in selected})
        self.assertEqual({row["id"] for row in null_account}, {"null-account"})
        self.assertEqual({row["id"] for row in empty_account}, {"empty-account"})
        self.assertTrue(any("runtime_agent_native_scope" in item for item in plan))
        self.assertTrue(any("runtime_agent_native_scope" in item for item in null_plan))

    def test_normal_hook_completion_updates_one_running_item(self):
        runtime = FakeRuntime([agent("chat")])
        params = {"threadId": "thread-a", "turnId": "turn-a", "run": hook()}
        self.assertTrue(consume_native_notification(runtime, {"method": "hook/started", "params": params},
                                                    "default", CURRENT_CONNECTION))
        params["run"] = hook("completed", [{"kind": "info", "text": "Command passed"}])
        consume_native_notification(runtime, {"method": "hook/completed", "params": params},
                                     "default", CURRENT_CONNECTION)
        items = [item for item in runtime.items("chat") if item.get("nativeHook")]
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0]["nativeHook"]["status"], "completed")
        self.assertIn("Command passed", items[0]["details"])

    def test_failed_hook_keeps_visible_entries_and_hides_context_and_later_warning(self):
        runtime = FakeRuntime([agent("chat")])
        entries = [{"kind": "warning", "text": "First warning"},
                   {"kind": "error", "text": "Command failed"},
                   {"kind": "context", "text": "Hidden model context"},
                   {"kind": "warning", "text": "Second warning"}]
        consume_native_notification(runtime, {"method": "hook/completed", "params": {
            "threadId": "thread-a", "turnId": "turn-a", "run": hook("failed", entries)}},
            "default", CURRENT_CONNECTION)
        item = next(row for row in runtime.items("chat") if row.get("nativeHook"))
        self.assertEqual(item["nativeHook"]["status"], "failed")
        self.assertIn("Command failed", item["details"])
        self.assertNotIn("Hidden model context", json.dumps(item))
        self.assertNotIn("Second warning", json.dumps(item))

    def test_late_thread_hook_is_accepted_but_old_connection_and_other_scope_are_ignored(self):
        runtime = FakeRuntime([agent("chat", inFlight=False, status="completed")])
        base = {"threadId": "thread-a", "run": hook("blocked", run_id="thread-hook", scope="thread")}
        consume_native_notification(runtime, {"method": "hook/completed", "params": base},
                                     "default", "old-connection")
        self.assertEqual(runtime.items("chat"), [])
        consume_native_notification(runtime, {"method": "hook/completed", "params": base},
                                     "other", "other-connection")
        self.assertEqual(runtime.items("chat"), [])
        consume_native_notification(runtime, {"method": "hook/completed", "params": base},
                                     "default", CURRENT_CONNECTION)
        self.assertEqual(next(item for item in runtime.items("chat") if item.get("nativeHook")
                              )["nativeHook"]["status"], "blocked")

    def test_quiet_success_writes_tombstone_after_running_hook(self):
        runtime = FakeRuntime([agent("chat")])
        for method, run in (("hook/started", hook()),
                            ("hook/completed", hook("completed", [{"kind": "context", "text": "hidden"}]))):
            consume_native_notification(runtime, {"method": method, "params": {
                "threadId": "thread-a", "turnId": "turn-a", "run": run}},
                "default", CURRENT_CONNECTION)
        item = next(row for row in runtime.items("chat") if row.get("nativeHook"))
        self.assertTrue(item["nativeHookQuiet"])
        self.assertEqual(item["text"], "")

    def test_threadless_account_notice_does_not_attach_to_agent(self):
        runtime = FakeRuntime([agent("chat")])
        runtime.connection.execute("CREATE TABLE runtime_native_notices (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        consume_native_notification(runtime, {"method": "configWarning", "params": {"summary": "Invalid setting"}},
                                    "default", CURRENT_CONNECTION)
        notices = list(runtime.connection.execute("SELECT record FROM runtime_native_notices"))
        self.assertEqual(json.loads(notices[0][0])["message"], "Invalid setting")
        self.assertEqual(runtime.items("chat"), [])


if __name__ == "__main__":
    unittest.main()
