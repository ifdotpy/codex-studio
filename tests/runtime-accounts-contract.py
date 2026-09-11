#!/usr/bin/env python3
"""Account isolation with colliding native identifiers. No paid model calls."""

import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "runtime_fixture", Path(__file__).with_name("runtime-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_limit_resets import consume_reset


class AccountServer(f.FakeServer):
    def __init__(self, root, *callbacks):
        super().__init__(root, *callbacks)
        self.account_key = (
            root.name if root.parent.name == "account-servers" else "default"
        )
        self.consumes = []
        self.billing_id = self.account_key
        self.fail_consume = False

    def call(self, method, params, timeout=60):
        if method == "account/rateLimits/read":
            return {
                "accountId": self.billing_id,
                "rateLimitResetCredits": {
                    "availableCount": 1,
                    "credits": [
                        {
                            "id": "same-credit",
                            "status": "available",
                            "resetType": "codexRateLimits",
                        }
                    ],
                },
            }
        if method == "account/rateLimitResetCredit/consume":
            self.consumes.append(copy.deepcopy(params))
            if self.fail_consume:
                raise RuntimeError("The response was lost")
            return {"outcome": "reset"}
        return super().call(method, params, timeout)


class ControlledRuntime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()


class AccountContracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="codex-account-runtime-")
        self.root = Path(self.tmp.name)
        self.home = self.root / "default-home"
        self.other = self.root / "other-home"
        for home, key in (
            (self.home, "billing-default"),
            (self.other, "billing-other"),
        ):
            home.mkdir()
            (home / "auth.json").write_text(
                json.dumps({"tokens": {"account_id": key, "access_token": "fixture"}})
            )
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(self.home)})
        self.env.start()
        self.state = self.root / "state"
        self.runtime = ControlledRuntime(self.state, AccountServer)
        self.other_key = self.runtime.accounts.register(str(self.other))

    def tearDown(self):
        self.runtime.close()
        self.env.stop()
        self.tmp.cleanup()

    def lead(self, account="default"):
        a = self.runtime.create(
            {
                "name": "Lead",
                "prompt": "",
                "cwd": str(self.root),
                "account_key": account,
            },
            draft=True,
        )
        return self.runtime.prepare(a)

    def test_two_accounts_route_turns_and_colliding_thread_notifications(self):
        a, b = self.lead(), self.lead(self.other_key)
        self.assertEqual(a["threadId"], b["threadId"])
        for agent in (a, b):
            self.runtime.send(agent["id"], "Start", str(uuid.uuid4()))
        self.runtime.dispatch()
        f.eventually(
            lambda: all(
                self.runtime.agent(x["id"])["status"] == "running" for x in (a, b)
            )
        )
        for agent, expected in ((a, "default"), (b, self.other_key)):
            server = self.runtime.connect(expected)
            turns = [p for method, p in server.calls if method == "turn/start"]
            self.assertEqual(len(turns), 1)
            server.notify(
                {
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": agent["threadId"],
                        "itemId": "same-message",
                        "delta": expected,
                    },
                }
            )
        self.assertEqual(self.runtime.agent(a["id"])["tail"], "default")
        self.assertEqual(self.runtime.agent(b["id"])["tail"], self.other_key)

    def test_same_rpc_ids_reply_to_exact_account_and_tool_results_do_not_alias(self):
        a, b = self.lead(), self.lead(self.other_key)
        for agent, key in ((a, "default"), (b, self.other_key)):
            server = self.runtime.connect(key)
            server.request(
                {
                    "id": 7,
                    "method": "item/tool/requestUserInput",
                    "params": {"threadId": agent["threadId"], "questions": []},
                }
            )
            self.runtime.dynamic(
                {
                    "id": 8,
                    "method": "item/tool/call",
                    "params": {
                        "threadId": agent["threadId"],
                        "callId": "same-call",
                        "tool": "orchestration_title",
                        "arguments": {"title": key},
                    },
                },
                key,
                self.runtime.connection_ids[key],
            )
        requests = self.runtime.snapshot()["requests"]
        self.assertEqual(len(requests), 2)
        for request in requests:
            self.runtime.answer(
                request["id"], {"answers": {"owner": request["accountKey"]}}
            )
        for agent, key in ((a, "default"), (b, self.other_key)):
            server = self.runtime.connect(key)
            self.assertEqual(self.runtime.agent(agent["id"])["name"], key)
            answers = [r for r in server.responses if r["id"] == 7]
            self.assertEqual(
                answers, [{"id": 7, "result": {"answers": {"owner": key}}}]
            )
        with self.runtime.db() as db:
            self.assertEqual(
                db.execute("SELECT COUNT(*) FROM runtime_tool_results").fetchone()[0], 2
            )

    def test_disconnect_only_loses_own_work_and_expires_own_requests(self):
        a, b = self.lead(), self.lead(self.other_key)
        with self.runtime.lock, self.runtime.db() as db:
            for agent in (a, b):
                agent.update(status="running", inFlight=True)
                self.runtime.put(db, "agents", agent)
                self.runtime.put(
                    db,
                    "tasks",
                    {"id": agent["id"], "agent": agent["id"], "status": "running"},
                )
                self.runtime.put(
                    db,
                    "monitors",
                    {"id": agent["id"], "agent": agent["id"], "status": "running"},
                )
                self.runtime.put(
                    db,
                    "requests",
                    {"id": agent["id"], "agent": agent["id"], "status": "pending"},
                )
                event = self.runtime.enqueue(db, agent, "user", "Pending")
                db.execute(
                    "UPDATE runtime_events SET status='dispatching' WHERE id=?",
                    (event,),
                )
        self.runtime.connect(self.other_key).died()
        self.assertEqual(self.runtime.agent(a["id"])["status"], "running")
        self.assertEqual(self.runtime.agent(b["id"])["status"], "interrupted")
        self.assertIn(a["id"], self.runtime.loaded)
        self.assertNotIn(b["id"], self.runtime.loaded)
        with self.runtime.db() as db:
            for table in ("tasks", "monitors", "requests"):
                records = {r["id"]: r for r in self.runtime.records(db, table)}
                self.assertEqual(
                    records[a["id"]]["status"],
                    "pending" if table == "requests" else "running",
                )
                self.assertEqual(
                    records[b["id"]]["status"],
                    "expired" if table == "requests" else "lost",
                )
            events = {
                r["agent"]: r["status"]
                for r in db.execute("SELECT agent,status FROM runtime_events")
            }
            self.assertEqual(events, {a["id"]: "dispatching", b["id"]: "uncertain"})

    def test_reconnect_rejects_old_connection_replies_and_notifications(self):
        a = self.lead(self.other_key)
        old = self.runtime.connect(self.other_key)
        generation = self.runtime.connection_ids[self.other_key]
        old.died()
        new = self.runtime.connect(self.other_key)
        old.notify(
            {
                "method": "item/agentMessage/delta",
                "params": {"threadId": a["threadId"], "delta": "stale"},
            }
        )
        self.assertEqual(self.runtime.agent(a["id"])["tail"], "")
        with self.assertRaisesRegex(RuntimeError, "original account connection"):
            self.runtime.reply({"id": 1, "result": {}}, self.other_key, generation)
        self.assertEqual(new.responses, [])
        old.died()
        self.assertNotIn(self.other_key, self.runtime.offline_accounts)

    def test_stale_callbacks_recheck_generation_after_waiting_for_state(self):
        a = self.lead(self.other_key)
        for operation in ("notification", "request", "dynamic"):
            server = self.runtime.connect(self.other_key)
            generation = self.runtime.connection_ids[self.other_key]
            entered, proceed = threading.Event(), threading.Event()
            original = self.runtime.connection_current
            calls = []

            def delayed(key, connection_id):
                result = original(key, connection_id)
                if threading.current_thread() is worker and not calls:
                    calls.append(True)
                    entered.set()
                    self.assertTrue(proceed.wait(2))
                return result

            message = {
                "id": 50,
                "method": "item/tool/requestUserInput",
                "params": {"threadId": a["threadId"], "questions": []},
            }
            if operation == "notification":
                message = {
                    "method": "item/agentMessage/delta",
                    "params": {"threadId": a["threadId"], "delta": "stale"},
                }
            elif operation == "dynamic":
                message = {
                    "id": 50,
                    "method": "item/tool/call",
                    "params": {
                        "threadId": a["threadId"],
                        "tool": "orchestration_title",
                        "arguments": {"title": "stale"},
                    },
                }
            worker = threading.Thread(
                target=getattr(self.runtime, operation),
                args=(message, self.other_key, generation),
            )
            with patch.object(self.runtime, "connection_current", side_effect=delayed):
                worker.start()
                self.assertTrue(entered.wait(2))
                server.died()
                self.runtime.connect(self.other_key)
                proceed.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())
            self.assertEqual(self.runtime.agent(a["id"])["tail"], "")
            self.assertEqual(self.runtime.agent(a["id"])["name"], "Lead")
            self.assertEqual(self.runtime.snapshot()["requests"], [])

    def test_children_inherit_and_account_binding_survives_restart(self):
        lead = self.lead(self.other_key)
        child = self.runtime.create(
            {"name": "Worker", "prompt": "Review", "role": "reviewer"},
            parent=lead["id"],
            defer=True,
        )
        self.assertEqual(child["accountKey"], self.other_key)
        with self.assertRaisesRegex(ValueError, "parent account"):
            self.runtime.create(
                {
                    "name": "Wrong",
                    "prompt": "Review",
                    "role": "reviewer",
                    "account_key": "default",
                },
                parent=lead["id"],
                defer=True,
            )
        self.runtime.close()
        self.runtime = ControlledRuntime(self.state, AccountServer)
        self.assertEqual(self.runtime.agent(child["id"])["accountKey"], self.other_key)
        self.assertEqual(
            self.runtime.prepare(self.runtime.agent(child["id"]))["accountKey"],
            self.other_key,
        )
        self.assertIn(self.other_key, self.runtime.servers)
        self.assertNotIn("default", self.runtime.servers)

    def test_only_empty_chat_can_change_account_and_reuses_empty_chat(self):
        a = self.runtime.new_lead({})
        b = self.runtime.new_lead({"previous": a["id"], "account_key": self.other_key})
        self.assertEqual(a["id"], b["id"])
        self.assertEqual(b["accountKey"], self.other_key)
        self.runtime.set_account(b["id"], "default")
        self.runtime.send(b["id"], "Pending", str(uuid.uuid4()))
        with self.assertRaisesRegex(ValueError, "fixed after"):
            self.runtime.set_account(b["id"], self.other_key)

    def test_new_chat_uses_saved_default_without_changing_existing_chat(self):
        previous = self.runtime.new_lead({})
        self.runtime.accounts.default(self.other_key)
        new_project = self.root / "new-project"
        new_project.mkdir()
        new = self.runtime.new_lead({"cwd": str(new_project)})
        self.assertEqual(new["accountKey"], self.other_key)
        self.assertEqual(self.runtime.new_lead({"cwd": previous["cwd"]})["accountKey"], "default")
        self.assertEqual(self.runtime.agent(previous["id"])["accountKey"], "default")

    def test_same_native_credit_retries_across_profiles_with_one_idempotency_key(self):
        other = self.runtime.connect(self.other_key)
        other.billing_id = "default"
        other.fail_consume = True
        first = consume_reset(
            self.runtime,
            {
                "account_key": self.other_key,
                "account_id": "default",
                "credit_id": "same-credit",
            },
        )
        self.assertEqual(first["outcome"], "uncertain")
        request = str(uuid.uuid4())
        params = {
            "account_key": "default",
            "account_id": "default",
            "credit_id": "same-credit",
            "request_id": request,
        }
        second = consume_reset(self.runtime, params)
        again = consume_reset(self.runtime, params)
        self.assertEqual(second["outcome"], "reset")
        self.assertEqual(again["outcome"], "reset")
        self.assertEqual(other.consumes, self.runtime.connect().consumes)
        self.assertEqual(len(other.consumes), 1)
        self.assertEqual(second["limits"]["accountKey"], "default")

    def test_nondefault_monitor_output_and_cancel_stay_on_its_connection(self):
        a = self.lead(self.other_key)
        monitor = self.runtime.monitor(a["id"], {"command": "fixture"}, approved=True)

        def running():
            with self.runtime.db() as db:
                return any(
                    m["id"] == monitor["id"] and m["status"] == "running"
                    for m in self.runtime.records(db, "monitors")
                )

        f.eventually(running)
        self.runtime.cancel_monitor(monitor["id"])
        server = self.runtime.connect(self.other_key)
        self.assertTrue(
            any(method == "command/exec/terminate" for method, _ in server.calls)
        )
        self.assertNotIn("default", self.runtime.servers)
        with self.runtime.db() as db:
            saved = self.runtime.records(db, "monitors")[0]
            self.assertIn("early output", saved["tail"])

    def test_limits_and_reset_credit_scope_are_independent(self):
        for key in ("default", self.other_key):
            self.assertEqual(self.runtime.limits(key)["data"]["accountId"], key)
        request = str(uuid.uuid4())
        result = consume_reset(
            self.runtime,
            {
                "account_key": self.other_key,
                "account_id": self.other_key,
                "credit_id": "same-credit",
                "request_id": request,
            },
        )
        self.assertEqual(result["outcome"], "reset")
        self.assertEqual(len(self.runtime.connect(self.other_key).consumes), 1)
        self.assertEqual(self.runtime.connect().consumes, [])
        with self.assertRaisesRegex(ValueError, "different account"):
            consume_reset(
                self.runtime,
                {
                    "account_id": "default",
                    "credit_id": "same-credit",
                    "request_id": request,
                },
            )
        with self.assertRaisesRegex(ValueError, "account changed"):
            consume_reset(
                self.runtime,
                {
                    "account_key": self.other_key,
                    "account_id": "default",
                    "credit_id": "same-credit",
                },
            )
        self.assertEqual(self.runtime.rate_limits_for()["data"]["accountId"], "default")


if __name__ == "__main__":
    unittest.main(verbosity=2)
