"""Parser contracts with no runtime or database fixture."""
import copy
from pathlib import Path
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from analytics.rollout_parser import normalize_item, rollout_actions, timestamp

THREAD = "01a07781-5d19-7390-bc74-c12094143962"
OTHER_THREAD = "01a07781-5d19-7390-bc74-c12094143963"


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

    def test_input_is_unchanged_and_fresh_context_replays_identically(self):
        records = [
            {"type": "turn_context", "payload": {"turn_id": "next", "model": "model"}},
            {"type": "event_msg", "payload": {"type": "item_completed", "item": {
                "type": "AgentMessage", "content": [{"text": "answer"}]}}},
        ]
        before = copy.deepcopy(records)
        contexts = [self.context(), self.context()]
        outputs = [[rollout_actions(record, context, str(i), 10)
                    for i, record in enumerate(records)] for context in contexts]
        self.assertEqual(outputs[0], outputs[1])
        self.assertEqual(contexts[0], contexts[1])
        self.assertEqual(contexts[0]["turnId"], "next")
        self.assertEqual(records, before)
        self.assertEqual(outputs[0][1][0][2]["item"]["text"], "answer")

    def test_invalid_record_shapes_are_ignored(self):
        for record in (None, [], {}, {"payload": []}, {"type": "unknown", "payload": {}}):
            with self.subTest(record=record):
                context = self.context()
                self.assertEqual(rollout_actions(record, context, "id", 10), [])
                self.assertEqual(context, self.context())

    def test_invalid_timestamps_use_fallback(self):
        for value in (True, -1, float("nan"), float("inf"), "bad", "2026-09-01T12:00:00", None):
            with self.subTest(value=value):
                self.assertEqual(timestamp(value, 7), 7)
        self.assertEqual(timestamp("1970-01-01T00:00:01Z", 7), 1)

    def test_normalization_preserves_unknown_fields_and_input(self):
        item = {"type": "CommandExecution", "exit_code": 1, "future_field": "kept"}
        before = dict(item)
        self.assertEqual(normalize_item(item), {
            "type": "commandExecution", "exitCode": 1, "future_field": "kept"})
        self.assertEqual(item, before)

    def test_standalone_import_and_benchmark_need_only_standard_library(self):
        scripts = Path(__file__).resolve().parents[2]
        probe = subprocess.run([
            sys.executable, "-B", "-I", "-S", "-c",
            "import sys; sys.path.insert(0, sys.argv[1]); "
            "import analytics.rollout_parser; "
            "assert not {'codex_runtime', 'codex_budget', 'sqlite3'} & sys.modules.keys()",
            str(scripts),
        ], capture_output=True, text=True, timeout=10)
        self.assertEqual(probe.returncode, 0, probe.stderr)
        check = subprocess.run([
            sys.executable, "-B", "-I", "-S",
            str(scripts / "analytics/benchmarks/bench_rollout.py"), "--check",
        ], capture_output=True, text=True, timeout=10)
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertIn("repeatable replay", check.stdout)


if __name__ == "__main__":
    unittest.main()
