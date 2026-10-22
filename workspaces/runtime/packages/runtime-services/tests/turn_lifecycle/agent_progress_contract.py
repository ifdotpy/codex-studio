"""Explicit post-port projection expectations for the legacy Python oracles.

The historical runtime predates commandProgress/stall. Add the new contract at
its view boundary before serialization, preserving complete payload/hash/write
comparisons. This independent fixture derivation reads the Python seed database;
it never copies Rust's actual projection into the expected snapshot.
"""
import json
from functools import wraps

from studio_api.sync.agent_progress_generated import AgentCommandProgress, AgentStall


def records(db, table):
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone():
        return []
    return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM {table} ORDER BY rowid")]


def expected_progress(db, agent):
    commands = [
        AgentCommandProgress(
            monitorId=monitor["id"], stage=monitor.get("command", "").split("\n")[0],
            startedAt=monitor.get("created", 0),
            lastOutputLine=next((line for line in reversed(monitor.get("tail", "").splitlines()) if line.strip()), ""),
        ).model_dump()
        for monitor in records(db, "runtime_monitors")
        if monitor.get("agent") == agent["id"] and monitor.get("status") in {"starting", "running"}
    ]
    stall = None
    if not commands and not agent.get("isLead") and agent.get("deletedAt") is None and agent.get("status") in {"idle", "completed"}:
        task = next((task for task in records(db, "runtime_work")
                     if task.get("owner") == agent["id"] and task.get("status") in {"ready", "running", "blocked"}), None)
        if task:
            turns = [turn for turn in records(db, "runtime_execution_runs") if turn.get("agent") == agent["id"]]
            latest = max(turns, key=lambda turn: turn.get("created", 0), default={})
            stall = AgentStall(taskId=task["id"], since=max(task.get("updated", 0), latest.get("finished") or 0)).model_dump()
    # The Python oracle predates typed admission waits. Derive the documented
    # view contract from its independent seed records, preserving hash comparisons.
    wait = None
    if agent.get("status") in {"queued", "starting"}:
        persisted = agent.get("startWait")
        if persisted and persisted["epoch"] == agent.get("epoch", 0):
            wait = persisted
        if agent.get("status") == "queued":
            reason = ("paused" if not agent.get("autoWake") else
                      "native_failure" if agent.get("nativeFailureHold") else
                      "account_transfer" if agent.get("accountTransferId") is not None else None)
            if reason or wait is None:
                wait = {"reason": reason or "dispatch_pending", "epoch": agent.get("epoch", 0)}
    return {"commandProgress": commands, "stall": stall, "startWait": wait}


def install_projection_contract(runtime_class):
    original = runtime_class.agent_entity_view
    if getattr(original, "_progress_contract", False):
        return

    @wraps(original)
    def agent_entity_view(self, db, record):
        return {**original(self, db, record), **expected_progress(db, record)}

    agent_entity_view._progress_contract = True
    runtime_class.agent_entity_view = agent_entity_view
