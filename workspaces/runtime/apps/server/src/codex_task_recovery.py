"""Recover exact terminal task results without starting or replaying work."""

from __future__ import annotations

from dataclasses import dataclass, field
import json
import sqlite3
import threading
import time
from typing import TYPE_CHECKING, Any, Callable, Protocol, cast

from codex_context_repair import _native_items
from codex_native_errors import NativeRpcError
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
    def after_events(self, callback: Callable[[], None]) -> None: ...


class _CheckedHistory:
    def __init__(self, server: HistoryServer):
        self.server = server

    def call(self, method: str, params: dict[str, Any], timeout: float) -> dict[str, Any]:
        page = self.server.call(method, params, timeout=timeout)
        cursor = page.get("nextCursor")
        if (not isinstance(page.get("data"), list)
                or (cursor is not None and (not isinstance(cursor, str) or not cursor))):
            raise ValueError("Native task item page is unavailable")
        return page


@dataclass(frozen=True)
class CommandAbsence:
    source: tuple[object, ...]


TaskProof = dict[str, Any] | CommandAbsence


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
    absence_source: tuple[object, ...] | None = None


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


def _absence_source(rt: Runtime, db: sqlite3.Connection, agent: AgentRecord,
                    task: dict[str, Any]) -> tuple[object, ...] | None:
    if (task.get("type") != "commandExecution" or type(task.get("processId")) not in (str, int)
            or not str(task["processId"])
            or agent.get("inFlight") or agent.get("turnId") or agent.get("activeTools")
            or agent.get("status") in {"running", "starting", "approval", "queued"}
            or agent.get("accountKey", "default") in getattr(rt, "_native_runtime_reservations", {})
            or agent.get("accountKey", "default") in getattr(rt, "_native_tools_refreshing", set())):
        return None
    preparation = rt.preparations.get(agent["id"])
    if preparation and not preparation["future"].done():
        return None
    # A copied history does not prove that a session on its old account ended.
    for move in agent.get("accountHistory") or []:
        at = move.get("at")
        if type(at) not in (int, float):
            return None
        if (at >= task["created"] and
                (move.get("accountKey") != agent.get("accountKey", "default")
                 or move.get("threadId") != agent.get("threadId"))):
            return None
    if not db.execute("SELECT 1 FROM runtime_completed_turns WHERE id=?",
                      (agent["id"] + ":" + task["turnId"],)).fetchone():
        return None
    return (*_scope(agent), agent.get("status"), agent.get("turnId"), agent.get("inFlight"))


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
                    terminal_receipt, _absence_source(rt, db, agent, task)))
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


def _event_barrier(server: HistoryServer, deadline: float) -> None:
    barrier = threading.Event()
    server.after_events(barrier.set)
    if not barrier.wait(max(0, deadline - time.monotonic())):
        raise TimeoutError("Native task event barrier timed out")
    for name in ("callbacks", "clock_replies"):
        queue = getattr(server, name, None)
        if queue is not None:
            with queue.all_tasks_done:
                if not queue.all_tasks_done.wait_for(lambda: not queue.unfinished_tasks,
                        timeout=max(0, deadline - time.monotonic())):
                    raise TimeoutError("Native task events remain pending")


def _quiet(server: HistoryServer | None) -> bool:
    pending = getattr(server, "pending", None)
    if not isinstance(pending, dict) or pending:
        return False
    for name in ("callbacks", "clock_replies"):
        queue = getattr(server, name, None)
        if queue is not None:
            with queue.mutex:
                if queue.unfinished_tasks:
                    return False
    return True


def _absent_commands(job: TaskCheck, tasks: list[TaskCheck], deadline: float) -> dict[str, TaskProof]:
    server = job.server
    if server is None or not tasks or not _quiet(server):
        return {}
    def read(method: str, params: dict[str, Any]) -> dict[str, Any]:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Native command absence check timed out")
        return server.call(method, params, timeout=remaining)
    native = read("thread/read", {"threadId": job.thread, "includeTurns": False}).get("thread")
    if (not isinstance(native, dict) or native.get("id") != job.thread
            or native.get("status", {}).get("type") not in {"idle", "notLoaded"}):
        return {}
    live_items: set[str] = set()
    live_processes: set[str] = set()
    cursor = None
    seen: set[str] = set()
    while True:
        params = {"threadId": job.thread, "limit": 100}
        if cursor is not None:
            params["cursor"] = cursor
        page = read("thread/backgroundTerminals/list", params)
        if not isinstance(page.get("data"), list):
            raise ValueError("Native command terminal list is unavailable")
        for terminal in page["data"]:
            if (not isinstance(terminal, dict) or not isinstance(terminal.get("itemId"), str)
                    or not terminal["itemId"] or not isinstance(terminal.get("processId"), str)
                    or not terminal["processId"] or terminal.get("threadId", job.thread) != job.thread):
                raise ValueError("Native command terminal identity is unavailable")
            live_items.add(terminal["itemId"])
            live_processes.add(terminal["processId"])
        cursor = page.get("nextCursor")
        if cursor is None:
            break
        if not isinstance(cursor, str) or not cursor or cursor in seen:
            raise ValueError("Native command terminal pagination is invalid")
        seen.add(cursor)
    _event_barrier(server, deadline)
    if not _quiet(server):
        return {}
    return {check.task["id"]: CommandAbsence(check.absence_source) for check in tasks
            if check.absence_source is not None and check.task["itemId"] not in live_items
            and str(check.task["processId"]) not in live_processes}


def _native_proofs(job: TaskCheck, tasks: list[TaskCheck], deadline: float,
                   errors: list[str]) -> dict[str, TaskProof]:
    if job.server is None or job.connection is None:
        return {}
    wanted = {check.task["itemId"] for check in tasks}
    matches: dict[str, list[dict[str, Any]]] = {}
    if any(check.task["type"] == "commandExecution" for check in tasks):
        _event_barrier(job.server, deadline)
    try:
        entries = _native_items(_CheckedHistory(job.server), job.thread, job.task["turnId"], deadline)
    except Exception as error:
        cause = error.__cause__ or error
        if not isinstance(cause, NativeRpcError) or cause.code != -32601:
            raise
        # An explicit unsupported method permits the idle/terminal check.
        # A timeout or unreadable history is never evidence of absence.
        entries = []
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
    proofs: dict[str, TaskProof] = {}
    absent_candidates: list[TaskCheck] = []
    for check in tasks:
        candidates = matches.get(check.task["itemId"], [])
        if (check.absence_source is not None and (not candidates or (len(candidates) == 1
                and candidates[0].get("type") == "commandExecution"
                and candidates[0].get("status") == "inProgress"
                and str(candidates[0].get("processId", check.task["processId"])) == str(check.task["processId"])))):
            absent_candidates.append(check)
        if len(candidates) != 1:
            continue
        item = candidates[0]
        terminal = {"completed", "failed", "declined"} if check.task["type"] in {
            "commandExecution", "fileChange"} else {"completed", "failed"}
        if item.get("type") == check.task["type"] and item.get("status") in terminal:
            proofs[check.task["id"]] = item
    try:
        proofs.update(_absent_commands(job, absent_candidates, deadline))
    except Exception as error:
        # An unavailable terminal list cannot erase another item's exact result.
        errors.append(str(error))
    return proofs


def _apply_proofs(rt: Runtime, jobs: list[TaskCheck], proofs: dict[str, TaskProof]) -> int:
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
            if isinstance(proof, CommandAbsence):
                if _absence_source(rt, db, agent, job.task) != proof.source or not _quiet(job.server):
                    continue
                task = dict(job.task)
                task.update(status="lost", finished=time.time(),
                            error="Native command session is absent. Exit outcome unknown; command was not replayed.")
                rt.put(db, "tasks", task)
                db.execute("UPDATE runtime_items SET record=json_set(record,'$.toolStatus','lost') "
                           "WHERE id=? AND agent=? AND json_extract(record,'$.turnId')=?",
                           (task["id"], agent["id"], task["turnId"]))
                rt.touch_ui(agent["id"], db)
                applied += 1
                continue
            rt.record_task(db, agent, "item/completed",
                           {"item": proof, "turnId": job.task["turnId"]}, stale=True)
            applied += 1
    return applied


def _run_task_recovery(rt: Runtime, state: RecoveryState, jobs: list[TaskCheck]) -> None:
    proofs: dict[str, TaskProof] = {}
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
                proofs.update(_native_proofs(group[0], group, deadline, errors))
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
