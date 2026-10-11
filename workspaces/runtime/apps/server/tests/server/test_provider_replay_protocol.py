"""The provider replay must reject unknown RPCs and answer only named no-ops."""

import json
import importlib.util
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from codex_layout import SERVER_TESTS_ROOT
sys.path.insert(0, str(SERVER_TESTS_ROOT))
from provider_replay_dispatch import dispatch_rpc_lines

REPLAY_SERVER = SERVER_TESTS_ROOT / "provider-replay-server.py"
PROVIDER_RUNNER_PATH = SERVER_TESTS_ROOT / "provider-replay-runner.py"
PROVIDER_RUNNER_SPEC = importlib.util.spec_from_file_location("provider_replay_runner", PROVIDER_RUNNER_PATH)
PROVIDER_RUNNER = importlib.util.module_from_spec(PROVIDER_RUNNER_SPEC)
PROVIDER_RUNNER_SPEC.loader.exec_module(PROVIDER_RUNNER)


class ReplayProtocol(unittest.TestCase):
    def test_dispatch_keeps_ordinary_rpc_order_and_overlaps_turn_starts(self):
        first_started = threading.Event()
        second_started = threading.Event()
        release_first = threading.Event()
        release_turns = threading.Event()
        errors = queue.Queue()
        ordinary_seen = []
        turn_seen = []

        def ordinary_handler(line):
            request_id = json.loads(line)["id"]
            ordinary_seen.append(request_id)
            if request_id == 1:
                first_started.set()
                if not release_first.wait(2):
                    raise AssertionError("test did not release first ordinary RPC")
            elif request_id == 2:
                second_started.set()

        def turn_handler(line):
            request_id = json.loads(line)["id"]
            turn_seen.append(request_id)
            (first_started if request_id == 3 else second_started).set()
            if not release_turns.wait(2):
                raise AssertionError("test did not release concurrent turns")

        def run(lines, handler, concurrent_methods):
            try:
                dispatch_rpc_lines(lines, handler, concurrent_methods)
            except BaseException as error:
                errors.put(error)

        ordinary = [json.dumps({"id": value, "method": "thread/read"}) for value in (1, 2)]
        worker = threading.Thread(target=run, args=(ordinary, ordinary_handler, set()))
        worker.start()
        self.assertTrue(first_started.wait(1), "first ordinary RPC did not start")
        self.assertFalse(second_started.wait(1), "second ordinary RPC overtook the first")
        release_first.set()
        worker.join(2)
        self.assertFalse(worker.is_alive(), "ordinary RPC dispatcher did not finish")
        self.assertEqual(ordinary_seen, [1, 2])
        if not errors.empty():
            raise errors.get()

        first_started.clear()
        second_started.clear()
        turns = [json.dumps({"id": value, "method": "turn/start"}) for value in (3, 4)]
        worker = threading.Thread(target=run, args=(turns, turn_handler, {"turn/start"}))
        worker.start()
        self.assertTrue(first_started.wait(1), "first turn did not start")
        self.assertTrue(second_started.wait(1), "second turn did not overlap the first")
        release_turns.set()
        worker.join(2)
        self.assertFalse(worker.is_alive(), "turn dispatcher did not finish")
        self.assertCountEqual(turn_seen, [3, 4])
        if not errors.empty():
            raise errors.get()

    def test_ordinary_rpc_error_is_propagated_only_after_stdin_exhaustion(self):
        requesting_next_line = threading.Event()
        release_eof = threading.Event()
        finished = threading.Event()
        errors = queue.Queue()

        def lines():
            yield json.dumps({"id": 1, "method": "unknown/method"})
            requesting_next_line.set()
            if not release_eof.wait(2):
                raise AssertionError("test did not release the simulated stdin EOF")

        def handler(_line):
            raise RuntimeError("unknown RPC")

        def dispatch():
            try:
                dispatch_rpc_lines(lines(), handler, set())
            except Exception as error:
                errors.put(error)
            finally:
                finished.set()

        worker = threading.Thread(target=dispatch)
        worker.start()
        self.assertTrue(requesting_next_line.wait(1), "dispatcher did not continue reading stdin")
        self.assertFalse(finished.is_set(), "handler error ended the process before stdin EOF")
        self.assertTrue(errors.empty(), "handler error escaped before stdin EOF")
        release_eof.set()
        worker.join(2)
        self.assertFalse(worker.is_alive(), "dispatcher did not finish after stdin EOF")
        self.assertIsInstance(errors.get_nowait(), RuntimeError)

    def test_supervisor_capture_prefix_waits_for_recorded_orphan_response(self):
        contract = PROVIDER_RUNNER.ProviderReplay()
        orphan = {"id": "supervisor-orphan-3",
                  "result": {"turn": {"id": "replay-turn-1", "status": "inProgress"}}}
        fixture = {
            "expectedMissingReplies": [{"method": "turn/start", "id": 3}],
            "allowedOrphanResponses": [orphan],
        }
        recorded = {"turn/start": {"direction": "in", "responseFor": "turn/start",
                                    "message": {"result": {"turn": {"id": "$turnId"}}}}}
        prefix = [{"direction": "out", "message": {
            "id": 3, "method": "turn/start", "params": {}}}]
        self.assertFalse(contract.capture_is_settled(prefix, recorded, fixture))
        complete = prefix + [{"direction": "in", "message": orphan}]
        self.assertTrue(contract.capture_is_settled(complete, recorded, fixture))

    def test_claude_startup_diagnostic_includes_child_stderr(self):
        with tempfile.TemporaryDirectory(prefix="provider-replay-stderr-") as directory:
            log = Path(directory) / "state" / "account-servers" / "claude" / "app-server.log"
            log.parent.mkdir(parents=True)
            log.write_text("Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'fixture-sdk'\n",
                           encoding="utf-8")
            self.assertIn("ERR_MODULE_NOT_FOUND", PROVIDER_RUNNER.app_server_stderr(Path(directory)))

    def invoke(self, method, recorded_replies=None, request_id=17):
        with tempfile.TemporaryDirectory(prefix="provider-replay-protocol-") as directory:
            root = Path(directory)
            fixture = root / "fixture.json"
            replies = root / "replies.json"
            fixture.write_text("{}", encoding="utf-8")
            replies.write_text(json.dumps(recorded_replies or {}), encoding="utf-8")
            message = {"method": method}
            if request_id is not None:
                message["id"] = request_id
            return subprocess.run(
                [sys.executable, "-B", str(REPLAY_SERVER), str(fixture), str(replies)],
                input=json.dumps(message) + "\n",
                text=True, capture_output=True, timeout=5, check=False)

    def test_unknown_rpc_fails_visibly(self):
        result = self.invoke("future/unrecognizedMethod")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unexpected provider RPC method in replay", result.stderr)
        self.assertIn("future/unrecognizedMethod", result.stderr)
        error = json.loads(result.stdout)
        self.assertEqual(error, {"id": 17, "error": {
            "code": -32601, "message": "Unexpected provider RPC method in replay: 'future/unrecognizedMethod'"}})

    def test_unknown_notification_fails_visibly(self):
        result = self.invoke("future/unrecognizedNotification", request_id=None)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("Unexpected provider RPC notification in replay", result.stderr)
        self.assertIn("future/unrecognizedNotification", result.stderr)

    def test_unknown_rpc_error_is_sent_while_stdin_remains_open(self):
        with tempfile.TemporaryDirectory(prefix="provider-replay-open-stdin-") as directory:
            root = Path(directory)
            fixture, replies = root / "fixture.json", root / "replies.json"
            fixture.write_text("{}", encoding="utf-8")
            replies.write_text("{}", encoding="utf-8")
            process = subprocess.Popen(
                [sys.executable, "-B", str(REPLAY_SERVER), str(fixture), str(replies)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, bufsize=1)
            try:
                output = queue.Queue()
                reader = threading.Thread(target=lambda: output.put(process.stdout.readline()), daemon=True)
                reader.start()
                process.stdin.write('{"id":19,"method":"future/while-open"}\n')
                process.stdin.flush()
                try:
                    response_line = output.get(timeout=2)
                except queue.Empty:
                    self.fail("replay withheld unknown-method response")
                response = json.loads(response_line)
                self.assertEqual(response["id"], 19)
                self.assertEqual(response["error"]["code"], -32601)
                self.assertIn("future/while-open", response["error"]["message"])
                self.assertIsNone(process.poll(), "server should remain readable until input closes")
            finally:
                process.stdin.close()
                process.wait(timeout=5)
                process.stdout.close()
                process.stderr.close()
            self.assertNotEqual(process.returncode, 0)

    def test_explicit_empty_result_method_has_exact_reply(self):
        result = self.invoke("thread/name/set")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, '{"id":17,"result":{}}\n')
        self.assertEqual(result.stderr, "")

    def test_recorded_thread_read_has_exact_reply(self):
        replies = {"thread/read": {
            "direction": "in", "responseFor": "thread/read",
            "message": {"result": {"thread": {"id": "$threadId"}}}}}
        result = self.invoke("thread/read", replies)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"id": 17,
            "result": {"thread": {"id": "replay-thread-1"}}})
        self.assertEqual(result.stderr, "")

    def test_recorded_thread_turns_list_has_exact_reply(self):
        replies = {"thread/turns/list": {
            "direction": "in", "responseFor": "thread/turns/list",
            "message": {"result": {"data": []}}}}
        result = self.invoke("thread/turns/list", replies)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"id": 17, "result": {"data": []}})
        self.assertEqual(result.stderr, "")

    def test_orphan_response_allowance_is_exact_and_fixture_scoped(self):
        contract = PROVIDER_RUNNER.ProviderReplay()
        frames = [{"direction": "in", "message": {
            "id": "supervisor-orphan-3",
            "result": {"turn": {"id": "replay-turn-1", "status": "inProgress"}}}}]
        allowed = [{"id": "supervisor-orphan-3",
                    "result": {"turn": {"id": "replay-turn-1", "status": "inProgress"}}}]
        contract.assert_replay_rpc_contract(frames, {}, {"allowedOrphanResponses": allowed})
        with self.assertRaises(AssertionError):
            contract.assert_replay_rpc_contract(frames, {}, {})
        altered = [{**allowed[0], "result": {"turn": {"id": "other", "status": "inProgress"}}}]
        with self.assertRaises(AssertionError):
            contract.assert_replay_rpc_contract(frames, {}, {"allowedOrphanResponses": altered})

    def test_rpc_error_cannot_satisfy_explicit_empty_result(self):
        contract = PROVIDER_RUNNER.ProviderReplay()
        frames = [
            {"direction": "out", "message": {"id": 4, "method": "thread/name/set"}},
            {"direction": "in", "message": {"id": 4, "error": {
                "code": -32601, "message": "method unsupported"}}},
        ]
        with self.assertRaisesRegex(AssertionError, "JSON-RPC error"):
            contract.assert_replay_rpc_contract(frames, {}, {})

    def test_missing_reply_allowance_matches_method_and_request_id(self):
        contract = PROVIDER_RUNNER.ProviderReplay()
        frames = [{"direction": "out", "message": {
            "id": 3, "method": "turn/start", "params": {}}}]
        replies = {"turn/start": {"direction": "in", "responseFor": "turn/start",
                                  "message": {"result": {"turn": {"id": "$turnId"}}}}}
        exact = {"expectedMissingReplies": [{"method": "turn/start", "id": 3}]}
        contract.assert_replay_rpc_contract(frames, replies, exact)
        altered = {"expectedMissingReplies": [{"method": "turn/start", "id": 4}]}
        with self.assertRaises(AssertionError):
            contract.assert_replay_rpc_contract(frames, replies, altered)

    def test_every_outbound_method_must_be_recorded_or_explicitly_allowed(self):
        contract = PROVIDER_RUNNER.ProviderReplay()
        with self.assertRaisesRegex(AssertionError, "not explicitly replayed or allowlisted"):
            contract.assert_replay_rpc_contract(
                [{"direction": "out", "message": {"method": "future/notification"}}], {}, {})
        contract.assert_replay_rpc_contract(
            [{"direction": "out", "message": {"method": "initialized"}}], {}, {})


if __name__ == "__main__":
    unittest.main(verbosity=2)
