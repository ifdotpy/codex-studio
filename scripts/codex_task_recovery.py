"""Recover exact terminal task results without starting or replaying work."""

from __future__ import annotations

from dataclasses import dataclass, field
import json
import sqlite3
import time
from typing import TYPE_CHECKING, Any, Protocol, cast

from codex_context_repair import _native_items
from codex_payloads import resolve_record, state_root
from codex_tool_requests import _prefix

if TYPE_CHECKING:
    from codex_records import AgentRecord
    from codex_runtime import Runtime


BATCH_SIZE = 32
SCAN_SECONDS = 30
READ_SECONDS = 5
TASK_TYPES = frozenset(("commandExecution", "fileChange", "dynamicToolCall", "mcpToolCall"))
REPAIR_ACTIVE = frozenset(("preparing", "submitted", "unknown", "ready"))
Position = tuple[int | float, int]
ReceiptRows = tuple[tuple[str, str], ...]


class HistoryServer(Protocol):
    def call(self, method: str, params: dict[str, Any], timeout: float) -> dict[str, Any]: ...


@dataclass
class RecoveryState:
    busy: bool = False
    next_scan: float = 0
    cursor: Position | None = None
    last: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ReceiptState:
    aliases: tuple[str, ...] = ()
    rows: ReceiptRows = ()

    def __bool__(self) -> bool:
        return bool(self.aliases or self.rows)


@dataclass
class TaskCheck:
    task: dict[str, Any]
    raw: str
    position: Position
    source: tuple[object, ...]
    account: str
    thread: str
    connection: str | None
    server: HistoryServer | None
    receipts: ReceiptState
    completed_receipt: dict[str, Any] | None


def _scope(agent: AgentRecord) -> tuple[object, ...]:
    return (agent["id"], agent["epoch"], agent.get("accountKey", "default"),
            agent.get("threadId"), agent.get("provider", "codex"),
            agent.get("environment"), agent.get("imageWorkspaceReady"))


def _eligible_owner(agent: AgentRecord) -> bool:
    repair = agent.get("contextRepair") or {}
    completed_owner = (agent.get("status") == "completed" and not agent.get("inFlight")
                       and not agent.get("turnId") and not agent.get("startAttempt"))
    return bool(not agent.get("deletedAt") and agent.get("provider", "codex") == "codex"
                and agent.get("threadId") and not agent.get("accountTransferId")
                and not agent.get("workspaceOperation")
                and (not agent.get("contextRepairWait") or completed_owner)
                and agent.get("status") not in {"queued", "starting"}
                and repair.get("phase") not in REPAIR_ACTIVE)


def _receipt_state(db: sqlite3.Connection, agent: AgentRecord, task: dict[str, Any]) -> ReceiptState:
    if task["type"] != "dynamicToolCall":
        return ReceiptState()
    aliases = tuple(row[0] for row in db.execute(
        "SELECT request FROM runtime_tool_request_aliases WHERE agent=? AND alias=? "
        "ORDER BY request LIMIT 2", (agent["id"], task["itemId"])))
    if len(aliases) > 1:
        return ReceiptState(aliases)
    key = aliases[0] if aliases else _prefix(agent.get("accountKey", "default"), agent["threadId"]) + task["itemId"]
    row = db.execute("SELECT id,record FROM runtime_tool_requests WHERE id=?", (key,)).fetchone()
    return ReceiptState(aliases, ((row[0], row[1]),) if row else ())


def _terminal_receipt(receipts: ReceiptState, agent: AgentRecord, task: dict[str, Any]) -> dict[str, Any] | None:
    if len(receipts.aliases) > 1 or len(receipts.rows) != 1:
        return None
    key, raw = receipts.rows[0]
    candidate = cast(dict[str, Any], json.loads(raw))
    if (candidate.get("id") != key or candidate.get("agent") != agent["id"]
            or candidate.get("accountKey") != agent.get("accountKey", "default")
            or candidate.get("threadId") != agent["threadId"]
            or candidate.get("turnId") != task["turnId"] or candidate.get("callId") != task["itemId"]
            or candidate.get("finished") is None):
        return None
    if ((candidate.get("stage") == "completed" and candidate.get("outcome") == "applied")
            or (candidate.get("stage") in {"completed", "failed"}
                and candidate.get("outcome") == "not_applied")):
        return candidate
    return None


def _batch(db: sqlite3.Connection, cursor: Position | None) -> list[sqlite3.Row]:
    # The status/created index includes rowid as its final stable ordering key.
    # Runtime.put uses an upsert, so output updates preserve that key.
    query = ("SELECT rowid,record,json_extract(record,'$.created') FROM runtime_tasks "
             "WHERE json_extract(record,'$.status')='running' "
             "AND json_extract(record,'$.created') IS NOT NULL ")
    order = "ORDER BY json_extract(record,'$.created'),rowid LIMIT ?"
    if cursor is None:
        return list(db.execute(query + order, (BATCH_SIZE,)))
    created, rowid = cursor
    rows = list(db.execute(query + "AND json_extract(record,'$.created')>=? "
        "AND (json_extract(record,'$.created')>? OR rowid>?) " + order,
        (created, created, rowid, BATCH_SIZE)))
    if len(rows) < BATCH_SIZE:
        rows.extend(db.execute(query + "AND json_extract(record,'$.created')<=? "
            "AND (json_extract(record,'$.created')<? OR rowid<=?) " + order,
            (created, created, rowid, BATCH_SIZE - len(rows))))
    return rows


def queue_task_recovery(rt: Runtime) -> bool:
    """Schedule one bounded history read; never wait for native input or output."""
    with rt.lock:
        state = cast(RecoveryState, rt.__dict__.setdefault("_task_recovery", RecoveryState()))
        now = time.monotonic()
        if rt.closed or state.busy or now < state.next_scan:
            return False
        state.next_scan = now + SCAN_SECONDS
        jobs: list[TaskCheck] = []
        with rt.read_db() as db:
            rows = _batch(db, state.cursor)
            for row in rows:
                if type(row[2]) not in (int, float):
                    continue
                position: Position = (row[2], row[0])
                state.cursor = position
                task = cast(dict[str, Any], json.loads(row[1]))
                if (task.get("type") not in TASK_TYPES or task.get("status") != "running"
                        or not isinstance(task.get("agent"), str) or not task["agent"]
                        or not isinstance(task.get("itemId"), str) or not task["itemId"]
                        or not isinstance(task.get("turnId"), str) or not task["turnId"]
                        or task.get("id") != task["agent"] + ":" + task["itemId"]
                        or task.get("kind") != ("command" if task["type"] == "commandExecution" else "tool")):
                    continue
                try:
                    agent = rt.agent(task["agent"], db)
                except ValueError:
                    continue
                if not _eligible_owner(agent):
                    continue
                account = agent.get("accountKey", "default")
                connection = rt.agent_connection(agent)
                server = rt.server_for(account, connection)
                receipts = _receipt_state(db, agent, task)
                terminal_receipt = _terminal_receipt(receipts, agent, task)
                native_ready = (isinstance(connection, str) and bool(connection) and server is not None
                                and rt.connection_current(account, connection))
                if not receipts and not native_ready:
                    continue
                jobs.append(TaskCheck(task, row[1], position, _scope(agent), account,
                    cast(str, agent["threadId"]), connection, server, receipts,
                    terminal_receipt))
        if not jobs:
            return False
        state.busy = True
        try:
            rt.recovery_pool.submit(_run_task_recovery, rt, state, jobs)
        except RuntimeError:
            state.busy = False
            return False
        return True


def _receipt_proof(rt: Runtime, job: TaskCheck) -> dict[str, Any] | None:
    if job.completed_receipt is None:
        return None
    resolved = resolve_record(state_root(rt), job.completed_receipt)
    result = resolved.get("result")
    if not isinstance(result, dict) or type(result.get("success")) is not bool:
        return None
    if resolved.get("outcome") == "not_applied" and result["success"] is not False:
        return None
    return {"id": job.task["itemId"], "type": job.task["type"], "status": "completed",
            "success": result["success"], "contentItems": result.get("contentItems", [])}


def _native_proofs(job: TaskCheck, tasks: list[TaskCheck], deadline: float) -> dict[str, dict[str, Any]]:
    if job.server is None or job.connection is None:
        return {}
    wanted = {check.task["itemId"] for check in tasks}
    matches: dict[str, list[dict[str, Any]]] = {}
    entries = _native_items(job.server, job.thread, job.task["turnId"], deadline)
    for entry in entries:
        if time.monotonic() >= deadline:
            raise TimeoutError("Native task history read timed out")
        if (not isinstance(entry, dict) or entry.get("turnId") != job.task["turnId"]
                or entry.get("threadId", job.thread) != job.thread):
            raise ValueError("Native task history changed the source identity")
        item = entry.get("item")
        if (not isinstance(item, dict) or not isinstance(item.get("id"), str)
                or item.get("turnId", job.task["turnId"]) != job.task["turnId"]
                or item.get("threadId", job.thread) != job.thread):
            raise ValueError("Native task history changed the item identity")
        if item["id"] in wanted:
            matches.setdefault(item["id"], []).append(item)
    proofs: dict[str, dict[str, Any]] = {}
    for check in tasks:
        candidates = matches.get(check.task["itemId"], [])
        if len(candidates) != 1:
            continue
        item = candidates[0]
        terminal = {"completed", "failed", "declined"} if check.task["type"] in {
            "commandExecution", "fileChange"} else {"completed", "failed"}
        if item.get("type") == check.task["type"] and item.get("status") in terminal:
            proofs[check.task["id"]] = item
    return proofs


def _apply_proofs(rt: Runtime, jobs: list[TaskCheck], proofs: dict[str, dict[str, Any]]) -> int:
    if not proofs:
        return 0
    applied = 0
    with rt.lock, rt.db() as db:
        if rt.closed:
            return 0
        for job in jobs:
            proof = proofs.get(job.task["id"])
            if proof is None:
                continue
            try:
                agent = rt.agent(job.task["agent"], db)
            except ValueError:
                continue
            if not _eligible_owner(agent) or _scope(agent) != job.source:
                continue
            if not job.receipts and (rt.agent_connection(agent) != job.connection
                    or rt.server_for(job.account, job.connection) is not job.server
                    or not rt.connection_current(job.account, job.connection)):
                continue
            row = db.execute("SELECT record FROM runtime_tasks WHERE id=?", (job.task["id"],)).fetchone()
            if not row or row[0] != job.raw or _receipt_state(db, agent, job.task) != job.receipts:
                continue
            rt.record_task(db, agent, "item/completed",
                           {"item": proof, "turnId": job.task["turnId"]}, stale=True)
            applied += 1
    return applied


def _run_task_recovery(rt: Runtime, state: RecoveryState, jobs: list[TaskCheck]) -> None:
    proofs: dict[str, dict[str, Any]] = {}
    groups: dict[tuple[object, ...], list[TaskCheck]] = {}
    errors: list[str] = []
    applied = 0
    exhausted_cursor: Position | None = None
    last_attempted: Position | None = None
    try:
        for job in jobs:
            # A local operation still in progress or with an unknown result
            # cannot be retired by a native response delivery error.
            if job.receipts:
                try:
                    proof = _receipt_proof(rt, job)
                    if proof is not None:
                        proofs[job.task["id"]] = proof
                except (OSError, ValueError, KeyError, TypeError) as error:
                    errors.append(str(error))
                continue
            key = (id(job.server), job.account, job.connection, job.thread, job.task["turnId"])
            groups.setdefault(key, []).append(job)
        deadline = time.monotonic() + READ_SECONDS
        for group in groups.values():
            if rt.closed:
                return
            if time.monotonic() >= deadline:
                # Continue after the last attempted group's first task. A slow
                # account must not permanently hide later tasks in this batch.
                exhausted_cursor = last_attempted
                break
            last_attempted = group[0].position
            try:
                proofs.update(_native_proofs(group[0], group, deadline))
            except Exception as error:
                errors.append(str(error))
        applied = _apply_proofs(rt, jobs, proofs)
    finally:
        with rt.lock:
            if exhausted_cursor is not None:
                state.cursor = exhausted_cursor
            state.last = {"at": time.time(), "checked": len(jobs), "reconciled": applied,
                          "errors": errors[:8]}
            state.busy = False
