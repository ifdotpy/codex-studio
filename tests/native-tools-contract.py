#!/usr/bin/env python3
"""Native tool refresh preserves history and refuses unconfirmed account idleness."""
import copy
import concurrent.futures
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import queue
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_native_tools as tools

OLD = [{"name": "old_tool", "description": "Previous tool instructions. " * 10,
        "inputSchema": {"type": "object", "properties": {}}}]
NEW = [{"name": "orchestration_review", "description": "Review", "inputSchema": {"type": "object", "properties": {}}}]


class Native:
    def __init__(self, rt):
        self.rt = rt
        self.pending = {}
        self.callbacks, self.clock_replies = queue.Queue(), queue.Queue()
        self.calls, self.close_count = [], 0
        self.extra = {}
        self.paths = {}
        self.fail_method = None
        self.background = []
        self.native_queue = []
        self.refuse_exit = False
        self.defer_fork = False
        self.forks = []
        self.proc = self

    def call(self, method, params, timeout=60):
        self.calls.append((method, copy.deepcopy(params)))
        if method == self.fail_method:
            raise TimeoutError("Native state is unknown")
        if method == "thread/loaded/list":
            return {"data": ["thread-1", *self.extra], "nextCursor": None}
        if method == "thread/read":
            tid = params["threadId"]
            return {"thread": {"id": tid, "path": str(self.paths.get(tid, self.rt.path)),
                               "status": {"type": self.extra.get(tid, "idle")}}}
        if method == "thread/queue/list":
            return {"data": self.native_queue}
        if method == "thread/backgroundTerminals/list":
            return {"data": self.background}
        raise AssertionError("Unexpected native request " + method)

    def close(self):
        assert not self.rt.lock._is_owned(), "Process close must not hold Runtime.lock"
        assert tools.account_reserved(self.rt, "default")
        assert self.rt.connection_ids["default"].startswith("retired:")
        self.close_count += 1

    def after_events(self, callback):
        callback()

    def submit(self, method, params):
        assert method == "thread/fork", method
        self.calls.append((method, copy.deepcopy(params)))
        destination = "fork-" + str(len(self.forks) + 1)
        path = self.rt.path.with_name("rollout-" + destination + ".jsonl")
        source = self.paths.get(params["threadId"], self.rt.path)
        metadata = json.loads(source.read_bytes().partition(b"\n")[0])
        metadata["payload"].update(id=destination, forked_from_id=params["threadId"],
                                   history_base={"thread_id": params["threadId"], "end_byte_offset": source.stat().st_size})
        path.write_text(json.dumps(metadata) + "\n")
        self.paths[destination] = path
        self.extra[destination] = "idle"
        result = {"thread": {"id": destination, "path": str(path)}}
        future = concurrent.futures.Future()
        self.forks.append((future, result))
        if not self.defer_fork:
            future.set_result(result)
        return future

    def on_result(self, future, callback):
        future.add_done_callback(callback)

    def wait(self, future, timeout=60):
        if not future.done():
            raise TimeoutError("Fixture fork receipt is pending")
        return future.result()

    def poll(self):
        return None if self.refuse_exit or not self.close_count else 0


class Runtime:
    def __init__(self, root):
        self.root = root
        self.home = root / "home"
        self.path = self.home / "sessions" / "rollout-thread-1.jsonl"
        self.path.parent.mkdir(parents=True)
        self.original = (json.dumps({"type": "session_meta", "payload": {
            "id": "thread-1", "dynamic_tools": tools.catalog(OLD), "custom": {"keep": True}}}) + "\n").encode()
        self.tail = b'{"type":"response_item","payload":{"text":"Keep complete history"}}\n' * 20000
        self.path.write_bytes(self.original + self.tail)
        self.closed = False
        self.lock, self.start_lock = threading.RLock(), threading.Lock()
        self.changed = threading.Event()
        self.preparations = {}
        self.loaded = {"chat-1"}
        self.connection_ids = {"default": "connection-1"}
        self.accounts = self
        self.servers = {"default": Native(self)}
        self.server = self.servers["default"]
        self.db_path = root / "state.sqlite3"
        with self.db() as db:
            for name in ("agents", "requests", "tasks", "monitors", "tool_requests"):
                db.execute(f"CREATE TABLE runtime_{name}(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            db.execute("CREATE INDEX runtime_tool_request_actor ON runtime_tool_requests("
                       "json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC)")
            db.execute("CREATE TABLE runtime_events(id TEXT, agent TEXT, status TEXT)")
            self.put(db, "agents", {"id": "chat-1", "threadId": "thread-1", "epoch": 3,
                "accountKey": "default", "status": "complete", "inFlight": False,
                "cwd": str(root), "provider": "codex", "accountHistory": [{"threadId": "earlier", "accountKey": "other"}]})

    def get(self, key):
        return {"provider": "codex"}

    def home(self, key):
        raise AssertionError("Instance home path shadows this method")

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.db_path)
        try:
            with db:
                yield db
        finally:
            db.close()

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]

    def put(self, db, table, record):
        db.execute(f"INSERT INTO runtime_{table} VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (record["id"], json.dumps(record)))

    def agent(self, key, db=None):
        if db is None:
            with self.db() as db:
                return self.agent(key, db)
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

    def tool_definitions(self, agent):
        return NEW

    def new_thread_params(self, agent):
        return {"cwd": agent["cwd"], "model": "gpt-5.6-sol", "dynamicTools": self.tool_definitions(agent)}

    def connect(self, key):
        return self.servers[key]

    @staticmethod
    def submit_reserved(server, method, params):
        return server.submit(method, params)


class Contract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-native-tools-")
        self.rt = Runtime(Path(self.temp.name))
        self.native = self.rt.server
        home = self.rt.home
        self.rt.accounts = type("Accounts", (), {"get": lambda _, key: {"provider": "codex"},
                                                "home": lambda _, key: home})()

    def tearDown(self):
        self.temp.cleanup()

    def change_agent(self, **changes):
        with self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            agent.update(changes)
            self.rt.put(db, "agents", agent)

    def test_preserves_chat_identity_full_history_and_receipt_scopes(self):
        before = self.rt.agent("chat-1")
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(self.native.close_count, 1)
        after = self.rt.agent("chat-1")
        self.assertEqual({k: after[k] for k in before}, before)
        self.assertFalse(tools.needs_refresh(after, NEW))
        self.assertEqual(self.rt.path.read_bytes().partition(b"\n")[2], self.rt.tail)
        self.assertEqual(len(self.rt.path.read_bytes().partition(b"\n")[0]) + 1, len(self.rt.original))
        file = result["files"]["chat-1"]
        self.assertEqual(file["tailSha256"], hashlib.sha256(self.rt.tail).hexdigest())
        self.assertEqual(file["newSha256"], hashlib.sha256(self.rt.path.read_bytes()).hexdigest())
        self.assertEqual(file["status"], "completed")
        self.assertFalse(Path(file["temporary"]).exists())
        self.assertFalse(tools.account_reserved(self.rt, "default"))
        self.assertNotIn("default", self.rt.servers)
        self.assertNotIn("chat-1", self.rt.loaded)
        self.assertFalse(any(method in {"turn/start", "thread/fork", "thread/archive"} for method, _ in self.native.calls))

    def test_current_metadata_requires_no_recycle(self):
        tools.replace_header(self.rt.path, "thread-1", NEW, lambda _: None)
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(self.native.close_count, 0)
        self.assertFalse(tools.needs_refresh(self.rt.agent("chat-1"), NEW))

    def test_active_studio_agent_blocks_before_native_calls(self):
        self.change_agent(inFlight=True, status="running")
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "waiting")
        self.assertEqual(self.native.calls, [])
        self.assertEqual(self.native.close_count, 0)

    def test_active_native_child_outside_studio_blocks(self):
        self.native.extra = {"unmanaged-child": "active"}
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)
        self.assertTrue(any(params.get("threadId") == "unmanaged-child" for _, params in self.native.calls))

    def test_unknown_native_background_state_blocks(self):
        self.native.fail_method = "thread/backgroundTerminals/list"
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)
        self.assertEqual(self.rt.path.read_bytes(), self.rt.original + self.rt.tail)

    def test_native_background_or_queue_blocks(self):
        for attr in ("background", "native_queue"):
            with self.subTest(attr=attr):
                setattr(self.native, attr, [{"id": "pending"}])
                self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
                self.assertEqual(self.native.close_count, 0)
                setattr(self.native, attr, [])

    def test_pending_tool_receipt_blocks(self):
        with self.rt.db() as db:
            self.rt.put(db, "tool_requests", {"id": "unknown", "agent": "chat-1", "stage": "running", "outcome": "unknown"})
        self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)

    def test_historical_uncertainty_does_not_block_confirmed_idle_process(self):
        record = {"id": "unknown", "agent": "chat-1", "stage": "failed", "outcome": "unknown"}
        with self.rt.db() as db:
            self.rt.put(db, "tool_requests", record)
            db.execute("INSERT INTO runtime_events VALUES ('old-input','chat-1','uncertain')")
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "completed", result)
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db, "tool_requests"), [record])
            self.assertEqual(db.execute("SELECT status FROM runtime_events").fetchone()[0], "uncertain")
        self.assertEqual(self.native.close_count, 1)

    def transfer_record(self, **changes):
        operation = {"id": "old-transfer", "leadId": "other-lead", "status": "pending",
                     "targetAccountKey": "default", "members": {"other-lead": {"phase": "waiting"}}}
        operation.update(changes)
        with self.rt.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            self.rt.put(db, "account_transfers", operation)
        return operation

    def test_orphan_waiting_transfer_does_not_block_or_change_audit_record(self):
        operation = self.transfer_record()
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "completed", result)
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db, "account_transfers"), [operation])

    def test_owned_target_transfer_blocks_even_when_source_account_differs(self):
        self.transfer_record()
        with self.rt.db() as db:
            self.rt.put(db, "agents", {"id": "other-lead", "accountKey": "other",
                                      "accountTransferId": "old-transfer"})
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "waiting", result)
        self.assertEqual(self.native.close_count, 0)

    def test_pending_transfer_summary_keeps_scheduler_ownership(self):
        self.transfer_record()
        with self.rt.db() as db:
            self.rt.put(db, "agents", {"id": "other-lead", "accountKey": "other",
                "accountTransfer": {"id": "old-transfer", "status": "pending"}})
        self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)

    def test_unowned_transfer_mutation_receipt_still_blocks(self):
        for phase in ("reading", "submitted", "unknown", "ready"):
            with self.subTest(phase=phase):
                self.transfer_record(members={"other-lead": {"phase": phase,
                                     "nativeMethod": "thread/fork"}})
                self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
                self.assertEqual(self.native.close_count, 0)

    def test_transfer_worker_blocks_before_native_submission(self):
        from types import SimpleNamespace
        self.transfer_record()
        for running, futures in (({"other-lead"}, {}), (set(), {("old-transfer", "other-lead"): object()})):
            self.rt._account_transfers = SimpleNamespace(running=running, futures=futures)
            self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
            self.assertEqual(self.native.close_count, 0)

    def test_mark_current_clears_only_owned_update_notice(self):
        for error in ("The native tool fork receipt arrived", "Unrelated native error"):
            with self.subTest(error=error):
                agent = {"threadId": "current-thread", "error": error,
                         "nativeToolUpdate": {"message": "The native tool fork receipt arrived"}}
                tools.mark_current(agent, NEW)
                self.assertNotIn("nativeToolUpdate", agent)
                self.assertFalse(tools.needs_refresh(agent, NEW))
                if error == "Unrelated native error":
                    self.assertEqual(agent["error"], error)
                else:
                    self.assertIsNone(agent["error"])

    def test_pending_native_callback_blocks(self):
        self.native.callbacks.put({"method": "turn/started"})
        self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)

    def test_running_callback_blocks_even_when_queue_empty(self):
        self.native.callbacks.put({"method": "turn/started"})
        self.native.callbacks.get_nowait()
        self.assertTrue(self.native.callbacks.empty())
        self.assertEqual(tools.refresh_account(self.rt)["status"], "waiting")
        self.assertEqual(self.native.close_count, 0)

    def test_unconfirmed_process_exit_never_changes_header(self):
        self.native.refuse_exit = True
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "blocked")
        self.assertEqual(self.rt.path.read_bytes(), self.rt.original + self.rt.tail)
        self.assertTrue(tools.needs_refresh(self.rt.agent("chat-1"), NEW))
        self.assertTrue(tools.account_reserved(self.rt, "default"))
        self.assertIs(self.rt._native_tools_retiring["default"]["server"], self.native)
        with self.assertRaisesRegex(ValueError, "previous native process has not stopped"):
            tools.assert_connect_allowed(self.rt, "default")
        self.native.refuse_exit = False
        tools.assert_connect_allowed(self.rt, "default")
        self.assertFalse(tools.account_reserved(self.rt, "default"))

    def test_local_optional_question_is_preserved_and_does_not_block(self):
        request = {"id": "question", "agent": "chat-1", "method": "agent/asyncQuestion", "status": "pending"}
        with self.rt.db() as db:
            self.rt.put(db, "requests", request)
        result = tools.refresh_account(self.rt)
        self.assertEqual(result["status"], "completed", result)
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db, "requests"), [request])

    def test_unrelated_receipt_volume_does_not_increase_idle_query_work(self):
        with self.rt.db() as db:
            record = json.dumps({"id": "other", "agent": "another-account", "stage": "completed", "result": "x" * 2048})
            db.executemany("INSERT INTO runtime_tool_requests VALUES (?,?)", ((str(i), record) for i in range(10000)))
            plan = db.execute("EXPLAIN QUERY PLAN SELECT 1 FROM runtime_tool_requests WHERE "
                              "json_extract(record,'$.agent')=? AND json_extract(record,'$.stage') "
                              "IN ('queued','running') LIMIT 1", ("chat-1",)).fetchall()
            self.assertIn("runtime_tool_request_actor", str(plan))
            steps = 0
            def step():
                nonlocal steps
                steps += 1
                return 0
            db.set_progress_handler(step, 1)
            with patch.object(self.rt, "records", wraps=self.rt.records) as records:
                agents, reason = tools._local_idle(self.rt, db, "default", self.native)
                self.assertIsNone(reason)
                self.assertEqual([call.args[1] for call in records.call_args_list], ["agents"])
            db.set_progress_handler(None, 0)
            self.assertLess(steps, 1000, steps)

    def test_blocked_gate_retains_queued_input_without_repeat_worker(self):
        with self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            agent["status"] = "queued"
            tools._wait_notice(self.rt, db, agent, NEW, "blocked", "Native rollout is missing")
            db.execute("INSERT INTO runtime_events VALUES ('pending','chat-1','pending')")
            with patch.object(tools.threading, "Thread", side_effect=AssertionError("Do not repeat blocked update")):
                self.assertFalse(tools.gate(self.rt, db, agent, NEW))
            self.assertEqual(db.execute("SELECT status FROM runtime_events").fetchone()[0], "pending")
            self.assertEqual(self.rt.agent("chat-1", db)["error"], "Native rollout is missing")

    def test_busy_account_admits_continuation_without_catalog_worker_or_wait_notice(self):
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            agent.update(status="queued", autoWake=True)
            tools._wait_notice(self.rt, db, agent, NEW, "waiting", "Waiting for the account tool catalog update")
            self.rt.put(db, "agents", {**agent, "id": "busy-chat", "inFlight": True, "status": "running"})
            db.execute("INSERT INTO runtime_events VALUES ('pending','chat-1','pending')")
            with patch.object(tools.threading, "Thread", side_effect=AssertionError("No catalog worker while account is busy")):
                self.assertTrue(tools.gate(self.rt, db, agent, NEW))
            current = self.rt.agent("chat-1", db)
            self.assertNotIn("nativeToolUpdate", current)
            self.assertIsNone(current["error"])
            self.assertTrue(tools.needs_refresh(current, NEW))
            self.assertEqual(db.execute("SELECT status FROM runtime_events").fetchone()[0], "pending")
        self.assertEqual(self.native.close_count, 0)
        self.assertEqual(self.native.calls, [])

    def test_idle_account_schedules_one_refresh_then_admits_current_catalog(self):
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            self.assertFalse(tools.gate(self.rt, db, agent, NEW))
            self.assertFalse(tools.gate(self.rt, db, agent, NEW))
        tools.wait_updates(self.rt)
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            self.assertTrue(tools.gate(self.rt, db, agent, NEW))
            self.assertNotIn("nativeToolUpdate", agent)
        self.assertEqual(self.native.close_count, 1)

    def test_worker_race_with_busy_account_defers_without_visible_error(self):
        entered, release = threading.Event(), threading.Event()
        original = self.rt.connect
        def connect(key):
            entered.set()
            if not release.wait(5):
                raise AssertionError("Test did not release the catalog worker")
            return original(key)
        with patch.object(self.rt, "connect", side_effect=connect):
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent("chat-1", db)
                self.assertFalse(tools.gate(self.rt, db, agent, NEW))
            self.assertTrue(entered.wait(5))
            with self.rt.lock, self.rt.db() as db:
                self.rt.put(db, "agents", {**agent, "id": "busy-chat", "inFlight": True, "status": "running"})
            release.set()
            tools.wait_updates(self.rt)
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            self.assertTrue(tools.gate(self.rt, db, agent, NEW))
            self.assertNotIn("nativeToolUpdate", agent)
            self.assertFalse(agent.get("error"))
        self.assertEqual(self.native.close_count, 0)

    def test_actual_catalog_reservation_still_blocks_continuation(self):
        self.rt._native_tools_refreshing = {"default"}
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            self.rt.put(db, "agents", {**agent, "id": "busy-chat", "inFlight": True, "status": "running"})
            self.assertFalse(tools.gate(self.rt, db, agent, NEW))
        self.assertEqual(self.native.close_count, 0)

    def test_busy_account_does_not_bypass_unsettled_native_fork(self):
        self.growth_tools()
        self.native.defer_fork = True
        tools.refresh_account(self.rt, agent_ids=["chat-1"])
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            self.rt.put(db, "agents", {**agent, "id": "busy-chat", "inFlight": True, "status": "running"})
            self.assertFalse(tools.gate(self.rt, db, agent, self.rt.tool_definitions(agent)))
        self.assertEqual(len(self.native.forks), 1)

    def test_source_changed_during_copy_is_not_overwritten(self):
        def persist(record):
            if record["status"] == "prepared":
                with self.rt.path.open("ab") as stream:
                    stream.write(b'{"new":"external turn"}\n')
        with self.assertRaisesRegex(ValueError, "changed before the replacement"):
            tools.replace_header(self.rt.path, "thread-1", NEW, persist)
        self.assertTrue(self.rt.path.read_bytes().startswith(self.rt.original))
        self.assertTrue(self.rt.path.read_bytes().endswith(b'{"new":"external turn"}\n'))
        self.assertEqual(list(self.rt.path.parent.glob("*.tmp")), [])

    def test_receipt_failure_before_replace_leaves_original(self):
        def persist(record):
            if record["status"] == "prepared":
                raise OSError("Database unavailable")
        with self.assertRaisesRegex(OSError, "Database unavailable"):
            tools.replace_header(self.rt.path, "thread-1", NEW, persist)
        self.assertEqual(self.rt.path.read_bytes(), self.rt.original + self.rt.tail)
        self.assertEqual(list(self.rt.path.parent.glob("*.tmp")), [])

    def test_lost_final_receipt_does_not_repeat_write(self):
        receipts = []
        def persist(record):
            receipts.append(record)
            if record["status"] == "completed":
                raise OSError("Database unavailable after replace")
        with self.assertRaises(OSError):
            tools.replace_header(self.rt.path, "thread-1", NEW, persist)
        after = self.rt.path.read_bytes()
        with patch.object(tools.os, "replace", side_effect=AssertionError("Do not repeat replacement")):
            result = tools.replace_header(self.rt.path, "thread-1", NEW, lambda _: None)
        self.assertEqual(result["status"], "unchanged")
        self.assertEqual(self.rt.path.read_bytes(), after)
        self.assertEqual(receipts[-2]["newSha256"], hashlib.sha256(after).hexdigest())

    def test_wrong_native_identity_fails_before_retirement(self):
        self.rt.path.write_bytes(self.rt.original.replace(b'thread-1', b'thread-2') + self.rt.tail)
        self.assertEqual(tools.refresh_account(self.rt)["status"], "blocked")
        self.assertEqual(self.native.close_count, 0)

    def test_broken_dormant_thread_does_not_block_requested_valid_chat(self):
        with self.rt.db() as db:
            missing = {**self.rt.agent("chat-1", db), "id": "missing-chat", "threadId": "missing-native", "status": "paused"}
            self.rt.put(db, "agents", missing)
        result = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(self.native.close_count, 1)
        self.assertFalse(any(p.get("threadId") == "missing-native" for _, p in self.native.calls))
        self.assertEqual(self.rt.agent("missing-chat"), missing)

    def test_two_queued_chats_refresh_with_one_idle_process_recycle(self):
        second = self.rt.path.with_name("rollout-thread-2.jsonl")
        second.write_bytes(self.rt.original.replace(b'thread-1', b'thread-2') + self.rt.tail)
        self.native.extra = {"thread-2": "idle"}
        self.native.paths["thread-2"] = second
        with self.rt.db() as db:
            first = self.rt.agent("chat-1", db)
            first.update(status="queued", autoWake=True)
            self.rt.put(db, "agents", first)
            self.rt.put(db, "agents", {**first, "id": "chat-2", "threadId": "thread-2"})
        result = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(self.native.close_count, 1)
        self.assertFalse(tools.needs_refresh(self.rt.agent("chat-1"), NEW))
        self.assertFalse(tools.needs_refresh(self.rt.agent("chat-2"), NEW))

    def test_broken_queued_chat_is_blocked_without_blocking_valid_batch_member(self):
        with self.rt.db() as db:
            first = self.rt.agent("chat-1", db)
            first.update(status="queued", autoWake=True)
            self.rt.put(db, "agents", first)
            self.rt.put(db, "agents", {**first, "id": "bad-chat", "threadId": "missing-native"})
        result = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(result["status"], "partial", result)
        self.assertEqual(self.native.close_count, 1)
        self.assertFalse(tools.needs_refresh(self.rt.agent("chat-1"), NEW))
        self.assertEqual(self.rt.agent("bad-chat")["nativeToolUpdate"]["status"], "blocked")

    def growth_tools(self):
        definitions = [{**NEW[0], "description": "Current tool instructions. " * 100}]
        self.rt.tool_definitions = lambda _: definitions
        return definitions

    def test_growth_forks_original_and_preserves_ancestor_byte_boundaries(self):
        definitions = self.growth_tools()
        original = self.rt.path.read_bytes()
        result = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(len(self.native.forks), 1)
        agent = self.rt.agent("chat-1")
        self.assertEqual(agent["id"], "chat-1")
        self.assertEqual(agent["threadId"], "fork-1")
        self.assertEqual(agent["accountHistory"][-1]["threadId"], "thread-1")
        self.assertFalse(tools.needs_refresh(agent, definitions))
        self.assertEqual(self.rt.path.read_bytes(), original)
        metadata = json.loads(self.native.paths["fork-1"].read_bytes().partition(b"\n")[0])["payload"]
        self.assertEqual(metadata["history_base"]["end_byte_offset"], len(original))
        self.assertEqual(metadata["dynamic_tools"], tools.catalog(definitions))

    def test_unknown_fork_receipt_is_not_repeated_after_restart_or_late_response(self):
        definitions = self.growth_tools()
        self.native.defer_fork = True
        first = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(first["status"], "blocked", first)
        self.assertEqual(self.native.close_count, 0)
        ticket_id = self.rt.agent("chat-1")["nativeToolRefreshId"]
        self.assertEqual(tools._ticket(self.rt, ticket_id)["status"], "unknown")
        # A new caller has only the SQLite receipt, no pending future map.
        second = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(second["status"], "blocked", second)
        self.assertEqual(len(self.native.forks), 1)
        future, result = self.native.forks[0]
        future.set_result(result)
        self.assertEqual(tools._ticket(self.rt, ticket_id)["status"], "ready")
        final = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(final["status"], "completed", final)
        self.assertEqual(len(self.native.forks), 1)
        self.assertFalse(tools.needs_refresh(self.rt.agent("chat-1"), definitions))

    def test_destination_descendant_blocks_growth_without_changing_original(self):
        self.growth_tools()
        self.native.defer_fork = True
        tools.refresh_account(self.rt, agent_ids=["chat-1"])
        future, result = self.native.forks[0]
        future.set_result(result)
        child = self.rt.path.with_name("rollout-descendant.jsonl")
        child.write_text(json.dumps({"type": "session_meta", "payload": {
            "id": "descendant", "history_base": {"thread_id": "fork-1", "end_byte_offset": 123}}}) + "\n")
        before = self.native.paths["fork-1"].read_bytes()
        final = tools.refresh_account(self.rt, agent_ids=["chat-1"])
        self.assertEqual(final["status"], "blocked", final)
        self.assertIn("descendant", final["reason"])
        self.assertEqual(self.native.paths["fork-1"].read_bytes(), before)
        self.assertEqual(self.rt.agent("chat-1")["threadId"], "thread-1")
        self.assertEqual(len(self.native.forks), 1)


if __name__ == "__main__":
    unittest.main()
