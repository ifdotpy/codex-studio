#!/usr/bin/env python3
"""End-to-end restart scenarios for watches, monitors, and parked workers."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location(
    "monitor_fixture", ROOT / "tests/monitor-lifecycle-contract.py"
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_agent_management import manage_agent
from codex_monitor_recovery import (_path as monitor_result_path,
                                   persist_monitor_result, recover_monitor_results)
from codex_runtime import ResponseTimeout, Runtime


def eventually(predicate, timeout=4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError("The expected runtime state did not occur")


class LifecycleScenarios(unittest.TestCase):
    setUp = fixture.MonitorLifecycleContract.setUp
    tearDown = fixture.MonitorLifecycleContract.tearDown
    record = fixture.MonitorLifecycleContract.record
    exits = fixture.MonitorLifecycleContract.exits

    def monitor(self, **options):
        result = self.runtime.monitor(self.agent["id"], {
            "command": "fixture-command", "timeout_ms": 1000, **options
        }, approved=True)
        eventually(lambda: "default" in self.runtime.servers
                   and self.runtime.servers["default"].command_wait_entered.is_set())
        self.server = self.runtime.servers["default"]
        return result["id"]

    def no_command_replayed(self):
        server = self.runtime.servers.get("default")
        return not server or not any(method == "command/exec" for method, _ in server.calls)

    def restart(self, *, hold_rules_tick=False):
        self.runtime.close()
        for thread in self.monitor_threads:
            thread.join(3)
            self.assertFalse(thread.is_alive())
        self.monitor_threads.clear()
        if hold_rules_tick:
            tick = Runtime.rules_tick
            Runtime.rules_tick = lambda runtime: None
        try:
            self.runtime = Runtime(Path(self.temp.name), fixture.MonitorServer)
        finally:
            if hold_rules_tick:
                Runtime.rules_tick = tick
        self.agent = self.runtime.agent(self.agent["id"])
        self.server = self.runtime.server or self.runtime.servers.get("default")
        original = self.runtime.run_monitor

        def run(key):
            self.monitor_threads.append(threading.current_thread())
            return original(key)

        self.runtime.run_monitor = run

    def monitor_record(self, key):
        with self.runtime.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_monitors WHERE id=?", (key,)
            ).fetchone()
        return json.loads(row[0])

    def monitor_count(self, key):
        with self.runtime.db() as db:
            return db.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?", ("monitor:" + key,)
            ).fetchone()[0]

    def abandon_pending_commands(self):
        self.server.timeout_commands = True
        self.server.commands.clear()

    def test_scenario_11_success_exit_code_survives_restart_at_each_receipt_boundary(self):
        # No durable result file: restart reports unknown and does not rerun.
        self.server.timeout_commands = True
        unknown = self.monitor(success_exit_codes=[0, 1])
        eventually(lambda: bool(self.record(unknown).get("error")))
        self.abandon_pending_commands()
        self.restart()
        self.assertEqual(self.monitor_record(unknown)["status"], "lost")
        self.assertEqual(self.monitor_count(unknown), 1)
        self.assertTrue(self.no_command_replayed())

        self.server = self.runtime.connect("default")
        # A result file survives a failed SQLite commit and restores exit code 1.
        self.server.timeout_commands = False
        pending = self.monitor(success_exit_codes=[0, 1])
        original = self.runtime._finish_monitor

        def fail_commit(*args, **kwargs):
            raise sqlite3.OperationalError("injected commit failure")

        self.runtime._finish_monitor = fail_commit
        self.server.finish(pending, 1)
        receipt = monitor_result_path(self.temp.name, pending)
        eventually(receipt.exists)
        self.assertEqual(self.monitor_record(pending)["status"], "running")
        self.runtime._finish_monitor = original
        self.restart()
        self.assertEqual(self.monitor_record(pending)["status"], "completed")
        self.assertEqual(self.monitor_record(pending)["exitCode"], 1)
        self.assertEqual(self.monitor_count(pending), 1)

        # A committed result remains exact and does not create a second event.
        committed = self.monitor(success_exit_codes=[0, 1])
        self.server.finish(committed, 1)
        eventually(lambda: self.monitor_record(committed)["status"] == "completed")
        self.restart()
        self.assertEqual(self.monitor_record(committed)["exitCode"], 1)
        self.assertEqual(self.monitor_count(committed), 1)

    def test_scenario_11_late_exact_receipt_sends_one_explicit_correction(self):
        self.server.timeout_commands = True
        key = self.monitor(success_exit_codes=[0, 1])
        eventually(lambda: bool(self.record(key).get("error")))
        monitor = self.monitor_record(key)
        self.abandon_pending_commands()
        self.restart()
        with self.runtime.lock, self.runtime.db() as db:
            event = db.execute("SELECT text FROM runtime_events WHERE id=?",
                               ("monitor:" + key,)).fetchone()
            self.assertEqual(json.loads(event["text"])["status"], "lost")
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?",
                       ("monitor:" + key,))
        persist_monitor_result(self.temp.name, key, {
            "operation": monitor["operation"], "code": 1, "error": None,
            "finished": time.time(),
        })
        with self.runtime.lock, self.runtime.db() as db:
            restored = recover_monitor_results(self.runtime, db)
            correction = db.execute(
                "SELECT text,status FROM runtime_events WHERE id=?",
                ("monitor-correction:" + key,),
            ).fetchone()
            original = db.execute(
                "SELECT text,status FROM runtime_events WHERE id=?", ("monitor:" + key,)
            ).fetchone()
        self.assertEqual(restored["restored"], [key])
        self.assertEqual(self.monitor_record(key)["status"], "completed")
        self.assertEqual(original["status"], "delivered")
        self.assertEqual(json.loads(original["text"])["status"], "lost")
        self.assertEqual(correction["status"], "pending")
        correction_text = json.loads(correction["text"])
        self.assertIn("Do not repeat the command", correction_text["message"])
        self.assertEqual(correction_text["result"]["status"], "completed")
        with self.runtime.lock, self.runtime.db() as db:
            recover_monitor_results(self.runtime, db)
            self.assertEqual(db.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?",
                ("monitor-correction:" + key,),
            ).fetchone()[0], 1)

    def test_scenario_12_remote_loss_and_disconnect_do_not_repeat_commands(self):
        self.server = self.runtime.connect("default")
        self.server.timeout_commands = True
        key = self.monitor()
        self.runtime.monitor_unknown(key, self.monitor_record(key)["operation"],
                                     "Remote process was lost; outcome unknown")
        self.abandon_pending_commands()
        self.restart()
        self.assertEqual(self.monitor_record(key)["status"], "lost")
        self.assertEqual(self.monitor_count(key), 1)
        self.assertTrue(self.no_command_replayed())

        self.server = self.runtime.connect("default")
        self.server.timeout_commands = True
        key = self.monitor()
        self.server.died()
        self.assertEqual(self.monitor_record(key)["status"], "lost")
        self.abandon_pending_commands()
        self.restart()
        self.assertEqual(self.monitor_record(key)["status"], "lost")
        self.assertEqual(self.monitor_count(key), 1)

    def test_scenario_12_quiet_output_liveness_probe_is_not_repeated_after_restart(self):
        key = self.monitor(stall_timeout_seconds=1, liveness_command="true")
        with self.runtime.lock, self.runtime.db() as db:
            monitor = self.monitor_record(key)
            monitor["activityAt"] = time.time() - 120
            self.runtime.put(db, "monitors", monitor)
        self.runtime.monitors_tick()
        eventually(lambda: self.monitor_record(key).get("lastLivenessResult") is not None)
        before = sum(1 for method, params in self.server.calls
                     if method == "command/exec" and params.get("processId", "").startswith("liveness:"))
        self.assertEqual(before, 1)
        self.server.commands[key].set_exception(
            ResponseTimeout("command/exec response timed out; outcome unknown")
        )
        eventually(lambda: bool(self.monitor_record(key).get("error")))
        self.server.commands.clear()
        self.restart()
        self.assertEqual(self.monitor_record(key)["status"], "lost")
        self.assertIsNotNone(self.monitor_record(key).get("lastLivenessResult"))
        server = self.runtime.servers.get("default")
        after = sum(1 for method, params in (server.calls if server else [])
                    if method == "command/exec" and params.get("processId", "").startswith("liveness:"))
        self.assertEqual(after, 0)
        self.assertEqual(self.monitor_count(key), 1)

    def test_scenario_13_active_watch_intent_survives_temporary_owner_recovery(self):
        rules = []
        for kind in ("interval", "file", "event"):
            args = {"action": "save", "name": "Restart " + kind, "kind": kind,
                    "intervalSeconds": 3600, "at": time.time() + 3600,
                    "command": "true"}
            if kind == "file":
                args["path"] = str(Path(self.temp.name) / "watched")
            if kind == "event":
                args["event"] = "worker_completed"
            rules.append(self.runtime.rules_action(args, self.agent["id"], self.agent["epoch"]))
        with self.runtime.lock, self.runtime.db() as db:
            owner = self.runtime.agent(self.agent["id"], db)
            owner.update(status="running", inFlight=True)
            self.runtime.put(db, "agents", owner)
        self.restart(hold_rules_tick=True)
        with self.runtime.lock, self.runtime.db() as db:
            owner = self.runtime.agent(self.agent["id"], db)
            owner.update(status="interrupted", autoWake=False, inFlight=False,
                         restartRecovery={"stage": "pending", "autoWake": True,
                             "epoch": owner["epoch"], "accountKey": owner.get("accountKey", "default"),
                             "threadId": owner["threadId"]})
            self.runtime.put(db, "agents", owner)
        self.runtime.rules_tick()
        with self.runtime.db() as db:
            records = {r["id"]: r for r in self.runtime.records(db, "rules")}
        self.assertEqual([records[r["id"]]["status"] for r in rules], ["active"] * 3)
        with self.runtime.lock, self.runtime.db() as db:
            owner = self.runtime.agent(self.agent["id"], db)
            owner.pop("restartRecovery", None)
            owner["disconnectRecovery"] = {
                "epoch": owner["epoch"], "accountKey": owner.get("accountKey", "default"),
                "threadId": owner["threadId"], "autoWake": True,
            }
            self.runtime.put(db, "agents", owner)
        self.runtime.rules_tick()
        with self.runtime.db() as db:
            records = {r["id"]: r for r in self.runtime.records(db, "rules")}
        self.assertEqual([records[r["id"]]["status"] for r in rules], ["active"] * 3)

    def test_scenario_13_interrupted_check_is_held_and_visible_after_restart(self):
        worker = self.runtime.create({"name": "Watch owner", "prompt": "Wait",
            "role": "reviewer"}, self.agent["id"])
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "running")
        worker = self.runtime.agent(worker["id"])
        self.server.complete(worker["threadId"], worker["turnId"])
        eventually(lambda: self.runtime.agent(worker["id"])["status"] == "completed")
        rule = self.runtime.rules_action({"action": "save", "name": "In flight",
            "kind": "interval", "intervalSeconds": 3600, "command": "true",
            "agent": worker["id"]}, worker["id"], worker["epoch"])
        with self.runtime.lock, self.runtime.db() as db:
            current = next(r for r in self.runtime.records(db, "rules") if r["id"] == rule["id"])
            current.update(inFlight=True, checks=1)
            self.runtime.put(db, "rules", current)
        self.server.timeout_commands = True
        monitor = self.runtime.monitor(worker["id"], {"command": "true"},
                                       approved=True, rule={**rule, "inFlight": True, "checks": 1})
        self.server.command_wait_entered.wait(3)
        eventually(lambda: bool(self.monitor_record(monitor["id"]).get("error")))
        self.abandon_pending_commands()
        self.restart(hold_rules_tick=True)
        with self.runtime.db() as db:
            current = next(r for r in self.runtime.records(db, "rules") if r["id"] == rule["id"])
            events = db.execute("SELECT agent,status,text FROM runtime_events WHERE id LIKE ?",
                                ("rule-hold:%",)).fetchall()
        self.assertEqual(current["status"], "paused")
        self.assertIn("unknown", current["error"].lower())
        self.assertEqual({event["agent"] for event in events}, {worker["id"], self.agent["id"]})
        self.assertEqual(len(events), 2)
        self.assertTrue(all("not repeated" in event["text"] for event in events))

    def test_scenario_14_restart_park_stop_restart_emit_preserves_stop(self):
        worker = self.runtime.create({"name": "Parked worker", "prompt": "Wait",
            "role": "reviewer"}, self.agent["id"], defer=True)
        self.restart(hold_rules_tick=True)
        lead = self.runtime.agent(self.agent["id"])
        with self.runtime.lock, self.runtime.db() as db:
            lead.update(status="running", autoWake=True, inFlight=False)
            self.runtime.put(db, "agents", lead)
        lead_epoch = self.runtime.agent(self.agent["id"])["epoch"]
        manage_agent(self.runtime, self.agent["id"], {"action": "park",
            "agent_id": worker["id"], "event": "ready"}, lead_epoch)
        self.restart(hold_rules_tick=True)
        lead = self.runtime.agent(self.agent["id"])
        with self.runtime.lock, self.runtime.db() as db:
            lead.update(status="running", autoWake=True, inFlight=False)
            self.runtime.put(db, "agents", lead)
        self.runtime.stop(worker["id"], descendants=False)
        self.assertFalse(self.runtime.agent(worker["id"]).get("parkedEvent"))
        self.restart(hold_rules_tick=True)
        lead = self.runtime.agent(self.agent["id"])
        with self.runtime.lock, self.runtime.db() as db:
            lead.update(status="running", autoWake=True, inFlight=False)
            self.runtime.put(db, "agents", lead)
        result = manage_agent(self.runtime, self.agent["id"], {"action": "emit_event",
            "event": "ready", "request_id": "after-stop"}, lead["epoch"])
        self.assertEqual(result["woken"], [])
        saved = self.runtime.agent(worker["id"])
        self.assertFalse(saved["autoWake"])
        self.assertEqual(saved["status"], "paused")
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE agent=? AND kind='event_wake'",
                                        (worker["id"],)).fetchone()[0], 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
