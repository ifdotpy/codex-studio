#!/usr/bin/env python3
"""Exercise the canvas with local fixtures. No model or real worker is called."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
import uuid

sys.dont_write_bytecode = True
SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
from codex_canvas import Canvas, make_server, READ_LIMIT


class CanvasContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-canvas-test-")
        self.root = Path(self.temp.name) / "state"
        self.profile = Path(self.temp.name) / "profile"
        self.env = patch.dict(os.environ, {"CODEX_AGENTS_STATE_DIR": str(self.root), "CODEX_HOME": str(self.profile), "CODEX_BOARD_STATE_DIR": ""})
        self.env.start()
        self.canvas = Canvas()
        for wave in ("one", "two"):
            self.write(wave, [{"name": "worker", "threadId": "thread-" + wave, "runId": "run-" + wave,
                               "turnStatus": "running", "goalStatus": "active", "launcherPid": os.getpid(),
                               "boardOwner": wave + ":run-" + wave + ":worker"}])

    def tearDown(self):
        self.env.stop()
        self.temp.cleanup()

    def write(self, wave, rows):
        (self.root / f"codex-swarm-status.{wave}.json").write_text(json.dumps(rows))

    def group(self):
        key = str(uuid.uuid4())
        ids = [t["id"] for t in self.canvas.threads()]
        self.canvas.create_group("Runtime team", ids, key)
        return key, ids

    def test_snapshot_uses_process_liveness(self):
        self.write("dead", [{"name": "dead", "threadId": "dead", "runId": "dead", "turnStatus": "running", "launcherPid": 0}])
        dead = next(t for t in self.canvas.snapshot()["threads"] if t["name"] == "dead")
        self.assertEqual(dead["status"], "abandoned")
        self.assertFalse(dead["canSend"])
        self.assertEqual(len(self.canvas.snapshot()["groups"]), 3)

    def test_group_and_messages_survive_restart(self):
        room, ids = self.group()
        key = str(uuid.uuid4())
        message = self.canvas.post(room, "peer report", key, author=ids[0], notify=False)
        self.assertEqual(message["deliveries"], {})
        restarted = Canvas()
        self.assertEqual(restarted.messages(room)[0]["text"], "peer report")
        self.assertTrue(any(g["id"] == room for g in restarted.groups(restarted.threads())))
        self.assertEqual(restarted.post(room, "peer report", key, author=ids[0], notify=False)["id"], key)
        with self.assertRaisesRegex(ValueError, "different content"):
            restarted.post(room, "changed", key, author=ids[0], notify=False)

    def test_group_broadcast_and_idempotent_retry(self):
        room, ids = self.group()
        key = str(uuid.uuid4())
        result = self.canvas.post(room, "inspect only", key)
        self.assertEqual(set(result["deliveries"].values()), {"queued"})
        for wave in ("one", "two"):
            inbox = self.root / f"codex-inbox.{wave}.run-{wave}" / "worker.json"
            text = json.loads(inbox.read_text())["text"]
            self.assertIn("inspect only", text)
            self.assertIn("codex-chat", text)
            inbox.unlink()  # Simulate the launcher's acknowledgement.
        self.canvas.post(room, "inspect only", key)
        self.assertFalse(list(self.root.glob("codex-inbox.*/*.json")))
        self.assertEqual(len(self.canvas.messages(room)), 1)

    def test_direct_message_and_pending_mailbox_failure(self):
        target = self.canvas.threads()[0]
        self.canvas.post(target["id"], "--wave two literal", str(uuid.uuid4()))
        inbox = self.root / "codex-inbox.one.run-one" / "worker.json"
        self.assertEqual(json.loads(inbox.read_text())["text"], "--wave two literal\n")
        result = self.canvas.post(target["id"], "second message", str(uuid.uuid4()))
        self.assertIn("pending message", result["deliveries"][target["id"]])
        self.assertEqual(json.loads(inbox.read_text())["text"], "--wave two literal\n")

    def test_mailbox_timeout_is_unknown_and_retry_cannot_resend(self):
        target = self.canvas.threads()[0]
        key = str(uuid.uuid4())
        with patch("codex_canvas.subprocess.run", side_effect=subprocess.TimeoutExpired("codex-steer", 10)) as command:
            result = self.canvas.post(target["id"], "fixture", key)
            self.assertTrue(result["deliveries"][target["id"]].startswith("unknown:"))
            self.canvas.post(target["id"], "fixture", key)
            self.assertEqual(command.call_count, 1)

    def test_new_run_cannot_receive_old_group_message(self):
        room, ids = self.group()
        self.write("one", [{"name": "worker", "threadId": "replacement", "runId": "new-run", "turnStatus": "running", "launcherPid": os.getpid()}])
        result = self.canvas.post(room, "old run only", str(uuid.uuid4()))
        self.assertTrue(result["deliveries"][ids[0]].startswith("failed:"))
        self.assertFalse((self.root / "codex-inbox.one.new-run").exists())

    def test_steer_rejects_identity_change_after_canvas_read(self):
        result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", "one", "--expected-run", "stale", "--expected-thread", "thread-one", "worker", "text"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("identity changed", result.stderr)
        self.assertFalse(list(self.root.glob("codex-inbox.*")))

    def test_cli_agent_reply_and_membership(self):
        room, ids = self.group()
        result = subprocess.run([str(SCRIPTS / "codex-chat"), "post", room, "ready", "--owner", "one:run-one:worker"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.canvas.messages(room)[0]["author"], ids[0])
        with self.assertRaisesRegex(ValueError, "not a member"):
            self.canvas.post(room, "intruder", str(uuid.uuid4()), author="foreign", notify=False)

    def test_transcript_appends_and_omits_internal_reasoning(self):
        folder = self.profile / "sessions/2026/09/05"
        folder.mkdir(parents=True)
        path = folder / "rollout-thread-one.jsonl"
        def item(kind, **kwargs):
            return json.dumps({"type": "response_item", "timestamp": "2026-09-05T10:00:00Z", "payload": {"type": kind, **kwargs}}) + "\n"
        path.write_text(item("reasoning", encrypted_content="SECRET") + item("message", role="assistant", content=[{"type": "output_text", "text": "First message"}]))
        key = self.canvas.threads()[0]["id"]
        first = self.canvas.transcript(key)
        self.assertEqual(len(first["items"]), 1)
        self.assertNotIn("SECRET", json.dumps(first))
        with path.open("a") as handle:
            handle.write(item("function_call", name="exec_command", arguments='{"cmd":"pwd"}'))
            handle.write(item("function_call_output", output="/fixture"))
        updated = self.canvas.transcript(key)
        self.assertEqual(len(updated["items"]), 3)
        self.assertEqual(updated["items"][-1]["role"], "output")
        with path.open("w") as handle:
            handle.write("x" * (READ_LIMIT + 100) + "\n")
            handle.write(item("message", role="assistant", content=[{"type":"output_text","text":"Tail"}]))
        self.assertTrue(self.canvas.transcript(key)["truncated"])
        self.assertEqual(self.canvas.transcript(key)["items"][0]["text"], "Tail")

    def test_bad_status_fails_visibly_and_board_override(self):
        alternate = Path(self.temp.name) / "other-board"
        alternate.mkdir()
        (alternate / "codex-board.json").write_text('{"claims":{"native":{"worker":"unknown","at":1}}}')
        with patch.dict(os.environ, {"CODEX_BOARD_STATE_DIR": str(alternate)}):
            self.assertIn("native", self.canvas.snapshot()["board"]["claims"])
        (self.root / "codex-swarm-status.one.json").write_text("broken")
        with self.assertRaises(RuntimeError):
            self.canvas.snapshot()

    def test_http_local_origin_token_and_routes(self):
        server = make_server(self.canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        def request(path, body=None, headers=None):
            data = None if body is None else json.dumps(body).encode()
            req = urllib.request.Request(base + path, data=data, headers={"Content-Type": "application/json", **(headers or {})})
            try:
                with urllib.request.urlopen(req, timeout=5) as response:
                    return response.status, response.read()
            except urllib.error.HTTPError as error:
                return error.code, error.read()
        try:
            status, data = request("/api/state")
            self.assertEqual(status, 200)
            token = json.loads(data)["token"]
            self.assertEqual(request("/", headers={"Host": "evil.example"})[0], 403)
            self.assertEqual(request("/api/state", headers={"Origin": "https://evil.example"})[0], 403)
            self.assertEqual(request("/api/groups", {})[0], 403)
            body = {"id": str(uuid.uuid4()), "name": "team", "members": [t["id"] for t in self.canvas.threads()]}
            headers = {"Origin": base, "X-Canvas-Token": token}
            self.assertEqual(request("/api/groups", body, headers)[0], 200)
            self.assertEqual(request("/api/groups", body, headers)[0], 200)
            self.assertEqual(request("/../../scripts/codex_canvas.py")[0], 404)
            self.assertEqual(request("/app.js")[0], 200)
            self.assertEqual(request("/api/groups", [], headers)[0], 400)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main(verbosity=2)
