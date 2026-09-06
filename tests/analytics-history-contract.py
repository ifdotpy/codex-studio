#!/usr/bin/env python3
"""Native rollout parsing, profile boundaries, and resumable import contracts."""
import contextlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_analytics_history import AnalyticsHistoryMixin, normalize_item, rollout_actions

THREAD = "01a07781-5d19-7390-bc74-c12094143962"
OTHER_THREAD = "01a07781-5d19-7390-bc74-c12094143963"


def line(kind, payload):
    return json.dumps({"type": kind, "timestamp": "2026-09-06T18:00:00Z", "payload": payload}).encode() + b"\n"


class Fixture(AnalyticsHistoryMixin):
    def __init__(self, root, account="first"):
        self.root = Path(root)
        self.lock = threading.RLock()
        self.closed = False
        self.homes = {"first": self.root / "profile-one", "second": self.root / "profile-two"}
        for home in self.homes.values():
            (home / "sessions" / "2026").mkdir(parents=True, exist_ok=True)
        self.accounts = type("Accounts", (), {"home": lambda _, key: self.homes[key]})()
        self.path = self.homes[account] / "sessions" / "2026" / ("rollout-2026-09-06-" + THREAD + ".jsonl")
        with self.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_agents (id TEXT PRIMARY KEY, record TEXT)")
            db.execute("CREATE TABLE IF NOT EXISTS captured (kind TEXT, method TEXT, record TEXT)")
            db.execute("INSERT OR IGNORE INTO runtime_agents VALUES (?,?)", ("agent", json.dumps({
                "id": "agent", "accountKey": account, "threadId": THREAD, "model": "current-not-historical"})))
            self.analytics_history_init(db)

    @contextlib.contextmanager
    def db(self):
        db = sqlite3.connect(self.root / "fixture.sqlite3")
        try:
            with db:
                yield db
        finally:
            db.close()

    def records(self, db, table):
        return [json.loads(r[0]) for r in db.execute("SELECT record FROM runtime_" + table)]

    def agent(self, key, db):
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

    def analytics_event(self, db, a, method, p, *, at=None, source=None):
        db.execute("INSERT INTO captured VALUES (?,?,?)", ("event", method, json.dumps({"a": a, "p": p, "at": at, "source": source})))

    def analytics_model_payload(self, db, a, p, *, at=None, turn_id=None, source=None):
        db.execute("INSERT INTO captured VALUES (?,?,?)", ("payload", p.get("type"), json.dumps({"a": a, "p": p, "at": at, "turnId": turn_id, "source": source})))

    def state(self):
        with self.db() as db:
            row = db.execute("SELECT record FROM analytics_history").fetchone()
        return json.loads(row[0]) if row else None

    def captured(self):
        with self.db() as db:
            return [(r[0], r[1], json.loads(r[2])) for r in db.execute("SELECT * FROM captured")]


class ParsingTests(unittest.TestCase):
    def context(self):
        return {"threadId": THREAD, "turnId": "turn-one", "model": "gpt-6-astra", "window": 258400}

    def test_request_usage_preserves_raw_record_and_cache_writes(self):
        raw = {"thread_id": THREAD, "turn_id": "t", "response_id": "r", "session_id": "s",
               "usage": {"input_tokens": 120, "cached_input_tokens": 80, "cache_write_input_tokens": 10,
                         "output_tokens": 12, "reasoning_output_tokens": 7, "total_tokens": 132},
               "thread_token_usage": {"total_tokens": 2000}, "turn_token_usage": {"total_tokens": 600}}
        _, method, p, at = rollout_actions({"type": "token_usage_record", "payload": raw}, self.context(), "id", 4)[0]
        self.assertEqual(method, "thread/tokenUsage/updated")
        self.assertEqual(p["rawTokenUsageRecord"], raw)
        self.assertEqual(p["responseId"], "r")
        self.assertEqual(p["requestUsage"]["cacheWriteInputTokens"], 10)
        self.assertEqual(p["tokenUsage"]["total"]["totalTokens"], 2000)
        self.assertEqual(p["tokenUsage"]["last"]["totalTokens"], 132)
        self.assertEqual(p["tokenUsage"]["modelContextWindow"], 258400)

    def test_notice_association_handles_reset_counters_without_equal_size_aliasing(self):
        context = self.context()
        values = {"input_tokens": 100, "cached_input_tokens": 60, "output_tokens": 20,
                  "reasoning_output_tokens": 10, "total_tokens": 120}
        raw = {"thread_id": THREAD, "turn_id": context["turnId"], "response_id": "response-one",
               "usage": values, "thread_token_usage": {"total_tokens": 9000}}
        rollout_actions({"type": "token_usage_record", "payload": raw}, context, "one", 1)
        notice = {"type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": {"total_tokens": 120}, "last_token_usage": values}}}
        body = rollout_actions(notice, context, "two", 2)[0][2]
        self.assertEqual(body["responseId"], "response-one")
        self.assertEqual(body["tokenUsage"]["total"]["totalTokens"], 120)
        self.assertIsNone(context["pendingUsage"])
        self.assertEqual(rollout_actions(notice, context, "duplicate", 3)[0][2]["responseId"], "response-one")
        raw["response_id"] = "response-two"
        raw["thread_token_usage"] = {"total_tokens": 9120}
        rollout_actions({"type": "token_usage_record", "payload": raw}, context, "three", 4)
        notice["payload"]["info"]["total_token_usage"]["total_tokens"] = 240
        self.assertEqual(rollout_actions(notice, context, "four", 5)[0][2]["responseId"], "response-two")
        context["turnId"] = "different-turn"
        self.assertIsNone(rollout_actions(notice, context, "five", 6)[0][2]["responseId"])

    def test_timestamp_fallback_has_explicit_provenance(self):
        action = rollout_actions({"type": "response_item", "timestamp": "bad", "payload": {
            "type": "message", "role": "user", "content": []}}, self.context(), "id", 10)[0]
        self.assertEqual(action[2]["_analyticsTimestampSource"], "fileModifiedEstimate")

    def test_legacy_cumulative_sample_and_rate_limits(self):
        actions = rollout_actions({"type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": {"input_tokens": 100, "total_tokens": 120},
            "last_token_usage": {"input_tokens": 50, "total_tokens": 60}, "model_context_window": 500},
            "rate_limits": {"primary": {"used_percent": 20}}}}, self.context(), "id", 5)
        self.assertEqual(actions[0][2]["tokenUsage"]["total"]["totalTokens"], 120)
        self.assertEqual(actions[1][1], "analytics/rateLimits")
        self.assertNotIn("requestUsage", actions[0][2])

    def test_native_command_duration_and_payload_are_distinct(self):
        record = {"type": "event_msg", "payload": {"type": "item_completed", "thread_id": THREAD,
            "turn_id": "t", "started_at_ms": 1000, "completed_at_ms": 2500, "item": {
                "type": "CommandExecution", "id": "exec-123", "aggregated_output": "native stdout",
                "duration": {"secs": 1, "nanos": 500000000}, "exit_code": 2}}}
        _, method, p, _ = rollout_actions(record, self.context(), "id", 3)[0]
        self.assertEqual(p["item"]["durationMs"], 1500)
        self.assertEqual(p["startedAt"], 1)
        self.assertEqual(p["completedAt"], 2.5)
        self.assertEqual(p["item"]["type"], "commandExecution")
        self.assertEqual(p["item"]["exitCode"], 2)
        body = {"type": "function_call_output", "call_id": "call-wrapper", "output": [{"type": "input_text", "text": "model view"}]}
        action = rollout_actions({"type": "response_item", "payload": body}, self.context(), "id2", 4)[0]
        self.assertEqual(action[0], "payload")
        self.assertEqual(action[2]["call_id"], "call-wrapper")
        self.assertNotEqual(action[2]["call_id"], p["item"]["id"])

    def test_model_namespace_role_and_metadata_timestamp(self):
        body = {"type": "function_call", "namespace": "notes", "name": "write_file", "call_id": "call",
                "arguments": "{}", "internal_chat_message_metadata_passthrough": {"turn_id": "historical", "create_time": 99}}
        action = rollout_actions({"type": "response_item", "payload": body}, self.context(), "id", 3)[0]
        self.assertEqual(action[2]["namespace"], "notes")
        self.assertEqual(action[2]["_analyticsTurnId"], "historical")
        self.assertEqual(action[3], 99)
        action = rollout_actions({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": []}}, self.context(), "id", 3)[0]
        self.assertEqual(action[2]["role"], "developer")

    def test_compaction_replacements_have_separate_categories(self):
        actions = rollout_actions({"type": "compacted", "payload": {
            "window_number": 2, "replacement_history": [{"type": "message", "role": "system", "content": []}],
            "guardian_history": [{"type": "function_call", "call_id": "old-call", "arguments": "{}"}]}}, self.context(), "id", 3)
        self.assertEqual(actions[1][2]["_analyticsCategory"], "compactionReplacement")
        self.assertEqual(actions[2][2]["_analyticsCategory"], "compactionGuardian")
        self.assertNotEqual(actions[1][2]["_analyticsId"], actions[2][2]["_analyticsId"])

    def test_wrong_thread_record_is_coverage_error(self):
        actions = rollout_actions({"type": "token_usage_record", "payload": {"thread_id": OTHER_THREAD}}, self.context(), "id", 3)
        self.assertEqual(actions[0][0], "coverage")


class ImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.f = Fixture(self.temp.name)
        self.header = line("session_meta", {"id": THREAD})

    def tearDown(self):
        self.temp.cleanup()

    def test_bounded_import_resume_and_no_historical_model_guess(self):
        self.f.path.write_bytes(self.header + line("turn_context", {"turn_id": "past", "model": "past-model"}) +
                               line("response_item", {"type": "function_call", "call_id": "c", "arguments": "secret"}))
        self.assertTrue(self.f.analytics_history_step(max_records=1))
        self.assertEqual(self.f.state()["status"], "catchingUp")
        self.assertEqual(self.f.state()["importedRecords"], 1)
        other = Fixture(self.temp.name)
        other.analytics_history_step()
        self.assertEqual(other.state()["status"], "current")
        self.assertEqual(len(other.captured()), 1)
        self.assertEqual(other.captured()[0][2]["a"]["model"], "past-model")
        self.assertNotIn("secret", json.dumps(other.state()))
        self.assertFalse(other.analytics_history_step())
        self.assertEqual(len(other.captured()), 1)

    def test_deleted_agent_history_is_retained(self):
        self.f.path.write_bytes(self.header + line("response_item", {"type": "message", "role": "user", "content": []}))
        with self.f.db() as db:
            a = self.f.agent("agent", db)
            a["deletedAt"] = 1
            db.execute("UPDATE runtime_agents SET record=? WHERE id='agent'", (json.dumps(a),))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "current")
        self.assertEqual(self.f.state()["deletedAt"], 1)
        self.assertEqual(len(self.f.captured()), 1)

    def test_partial_final_line_is_retried_only_when_complete(self):
        output = line("response_item", {"type": "function_call_output", "call_id": "c", "output": "answer"})
        self.f.path.write_bytes(self.header + output[:-5])
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "partialLine")
        self.assertEqual(self.f.state()["offset"], len(self.header))
        with self.f.path.open("ab") as handle:
            handle.write(output[-5:])
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "current")
        self.assertEqual(len(self.f.captured()), 1)

    def test_malformed_complete_line_marks_partial_coverage_and_advances(self):
        self.f.path.write_bytes(self.header + b"not-json\n" + line("response_item", {"type": "message", "role": "user", "content": []}))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["malformedLines"], 1)
        self.assertEqual(self.f.state()["coverage"], "partial")
        self.assertEqual(self.f.state()["offset"], self.f.path.stat().st_size)
        self.assertEqual(len(self.f.captured()), 1)

    def test_file_replacement_retains_checkpoint(self):
        self.f.path.write_bytes(self.header)
        self.f.analytics_history_step()
        previous = self.f.state()["offset"]
        replacement = self.f.path.with_suffix(".new")
        replacement.write_bytes(self.header + b"{}\n")
        os.replace(replacement, self.f.path)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "identityChanged")
        self.assertEqual(self.f.state()["offset"], previous)

    def test_in_place_rewrite_is_detected_by_checkpoint_anchor(self):
        self.f.path.write_bytes(self.header)
        self.f.analytics_history_step()
        self.f.path.write_bytes(self.header.replace(b"session_meta", b"session_metb"))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "identityChanged")

    def test_missing_managed_profile_does_not_search_other_account(self):
        alternate = self.f.homes["second"] / "sessions" / ("rollout-" + THREAD + ".jsonl")
        alternate.write_bytes(self.header)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "missing")
        self.assertEqual(self.f.captured(), [])

    def test_wrong_header_is_rejected(self):
        self.f.path.write_bytes(line("session_meta", {"id": OTHER_THREAD}))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "wrongThread")
        self.assertEqual(self.f.state()["offset"], 0)

    def test_symlink_outside_profile_is_rejected(self):
        outside = Path(self.temp.name) / "outside.jsonl"
        outside.write_bytes(self.header)
        self.f.path.symlink_to(outside)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "outsideProfile")

    def test_ambiguous_thread_files_are_rejected(self):
        self.f.path.write_bytes(self.header)
        second = self.f.path.parent / ("duplicate-" + THREAD + ".jsonl")
        second.write_bytes(self.header)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "ambiguous")

    def test_collector_failure_rolls_back_checkpoint_and_collected_events(self):
        self.f.path.write_bytes(self.header + line("response_item", {"type": "message", "role": "user", "content": []}))
        def fail(*args, **kwargs):
            raise ValueError("collector failure")
        original = self.f.analytics_model_payload
        self.f.analytics_model_payload = fail
        with self.assertRaises(ValueError):
            self.f.analytics_history_step()
        self.assertIsNone(self.f.state())
        self.f.analytics_model_payload = original
        self.f.analytics_history_step()
        self.assertEqual(len(self.f.captured()), 1)


if __name__ == "__main__":
    unittest.main()
