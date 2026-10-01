#!/usr/bin/env python3
"""Measure event creation through native submission with 600 isolated fake agents."""

import importlib.util
from contextlib import nullcontext
import json
from pathlib import Path
import sys
import tempfile
import threading
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


class MeasuredLock:
    def __init__(self, lock):
        self.lock = lock
        self.local = threading.local()
        self.guard = threading.Lock()
        self.holds = {}

    def __repr__(self):
        return repr(self.lock)

    def acquire(self, *args, **kwargs):
        frame = sys._getframe(1)
        if frame.f_code.co_name == "__enter__":
            frame = frame.f_back
        caller = frame.f_code.co_name
        acquired = self.lock.acquire(*args, **kwargs)
        if not acquired:
            return False
        depth = getattr(self.local, "depth", 0)
        self.local.depth = depth + 1
        if not depth:
            self.local.caller = caller
            self.local.entered = time.monotonic_ns()
        return True

    def __enter__(self):
        self.acquire()
        return self

    def release(self):
        depth = self.local.depth - 1
        self.local.depth = depth
        if not depth:
            elapsed = (time.monotonic_ns() - self.local.entered) / 1e6
            with self.guard:
                self.holds.setdefault(self.local.caller, []).append(elapsed)
        self.lock.release()

    def __exit__(self, *_):
        self.release()


def measure(legacy, *, delayed_commit=False, heavy=False, indexed=True):
    with tempfile.TemporaryDirectory(prefix="studio-delivery-latency-") as root:
        runtime = Runtime(Path(root) / "state", fixture.FakeServer)
        if not indexed:
            runtime._dispatch_indexes_ready = True
        # Keep the periodic scheduler out of this deterministic path measure.
        # The full pass below is called directly with the same runtime object.
        runtime.closed = True
        runtime.changed.set()
        runtime.scheduler.join(timeout=5)
        assert not runtime.scheduler.is_alive()
        runtime.closed = False
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
                    if heavy:
                        agent["fixtureLargeRecord"] = "x" * 8192
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
                if heavy:
                    for index in range(240):
                        db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                            (f"old-{index}", f"fixture-{index+100:04d}", "followup",
                             "y" * 4096, "delivered", time.time(), agent["epoch"], None, None))
            traced = MeasuredLock(runtime.lock)
            runtime.lock = traced
            full_pass_began = time.monotonic_ns()
            runtime.dispatch_all()
            full_pass_ms = (time.monotonic_ns() - full_pass_began) / 1e6
            full_pass_lock_ms = sum(traced.holds.get("dispatch_all", []))
            full_pass_candidates_ms = sum(traced.holds.get("dispatch_lock", []))
            start = time.monotonic()
            before_commit_at = None
            for index, recipient in enumerate(recipients):
                guard = nullcontext() if delayed_commit and index == 0 else runtime.lock
                with guard, runtime.db() as db:
                    payload = json.dumps({"room": "broadcast:" + lead["id"],
                                          "message_id": f"message-{index}", "sender": lead["id"],
                                          "sender_name": "Fixture", "text": f"Message {index}"})
                    runtime.enqueue(db, runtime.agent(recipient["id"], db),
                                    "agent_message", payload, f"latency-{index}")
                    if delayed_commit and index == 0:
                        time.sleep(.08)
                        before_commit_at = time.monotonic_ns()
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
                names = {row[1] for row in db.execute("PRAGMA index_list('runtime_agents')")}
                expected = {"runtime_agent_dispatch_active", "runtime_agent_dispatch_workspace"}
                assert expected.issubset(names) == (indexed and not legacy)
                for row in db.execute("SELECT e.created,m.record FROM runtime_events e "
                                      "JOIN runtime_event_meta m ON m.id=e.id WHERE e.id LIKE 'latency-%' "
                                      "ORDER BY CAST(substr(e.id,9) AS INTEGER)"):
                    rows.append((row["created"], json.loads(row["record"])["timing"]))
            if delayed_commit and not legacy:
                assert rows[0][1]["fastEnteredAt"] >= before_commit_at, (
                    "Fast dispatch ran before the sender transaction committed")
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
            fast_segments = {}
            lock_holders = {}
            if not legacy:
                for _, marks in rows:
                    assert isinstance(marks.get("fastLockOwners"), list)
                    for holder in marks["fastLockOwners"]:
                        if holder:
                            name = holder.split("ms:", 1)[-1]
                            lock_holders[name] = lock_holders.get(name, 0) + 1
                for label, selected in (("busy", rows[:20]), ("idleReleased", rows[20:])):
                    fast_segments[label] = {}
                    for previous, current in (("enqueuedAt", "fastQueuedAt"),
                                              ("fastQueuedAt", "fastScheduledAt"),
                                              ("fastScheduledAt", "fastEnteredAt"),
                                              ("fastEnteredAt", "fastLockedAt"),
                                              ("fastLockedAt", "fastIndexesReadyAt"),
                                              ("fastIndexesReadyAt", "fastTeamCheckedAt"),
                                              ("fastTeamCheckedAt", "fastAgentLoadedAt"),
                                              ("fastAgentLoadedAt", "fastActiveScanAt"),
                                              ("fastActiveScanAt", "fastWorkspaceScanAt"),
                                              ("fastWorkspaceScanAt", "fastCandidatesAt"),
                                              ("fastCandidatesAt", "fastRadioCheckedAt"),
                                              ("fastRadioCheckedAt", "fastCapacityCheckedAt"),
                                              ("fastCapacityCheckedAt", "fastAccountCheckedAt"),
                                              ("fastAccountCheckedAt", "fastBudgetCheckedAt"),
                                              ("fastBudgetCheckedAt", "fastActorReloadedAt"),
                                              ("fastActorReloadedAt", "fastToolGateAt"),
                                              ("fastToolGateAt", "fastBatchLoadedAt"),
                                              ("fastBatchLoadedAt", "dispatchPickedAt")):
                        spans = [(marks[current] - marks[previous]) / 1e6
                                 for _, marks in selected]
                        fast_segments[label][previous + "To" + current] = {
                            "p50": percentile(spans, .5), "p90": percentile(spans, .9)}
            submission_segments = {}
            for label, selected in (("busy", rows[:20]), ("idleReleased", rows[20:])):
                submission_segments[label] = {}
                for previous, current in (("preparedAt", "transcriptReadyAt"),
                                          ("transcriptReadyAt", "turnParamsReadyAt"),
                                          ("turnParamsReadyAt", "reservationCommittedAt"),
                                          ("reservationCommittedAt", "nativeSubmitBeganAt"),
                                          ("nativeSubmitBeganAt", "submittedAt")):
                    spans = [(marks[current] - marks[previous]) / 1e6
                             for _, marks in selected]
                    submission_segments[label][previous + "To" + current] = {
                        "p50": percentile(spans, .5), "p90": percentile(spans, .9)}
            return {"mode": "full scheduler" if legacy else "per agent", "agents": 600,
                    "delayedCommit": delayed_commit, "largeRecordsAndEvents": heavy,
                    "indexed": indexed,
                    "fullPassMs": round(full_pass_ms, 3),
                    "fullPassLockHoldMs": round(full_pass_lock_ms, 3),
                    "fullPassCandidateLockHoldMs": round(full_pass_candidates_ms, 3),
                    "busyRecipients": 20, "idleReleasedRecipients": 10,
                    "events": len(rows), "elapsedMs": round((time.monotonic() - start) * 1000, 3),
                    "enqueuedToSubmittedMs": {"p50": percentile(totals, .5),
                                              "p90": percentile(totals, .9), "max": percentile(totals, 1)},
                    "recipientGroupsMs": groups,
                    "fastSegmentsMs": fast_segments,
                    "submissionSegmentsMs": submission_segments,
                    "fastLockHolderSamples": lock_holders,
                    "senderReservationMs": {"p50": percentile(reservations, .5),
                                            "p90": percentile(reservations, .9),
                                            "lockWaitP90": percentile(reservation_locks, .9)},
                    "segments": segments}
        finally:
            runtime.close()


if __name__ == "__main__":
    for legacy_mode, delayed_commit, heavy in ((True, False, False),
                                                (False, False, False),
                                                (True, True, True),
                                                (False, True, True)):
        print(json.dumps(measure(legacy_mode, delayed_commit=delayed_commit, heavy=heavy)), flush=True)
