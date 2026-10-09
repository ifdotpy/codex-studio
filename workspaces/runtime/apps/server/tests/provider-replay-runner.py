#!/usr/bin/env python3
"""Replay provider transcripts through the real Runtime and AppServer."""
from codex_layout import CLAUDE_BRIDGE_ROOT, REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
sys.dont_write_bytecode = True
ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
sys.path.insert(0, str(SERVER_TESTS_ROOT / "server"))
from rpc_replay_contract import EMPTY_RESULT_METHODS
from codex_runtime import AppServer, Runtime

OUTBOUND_NOTIFICATION_METHODS = frozenset({"initialized"})


def eventually(check, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.02)
    raise AssertionError("Replay did not reach its expected durable state")


class ProviderReplay(unittest.TestCase):
    def claude_transport(self, root, fixture_path):
        source = CLAUDE_BRIDGE_ROOT
        bridge_root = root / "claude-bridge"
        bridge_root.mkdir()
        for module in source.glob("*.mjs"):
            shutil.copy2(module, bridge_root / module.name)
        sdk = bridge_root / "node_modules" / "@anthropic-ai" / "claude-agent-sdk"
        sdk.mkdir(parents=True)
        (sdk / "package.json").write_text('{"name":"@anthropic-ai/claude-agent-sdk","type":"module","exports":"./index.mjs"}')
        (sdk / "index.mjs").write_text(textwrap.dedent('''
            import fs from "node:fs";
            const fixture = JSON.parse(fs.readFileSync(process.env.STUDIO_REPLAY_FIXTURE, "utf8"));
            export const tool = (name, description, schema, call) => ({name, description, schema, call});
            export const createSdkMcpServer = value => value;
            export const forkSession = async () => ({sessionId:"replay-fork"});
            export const getSessionMessages = async () => [];
            export function query({prompt, options}) {
              const abort = new AbortController();
              return {
                supportedModels: async () => [{value:"default",displayName:"Default",resolvedModel:"claude-replay",supportsEffort:true,supportedEffortLevels:["low","medium","high"]}],
                accountInfo: async () => ({email:process.env.STUDIO_CLAUDE_ACCOUNT,subscriptionType:"Claude Max",apiProvider:"firstParty"}),
                initializationResult: async () => ({commands:[]}),
                setModel: async () => {}, setPermissionMode: async () => {},
                setMcpServers: async () => ({added:[],removed:[],errors:{}}),
                applyFlagSettings: async () => {}, stopTask: async () => {},
                close() { abort.abort(); }, interrupt: async () => abort.abort(),
                async *[Symbol.asyncIterator]() {
                  for await (const input of prompt) {
                    yield {type:"user",uuid:input.uuid,isReplay:false};
                    for (const event of fixture.sdkEvents || []) yield event;
                  }
                }
              };
            }
        '''))
        zod_target = bridge_root / "node_modules" / "zod"
        zod_target.symlink_to(source / "node_modules" / "zod", target_is_directory=True)
        node = shutil.which("node")
        self.assertIsNotNone(node, "Node.js is required to replay the real Claude bridge")
        env = dict(os.environ)
        env.update({"STUDIO_CLAUDE_OPTIONS": "{}", "STUDIO_CLAUDE_ACCOUNT": "replay@example.test",
                    "STUDIO_CLAUDE_BIN": "claude-replay", "STUDIO_REPLAY_FIXTURE": str(fixture_path)})
        return [node, str(bridge_root / "bridge.mjs"), str(root / "claude-state")], env

    def run_supervisor_fixture(self, root, executable, fixture_path, transcript_path, fixture):
        from codex_process_supervisor import status
        state = root / "state"
        phase, release = root / "provider-phase", root / "provider-release"
        supervisor_log = (root / "supervisor.log").open("w")
        env = {"CODEX_AGENTS_STATE_DIR": str(state),
               "CODEX_AGENTS_SUPERVISOR_MODE": "1",
               "CODEX_AGENTS_BACKEND_ID": "provider-replay-backend",
               "PROVIDER_REPLAY_PHASE": str(phase),
               "PROVIDER_REPLAY_RELEASE": str(release),
               "CODEX_AGENTS_PROVIDER_CAPTURE": "1",
               "CODEX_AGENTS_PROVIDER_CAPTURE_FILE": str(transcript_path)}
        supervisor = None
        first = second = None
        with patch.dict(os.environ, env, clear=False), \
                patch("codex_native_runtime.executable_for", return_value={"path": str(executable)}):
            try:
                supervisor = subprocess.Popen([sys.executable, "-B",
                    str(SERVER_SOURCE_ROOT / "codex_process_supervisor.py"), "--state", str(state)],
                    stdout=subprocess.DEVNULL, stderr=supervisor_log)
                eventually(lambda: (state / "supervisor.sock").exists())
                eventually(lambda: status(state) is not None)
                first = Runtime(state, AppServer)
                agent = first.create({"name": "Supervisor replay", "cwd": str(root),
                                      "prompt": "Keep this turn active across restart"})
                eventually(phase.exists)
                first.close()
                first = None
                release.touch()
                second = Runtime(state, AppServer)
                second.connect()
                eventually(lambda: second.agent(agent["id"])["status"] == "completed", timeout=15)
                def provider_capture_settled():
                    if not transcript_path.exists():
                        return False
                    raw_frames = transcript_path.read_text()
                    complete_lines = [line for line in raw_frames.splitlines(keepends=True)
                                      if line.endswith("\n")]
                    frames = [json.loads(line) for line in complete_lines]
                    missing, orphaned = self.assert_replay_rpc_contract(
                        frames, json.loads((fixture_path.parent / "codex-rpc-replies.json").read_text()),
                        fixture, allow_pending=True)
                    expected_missing = fixture.get("expectedMissingReplies", [])
                    expected_orphans = fixture.get("allowedOrphanResponses", [])
                    return (missing == expected_missing and orphaned == expected_orphans)
                eventually(provider_capture_settled, timeout=15)
                with second.db() as db:
                    saved = second.agent(agent["id"], db)
                    events = [dict(row) for row in db.execute(
                        "SELECT kind,status,text FROM runtime_events WHERE agent=? ORDER BY created,id",
                        (agent["id"],))]
                    items = [json.loads(row[0]) for row in db.execute(
                        "SELECT record FROM runtime_items WHERE agent=? ORDER BY created,id",
                        (agent["id"],))]
                self.assertEqual(saved["status"], "completed")
                self.assertIn("user", [row["kind"] for row in events])
                return saved, events, items
            finally:
                if first:
                    first.close()
                if second:
                    second.close()
                if supervisor and supervisor.poll() is None:
                    supervisor.terminate()
                    supervisor.wait(timeout=5)
                supervisor_log.close()

    def run_fixture(self, name):
        fixture_path = SERVER_TESTS_ROOT / "fixtures" / "provider-replay" / f"{name}.json"
        fixture = json.loads(fixture_path.read_text())
        replies_path = fixture_path.parent / "codex-rpc-replies.json"
        with tempfile.TemporaryDirectory(prefix="provider-replay-") as temp:
            root = Path(temp)
            executable = root / "provider"
            executable.write_text("#!/bin/sh\nexec '" + sys.executable + "' '" +
                str(SERVER_TESTS_ROOT / "provider-replay-server.py") + "' '" +
                str(fixture_path) + "' '" + str(replies_path) + "'\n")
            executable.chmod(0o700)

            if fixture.get("supervisorReplay"):
                transcript_path = root / "captured.jsonl"
                _saved, _events, answer_items = self.run_supervisor_fixture(
                    root, executable, fixture_path, transcript_path, fixture)
                frames = [json.loads(line) for line in transcript_path.read_text().splitlines()]
                self.assertTrue(any(row["direction"] == "out" for row in frames))
                self.assertTrue(any(row["direction"] == "in" for row in frames))
                self.assert_replay_rpc_contract(frames, json.loads(replies_path.read_text()), fixture)
                self.assert_durable_answers(fixture, {"lead": answer_items, "child": []})
                return

            def factory(server_root, notify, request, died):
                provider = fixture.get("provider", "codex")
                return AppServer(server_root, notify, request, died, executable=(
                    str(executable) if provider == "codex" else None), provider=provider)

            old_wait = AppServer.wait
            if fixture.get("lostStartReply"):
                def short_wait(server, submitted, timeout=60):
                    short = timeout == 60 and submitted[1] == "turn/start"
                    return old_wait(server, submitted, timeout=.15 if short else timeout)
                waiter = patch.object(AppServer, "wait", short_wait)
            else:
                waiter = patch.object(AppServer, "wait", old_wait)
            with waiter:
                transcript_path = root / "captured.jsonl"
                env = {"CODEX_AGENTS_PROVIDER_CAPTURE": "1",
                       "CODEX_AGENTS_PROVIDER_CAPTURE_FILE": str(transcript_path)}
                with patch.dict(os.environ, env, clear=False):
                    runtime = Runtime(root / "state", factory)
                    provider_patches = []
                    if fixture.get("provider") == "claude":
                        import codex_claude
                        command, child_env = self.claude_transport(root, fixture_path)
                        identity = {"status": "ready", "accountId": "claude:replay@example.test",
                                    "email": "replay@example.test", "plan": "max",
                                    "_credentialIdentity": "claude:replay@example.test"}
                        runtime.accounts.data["accounts"]["claude-replay"] = {
                            "id": "claude-replay", "provider": "claude", "status": "ready",
                            "accountId": identity["accountId"], "email": identity["email"],
                            "_credentialIdentity": identity["_credentialIdentity"],
                            "claudeOptions": {"customModels": [], "launchArgs": ""},
                            "home": str(root), "label": "Claude replay", "source": "fixture"}
                        runtime.accounts._save()
                        provider_patches.extend([
                            patch.object(codex_claude, "auth_metadata", return_value=identity),
                            patch.object(codex_claude, "transport", return_value=(command, child_env)),
                        ])
                        for provider_patch in provider_patches:
                            provider_patch.start()
                    try:
                        payload = {"name": name, "cwd": str(root),
                                   "prompt": "Replay a local provider transcript"}
                        if fixture.get("provider") == "claude":
                            payload.update(account_key="claude-replay", model="default")
                        child = None
                        agent = runtime.create(payload)
                        if fixture.get("parentChildOrder"):
                            runtime.dispatch()
                            def parent_start_sent():
                                if not transcript_path.exists():
                                    return False
                                outbound = any(json.loads(line)["direction"] == "out" and
                                    json.loads(line)["message"].get("method") == "thread/start"
                                    for line in transcript_path.read_text().splitlines())
                                return outbound and runtime.agent(agent["id"]).get("threadId") is not None
                            try:
                                eventually(parent_start_sent)
                            except AssertionError:
                                self.fail(f"parent={runtime.agent(agent['id'])}; capture=" +
                                    (transcript_path.read_text() if transcript_path.exists() else "<missing>"))
                            child = runtime.create({"name": "Replay child", "cwd": str(root),
                                "prompt": "Complete before the lead turn", "role": "reviewer",
                                "_worktree": False}, parent=agent["id"])
                        expected = fixture.get("expectedStatus", "completed")
                        if fixture.get("lostStartReply"):
                            def lost_reply_replayed():
                                if not transcript_path.exists():
                                    return False
                                frames = [json.loads(line) for line in transcript_path.read_text().splitlines()]
                                sent = any(row["direction"] == "out" and
                                           row["message"].get("method") == "turn/start" for row in frames)
                                observed = any(row["direction"] == "in" and
                                               row["message"].get("method") == "turn/completed" for row in frames)
                                return sent and observed and runtime.agent(agent["id"])["status"] == expected
                            eventually(lost_reply_replayed)
                        else:
                            eventually(lambda: runtime.agent(agent["id"])["status"] in
                                       {expected, "failed", "interrupted"})
                        if fixture.get("assert", {}).get("taskStatuses"):
                            required = set(fixture["assert"]["taskStatuses"])
                            def tasks_saved():
                                with runtime.db() as db:
                                    states = {json.loads(row[0]).get("status") for row in db.execute(
                                        "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.agent')=?",
                                        (agent["id"],))}
                                return required <= states
                            eventually(tasks_saved)
                        if child:
                            try:
                                eventually(lambda: runtime.agent(child["id"])["status"] == "completed")
                            except AssertionError:
                                self.fail(f"child={runtime.agent(child['id'])}; capture=" +
                                    (transcript_path.read_text() if transcript_path.exists() else "<missing>"))
                        with runtime.db() as db:
                            saved_agent = runtime.agent(agent["id"], db)
                            events = [dict(row) for row in db.execute(
                                "SELECT id,kind,status,text FROM runtime_events WHERE agent=? ORDER BY created,id",
                                (agent["id"],))]
                            answer_items = [json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM runtime_items WHERE agent=? ORDER BY created,id",
                                (agent["id"],))]
                            usage_rows = [json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM analytics_usage WHERE agent=?", (agent["id"],))]
                            tasks = [json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.agent')=?",
                                (agent["id"],))]
                            child_events = ([dict(row) for row in db.execute(
                                "SELECT kind,status,text FROM runtime_events WHERE agent=? ORDER BY created,id",
                                (child["id"],))] if child else [])
                            child_items = ([json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM runtime_items WHERE agent=? ORDER BY created,id",
                                (child["id"],))] if child else [])
                        self.assertEqual(saved_agent["status"], expected, saved_agent)
                        self.assertTrue(events, "lead runtime_events must persist")
                        for expected_kind in fixture.get("assert", {}).get("eventKinds", []):
                            self.assertIn(expected_kind, [event["kind"] for event in events])
                        for expected_status in fixture.get("assert", {}).get("eventStatuses", []):
                            self.assertIn(expected_status, [event["status"] for event in events])
                        if fixture.get("assert", {}).get("usage"):
                            self.assertTrue(usage_rows, "analytics_usage must persist")
                            encoded = json.dumps(usage_rows)
                            for token in fixture["assert"]["usage"]:
                                self.assertIn(token, encoded)
                        for task_status in fixture.get("assert", {}).get("taskStatuses", []):
                            self.assertIn(task_status, [task.get("status") for task in tasks])
                        if child:
                            self.assertIn("child_result", [event["kind"] for event in events])
                            self.assertEqual(sum(event["kind"] == "child_result" for event in events),
                                             fixture.get("assert", {}).get("childResultCount", 1))
                            child_agent = runtime.agent(child["id"])
                            self.assertTrue(child_agent.get("lastCompletedTurn"), child_agent)
                        self.assert_durable_answers(fixture, {
                            "lead": answer_items,
                            "child": child_items,
                        })
                    finally:
                        runtime.close()
                        for provider_patch in reversed(provider_patches):
                            provider_patch.stop()
                frames = [json.loads(line) for line in transcript_path.read_text().splitlines()]
                self.assertTrue(any(row["direction"] == "out" for row in frames))
                self.assertTrue(any(row["direction"] == "in" for row in frames))
                self.assertTrue(all(row["provider"] == fixture.get("provider", "codex") for row in frames))
                self.assert_replay_rpc_contract(frames, json.loads(replies_path.read_text()), fixture)
                required_turn_starts = fixture.get("assert", {}).get("turnStarts")
                if required_turn_starts:
                    observed = sum(row["direction"] == "in" and row["message"].get("method") == "turn/started"
                                   for row in frames)
                    self.assertGreaterEqual(observed, required_turn_starts)
                if fixture.get("parentChildOrder"):
                    completions = [row["message"]["params"]["threadId"] for row in frames
                        if row["direction"] == "in" and row["message"].get("method") == "turn/completed"]
                    self.assertLess(completions.index("replay-thread-2"),
                                    completions.index("replay-thread-1"))
                return {"agent": saved_agent, "events": events, "usage": usage_rows,
                        "tasks": tasks, "items": answer_items, "childItems": child_items,
                        "childEvents": child_events, "frames": frames}

    def assert_durable_answers(self, fixture, items_by_owner):
        expected = fixture.get("assert", {}).get("answerItems", [])
        observed = []
        for owner, items in items_by_owner.items():
            for item in items:
                if item.get("role") == "assistant":
                    observed.append({"owner": owner, "id": item["id"], "text": item["text"]})
        expected_observed = []
        for answer in expected:
            matches = [item for item in observed if item["owner"] == answer["owner"]
                       and item["id"].endswith(":" + answer["nativeId"])]
            self.assertEqual(len(matches), answer.get("count", 1),
                             f"durable answer cardinality mismatch for {answer}; observed={observed!r}")
            for match in matches:
                self.assertEqual(match["text"], answer["text"])
            expected_observed.extend(matches)
        self.assertEqual(sorted(observed, key=lambda item: (item["owner"], item["id"])),
                         sorted(expected_observed, key=lambda item: (item["owner"], item["id"])),
                         "unexpected or missing durable assistant answer item")

    def assert_replay_rpc_contract(self, frames, recorded_replies, fixture, allow_pending=False):
        recorded_replies = {**recorded_replies, **fixture.get("rpcReplies", {})}
        def matches_template(actual, template):
            if isinstance(template, dict):
                return (isinstance(actual, dict) and actual.keys() == template.keys()
                        and all(matches_template(actual[key], value) for key, value in template.items()))
            if isinstance(template, list):
                return (isinstance(actual, list) and len(actual) == len(template)
                        and all(matches_template(value, expected)
                                for value, expected in zip(actual, template)))
            if isinstance(template, str) and template.startswith("$"):
                return isinstance(actual, str) and bool(actual)
            return actual == template

        epochs = []
        current = {"requests": {}, "responses": {}}
        epochs.append(current)
        for frame in frames:
            message = frame["message"]
            if frame["direction"] == "out":
                method = message.get("method")
                self.assertTrue(
                    method in recorded_replies or method in EMPTY_RESULT_METHODS
                    or ("id" not in message and method in OUTBOUND_NOTIFICATION_METHODS),
                    f"outbound provider method is not explicitly replayed or allowlisted: {method}")
            if frame["direction"] == "out" and "id" in message:
                if (message.get("method") in {"initialize", "thread/loaded/list"}
                        and message["id"] in current["requests"]):
                    current = {"requests": {}, "responses": {}}
                    epochs.append(current)
                requests = current["requests"]
                self.assertNotIn(message["id"], requests,
                                 f"duplicate outbound RPC id: {message}")
                requests[message["id"]] = message

        # Provider request IDs restart with a connection. A response from the
        # previous connection may arrive in the captured stream after reattach,
        # so correlate it to the oldest still-unanswered matching request ID.
        unmatched_responses = []
        for frame in frames:
            message = frame["message"]
            if frame["direction"] != "in" or "id" not in message:
                continue
            candidates = [epoch for epoch in epochs
                          if message["id"] in epoch["requests"]
                          and message["id"] not in epoch["responses"]]
            if len(candidates) > 1 and "result" in message:
                matching = []
                for epoch in candidates:
                    request = epoch["requests"][message["id"]]
                    method = request["method"]
                    template = (recorded_replies.get(method, {}).get("message", {}).get("result")
                                if method in recorded_replies else {})
                    if matches_template(message["result"], template):
                        matching.append(epoch)
                if len(matching) == 1:
                    candidates = matching
            if not candidates:
                unmatched_responses.append(message)
                continue
            candidates[0]["responses"][message["id"]] = message
        allowed_orphans = fixture.get("allowedOrphanResponses", [])
        if allow_pending:
            self.assertTrue(all(response in allowed_orphans for response in unmatched_responses),
                            "provider returned a duplicate or unrequested response")
        else:
            self.assertEqual(unmatched_responses, allowed_orphans,
                             "provider returned a duplicate or unrequested response")

        missing_replies = []
        for epoch in epochs:
            for request_id, request in epoch["requests"].items():
                method = request["method"]
                if request_id not in epoch["responses"]:
                    missing_replies.append((method, request_id))
            for request_id, response in epoch["responses"].items():
                request = epoch["requests"].get(request_id)
                self.assertIsNotNone(request, f"response has no exact outbound request id {request_id!r}")
                method = request["method"]
                self.assertNotIn("error", response,
                                 f"provider returned a JSON-RPC error for {method}: {response}")
                self.assertIn("result", response,
                              f"provider response has no result for {method}: {response}")
                if fixture.get("provider") == "claude" and method == "thread/start":
                    actual_result = response["result"]
                    thread = actual_result.get("thread")
                    echoed = {key: value for key, value in actual_result.items() if key != "thread"}
                    expected_echo = dict(request.get("params", {}))
                    expected_echo.update({"model": request.get("params", {}).get("model", "default"),
                                          "sandbox": None})
                    self.assertEqual(echoed, expected_echo,
                                     "Claude thread/start must echo its exact parameters")
                    self.assertIsInstance(thread, dict)
                    self.assertEqual(set(thread), {"id", "cwd", "createdAt", "updatedAt", "preview",
                                                   "name", "historyVersion", "turns", "status", "modelProvider"})
                    self.assertTrue(thread.get("id"))
                    self.assertEqual(thread.get("cwd"), request.get("params", {}).get("cwd"))
                    self.assertIsInstance(thread.get("createdAt"), int)
                    self.assertIsInstance(thread.get("updatedAt"), int)
                    self.assertEqual(thread.get("preview"), "")
                    self.assertIsNone(thread.get("name"))
                    self.assertRegex(thread.get("historyVersion", ""), r"^[0-9a-f]{64}$")
                    self.assertEqual(thread.get("turns"), [])
                    self.assertEqual(thread.get("status"), {"type": "idle"})
                    self.assertEqual(thread.get("modelProvider"), "claude")
                    continue
                if method not in recorded_replies:
                    self.assertEqual(response.get("result"), {},
                                     f"unexpected nonempty result for explicit replay method {method}")
                    continue
                recorded = recorded_replies[method]
                self.assertEqual(recorded.get("direction"), "in", method)
                self.assertEqual(recorded.get("responseFor"), method)
                expected_result = json.dumps(recorded["message"].get("result"))
                actual_result = response.get("result", {})
                thread_id = (actual_result.get("thread") or {}).get("id") or \
                            request.get("params", {}).get("threadId") or "replay-thread-1"
                turn_id = (actual_result.get("turn") or {}).get("id") or "replay-turn-1"
                expected_result = expected_result.replace("$threadId", thread_id)
                expected_result = expected_result.replace("$turnId", turn_id)
                expected_result = expected_result.replace(
                    "$model", request.get("params", {}).get("model", "fixture-model"))
                expected_result = expected_result.replace("$platform", sys.platform)
                self.assertEqual(actual_result, json.loads(expected_result),
                                 f"exact provider RPC result mismatch for {method}; "
                                 f"request={request!r}; response={response!r}")
        expected_missing = fixture.get("expectedMissingReplies")
        missing = [{"method": method, "id": request_id}
                   for method, request_id in missing_replies]
        if allow_pending:
            return missing, unmatched_responses
        if expected_missing is not None:
            self.assertEqual(missing, expected_missing)
        else:
            self.assertEqual(missing, [], "every outbound RPC requires exactly one reply")

    def test_all_recorded_fixtures(self):
        fixture_dir = SERVER_TESTS_ROOT / "fixtures" / "provider-replay"
        names = sorted(path.stem for path in fixture_dir.glob("*.json")
                       if path.name != "codex-rpc-replies.json")
        self.assertGreaterEqual(len(names), 6)
        if os.environ.get("PROVIDER_REPLAY_FILTER"):
            names = [os.environ["PROVIDER_REPLAY_FILTER"]]
        for name in names:
            with self.subTest(fixture=name):
                self.run_fixture(name)


if __name__ == "__main__":
    unittest.main(verbosity=2)
