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

REPLAY_SERVER = SERVER_TESTS_ROOT / "provider-replay-server.py"
sys.path.insert(0, str(SERVER_TESTS_ROOT))
PROVIDER_RUNNER_PATH = SERVER_TESTS_ROOT / "provider-replay-runner.py"
PROVIDER_RUNNER_SPEC = importlib.util.spec_from_file_location("provider_replay_runner", PROVIDER_RUNNER_PATH)
PROVIDER_RUNNER = importlib.util.module_from_spec(PROVIDER_RUNNER_SPEC)
PROVIDER_RUNNER_SPEC.loader.exec_module(PROVIDER_RUNNER)


class ReplayProtocol(unittest.TestCase):
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
