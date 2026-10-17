"""Contract tests for the shared process identity and Stop facade."""
from __future__ import annotations

from contextlib import contextmanager
import json
import sqlite3
import threading
import unittest
from pathlib import Path
from unittest.mock import patch
from typing import Any, Iterator, cast

from codex_process_controls import action, listing, receipt, _listed_processes, _record_process, _stop_process, _server
from codex_runtime import Runtime
from studio_api.io.models import ProcessControlRequest


class FakeNativeServer:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []
        self.task_running = True
        self.fail_interrupt = False
        self.fail_terminate = False
        self.on_state: Any = None
        self.wait_count = 0

    def wait(self, _submitted: Any, timeout: float | None = None) -> dict[str, Any]:
        self.wait_count += 1
        return {"config": {}} if self.wait_count == 1 else {"exitCode": 0}

    def after_events(self, callback: Any) -> None:
        callback()

    def call(self, method: str, params: dict[str, Any], timeout: int | None = None) -> dict[str, Any]:
        self.calls.append((method, params))
        if method == "turn/interrupt":
            if self.fail_interrupt:
                raise TimeoutError("interrupt reply was lost")
            return {}
        if method == "command/exec/terminate":
            if self.fail_terminate:
                raise TimeoutError("terminate reply was lost")
            return {}
        if method == "claude/state":
            if self.on_state:
                self.on_state()
            return {"tasks": [{"task_id": "task-1"}] if self.task_running else []}
        if method == "claude/stopTask":
            self.task_running = False
            return {}
        raise AssertionError(f"Unexpected native call: {method}")


class FakeRuntime:
    def __init__(self) -> None:
        self.connection = sqlite3.connect(":memory:", check_same_thread=False)
        self.connection.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_monitors (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.commit()
        self.lock = threading.RLock()
        self.monitor_condition = threading.Condition(self.lock)
        self.changed = threading.Event()
        self.accounts = {"acct": {"provider": "codex"}}
        self.connection_ids = {"acct": "connection-1"}
        self.native_connection_id = "connection-1"
        self.native_server = FakeNativeServer()
        self.stop_calls: list[dict[str, Any]] = []
        self.cancel_calls: list[str] = []
        self.cancel_contexts: list[dict[str, Any]] = []
        self.fail_cancel_after_apply = False
        self.cancel_result: dict[str, Any] | None = None
        self.fail_interrupt = False
        self.on_stop_response: Any = None
        self.root = Path("/tmp")
        self.closed = False

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        yield self.connection

    def read_db(self) -> Any:
        return self.db()

    def records(self, db: Any, table: str) -> list[dict[str, Any]]:
        mapped = {"requests": "runtime_requests"}.get(table, table)
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM {mapped}")]

    def put(self, *args: Any) -> None:
        if len(args) == 3 and hasattr(args[0], "execute"):
            db, table, value = args
            key = value["id"]
        else:
            table, key, value = args
            db = self.connection
        table = {"agents": "runtime_agents", "monitors": "runtime_monitors",
                 "tasks": "runtime_tasks", "requests": "runtime_requests"}.get(table, table)
        with db:
            db.execute(f"INSERT OR REPLACE INTO {table}(id,record) VALUES(?,?)",
                       (key, json.dumps(value)))

    def agent_connection(self, _owner: dict[str, Any]) -> str:
        return self.native_connection_id

    def agent(self, key: str, db: Any = None) -> dict[str, Any]:
        row = (db or self.connection).execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        return cast(dict[str, Any], json.loads(row[0]))

    def server_for(self, _account: str, _connection: str) -> FakeNativeServer:
        return self.native_server

    def cancel_monitor(self, key: str, *, owner: str, **expected: Any) -> dict[str, Any]:
        self.cancel_calls.append(key)
        self.cancel_contexts.append(expected)
        row = self.connection.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()
        record = json.loads(row[0])
        if expected.get("expected_status") and record["status"] != expected["expected_status"]:
            raise ValueError("The command status changed before it could stop")
        record["status"] = "cancelled"
        self.put("runtime_monitors", key, record)
        if self.fail_cancel_after_apply:
            raise TimeoutError("cancel reply was lost after dispatch")
        if self.cancel_result is not None:
            return self.cancel_result
        return {"id": key, "status": "cancelled"}

    def stop_response(self, agent: str, *, expected_account_key: str,
                      expected_epoch: int, expected_connection_id: str,
                      expected_thread_id: str, expected_turn_id: str) -> dict[str, Any]:
        if self.on_stop_response:
            self.on_stop_response()
        self.stop_calls.append({"agent": agent, "expected_account_key": expected_account_key,
                                "expected_epoch": expected_epoch,
                                "expected_connection_id": expected_connection_id,
                                "expected_thread_id": expected_thread_id,
                                "expected_turn_id": expected_turn_id})
        current = self.agent(agent)
        if (current.get("accountKey") != expected_account_key
                or current.get("epoch") != expected_epoch
                or self.agent_connection(current) != expected_connection_id
                or current.get("threadId") != expected_thread_id
                or current.get("turnId") != expected_turn_id or not current.get("inFlight")):
            raise ValueError("The response owner changed")
        if self.fail_interrupt:
            return {"status": "uncertain", "error": "interrupt reply was lost"}
        return {"status": "requested"}


def fixture() -> FakeRuntime:
    rt = FakeRuntime()
    rt.put("runtime_agents", "lead", {
        "id": "lead", "rootId": "lead", "accountKey": "acct", "epoch": 4,
        "cwd": "/tmp", "sandbox": "workspace-write",
        "inFlight": True, "turnId": "turn-1", "threadId": "thread-1", "provider": "codex",
        "autoWake": True,
    })
    for ident in ("monitor-1", "monitor-2"):
        rt.put("runtime_monitors", ident, {
            "id": ident, "agent": "lead", "epoch": 4, "status": "running",
            "created": 1, "command": "sleep 100", "interactive": True,
            "operation": {"accountKey": "acct", "connectionId": "connection-1",
                           "threadId": "thread-1"},
        })
    return rt


def response_request(rt: FakeRuntime, request_id: str) -> dict[str, Any]:
    target = listing(rt, "lead")["responseTarget"]
    return {"action": "stop_response", "requestId": request_id, "agent": "lead",
            "accountKey": "acct", "epoch": 4, "responseTarget": target, "processes": []}


class ProcessControlsTests(unittest.TestCase):
    def make_runtime(self) -> FakeRuntime:
        rt = fixture()
        self.addCleanup(rt.connection.close)
        return rt

    def test_listing_publishes_identity_and_monitor_capabilities(self) -> None:
        rt = self.make_runtime()
        result = listing(rt, "lead")
        self.assertEqual(len(result["processes"]), 2)
        item = result["processes"][0]
        self.assertTrue(item["key"])
        self.assertTrue(item["generation"])
        self.assertEqual(item["accountKey"], "acct")
        self.assertEqual(item["epoch"], 4)
        self.assertEqual(item["capabilities"], {
            "readOutput": True, "sendInput": True, "resize": True, "stop": True,
        })

    def test_runtime_monitor_launch_record_has_facade_controls(self) -> None:
        rt = self.make_runtime()
        runtime_stub = cast(Any, rt)
        runtime_stub._ensure_private_monitor_log = Runtime._ensure_private_monitor_log
        runtime_stub.launch_monitor = lambda _key: None
        runtime_stub.assert_workspace_available = lambda *_args: None
        runtime_stub.prepare = lambda agent: agent
        runtime_stub.connect_agent = lambda _agent: rt.native_server
        runtime_stub.submit_reserved = lambda *_args, **_kwargs: "submitted"
        runtime_stub.turn_permissions = lambda _agent: {"sandboxPolicy": "workspace-write"}
        runtime_stub.monitor_accepted = lambda *_args: None
        launched = Runtime.monitor(cast(Runtime, rt), "lead", {
            "command": "echo ok", "interactive": True,
        }, key="real-monitor-request", approved=True)
        self.assertEqual(launched["status"], "starting")
        Runtime.run_monitor(cast(Runtime, rt), launched["id"], {
            "allow_login_shell": False, "features": {"shell_snapshot": False},
        })
        record = json.loads(rt.connection.execute(
            "SELECT record FROM runtime_monitors WHERE id=?", (launched["id"],)
        ).fetchone()[0])
        self.assertEqual(record["operation"]["threadId"], "thread-1")
        process = next(item for item in listing(rt, "lead")["processes"]
                       if item["id"] == launched["id"])
        self.assertTrue(process["capabilities"]["stop"])
        self.assertTrue(process["capabilities"]["sendInput"])
        identity = {key: process[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        stopped = action(rt, {"action": "stop_selected", "requestId": "real-run-monitor-stop",
                              "agent": "lead", "accountKey": "acct", "epoch": 4,
                              "process": identity})
        self.assertEqual(stopped["processes"][0]["status"], "stopped")
        self.assertEqual(rt.cancel_contexts[-1]["expected_thread_id"], "thread-1")

    def test_proven_monitor_reattach_uses_current_connection(self) -> None:
        rt = self.make_runtime()
        rt.native_connection_id = "connection-2"
        raw = json.loads(rt.connection.execute(
            "SELECT record FROM runtime_monitors WHERE id='monitor-1'"
        ).fetchone()[0])
        raw["reattachedConnectionId"] = "connection-2"
        raw["operation"]["threadId"] = "old-thread"
        rt.put("runtime_monitors", "monitor-1", raw)
        process = next(item for item in _listed_processes(rt, rt.connection, "lead")
                       if item["id"] == "monitor-1")
        self.assertTrue(process["capabilities"]["stop"])
        self.assertEqual(process["_sourceConnectionId"], "connection-2")
        self.assertEqual(process["_sourceThreadId"], "thread-1")
        result = _stop_process(rt, process)
        self.assertEqual(result["status"], "stopped")
        self.assertEqual(rt.cancel_contexts[-1]["expected_connection_id"], "connection-2")

    def test_pending_monitor_can_cancel_without_native_identity(self) -> None:
        rt = self.make_runtime()
        cast(Any, rt).assert_workspace_available = lambda *_args: None
        monitor = Runtime.monitor(cast(Runtime, rt), "lead", {
            "command": "echo approval", "interactive": True,
        }, key="approval-monitor", approved=False)
        process = next(item for item in listing(rt, "lead")["processes"]
                       if item["id"] == monitor["id"])
        self.assertTrue(process["capabilities"]["stop"])
        self.assertFalse(process["capabilities"]["sendInput"])
        identity = {key: process[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        result = action(rt, {"action": "stop_selected", "requestId": "approval-monitor-stop",
                             "agent": "lead", "accountKey": "acct", "epoch": 4,
                             "process": identity})
        self.assertEqual(result["processes"][0]["status"], "stopped")
        self.assertEqual(rt.cancel_contexts[-1]["expected_status"], "approval")

    def test_starting_monitor_can_cancel_before_native_submission(self) -> None:
        rt = self.make_runtime()
        runtime_stub = cast(Any, rt)
        runtime_stub._ensure_private_monitor_log = Runtime._ensure_private_monitor_log
        runtime_stub.launch_monitor = lambda _key: None
        runtime_stub.assert_workspace_available = lambda *_args: None
        monitor = Runtime.monitor(cast(Runtime, rt), "lead", {"command": "echo pending"},
                                  key="starting-monitor", approved=True)
        process = next(item for item in listing(rt, "lead")["processes"]
                       if item["id"] == monitor["id"])
        self.assertTrue(process["capabilities"]["stop"])
        self.assertFalse(process["capabilities"]["sendInput"])
        identity = {key: process[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        result = action(rt, {"action": "stop_selected", "requestId": "starting-monitor-stop",
                             "agent": "lead", "accountKey": "acct", "epoch": 4,
                             "process": identity})
        self.assertEqual(result["processes"][0]["status"], "stopped")
        self.assertEqual(rt.cancel_contexts[-1]["expected_status"], "starting")

    def test_scope_uses_parent_tree_and_selected_workers_need_opt_in(self) -> None:
        rt = self.make_runtime()
        rt.put("runtime_agents", "worker-a", {
            "id": "worker-a", "rootId": "lead", "parentId": "lead", "accountKey": "acct",
            "epoch": 2, "threadId": "worker-thread", "provider": "codex",
        })
        rt.put("runtime_agents", "worker-b", {
            "id": "worker-b", "rootId": "lead", "parentId": "another-parent", "accountKey": "acct",
            "epoch": 2, "threadId": "worker-thread-b", "provider": "codex",
        })
        rt.put("runtime_monitors", "worker-monitor", {
            "id": "worker-monitor", "agent": "worker-a", "epoch": 2, "status": "running",
            "created": 2, "command": "sleep 100", "operation": {
                "accountKey": "acct", "connectionId": "connection-1", "threadId": "worker-thread",
            },
        })
        items = listing(rt, "lead")["processes"]
        self.assertNotIn("worker-b", [item["agent"] for item in items])
        worker = next(item for item in items if item["agent"] == "worker-a")
        identity = {key: worker[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        denied = action(rt, {"action": "stop_selected", "requestId": "selected-worker-denied",
                             "agent": "lead", "accountKey": "acct", "epoch": 4,
                             "process": identity})
        self.assertEqual(denied["processes"][0]["status"], "stale")
        allowed = action(rt, {"action": "stop_selected", "requestId": "selected-worker-allowed",
                              "agent": "lead", "accountKey": "acct", "epoch": 4,
                              "process": identity, "includeDescendants": True})
        self.assertEqual(allowed["processes"][0]["status"], "stopped")

    def test_response_only_stop_preserves_monitors_and_replays_one_receipt(self) -> None:
        rt = self.make_runtime()
        request = {"action": "stop_response", "requestId": "receipt-1", "agent": "lead",
                   "accountKey": "acct", "epoch": 4, "responseTarget": listing(rt, "lead")["responseTarget"],
                   "processes": []}
        first = action(rt, request)
        replay = action(rt, request)
        self.assertEqual(first, replay)
        self.assertEqual(first["status"], "completed")
        self.assertEqual(len(rt.stop_calls), 1)
        self.assertEqual(rt.stop_calls[0], {"agent": "lead", "expected_account_key": "acct",
                                            "expected_epoch": 4, "expected_connection_id": "connection-1",
                                            "expected_thread_id": "thread-1", "expected_turn_id": "turn-1"})
        current = rt.agent("lead")
        self.assertTrue(current["inFlight"])
        self.assertEqual(current["epoch"], 4)
        self.assertEqual(rt.cancel_calls, [])
        saved = json.loads(rt.connection.execute(
            "SELECT record FROM runtime_process_actions WHERE id='receipt-1'"
        ).fetchone()[0])
        self.assertEqual(saved["responseTarget"], listing(rt, "lead")["responseTarget"])

    def test_receipt_lookup_is_read_only_and_scoped_to_owner_account_and_epoch(self) -> None:
        rt = self.make_runtime()
        query = {"requestId": "receipt-read-only", "agent": "lead", "accountKey": "acct", "epoch": 4}
        self.assertEqual(receipt(rt, query), {"found": False, "result": None})
        self.assertIsNone(rt.connection.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_process_actions'"
        ).fetchone())
        result = action(rt, response_request(rt, "receipt-read-only"))
        self.assertEqual(receipt(rt, query), {"found": True, "result": result})
        self.assertEqual(receipt(rt, {**query, "accountKey": "other"}),
                         {"found": False, "result": None})
        self.assertEqual(receipt(rt, {**query, "epoch": 5}), {"found": False, "result": None})
        self.assertEqual(receipt(rt, {**query, "agent": "worker"}), {"found": False, "result": None})
        self.assertEqual(len(rt.stop_calls), 1)

    def test_receipt_lookup_returns_uncertainty_without_repeating_stop(self) -> None:
        rt = self.make_runtime()
        rt.fail_interrupt = True
        request = response_request(rt, "receipt-read-uncertain")
        result = action(rt, request)
        for _ in range(2):
            looked_up = receipt(rt, {"requestId": request["requestId"], "agent": "lead",
                                     "accountKey": "acct", "epoch": 4})
            self.assertTrue(looked_up["found"])
            self.assertEqual(looked_up["result"], result)
        self.assertEqual(result["status"], "uncertain")
        self.assertEqual(len(rt.stop_calls), 1)

    def test_response_request_requires_full_identity_and_owned_stop_rejects_empty_list(self) -> None:
        rt = self.make_runtime()
        target = response_request(rt, "schema-response")["responseTarget"]
        request = {"action": "stop_response", "requestId": "schema-response", "agent": "lead",
                   "accountKey": "acct", "epoch": 4, "responseTarget": target}
        ProcessControlRequest.model_validate(request)
        with self.assertRaises(ValueError):
            ProcessControlRequest.model_validate({key: value for key, value in request.items()
                                                  if key != "responseTarget"})
        with self.assertRaises(ValueError):
            ProcessControlRequest.model_validate({"action": "stop_owned", "requestId": "empty-owned",
                "agent": "lead", "accountKey": "acct", "epoch": 4})

    def test_response_stop_failure_keeps_uncertain_receipt(self) -> None:
        rt = self.make_runtime()
        rt.fail_interrupt = True
        request = response_request(rt, "receipt-lost-response")
        first = action(rt, request)
        replay = action(rt, request)
        self.assertEqual(first["status"], "uncertain")
        self.assertEqual(first, replay)
        self.assertEqual(rt.stop_calls, [{"agent": "lead", "expected_account_key": "acct",
                                          "expected_epoch": 4, "expected_connection_id": "connection-1",
                                          "expected_thread_id": "thread-1", "expected_turn_id": "turn-1"}])
        self.assertTrue(rt.agent("lead")["inFlight"])

    def test_response_stop_rejects_same_epoch_replacement_after_receipt_snapshot(self) -> None:
        rt = self.make_runtime()
        request = response_request(rt, "same-epoch-response-race")

        def replace_turn() -> None:
            current = rt.agent("lead")
            current["turnId"] = "turn-2"
            rt.put("runtime_agents", "lead", current)

        rt.on_stop_response = replace_turn
        first = action(rt, request)
        replay = action(rt, request)
        self.assertEqual(first, replay)
        self.assertEqual(first["response"]["status"], "stale")
        self.assertEqual(first["status"], "completed")
        self.assertEqual(len(rt.stop_calls), 1)
        self.assertEqual(rt.native_server.calls, [])

    def test_runtime_response_interrupt_preserves_claude_tasks_and_owner_epoch(self) -> None:
        rt = self.make_runtime()
        rt.accounts["acct"]["provider"] = "claude"
        result = Runtime.stop_response(cast(Runtime, rt), "lead",
                                       expected_account_key="acct", expected_epoch=4,
                                       expected_connection_id="connection-1", expected_thread_id="thread-1",
                                       expected_turn_id="turn-1")
        self.assertEqual(result, {"status": "requested"})
        self.assertEqual(rt.native_server.calls, [("turn/interrupt", {
            "threadId": "thread-1", "turnId": "turn-1", "stopBackgroundTasks": False,
        })])
        self.assertTrue(rt.agent("lead")["inFlight"])
        self.assertEqual(rt.agent("lead")["epoch"], 4)
        self.assertEqual(rt.connection.execute(
            "SELECT count(*) FROM runtime_monitors WHERE json_extract(record,'$.status')='running'"
        ).fetchone()[0], 2)

    def test_legacy_runtime_interrupt_still_requests_claude_task_stop(self) -> None:
        rt = self.make_runtime()
        rt.accounts["acct"]["provider"] = "claude"
        Runtime.interrupt(cast(Runtime, rt), rt.agent("lead"))
        self.assertEqual(rt.native_server.calls, [("turn/interrupt", {
            "threadId": "thread-1", "turnId": "turn-1", "stopBackgroundTasks": True,
        })])

    def test_runtime_response_interrupt_rejects_changed_identity_and_keeps_lost_reply_uncertain(self) -> None:
        rt = self.make_runtime()
        with self.assertRaisesRegex(ValueError, "owner changed"):
            Runtime.stop_response(cast(Runtime, rt), "lead",
                                  expected_account_key="acct", expected_epoch=3,
                                  expected_connection_id="connection-1", expected_thread_id="thread-1",
                                  expected_turn_id="turn-1")
        self.assertEqual(rt.native_server.calls, [])
        with self.assertRaisesRegex(ValueError, "owner changed"):
            Runtime.stop_response(cast(Runtime, rt), "lead",
                                  expected_account_key="acct", expected_epoch=4,
                                  expected_connection_id="connection-1", expected_thread_id="thread-1",
                                  expected_turn_id="old-turn")
        self.assertEqual(rt.native_server.calls, [])
        rt.native_server.fail_interrupt = True
        result = Runtime.stop_response(cast(Runtime, rt), "lead",
                                       expected_account_key="acct", expected_epoch=4,
                                       expected_connection_id="connection-1", expected_thread_id="thread-1",
                                       expected_turn_id="turn-1")
        self.assertEqual(result["status"], "uncertain")
        self.assertTrue(rt.agent("lead")["inFlight"])
        self.assertEqual(rt.connection.execute(
            "SELECT count(*) FROM runtime_monitors WHERE json_extract(record,'$.status')='running'"
        ).fetchone()[0], 2)

    def test_runtime_rechecks_response_identity_at_native_dispatch(self) -> None:
        rt = self.make_runtime()

        class TurnChangingStream:
            def flush_locked(self, *_args: Any, **_kwargs: Any) -> None:
                current = rt.agent("lead")
                current["turnId"] = "replacement-turn"
                rt.put("runtime_agents", "lead", current)

        cast(Any, rt)._stream_buffer = TurnChangingStream()
        result = Runtime.stop_response(cast(Runtime, rt), "lead",
                                       expected_account_key="acct", expected_epoch=4,
                                       expected_connection_id="connection-1", expected_thread_id="thread-1",
                                       expected_turn_id="turn-1")
        self.assertEqual(result["status"], "stale")
        self.assertEqual(rt.native_server.calls, [])

    def test_owned_stop_uses_only_the_supplied_current_identity_snapshot(self) -> None:
        rt = self.make_runtime()
        listed = listing(rt, "lead")["processes"]
        selected = listed[0]
        identity = {key: selected[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        request = {"action": "stop_owned", "requestId": "receipt-2", "agent": "lead",
                   "accountKey": "acct", "epoch": 4, "processes": [identity]}
        result = action(rt, request)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(rt.cancel_calls, [selected["id"]])
        self.assertEqual(rt.cancel_contexts[0], {
            "expected_account_key": "acct", "expected_epoch": 4,
            "expected_connection_id": "connection-1", "expected_thread_id": "thread-1",
            "expected_monitor_epoch": 4, "expected_status": "running",
        })
        remaining = listing(rt, "lead")["processes"]
        self.assertEqual(len(remaining), 1)

    def test_owned_stop_reports_mixed_current_and_stale_items_per_process(self) -> None:
        rt = self.make_runtime()
        listed = listing(rt, "lead")["processes"]
        valid = {key: listed[0][key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        stale = {key: listed[1][key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        stale["generation"] = "old-generation"
        result = action(rt, {"action": "stop_owned", "requestId": "mixed-snapshot",
                             "agent": "lead", "accountKey": "acct", "epoch": 4,
                             "processes": [valid, stale]})
        self.assertEqual(result["status"], "partial")
        self.assertEqual([item["status"] for item in result["processes"]], ["stopped", "stale"])
        self.assertEqual(len(rt.cancel_calls), 1)

    def test_missing_saved_native_identity_disables_unsupported_stop(self) -> None:
        owner = {"id": "lead", "accountKey": "acct", "epoch": 4, "threadId": "thread-1"}
        process = _record_process("claude_task", {
            "id": "lead:task:task-1", "agent": "lead", "itemId": "task:task-1",
            "ownerEpoch": 4, "status": "running", "kind": "tool",
        }, owner)
        self.assertFalse(process["capabilities"]["readOutput"])
        self.assertFalse(process["capabilities"]["sendInput"])
        self.assertFalse(process["capabilities"]["stop"])
        codex_process = _record_process("codex_task", {
            "id": "lead:task:command-1", "agent": "lead", "itemId": "command-1",
            "processId": "native-1", "ownerEpoch": 4, "sourceAccountKey": "acct",
            "sourceThreadId": "thread-1", "status": "running", "kind": "command",
        }, owner)
        self.assertTrue(codex_process["capabilities"]["readOutput"])
        self.assertFalse(codex_process["capabilities"]["stop"])

    def test_saved_native_account_mismatch_disables_stop(self) -> None:
        owner = {"id": "lead", "accountKey": "current-account", "epoch": 4}
        process = _record_process("claude_task", {
            "id": "lead:task:task-1", "agent": "lead", "itemId": "task:task-1",
            "ownerEpoch": 4, "sourceAccountKey": "old-account",
            "sourceConnectionId": "old-connection", "sourceThreadId": "old-thread",
            "status": "running", "kind": "tool",
        }, owner)
        self.assertFalse(process["capabilities"]["stop"])

    def test_stale_native_task_epoch_disables_stop_and_input(self) -> None:
        owner = {"id": "lead", "accountKey": "acct", "epoch": 4, "threadId": "thread-1",
                 "autoWake": True}
        process = _record_process("codex_task", {
            "id": "lead:task:command-1", "agent": "lead", "itemId": "command-1",
            "processId": "native-1", "ownerEpoch": 3, "sourceAccountKey": "acct",
            "sourceConnectionId": "connection-1", "sourceThreadId": "thread-1",
            "status": "running", "kind": "command",
        }, owner, "connection-1")
        self.assertFalse(process["capabilities"]["stop"])
        self.assertFalse(process["capabilities"]["sendInput"])

    def test_codex_stop_checks_item_identity_and_rejects_repeated_page_cursor(self) -> None:
        rt = self.make_runtime()
        process = _record_process("codex_task", {
            "id": "lead:command-1", "agent": "lead", "itemId": "command-1",
            "processId": "native-1", "ownerEpoch": 4, "sourceAccountKey": "acct",
            "sourceConnectionId": "connection-1", "sourceThreadId": "thread-1",
            "turnId": "turn-1", "kind": "command", "status": "running",
        }, rt.agent("lead"), "connection-1")
        with patch.object(rt.native_server, "call", return_value={
            "data": [{"itemId": "other-item", "processId": "native-1"}],
        }) as native:
            self.assertEqual(_stop_process(rt, process)["status"], "already_finished")
            self.assertEqual(native.call_count, 1)
        with patch.object(rt.native_server, "call", return_value={"data": [], "nextCursor": "same"}) as native:
            with self.assertRaisesRegex(ValueError, "did not advance"):
                _stop_process(rt, process)
            self.assertEqual(native.call_count, 2)

    def test_native_stop_rejects_changed_connection_before_provider_call(self) -> None:
        rt = self.make_runtime()
        owner = rt.agent("lead")
        process = _record_process("codex_task", {
            "id": "lead:task:command-1", "agent": "lead", "itemId": "command-1",
            "processId": "native-1", "ownerEpoch": 4, "sourceAccountKey": "acct",
            "sourceConnectionId": "connection-1", "sourceThreadId": "thread-1",
            "turnId": "turn-1", "kind": "command", "status": "running",
        }, owner)
        rt.native_connection_id = "connection-2"
        with self.assertRaisesRegex(ValueError, "connection changed"):
            _server(rt, process)
        self.assertEqual(rt.native_server.calls, [])

    def test_claude_stop_rechecks_owner_epoch_after_task_lookup(self) -> None:
        rt = self.make_runtime()
        rt.accounts["acct"]["provider"] = "claude"
        owner = rt.agent("lead")
        task = {"id": "lead:task:task-1", "agent": "lead", "itemId": "task:task-1",
                "kind": "tool", "status": "running", "created": 1, "ownerEpoch": 4,
                "sourceAccountKey": "acct", "sourceConnectionId": "connection-1",
                "sourceThreadId": "thread-1", "turnId": "turn-1"}
        process = _record_process("claude_task", task, owner, "connection-1")

        def change_epoch() -> None:
            latest = rt.agent("lead")
            latest["epoch"] = 5
            rt.put("runtime_agents", "lead", latest)

        rt.native_server.on_state = change_epoch
        with self.assertRaisesRegex(ValueError, "owner changed"):
            _stop_process(rt, process)
        self.assertNotIn("claude/stopTask", [call[0] for call in rt.native_server.calls])

    def test_owned_stop_excludes_workers_until_the_request_names_them(self) -> None:
        rt = self.make_runtime()
        rt.put("runtime_agents", "worker", {
            "id": "worker", "rootId": "lead", "parentId": "lead", "accountKey": "acct",
            "epoch": 2, "threadId": "worker-thread", "provider": "codex",
        })
        rt.put("runtime_monitors", "worker-monitor", {
            "id": "worker-monitor", "agent": "worker", "epoch": 2, "status": "running",
            "created": 2, "command": "sleep 100", "interactive": True,
            "operation": {"accountKey": "acct", "connectionId": "connection-1",
                           "threadId": "worker-thread"},
        })
        worker = next(item for item in listing(rt, "lead")["processes"] if item["agent"] == "worker")
        identity = {key: worker[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        without_scope = action(rt, {
            "action": "stop_owned", "requestId": "worker-no-scope", "agent": "lead",
            "accountKey": "acct", "epoch": 4, "processes": [identity],
        })
        self.assertEqual(without_scope["status"], "partial")
        self.assertEqual(without_scope["processes"][0]["status"], "stale")
        self.assertEqual(rt.cancel_calls, [])
        with_scope = action(rt, {
            "action": "stop_owned", "requestId": "worker-explicit-scope", "agent": "lead",
            "accountKey": "acct", "epoch": 4, "processes": [identity], "includeDescendants": True,
        })
        self.assertEqual(with_scope["status"], "completed")
        self.assertEqual(rt.cancel_calls, ["worker-monitor"])

    def test_changed_owner_epoch_rejects_process_control(self) -> None:
        rt = self.make_runtime()
        with self.assertRaisesRegex(ValueError, "owner changed"):
            action(rt, {**response_request(rt, "receipt-3"), "epoch": 3})
        self.assertEqual(rt.stop_calls, [])

    def test_lost_monitor_reply_is_uncertain_and_same_receipt_does_not_repeat(self) -> None:
        rt = self.make_runtime()
        listed = listing(rt, "lead")["processes"][0]
        identity = {key: listed[key] for key in ("key", "generation", "agent", "accountKey", "epoch")}
        rt.fail_cancel_after_apply = True
        request = {"action": "stop_selected", "requestId": "receipt-lost", "agent": "lead",
                   "accountKey": "acct", "epoch": 4, "process": identity, "processes": []}
        first = action(rt, request)
        replay = action(rt, request)
        self.assertEqual(first["status"], "uncertain")
        self.assertEqual(first, replay)
        self.assertEqual(first["processes"][0]["status"], "uncertain")
        self.assertEqual(len(rt.cancel_calls), 1)

    def test_monitor_termination_error_stays_uncertain_on_retry(self) -> None:
        rt = self.make_runtime()
        process = _listed_processes(rt, rt.connection, "lead")[0]
        rt.cancel_result = {"id": process["id"], "status": "running",
                            "cancelRequested": True, "error": "terminate reply was lost"}
        result = _stop_process(rt, process)
        self.assertEqual(result["status"], "uncertain")
        self.assertEqual(result["error"], "terminate reply was lost")

    def test_runtime_keeps_monitor_termination_error_after_cancel_request(self) -> None:
        rt = self.make_runtime()
        rt.native_server.fail_terminate = True
        raw = {"id": "monitor-error", "agent": "lead", "epoch": 4, "status": "running",
               "command": "sleep 10", "tail": "", "operation": {
                   "accountKey": "acct", "connectionId": "connection-1", "threadId": "thread-1",
               }}
        rt.put("runtime_monitors", raw["id"], raw)
        expected = {
            "expected_account_key": "acct", "expected_epoch": 4,
            "expected_connection_id": "connection-1", "expected_thread_id": "thread-1",
            "expected_monitor_epoch": 4, "expected_status": "running",
        }
        first = Runtime.cancel_monitor(cast(Runtime, rt), raw["id"], owner="lead", **expected)
        second = Runtime.cancel_monitor(cast(Runtime, rt), raw["id"], owner="lead", **expected)
        self.assertIn("terminate reply was lost", first["error"])
        self.assertEqual(first["error"], second["error"])
        self.assertEqual([call[0] for call in rt.native_server.calls], ["command/exec/terminate"])

    def test_claude_task_stop_uses_task_stop_without_interrupting_the_query(self) -> None:
        rt = self.make_runtime()
        rt.accounts["acct"]["provider"] = "claude"
        owner = json.loads(rt.connection.execute(
            "SELECT record FROM runtime_agents WHERE id='lead'"
        ).fetchone()[0])
        task = {"id": "lead:task:task-1", "agent": "lead", "itemId": "task:task-1",
                "kind": "tool", "status": "running", "created": 1, "ownerEpoch": 4,
                "sourceAccountKey": "acct", "sourceConnectionId": "connection-1",
                "sourceThreadId": "thread-1", "turnId": "turn-1"}
        process = _record_process("claude_task", task, owner)
        result = _stop_process(rt, process)
        self.assertEqual(result["status"], "stopped")
        self.assertEqual([call[0] for call in rt.native_server.calls], [
            "claude/state", "claude/stopTask", "claude/state",
        ])
        self.assertNotIn("turn/interrupt", [call[0] for call in rt.native_server.calls])


if __name__ == "__main__":
    unittest.main()
