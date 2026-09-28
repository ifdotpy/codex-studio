#!/usr/bin/env python3
"""Measure event creation through native submission with 600 isolated fake agents."""

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location(
    "delivery_fixture_server", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def percentile(values, fraction):
    values = sorted(values)
    return round(values[min(len(values) - 1, int((len(values) - 1) * fraction))], 3)


def measure(legacy):
    with tempfile.TemporaryDirectory(prefix="studio-delivery-latency-") as root:
        runtime = Runtime(Path(root) / "state", fixture.FakeServer)
        runtime._fast_delivery_enabled = not legacy
        try:
            lead = runtime.create({"name": "Fixture", "cwd": root, "prompt": ""}, draft=True, defer=True)
            runtime.connect()
            recipients = []
            with runtime.lock, runtime.db() as db:
                for index in range(600):
                    agent = dict(lead)
                    agent_id = f"fixture-{index:04d}"
                    agent.update(id=agent_id, rootId=lead["id"], parentId=lead["id"],
                                 isLead=False, name=agent_id, role="implementer",
                                 concurrency=64, maxAgents=1000, threadId=f"thread-{agent_id}",
                                 turnId=f"turn-{agent_id}" if index < 20 else None,
                                 inFlight=index < 20,
                                 status="running" if index < 20 else "idle")
                    runtime.put(db, "agents", agent)
                    if index < 20:
                        runtime.loaded.add(agent_id)
                    if index < 30:
                        recipients.append(agent)
                root_agent = runtime.agent(lead["id"], db)
                root_agent.update(concurrency=64, maxAgents=1000, threadId="fixture-sender")
                runtime.put(db, "agents", root_agent)
                runtime.put(db, "rooms", {"id": "broadcast:" + lead["id"],
                                          "kind": "broadcast", "rootId": lead["id"]})
            start = time.monotonic()
            for index, recipient in enumerate(recipients):
                with runtime.lock, runtime.db() as db:
                    payload = json.dumps({"room": "broadcast:" + lead["id"],
                                          "message_id": f"message-{index}", "sender": lead["id"],
                                          "sender_name": "Fixture", "text": f"Message {index}"})
                    runtime.enqueue(db, runtime.agent(recipient["id"], db),
                                    "agent_message", payload, f"latency-{index}")
            if legacy:
                runtime.dispatch()
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                with runtime.db() as db:
                    done = db.execute("SELECT COUNT(*) FROM runtime_event_meta WHERE id LIKE 'latency-%' "
                                      "AND json_extract(record,'$.timing.submittedAt') IS NOT NULL").fetchone()[0]
                if done == len(recipients):
                    break
                time.sleep(.01)
            else:
                raise AssertionError(f"Only {done} of {len(recipients)} submissions completed")
            rows = []
            with runtime.db() as db:
                for row in db.execute("SELECT e.created,m.record FROM runtime_events e "
                                      "JOIN runtime_event_meta m ON m.id=e.id WHERE e.id LIKE 'latency-%' "
                                      "ORDER BY CAST(substr(e.id,9) AS INTEGER)"):
                    rows.append((row["created"], json.loads(row["record"])["timing"]))
            fields = ["enqueuedAt", "dispatchPickedAt", "startBeganAt", "validatedAt",
                      "programReadyAt", "repairReadyAt", "preparedAt",
                      "connectedAt", "submittedAt"]
            segments = {}
            for previous, current in zip(fields, fields[1:]):
                values = [(marks[current] - marks[previous]) / 1e6 for _, marks in rows]
                segments[previous + "To" + current] = {
                    "p50Ms": percentile(values, .5), "p90Ms": percentile(values, .9),
                    "maxMs": percentile(values, 1)}
            totals = [(marks["submittedAt"] - marks["enqueuedAt"]) / 1e6 for _, marks in rows]
            reservations = []
            reservation_locks = []
            for index in range(20):
                message = {"id": "tool-" + str(index), "params": {
                    "threadId": "fixture-sender", "turnId": "fixture-turn",
                    "callId": "tool-" + str(index), "tool": "orchestration_status",
                    "arguments": {}}}
                began = time.monotonic_ns()
                receipt = runtime.reserve_tool_request(message)
                reservations.append((time.monotonic_ns() - began) / 1e6)
                marks = receipt["timing"]
                reservation_locks.append((marks["reservationLockedAt"] - marks["reservationBeganAt"]) / 1e6)
            groups = {}
            for label, selected in (("busy", rows[:20]), ("idleReleased", rows[20:])):
                values = [(marks["submittedAt"] - marks["enqueuedAt"]) / 1e6
                          for _, marks in selected]
                group_segments = {}
                for previous, current in zip(fields, fields[1:]):
                    spans = [(marks[current] - marks[previous]) / 1e6 for _, marks in selected]
                    group_segments[previous + "To" + current] = {
                        "p50": percentile(spans, .5), "p90": percentile(spans, .9)}
                groups[label] = {"p50": percentile(values, .5), "p90": percentile(values, .9),
                                 "segments": group_segments}
            return {"mode": "full scheduler" if legacy else "per agent", "agents": 600,
                    "busyRecipients": 20, "idleReleasedRecipients": 10,
                    "events": len(rows), "elapsedMs": round((time.monotonic() - start) * 1000, 3),
                    "enqueuedToSubmittedMs": {"p50": percentile(totals, .5),
                                              "p90": percentile(totals, .9), "max": percentile(totals, 1)},
                    "recipientGroupsMs": groups,
                    "senderReservationMs": {"p50": percentile(reservations, .5),
                                            "p90": percentile(reservations, .9),
                                            "lockWaitP90": percentile(reservation_locks, .9)},
                    "segments": segments}
        finally:
            runtime.close()


if __name__ == "__main__":
    for legacy_mode in (True, False):
        print(json.dumps(measure(legacy_mode)), flush=True)
