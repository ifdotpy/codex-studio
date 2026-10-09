#!/usr/bin/env python3
"""Preparation, steering, and native action receipts with delayed acknowledgements."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.request
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from studio_api.testing import read_runtime_state
from codex_runtime import PreparationPending, ResponseTimeout, Runtime
from codex_worktree_creation import WorktreeNeedsReview
from codex_canvas import Canvas, make_server
from studio_api.testing import read_test_state

spec = importlib.util.spec_from_file_location("prepare_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
eventually = fixture.eventually


class DelayedServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.hold = set()
        self.delayed = []

    def submit(self, method, params):
        if method not in self.hold:
            return super().submit(method, params)
        future = concurrent.futures.Future()
        self.calls.append((method, params))
        self.delayed.append({"method": method, "params": params, "future": future})
        return future

    def wait(self, future, timeout=60):
        if any(entry["future"] is future for entry in self.delayed):
            raise ResponseTimeout("Native response timed out; outcome unknown")
        return super().wait(future, timeout)

    def close(self):
        for entry in self.delayed:
            if not entry["future"].done():
                entry["future"].set_exception(RuntimeError("Codex disconnected; outcome unknown"))
        super().close()


class PrepareSteerContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, DelayedServer)
        self.runtime.preparation_wait_seconds = .01
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def create(self, **extra):
        return self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "First", **extra})

    def lead(self):
        a = self.create()
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        return self.runtime.agent(a["id"])

    def pending_prepare(self, a):
        # A pending acknowledgement is a wait state, not an agent error.
        eventually(lambda: "preparation acknowledgement" in str(
            (self.runtime.agent(a["id"]).get("startAttempt") or {}).get("prepareError")))
        self.assertNotIn("preparation acknowledgement", str(self.runtime.agent(a["id"]).get("error")))
        return next(e for e in self.server.delayed if e["method"] in {"thread/start", "thread/resume"})

    def accept_prepare(self, entry, **extra):
        result = {"thread": {"id": entry["params"].get("threadId", "new-prepared-thread")},
                  "model": entry["params"].get("model"), "sandbox": {"type": "readOnly"}, "approvalPolicy": "on-request"}
        result.update(extra)
        entry["future"].set_result(result)

    def count(self, method):
        return sum(m == method for m, _ in self.server.calls)

    def test_late_new_thread_continues_exact_batch_once(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.assertTrue(self.runtime.agent(a["id"])["inFlight"])
        first_id = read_runtime_state(self.runtime)["events"][0]["id"]
        self.assertEqual(self.runtime.delivery_receipt(first_id)["status"], "reserved")
        second_id = self.runtime.send(a["id"], "Second")["id"]
        self.runtime.dispatch()
        self.assertEqual(self.count("thread/start"), 1)
        self.assertEqual(self.count("turn/start"), 0)
        self.assertEqual(self.runtime.agent(a["id"])["startAttempt"]["events"], [first_id])
        self.assertEqual(self.runtime.delivery_receipt(second_id)["status"], "pending")
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        self.runtime.dispatch()
        eventually(lambda: self.runtime.delivery_receipt(second_id)["status"] == "delivered")
        starts = [p["clientUserMessageId"] for method, p in self.server.calls if method == "turn/start"]
        self.assertEqual(starts.count(first_id), 1)
        self.assertEqual(starts.count(second_id), 1)
        self.assertEqual(self.runtime.agent(a["id"])["threadId"], "new-prepared-thread")
        self.assertIsNone(self.runtime.agent(a["id"])["error"])

    def test_late_resume_uses_same_thread(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        self.runtime.send(a["id"], "Continue")
        entry = self.pending_prepare(a)
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        self.assertEqual(self.runtime.agent(a["id"])["threadId"], a["threadId"])
        self.assertEqual(self.count("thread/resume"), 1)
        self.assertEqual(self.count("turn/start"), 2)

    def test_concurrent_preparation_shares_one_native_request(self):
        self.server.hold.add("thread/start")
        a = self.runtime.create({"name": "Idle", "cwd": str(self.root), "prompt": "First"}, defer=True)
        def prepare():
            try:
                self.runtime.prepare(a)
            except PreparationPending as error:
                return error.future
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            futures = list(pool.map(lambda _: prepare(), range(2)))
        self.assertIs(futures[0], futures[1])
        self.assertEqual(self.count("thread/start"), 1)
        self.accept_prepare(self.server.delayed[0])
        self.assertEqual(self.runtime.prepare(a)["threadId"], "new-prepared-thread")

    def test_stopped_preparation_never_submits_turn_or_changes_thread(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.runtime.stop(a["id"])
        self.accept_prepare(entry)
        eventually(lambda: not self.runtime.agent(a["id"])["inFlight"])
        current = self.runtime.agent(a["id"])
        self.assertEqual(current["status"], "paused")
        self.assertIsNone(current["threadId"])
        self.assertNotIn(a["id"], self.runtime.loaded)
        self.assertEqual(self.count("turn/start"), 0)

    def test_definitive_preparation_failure_is_not_uncertain_input(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        entry["future"].set_exception(RuntimeError("invalid project"))
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        # Never sent, so the first prompt waits for the next start instead of being lost.
        self.assertEqual(read_runtime_state(self.runtime)["events"][0]["status"], "pending")
        self.assertEqual(self.count("turn/start"), 0)
        self.server.hold.discard("thread/start")
        self.runtime.send(a["id"], "Try again", "retry-after-prepare")
        eventually(lambda: self.count("turn/start") == 1)
        text = next(p for m, p in self.server.calls if m == "turn/start")["input"][0]["text"]
        self.assertIn("First", text)
        self.assertIn("Try again", text)

    def test_worktree_review_pauses_worker_and_keeps_initial_input(self):
        lead = self.lead()
        worker = self.runtime.create({"name": "Worker", "prompt": "First task", "role": "reviewer"},
                                     parent=lead["id"], defer=True)
        error = WorktreeNeedsReview("Inspect and repair the checkout, then resume this worker")
        with patch.object(self.runtime, "prepare", side_effect=error):
            self.runtime.send(worker["id"], "Start", "worktree-start")
            eventually(lambda: self.runtime.agent(worker["id"])["status"] == "paused")
        current = self.runtime.agent(worker["id"])
        self.assertFalse(current["autoWake"])
        self.assertEqual(current["error"], str(error))
        with self.runtime.db() as db:
            rows = db.execute("SELECT kind,text,status FROM runtime_events WHERE agent=?", (worker["id"],)).fetchall()
            self.assertTrue(any(row["status"] == "pending" for row in rows))
            notice = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'",
                                (lead["id"],)).fetchone()
            self.assertIn('"next_step": "send"', notice[0])
        self.runtime.send(worker["id"], "Resume", "worktree-resume")
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "running")

    def test_new_worktree_starts_worker_after_checkout(self):
        repo = (self.root / "real-repo").resolve()
        repo.mkdir()
        subprocess.run(["git", "-C", str(repo), "init"], check=True, capture_output=True)
        (repo / "tracked.txt").write_text("base")
        subprocess.run(["git", "-C", str(repo), "add", "tracked.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=Test",
                        "-c", "user.email=test@example.invalid", "commit", "-m", "Base"],
                       check=True, capture_output=True)
        lead = self.lead()
        worker = self.runtime.create({"name": "Worker", "cwd": str(repo), "prompt": "Task"},
                                     parent=lead["id"], defer=True)
        self.runtime.send(worker["id"], "Start")
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "running")
        current = self.runtime.agent(worker["id"])
        self.assertTrue(current["worktreeReady"])
        self.assertEqual((Path(current["cwd"]) / "tracked.txt").read_text(), "base")
        self.assertEqual(current["branch"], "codex-agent/" + worker["id"])

    def test_existing_partial_worktree_pauses_then_resumes_after_repair(self):
        repo = (self.root / "partial-repo").resolve()
        repo.mkdir()
        subprocess.run(["git", "-C", str(repo), "init"], check=True, capture_output=True)
        (repo / "tracked.txt").write_text("base")
        subprocess.run(["git", "-C", str(repo), "add", "tracked.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=Test",
                        "-c", "user.email=test@example.invalid", "commit", "-m", "Base"],
                       check=True, capture_output=True)
        lead = self.lead()
        worker = self.runtime.create({"name": "Worker", "cwd": str(repo), "prompt": "Task"},
                                     parent=lead["id"], defer=True)
        path = repo / ".worktrees" / "codex-agents" / worker["id"]
        subprocess.run(["git", "-C", str(repo), "worktree", "add", "--no-checkout", "-b",
                        "codex-agent/" + worker["id"], str(path), "HEAD"],
                       check=True, capture_output=True)
        (path / "user-file.txt").write_text("keep")
        self.runtime.send(worker["id"], "Start")
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "paused")
        self.assertFalse(self.runtime.agent(worker["id"])["worktreeReady"])
        self.assertEqual((path / "user-file.txt").read_text(), "keep")
        self.assertIn(str(path), self.runtime.agent(worker["id"])["error"])
        subprocess.run(["git", "-C", str(path), "reset", "--hard", "HEAD"],
                       check=True, capture_output=True)
        self.runtime.send(worker["id"], "Resume")
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "running")
        self.assertTrue(self.runtime.agent(worker["id"])["worktreeReady"])
        self.assertEqual((path / "user-file.txt").read_text(), "keep")

    def test_worktree_checkout_serializes_per_repo_without_blocking_runtime(self):
        repos = [(self.root / "repo-one").resolve(), (self.root / "repo-two").resolve()]
        for repo in repos:
            repo.mkdir()
            subprocess.run(["git", "-C", str(repo), "init"], check=True, capture_output=True)
            (repo / "tracked.txt").write_text("base")
            subprocess.run(["git", "-C", str(repo), "add", "tracked.txt"], check=True)
            subprocess.run(["git", "-C", str(repo), "-c", "user.name=Test",
                            "-c", "user.email=test@example.invalid", "commit", "-m", "Base"],
                           check=True, capture_output=True)
        lead = self.lead()
        workers = [self.runtime.create({"name": f"Worker {i}", "cwd": str(repo),
                                        "prompt": "Task"}, parent=lead["id"], defer=True)
                   for i, repo in enumerate([repos[0], repos[0], repos[1]])]
        entered_first = threading.Event()
        entered_other = threading.Event()
        release = threading.Event()
        calls = []
        calls_lock = threading.Lock()

        def checkout(repo, *_args, **_kwargs):
            with calls_lock:
                calls.append(str(repo))
            if str(repo) == str(repos[0]):
                entered_first.set()
                release.wait(timeout=10)
            else:
                entered_other.set()
            return True

        try:
            with patch("codex_worktree_creation.create_worker_worktree", side_effect=checkout):
                for worker in workers:
                    self.runtime.send(worker["id"], "Start")
                self.assertTrue(entered_first.wait(5),
                                str([(self.runtime.agent(w["id"])["status"],
                                      self.runtime.agent(w["id"]).get("error"),
                                      self.runtime.agent(w["id"]).get("worktree"),
                                      self.runtime.agent(w["id"]).get("worktreeReady"),
                                      self.runtime.agent(w["id"]).get("cwd")) for w in workers]))
                self.assertTrue(entered_other.wait(5), "another repository starts during checkout")
                eventually(lambda: any(self.runtime.agent(w["id"]).get("worktreePreparation") == "waiting"
                                       for w in workers[:2]))
                with calls_lock:
                    self.assertEqual(calls.count(str(repos[0])), 1)
                snapshot = read_runtime_state(self.runtime)
                self.assertIn("waiting", [agent.get("worktreePreparation")
                                          for agent in snapshot["agents"]])
                with self.runtime.db() as db:
                    self.assertIsNotNone(db.execute("SELECT 1").fetchone())
                canvas = Canvas(self.root)
                canvas.runtime = self.runtime
                server = make_server(canvas)
                http_thread = threading.Thread(target=server.serve_forever, daemon=True)
                http_thread.start()
                try:
                    origin = f"http://127.0.0.1:{server.server_port}"
                    def get_json(path):
                        with urllib.request.urlopen(origin + path, timeout=3) as response:
                            return json.load(response)
                    state = read_test_state(get_json)
                    self.assertIn("waiting", [agent.get("worktreePreparation")
                                              for agent in state.values("agent")])
                finally:
                    server.shutdown()
                    server.server_close()
                    http_thread.join(timeout=3)
                extra = self.runtime.create({"name": "Another repo worker", "cwd": str(repos[1]),
                                             "prompt": "Task"}, parent=lead["id"], defer=True)
                self.assertEqual(extra["cwd"], str(repos[1]))
                release.set()
                eventually(lambda: len([repo for repo in calls if repo == str(repos[0])]) == 2)
        finally:
            release.set()

    def test_seventy_queued_checkouts_do_not_occupy_delivery_threads(self):
        repo = (self.root / "queued-repo").resolve()
        repo.mkdir()
        subprocess.run(["git", "-C", str(repo), "init"], check=True, capture_output=True)
        lead = self.create(maxAgents=128)
        eventually(lambda: self.runtime.agent(lead["id"])["status"] == "running")
        workers = [self.runtime.create({"name": f"Worker {i}", "cwd": str(repo),
                                        "prompt": "Task"}, parent=lead["id"], defer=True)
                   for i in range(70)]
        entered = threading.Event()
        release = threading.Event()
        def checkout(*_args):
            entered.set()
            release.wait(timeout=60)
        try:
            with patch.object(self.runtime, "prepare_worker_worktree", side_effect=checkout):
                for worker in workers:
                    with self.assertRaises(PreparationPending):
                        self.runtime.prepare(worker)
                self.assertTrue(entered.wait(3))
                key = str(subprocess.run(
                    ["git", "-C", str(repo), "rev-parse", "--path-format=absolute", "--git-common-dir"],
                    check=True, capture_output=True, text=True).stdout.strip())
                executor = self.runtime.worktree_creation_executors[key]
                self.assertEqual(executor._work_queue.qsize(), 69)
                delivered = self.runtime.delivery_executor().submit(lambda: "delivered")
                self.assertEqual(delivered.result(timeout=2), "delivered")
                other = self.create()
                eventually(lambda: self.runtime.agent(other["id"])["status"] == "running")
        finally:
            release.set()

    def test_disconnect_finishes_shared_preparation_without_accepting_late_identity(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.runtime.disconnected()
        self.assertTrue(self.runtime.preparations[a["id"]]["future"].done())
        self.accept_prepare(entry)
        self.assertEqual(self.runtime.agent(a["id"])["status"], "interrupted")
        self.assertIsNone(self.runtime.agent(a["id"])["threadId"])
        self.assertNotIn(a["id"], self.runtime.loaded)
        self.assertEqual(self.count("turn/start"), 0)

    def test_preparation_connection_and_configuration_are_fenced(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a["id"], db)
            current["model"] = "changed-model"
            self.runtime.put(db, "agents", current)
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.preparations[a["id"]]["future"].done())
        self.assertEqual(self.runtime.agent(a["id"])["model"], "changed-model")
        self.assertIsNone(self.runtime.agent(a["id"])["threadId"])
        self.assertEqual(self.count("turn/start"), 0)

    def test_monitor_preparation_blocks_model_edit(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        m = self.runtime.monitor(a["id"], {"command": "example-command"}, approved=True)
        eventually(lambda: bool(self.server.delayed))
        with self.assertRaisesRegex(ValueError, "preparation"):
            self.runtime.conversation_settings(a["id"], {"model": "gpt-5.6-sol"})
        self.accept_prepare(self.server.delayed[0])
        eventually(lambda: self.count("command/exec") == 1)
        self.assertEqual(next(v for v in read_runtime_state(self.runtime)["monitors"] if v["id"] == m["id"])["status"], "running")

    def delayed_busy_input(self, a, message_id='busy-1'):
        self.server.hold.add('turn/start')
        result = self.runtime.send(a['id'], 'Correction', message_id, delivery='steer')
        eventually(lambda: len([e for e in self.server.delayed if e['method'] == 'turn/start']) == 1)
        entry = next(e for e in self.server.delayed if e['method'] == 'turn/start')
        eventually(lambda: self.runtime.delivery_receipt(message_id)['status'] == 'uncertain')
        return result, entry

    def test_late_busy_ack_confirms_original_receipt_once(self):
        a = self.lead()
        result, entry = self.delayed_busy_input(a)
        self.assertEqual(result['status'], 'queued')
        self.assertEqual(self.runtime.send(a['id'], 'Correction', 'busy-1')['status'], 'uncertain')
        entry['future'].set_result({'turn': {'id': a['turnId'], 'status': 'inProgress'}})
        eventually(lambda: self.runtime.delivery_receipt('busy-1')['status'] == 'delivered')
        self.assertEqual(self.count('turn/start'), 2)
        self.assertEqual(self.count('turn/steer'), 0)

    def test_client_proof_survives_late_error_after_completion(self):
        a = self.lead()
        _, entry = self.delayed_busy_input(a)
        self.server.notify({'method': 'item/completed', 'params': {'threadId': a['threadId'],
            'turnId': a['turnId'], 'item': {'type': 'userMessage', 'id': 'native-user',
                                           'clientId': 'busy-1', 'content': []}}})
        self.assertEqual(self.runtime.delivery_receipt('busy-1')['status'], 'delivered')
        self.server.complete(a['threadId'], a['turnId'])
        entry['future'].set_exception(RuntimeError('late conflicting error'))
        self.assertEqual(self.runtime.delivery_receipt('busy-1')['status'], 'delivered')

    def test_late_new_turn_id_is_bound_without_start_steer_inference(self):
        a = self.lead()
        _, entry = self.delayed_busy_input(a)
        entry['future'].set_result({'turn': {'id': 'new-native-turn', 'status': 'inProgress'}})
        eventually(lambda: self.runtime.delivery_receipt('busy-1')['status'] == 'delivered')
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], 'new-native-turn')

    def test_write_failure_keeps_exact_uncertain_receipt(self):
        a = self.lead()
        with patch.object(self.server, 'submit', side_effect=OSError('write disconnected')):
            self.runtime.send(a['id'], 'Correction', 'busy-write')
            eventually(lambda: self.runtime.delivery_receipt('busy-write')['status'] == 'uncertain')
        self.runtime.send(a['id'], 'Correction', 'busy-write')
        self.assertEqual(self.count('turn/start'), 1)

    def test_native_review_late_ack_preserves_completed_turn(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.server.hold.add("review/start")
        result = self.runtime.native_action(a["id"], "review")
        self.assertTrue(result["pending"])
        turn = {"id": "review-turn", "status": "inProgress"}
        self.server.notify({"method": "turn/started", "params": {"threadId": a["threadId"], "turn": turn}})
        self.server.notify({"method": "item/started", "params": {"threadId": a["threadId"], "turnId": turn["id"],
            "item": {"id": "review-user", "type": "userMessage", "clientId": None, "content": []}}})
        self.server.complete(a["threadId"], turn["id"])
        entry = next(e for e in self.server.delayed if e["method"] == "review/start")
        entry["future"].set_result({"turn": turn})
        self.runtime.native_action_result(a["id"], dict(self.runtime.agent(a["id"])["startAttempt"]), entry["future"])
        self.assertEqual(self.runtime.agent(a["id"])["status"], "completed")
        self.assertFalse(self.runtime.agent(a["id"])["inFlight"])

    def test_stop_during_native_action_preparation_prevents_submission(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        result = self.runtime.native_action(a["id"], "review")
        self.assertTrue(result["pending"])
        self.runtime.stop(a["id"])
        self.accept_prepare(self.server.delayed[0])
        eventually(lambda: not self.runtime.agent(a["id"])["inFlight"])
        self.assertEqual(self.count("review/start"), 0)
        self.assertEqual(self.runtime.agent(a["id"])["status"], "paused")



class BackloggedServer(fixture.FakeServer):
    """Delivers receipts only through on_result_now, as if the event queue lagged."""
    def on_result(self, future, callback):
        self.stuck = getattr(self, "stuck", []) + [(future, callback)]

    def on_result_now(self, future, callback):
        future.add_done_callback(callback)


class PreparationReceiptContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), BackloggedServer)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def test_thread_receipt_bypasses_a_lagging_event_queue(self):
        lead = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "Work"})
        eventually(lambda: self.runtime.agent(lead["id"])["status"] == "running")
        agent = self.runtime.agent(lead["id"])
        self.assertTrue(agent["threadId"])
        self.assertIn(lead["id"], self.runtime.loaded)
        self.assertNotIn("prepareError", agent.get("startAttempt") or {})
        self.assertFalse([cb for _, cb in getattr(self.runtime.server, "stuck", [])
                          if getattr(cb, "__name__", "") == "<lambda>" and "prepared_result" in repr(cb.__code__.co_names)])

    def test_app_server_receipt_runs_on_completion_not_in_the_queue(self):
        from codex_runtime import AppServer
        server = AppServer.__new__(AppServer)
        server.enqueue = lambda *args: self.fail("receipt entered the event queue")
        future, seen = concurrent.futures.Future(), []
        AppServer.on_result_now(server, (1, "thread/start", future), seen.append)
        future.set_result({"thread": {"id": "t"}})
        self.assertEqual(seen, [future])


if __name__ == "__main__":
    unittest.main()
