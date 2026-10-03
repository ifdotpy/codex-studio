#!/usr/bin/env python3
"""Replay provider transcripts through the real Runtime and AppServer."""
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
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_runtime import AppServer, Runtime


def eventually(check, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.02)
    raise AssertionError("Replay did not reach its expected durable state")


class ProviderReplay(unittest.TestCase):
    def claude_transport(self, root, fixture_path):
        source = ROOT / "scripts" / "claude_bridge"
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

    def run_supervisor_fixture(self, root, executable, fixture_path, transcript_path):
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
                    str(ROOT / "scripts" / "codex_process_supervisor.py"), "--state", str(state)],
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
                with second.db() as db:
                    saved = second.agent(agent["id"], db)
                    events = [dict(row) for row in db.execute(
                        "SELECT kind,status,text FROM runtime_events WHERE agent=? ORDER BY created,id",
                        (agent["id"],))]
                self.assertEqual(saved["status"], "completed")
                self.assertIn("user", [row["kind"] for row in events])
                return saved, events
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
        fixture_path = ROOT / "tests" / "fixtures" / "provider-replay" / f"{name}.json"
        fixture = json.loads(fixture_path.read_text())
        replies_path = fixture_path.parent / "codex-rpc-replies.json"
        with tempfile.TemporaryDirectory(prefix="provider-replay-") as temp:
            root = Path(temp)
            executable = root / "provider"
            executable.write_text("#!/bin/sh\nexec '" + sys.executable + "' '" +
                str(ROOT / "tests" / "provider-replay-server.py") + "' '" +
                str(fixture_path) + "' '" + str(replies_path) + "'\n")
            executable.chmod(0o700)

            if fixture.get("supervisorReplay"):
                transcript_path = root / "captured.jsonl"
                self.run_supervisor_fixture(root, executable, fixture_path, transcript_path)
                frames = [json.loads(line) for line in transcript_path.read_text().splitlines()]
                self.assertTrue(any(row["direction"] == "out" for row in frames))
                self.assertTrue(any(row["direction"] == "in" for row in frames))
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
                            usage_rows = [json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM analytics_usage WHERE agent=?", (agent["id"],))]
                            tasks = [json.loads(row[0]) for row in db.execute(
                                "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.agent')=?",
                                (agent["id"],))]
                            child_events = ([dict(row) for row in db.execute(
                                "SELECT kind,status,text FROM runtime_events WHERE agent=? ORDER BY created,id",
                                (agent["id"],))] if child else [])
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
                            child_agent = runtime.agent(child["id"])
                            self.assertTrue(child_agent.get("lastCompletedTurn"), child_agent)
                    finally:
                        runtime.close()
                        for provider_patch in reversed(provider_patches):
                            provider_patch.stop()
                frames = [json.loads(line) for line in transcript_path.read_text().splitlines()]
                self.assertTrue(any(row["direction"] == "out" for row in frames))
                self.assertTrue(any(row["direction"] == "in" for row in frames))
                self.assertTrue(all(row["provider"] == fixture.get("provider", "codex") for row in frames))
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
                        "tasks": tasks, "frames": frames}

    def test_all_recorded_fixtures(self):
        fixture_dir = ROOT / "tests" / "fixtures" / "provider-replay"
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
