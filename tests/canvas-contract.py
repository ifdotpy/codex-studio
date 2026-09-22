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
                               "agentOwner": wave + ":run-" + wave + ":worker"}])

        one = json.loads((self.root / "codex-swarm-status.one.json").read_text())
        one.append({**one[0], "name": "peer", "threadId": "thread-peer", "agentOwner": "one:run-one:peer"})
        self.write("one", one)

    def tearDown(self):
        self.env.stop()
        self.temp.cleanup()

    def write(self, wave, rows):
        (self.root / f"codex-swarm-status.{wave}.json").write_text(json.dumps(rows))

    def group(self):
        key = str(uuid.uuid4())
        ids = [t["id"] for t in self.canvas.threads() if t["wave"] == "one"]
        self.canvas.create_chat("Runtime team", ids, key)
        return key, ids

    def test_snapshot_uses_process_liveness(self):
        self.write("dead", [{"name": "dead", "threadId": "dead", "runId": "dead", "turnStatus": "running", "launcherPid": 0}])
        dead = next(t for t in self.canvas.snapshot()["threads"] if t["name"] == "dead")
        self.assertEqual(dead["status"], "abandoned")
        self.assertFalse(dead["canSend"])
        self.assertEqual(len(self.canvas.snapshot()["chats"]), 0)

    def test_chat_is_a_node_and_connections_define_membership(self):
        chat = str(uuid.uuid4())
        self.canvas.create_chat('Independent chat', [], chat)
        first, second = [t['id'] for t in self.canvas.threads() if t['wave'] == 'one']
        self.canvas.connect_chat(first, chat)
        self.canvas.connect_chat(first, chat)
        other = str(uuid.uuid4())
        self.canvas.create_chat('Other chat', [first, second], other)
        snapshot = self.canvas.snapshot()
        self.assertEqual(len([n for n in snapshot['nodes'] if n['kind'] == 'chat']), 2)
        self.assertEqual(len([e for e in snapshot['edges'] if e['source'] == first]), 2)
        self.canvas.post(chat, 'history', str(uuid.uuid4()), author=first, notify=False)
        self.canvas.connect_chat(first, chat, False)
        self.assertEqual(next(c for c in self.canvas.chats() if c['id'] == chat)['members'], [])
        self.assertEqual(self.canvas.messages(chat)[0]['text'], 'history')
        with self.assertRaisesRegex(ValueError, 'not a member'):
            self.canvas.post(chat, 'not connected', str(uuid.uuid4()), author=first, notify=False)
        with self.assertRaisesRegex(ValueError, 'target must be a chat'):
            self.canvas.connect_chat(first, second)
        restarted = Canvas()
        self.assertEqual(next(c for c in restarted.chats() if c['id'] == chat)['members'], [])

    def test_registered_native_parent_and_app_server_parent_edges(self):
        self.canvas.register_agent('native-root', 'Lead', thread_id='root-thread', status='running')
        self.canvas.register_agent('native-child', 'Research', parent='native-root', status='completed')
        self.canvas.register_agent('native-child', 'Research', parent='native-root', status='completed')
        with self.assertRaisesRegex(ValueError, 'different parent'):
            self.canvas.register_agent('native-child', 'Research', parent=None)
        with self.assertRaisesRegex(ValueError, 'Register the parent'):
            self.canvas.register_agent('bad-child', 'Bad', parent='missing')
        rows = self.canvas.threads()
        self.write('three', [{'name':'third', 'threadId':'third-thread', 'runId':'third-run',
                             'turnStatus':'running','launcherPid':os.getpid(), 'orchestratorId':'native-root'}])
        graph = self.canvas.snapshot()
        children = [e for e in graph['edges'] if e['source'] == 'native-root' and e['kind'] == 'spawn']
        self.assertEqual(len(children), 2)
        self.assertEqual(len([t for t in graph['threads'] if t['id'] == 'native-root']), 1)
        native = next(t for t in graph['threads'] if t['id'] == 'native-child')
        self.assertFalse(native['canSend'])
        self.assertEqual(native['status'], 'completed')
        self.assertIn('reportedAt', native)
        self.canvas.register_agent('/root', 'Host root')
        self.canvas.register_agent('/root/reviewer', 'Host reviewer', parent='/root')
        self.assertTrue(any(e['source']=='/root' and e['target']=='/root/reviewer' for e in self.canvas.edges()))

    def test_explicit_orchestrator_reference_has_unknown_status(self):
        self.write('creator', [{'name':'child','threadId':'child-thread','runId':'child-run',
                               'turnStatus':'running','launcherPid':os.getpid(),
                               'orchestratorId':'actual-parent','orchestratorName':'Build lead'}])
        parent = next(t for t in self.canvas.threads() if t['id'] == 'actual-parent')
        self.assertEqual(parent['role'], 'orchestrator')
        self.assertEqual(parent['status'], 'unknown')
        self.assertFalse(parent['canSend'])

    def test_chat_restart_preserves_history_without_readding_removed_edges(self):
        chat = str(uuid.uuid4())
        member = self.canvas.threads()[0]['id']
        with self.canvas.connect() as db:
            db.execute('INSERT INTO groups VALUES (?,?,?)', (chat,'Old team',json.dumps([member])))
            db.execute('INSERT INTO messages VALUES (?,?,?,?,?,?)', (str(uuid.uuid4()),chat,'user','Keep this',1,'{}'))
        self.canvas.connect_chat(member, chat, True)
        restarted = Canvas()
        self.assertEqual(restarted.chats()[0]['members'], [member])
        restarted.connect_chat(member, chat, False)
        again = Canvas()
        self.assertEqual(again.chats()[0]['members'], [])
        self.assertEqual(again.messages(chat)[0]['text'], 'Keep this')

    def test_cli_creates_chat_and_connects_native_agent(self):
        result = subprocess.run([str(SCRIPTS/'codex-graph'), 'agent', '--id','host-root','--name','Lead'], capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr)
        created = subprocess.run([str(SCRIPTS/'codex-chat'),'create','Planning','--agent','host-root'],capture_output=True,text=True)
        self.assertEqual(created.returncode,0,created.stderr)
        chat = json.loads(created.stdout)['id']
        connected = subprocess.run([str(SCRIPTS/'codex-chat'),'connect',chat,'--agent','host-root'],capture_output=True,text=True)
        self.assertEqual(connected.returncode,0,connected.stderr)
        posted = subprocess.run([str(SCRIPTS/'codex-chat'),'post',chat,'Native report','--agent','host-root'],capture_output=True,text=True)
        self.assertEqual(posted.returncode,0,posted.stderr)
        self.assertEqual(self.canvas.messages(chat)[0]['author'],'host-root')

    def test_chat_reads_do_not_write_state(self):
        chat, ids = self.group()
        before = {p.name:p.stat().st_mtime_ns for p in self.root.iterdir()}
        for command, args in [('codex-chat',['list','--agent',ids[0]]),('codex-chat',['read',chat,'--agent',ids[0]]),('codex-graph',['list'])]:
            result = subprocess.run([str(SCRIPTS/command),*args],capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(before, {p.name:p.stat().st_mtime_ns for p in self.root.iterdir()})

    def test_group_and_messages_survive_restart(self):
        room, ids = self.group()
        key = str(uuid.uuid4())
        message = self.canvas.post(room, "peer report", key, author=ids[0], notify=False)
        self.assertEqual(message["deliveries"], {})
        self.assertEqual(message["status"], "delivered")
        restarted = Canvas()
        self.assertEqual(restarted.messages(room)[0]["text"], "peer report")
        self.assertTrue(any(g["id"] == room for g in restarted.chats()))
        retry = restarted.post(room, "peer report", key, author=ids[0], notify=False)
        self.assertEqual(retry["id"], key)
        self.assertEqual(retry["status"], "delivered")
        with self.assertRaisesRegex(ValueError, "different content"):
            restarted.post(room, "changed", key, author=ids[0], notify=False)

    def test_group_broadcast_and_idempotent_retry(self):
        room, ids = self.group()
        key = str(uuid.uuid4())
        result = self.canvas.post(room, "inspect only", key)
        self.assertEqual(set(result["deliveries"].values()), {"queued"})
        self.assertEqual(result["status"], "queued")
        for name in ("worker", "peer"):
            inbox = self.root / "codex-inbox.one.run-one" / f"{name}.json"
            text = json.loads(inbox.read_text())["text"]
            self.assertIn("inspect only", text)
            self.assertIn("codex-chat", text)
            inbox.unlink()  # Simulate the launcher's acknowledgement.
        self.assertEqual(self.canvas.post(room, "inspect only", key), result)
        self.assertFalse(list(self.root.glob("codex-inbox.*/*.json")))
        self.assertEqual(len(self.canvas.messages(room)), 1)

    def test_direct_message_and_pending_mailbox_failure(self):
        target = self.canvas.threads()[0]
        self.canvas.post(target["id"], "--wave two literal", str(uuid.uuid4()))
        inbox = self.root / "codex-inbox.one.run-one" / "worker.json"
        self.assertEqual(json.loads(inbox.read_text())["text"], "--wave two literal\n")
        result = self.canvas.post(target["id"], "second message", str(uuid.uuid4()))
        self.assertEqual(result["status"], "failed")
        self.assertIn("pending message", result["deliveries"][target["id"]])
        self.assertEqual(json.loads(inbox.read_text())["text"], "--wave two literal\n")

    def test_mailbox_timeout_is_unknown_and_retry_cannot_resend(self):
        target = self.canvas.threads()[0]
        key = str(uuid.uuid4())
        with patch("codex_canvas.subprocess.run", side_effect=subprocess.TimeoutExpired("codex-steer", 10)) as command:
            result = self.canvas.post(target["id"], "fixture", key)
            self.assertTrue(result["deliveries"][target["id"]].startswith("unknown:"))
            self.assertEqual(result["status"], "uncertain")
            self.assertEqual(self.canvas.post(target["id"], "fixture", key), result)
            self.assertEqual(command.call_count, 1)

    def test_partial_group_receipt_is_uncertain_without_repeat_delivery(self):
        room, ids = self.group()
        key = str(uuid.uuid4())
        replies = [subprocess.CompletedProcess([], 0, "", ""),
                   subprocess.CompletedProcess([], 2, "", "Mailbox rejected the request")]
        with patch("codex_canvas.subprocess.run", side_effect=replies) as command:
            result = self.canvas.post(room, "partial fixture", key)
            self.assertEqual(result["status"], "uncertain")
            self.assertEqual(set(result["deliveries"]), set(ids))
            self.assertEqual(set(result["deliveries"].values()),
                             {"queued", "failed: Mailbox rejected the request"})
            self.assertIn("Check the saved deliveries", result["error"])
            self.assertEqual(self.canvas.post(room, "partial fixture", key), result)
            self.assertEqual(command.call_count, 2)
        self.assertEqual(len(self.canvas.messages(room)), 1)

    def test_new_run_cannot_receive_old_group_message(self):
        room, ids = self.group()
        self.write("one", [{"name": "worker", "threadId": "replacement", "runId": "new-run", "turnStatus": "running", "launcherPid": os.getpid()}])
        with self.assertRaisesRegex(ValueError, "team cannot be verified"):
            self.canvas.post(room, "old run only", str(uuid.uuid4()))
        self.assertEqual(self.canvas.messages(room), [])
        self.assertFalse((self.root / "codex-inbox.one.new-run").exists())

    def test_steer_rejects_identity_change_after_canvas_read(self):
        result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", "one", "--expected-run", "stale", "--expected-thread", "thread-one", "worker", "text"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("identity changed", result.stderr)
        self.assertFalse(list(self.root.glob("codex-inbox.*")))

    def test_cli_agent_reply_and_membership(self):
        room, ids = self.group()
        first_path = self.root / "codex-swarm-status.one.json"
        first_bytes = first_path.read_bytes()
        second_path = self.root / "codex-swarm-status.two.json"
        current_bytes = second_path.read_bytes()
        threads = {t["id"]: t for t in self.canvas.threads()}
        self.assertEqual(threads[ids[0]]["agentOwner"], "one:run-one:worker")
        foreign = next(t for t in threads.values() if t["wave"] == "two")
        self.assertEqual(foreign["agentOwner"], "two:run-two:worker")
        self.assertTrue(all("boardOwner" not in t for t in threads.values()))
        cases = [
            (["--owner", "one:run-one:worker"], {}, ids[0]),
            ([], {"CODEX_AGENT_OWNER": "one:run-one:worker"}, ids[0]),
            ([], {"CODEX_AGENT_OWNER": "one:run-one:peer"}, ids[1]),
        ]
        for index, (args, overrides, author) in enumerate(cases):
            env = {**os.environ, "CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": "", **overrides}
            result = subprocess.run(
                [str(SCRIPTS / "codex-chat"), "post", room, f"ready {index}", *args],
                capture_output=True, text=True, env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.canvas.messages(room)[index]["author"], author)
        self.assertEqual(first_path.read_bytes(), first_bytes)
        self.assertEqual(second_path.read_bytes(), current_bytes)
        with self.assertRaisesRegex(ValueError, "not a member"):
            self.canvas.post(room, "intruder", str(uuid.uuid4()), author="foreign", notify=False)

    def test_cross_team_create_connect_and_post_fail_before_delivery(self):
        room, ids = self.group()
        foreign = next(t["id"] for t in self.canvas.threads() if t["wave"] == "two")
        with self.assertRaisesRegex(ValueError, "one team"):
            self.canvas.create_chat("Mixed", [ids[0], foreign], str(uuid.uuid4()))
        with self.assertRaisesRegex(ValueError, "one team"):
            self.canvas.connect_chat(foreign, room)
        self.assertEqual(next(g for g in self.canvas.chats() if g["id"] == room)["members"], sorted(ids))
        # A pre-upgrade room can already contain both teams.
        with self.canvas.connect() as db:
            db.execute("INSERT INTO graph_edges VALUES (?,?,?,?)", ("old-foreign-edge", foreign, room, "chat"))
        with patch("codex_canvas.subprocess.run") as send:
            for author in ("user", ids[0]):
                with self.assertRaisesRegex(ValueError, "one team"):
                    self.canvas.post(room, "No relay", str(uuid.uuid4()), author=author)
            send.assert_not_called()
        self.assertEqual(self.canvas.messages(room), [])

    def test_old_mixed_room_retry_and_read_cannot_bypass_team_guard(self):
        room, ids = self.group()
        foreign = next(t["id"] for t in self.canvas.threads() if t["wave"] == "two")
        key = str(uuid.uuid4())
        with self.canvas.connect() as db:
            db.execute("INSERT INTO messages VALUES (?,?,?,?,?,?)",
                       (key, room, "user", "Old mixed delivery", 1, json.dumps({ids[0]: "queued", foreign: "pending"})))
        with patch("codex_canvas.subprocess.run") as send:
            with self.assertRaisesRegex(ValueError, "one team"):
                self.canvas.post(room, "Old mixed delivery", key)
            send.assert_not_called()
        with self.assertRaisesRegex(ValueError, "one team"):
            self.canvas.agent_messages(room, ids[0])
        self.assertEqual(self.canvas.agent_chats(ids[0]), [])
        # User history and its original receipts stay intact.
        self.assertEqual(self.canvas.messages(room)[0]["deliveries"][foreign], "pending")
        with self.assertRaisesRegex(ValueError, "one team"):
            self.canvas.connect_chat(ids[1], room)

    def test_managed_teams_use_stored_root_and_unknown_teams_fail(self):
        agents = {
            "a": {"id": "a", "rootId": "one", "source": "managed"},
            "b": {"id": "b", "rootId": "two", "source": "managed"},
            "unknown": {"id": "unknown", "source": "managed", "cwd": "/same"},
        }
        with patch.object(self.canvas, "threads", return_value=list(agents.values())):
            with self.assertRaisesRegex(ValueError, "one team"):
                self.canvas.create_chat("Mixed", ["a", "b"], str(uuid.uuid4()))
            with self.assertRaisesRegex(ValueError, "team cannot be verified"):
                self.canvas.create_chat("Unknown", ["unknown"], str(uuid.uuid4()))

    def test_cli_preserves_user_reads_and_rejects_foreign_agent_access(self):
        room, ids = self.group()
        foreign = next(t["id"] for t in self.canvas.threads() if t["wave"] == "two")
        self.canvas.post(room, "Private team history", str(uuid.uuid4()), author=ids[0], notify=False)
        user_env = {**os.environ, "CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}
        user_read = subprocess.run([str(SCRIPTS / "codex-chat"), "read", room], env=user_env, capture_output=True, text=True)
        self.assertEqual(user_read.returncode, 0, user_read.stderr)
        self.assertIn("Private team history", user_read.stdout)
        cases = [
            (["read", room, "--agent", foreign], {}, "not a member"),
            (["connect", room, "--agent", foreign], {"CODEX_AGENT_OWNER": "one:run-one:worker"}, "one team"),
            (["disconnect", room, "--agent", ids[0]], {"CODEX_AGENT_OWNER": "two:run-two:worker"}, "one team"),
            (["read", room, "--owner", "one:run-one:worker"], {"CODEX_AGENT_OWNER": "two:run-two:worker"}, "inherited worker"),
        ]
        for args, overrides, error in cases:
            env = {**os.environ, "CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": "", **overrides}
            result = subprocess.run([str(SCRIPTS / "codex-chat"), *args], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(error, result.stderr)
            self.assertNotIn("Private team history", result.stdout)
        self.assertEqual(next(g for g in self.canvas.chats() if g["id"] == room)["members"], sorted(ids))

    def test_cli_managed_actor_uses_runtime_tools(self):
        with self.canvas.connect() as db:
            db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            record = {"id": "managed", "rootId": "managed", "name": "Lead", "agentOwner": "managed-owner"}
            db.execute("INSERT INTO runtime_agents VALUES (?,?)", ("managed", json.dumps(record)))
        for args, owner in ((["--agent", "managed"], ""), ([], "managed-owner")):
            env = {**os.environ, "CODEX_AGENT_OWNER": owner, "CODEX_BOARD_OWNER": ""}
            result = subprocess.run([str(SCRIPTS / "codex-chat"), "list", *args], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("runtime chat tools", result.stderr)

    def test_steer_inherited_owner_cannot_select_foreign_wave_or_run(self):
        self.write("one", [
            {"name": "worker", "threadId": "thread-one", "runId": "run-one", "launcherPid": os.getpid(),
             "agentOwner": "one:run-one:worker"},
            {"name": "peer", "threadId": "thread-peer", "runId": "run-one", "launcherPid": os.getpid()},
            {"name": "stale", "threadId": "thread-old", "runId": "old-run", "launcherPid": os.getpid()},
        ])
        env = {**os.environ, "CODEX_AGENT_OWNER": "one:run-one:worker", "CODEX_BOARD_OWNER": ""}
        for wave, worker in (("two", "worker"), ("one", "stale")):
            result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", wave, worker, "No relay"],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("one team", result.stderr)
            self.assertFalse(list(self.root.glob("codex-inbox.*")))
        result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", "one", "peer", "Same team"],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        inbox = self.root / "codex-inbox.one.run-one" / "peer.json"
        self.assertEqual(json.loads(inbox.read_text())["text"], "Same team\n")
        inbox.unlink()
        inbox.parent.rmdir()
        env = {**os.environ, "CODEX_AGENT_OWNER": "managed:agent-id", "CODEX_BOARD_OWNER": ""}
        result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", "one", "peer", "No relay"],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("runtime tools", result.stderr)
        self.assertFalse(list(self.root.glob("codex-inbox.*")))

    def test_graph_refuses_agent_context_before_read_or_registration(self):
        before = self.canvas.snapshot()
        for variable in ("CODEX_AGENT_OWNER", "CODEX_BOARD_OWNER"):
            env = {**os.environ, "CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": "", variable: "one:run-one:worker"}
            for args in (["list"], ["agent", "--id", "foreign-parent", "--name", "Foreign"]):
                result = subprocess.run([str(SCRIPTS / "codex-graph"), *args], env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unsupported" if variable == "CODEX_BOARD_OWNER" else "team runtime tools", result.stderr)
                self.assertEqual(result.stdout, "")
        after = self.canvas.snapshot()
        self.assertEqual(before["nodes"], after["nodes"])
        self.assertEqual(before["edges"], after["edges"])

    def test_old_owner_environment_is_rejected_before_user_or_worker_access(self):
        room, ids = self.group()
        self.canvas.post(room, "Private history", str(uuid.uuid4()), author=ids[0], notify=False)
        before = self.canvas.snapshot()
        for current_owner in ("", "one:run-one:worker"):
            env = {**os.environ, "CODEX_AGENT_OWNER": current_owner,
                   "CODEX_BOARD_OWNER": "one:run-one:worker"}
            for command in (
                ["codex-chat", "read", room],
                ["codex-chat", "create", "Forbidden"],
                ["codex-chat", "post", room, "Forbidden", "--agent", ids[0]],
                ["codex-steer", "--wave", "one", "peer", "Forbidden"],
                ["codex-graph", "list"],
                ["codex-control", "list"],
            ):
                result = subprocess.run([str(SCRIPTS / command[0]), *command[1:]],
                                        env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0, command)
                self.assertIn("CODEX_BOARD_OWNER is unsupported", result.stderr)
                self.assertEqual(result.stdout, "")
        self.assertFalse(list(self.root.glob("codex-inbox.*")))
        after = self.canvas.snapshot()
        self.assertEqual(before["nodes"], after["nodes"])
        self.assertEqual(before["edges"], after["edges"])
        self.assertEqual(len(self.canvas.messages(room)), 1)

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

    def test_bad_status_fails_visibly_and_legacy_board_is_ignored(self):
        alternate = Path(self.temp.name) / "other-board"
        alternate.mkdir()
        (alternate / "codex-board.json").write_text('{"claims":{"native":{"worker":"unknown","at":1}}}')
        with patch.dict(os.environ, {"CODEX_BOARD_STATE_DIR": str(alternate)}):
            self.assertNotIn("board", self.canvas.snapshot())
            self.assertNotIn("boardError", self.canvas.snapshot())
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
            self.assertEqual(request("/api/chats", {})[0], 403)
            body = {"id": str(uuid.uuid4()), "name": "team", "members": [t["id"] for t in self.canvas.threads() if t["wave"] == "one"]}
            headers = {"Origin": base, "X-Canvas-Token": token}
            self.assertEqual(request("/api/chats", body, headers)[0], 200)
            self.assertEqual(request("/api/chats", body, headers)[0], 200)
            self.assertEqual(request("/../../scripts/codex_canvas.py")[0], 404)
            self.assertEqual(request("/")[0], 200)
            self.assertEqual(request("/api/chats", [], headers)[0], 400)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main(verbosity=2)
