#!/usr/bin/env python3
"""Installed Codex retains a native thread and its full context after tool refresh."""
import importlib.util
import json
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent


def fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


n = fixture("native_fixture", "native-primitives-integration.py")
c = fixture("catalog_fixture", "native-tools-contract.py")
import codex_native_tools as tools


class NativeTools(unittest.TestCase):
    def test_growth_preserves_source_and_existing_descendant_context(self):
        with n.native_server() as (server, _, provider, events, restart):
            provider.release.set()
            old = [{**c.OLD[0], "description": ""}]
            tid = server.call("thread/start", {"cwd": "/tmp", "model": "gpt-5.6-sol",
                              "config": n.n.THREAD_CONFIG, "dynamicTools": old})["thread"]["id"]
            turn = server.call("turn/start", {"threadId": tid, "input": [{"type": "text",
                                "text": "Preserve ancestor marker maple-842."}]})["turn"]["id"]
            n.n.until(lambda: any(e.get("method") == "turn/completed" and
                      e["params"]["turn"]["id"] == turn for e in events), "source turn")
            source = server.call("thread/read", {"threadId": tid})["thread"]
            child = server.call("thread/fork", {"threadId": tid, "excludeTurns": True,
                                "deferGoalContinuation": True, "config": n.n.THREAD_CONFIG})["thread"]
            source_path, child_path = Path(source["path"]), Path(child["path"])
            original_source, original_child = source_path.read_bytes(), child_path.read_bytes()
            home = next(parent for parent in source_path.parents if (parent / "config.toml").exists())
            state = home.parent / "catalog-state"
            state.mkdir()
            rt = c.Runtime(state)
            rt.servers["default"] = server
            rt.server = server
            rt.accounts = type("Accounts", (), {"get": lambda _, key: {"provider": "codex"},
                                                "home": lambda _, key: home})()
            rt.new_thread_params = lambda agent: {"cwd": "/tmp", "model": "gpt-5.6-sol",
                                                  "config": n.n.THREAD_CONFIG, "dynamicTools": c.NEW}
            with rt.db() as db:
                agent = rt.agent("chat-1", db)
                agent["threadId"] = tid
                rt.put(db, "agents", agent)
            count = len(provider.requests)
            result = tools.refresh_account(rt, agent_ids=["chat-1"])
            self.assertEqual(result["status"], "completed", result)
            target = rt.agent("chat-1")["threadId"]
            self.assertNotEqual(target, tid)
            self.assertEqual(len(provider.requests), count)
            self.assertEqual(source_path.read_bytes(), original_source)
            self.assertEqual(child_path.read_bytes(), original_child)
            resumed = restart()
            for native_id, expected_tool in ((target, "orchestration_review"), (child["id"], "old_tool")):
                resumed.call("thread/resume", {"threadId": native_id, "excludeTurns": True,
                                              "config": n.n.THREAD_CONFIG})
                next_turn = resumed.call("turn/start", {"threadId": native_id, "input": [{"type": "text",
                                        "text": "Continue from the ancestor decision."}]})["turn"]["id"]
                n.n.until(lambda: any(e.get("method") == "turn/completed" and
                          e["params"]["turn"]["id"] == next_turn for e in events), "resumed descendant turn")
                request = json.dumps(provider.requests[-1])
                self.assertIn("maple-842", request)
                self.assertIn(expected_tool, request)
            self.assertEqual(provider.unexpected, [])
            print(json.dumps({"headerGrowth": True, "sourceUnchanged": True, "existingDescendantResumed": True,
                              "targetResumed": True, "migrationModelRequests": 0, "cloudRequests": 0}))

    def test_exact_thread_context_and_current_tools_after_idle_process_recycle(self):
        with n.native_server() as (server, _, provider, events, restart):
            provider.release.set()
            native = server.call("thread/start", {"cwd": "/tmp", "model": "gpt-5.6-sol",
                                "config": n.n.THREAD_CONFIG, "dynamicTools": c.OLD})["thread"]
            tid = native["id"]
            turn = server.call("turn/start", {"threadId": tid, "input": [{"type": "text",
                                "text": "Preserve native history marker cedar-921."}]})["turn"]["id"]
            n.n.until(lambda: any(e.get("method") == "turn/completed" and
                      e["params"]["turn"]["id"] == turn for e in events), "initial native turn")
            native = server.call("thread/read", {"threadId": tid})["thread"]
            rollout = Path(native["path"])
            home = next(parent for parent in rollout.parents if (parent / "config.toml").exists())
            state = home.parent / "catalog-state"
            state.mkdir()
            rt = c.Runtime(state)
            rt.servers["default"] = server
            rt.server = server
            rt.accounts = type("Accounts", (), {"get": lambda _, key: {"provider": "codex"},
                                                "home": lambda _, key: home})()
            with rt.db() as db:
                agent = rt.agent("chat-1", db)
                agent["threadId"] = tid
                rt.put(db, "agents", agent)
            original_tail = rollout.read_bytes().partition(b"\n")[2]
            requests_before = len(provider.requests)
            result = tools.refresh_account(rt, agent_ids=["chat-1"])
            self.assertEqual(result["status"], "completed", result)
            self.assertIsNotNone(server.proc.poll())
            self.assertEqual(len(provider.requests), requests_before)
            self.assertEqual(rt.agent("chat-1")["threadId"], tid)
            self.assertEqual(rollout.read_bytes().partition(b"\n")[2], original_tail)
            resumed_server = restart()
            resumed = resumed_server.call("thread/resume", {"threadId": tid, "excludeTurns": True,
                                            "config": n.n.THREAD_CONFIG})["thread"]
            self.assertEqual(resumed["id"], tid)
            self.assertEqual(len(provider.requests), requests_before)
            second = resumed_server.call("turn/start", {"threadId": tid, "input": [{"type": "text",
                                          "text": "Continue with the current tools."}]})["turn"]["id"]
            n.n.until(lambda: any(e.get("method") == "turn/completed" and
                      e["params"]["turn"]["id"] == second for e in events), "resumed native turn")
            request = json.dumps(provider.requests[-1])
            self.assertIn("cedar-921", request)
            self.assertIn("orchestration_review", request)
            self.assertNotIn("old_tool", request)
            self.assertEqual(provider.unexpected, [])
            print(json.dumps({"sameNativeThread": True, "fullHistoryPreserved": True,
                              "currentTools": True, "migrationModelRequests": 0, "cloudRequests": 0}))


if __name__ == "__main__":
    unittest.main()
