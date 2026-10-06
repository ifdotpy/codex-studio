"""Release idle Codex subscriptions without claiming native session shutdown."""

import threading
import time
import uuid
from datetime import datetime
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import sqlite3
    from codex_records import AgentRecord, NativeReleaseRecord
    from codex_runtime import Runtime


IDLE_SECONDS = 15 * 60
SCAN_SECONDS = 30
MAX_PER_TICK = 2
MAX_SCAN_PER_TICK = 32
ACTIVE = {"queued", "starting", "running", "approval"}


def _idle_since(agent: "AgentRecord") -> float:
    value = agent.get("lastEvent")
    if value:
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
        except ValueError:
            return time.time()
    return agent.get("created", time.time())


def _local_blocker(rt: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord") -> str | None:
    from codex_workspace import active_task_records

    key = agent["id"]
    if (agent.get("inFlight") or agent.get("turnId") or agent["status"] in ACTIVE
            or agent.get("activeTools") or agent.get("workspaceOperation")
            or agent.get("accountTransferId")):
        return "active turn or workspace operation"
    preparation = rt.preparations.get(key)
    if preparation and not preparation["future"].done():
        return "thread preparation"
    if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND "
                  "(status IN ('reserved','dispatching','uncertain') OR "
                  "(status='pending' AND epoch=?)) LIMIT 1",
                  (key, agent["epoch"])).fetchone():
        return "pending or unknown input"
    if active_task_records(db, ("running", "starting", "pending", "unknown"), agent=key):
        return "active native task"
    if db.execute("SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
                  "AND json_extract(record,'$.status') IN ('pending','answering') LIMIT 1",
                  (key,)).fetchone():
        return "pending request"
    # Assigned work and Studio command monitors survive native unsubscribe.
    # Native tasks above can continue after their model turn ends.
    if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                  "AND json_extract(record,'$.stage') IN ('queued','running') LIMIT 1",
                  (key,)).fetchone():
        return "active tool request"
    return None


def _same(rt: "Runtime", agent: "AgentRecord", identity: tuple[Any, ...]) -> bool:
    return (agent["epoch"], agent.get("threadId"), agent.get("accountKey", "default"),
            rt.connection_ids.get(agent.get("accountKey", "default"))) == identity


def _mark(rt: "Runtime", agent_id: str, identity: tuple[Any, ...], phase: str,
          **details: Any) -> bool:
    release_id = details.pop("_release_id", None)
    inspection_only = details.pop("_inspection_only", False)
    with rt.lock, rt.db() as db:
        agent = rt.agent(agent_id, db)
        if not _same(rt, agent, identity):
            return False
        release = agent.get("nativeRelease") or {}
        if release_id is not None and release.get("id") != release_id:
            return False
        if inspection_only and (release.get("submittedAt") or release.get("phase") != "checking"):
            return False
        release.update(phase=phase, **details)  # type: ignore[call-arg]  # typed-update
        agent["nativeRelease"] = release
        rt.put(db, "agents", agent)
        if phase == "released":
            rt.loaded.discard(agent_id)
        return True


def _actor_scope(rt: "Runtime", actor: "AgentRecord") -> dict[str, Any]:
    account = actor.get("accountKey", "default")
    return {"id": actor["id"], "epoch": actor["epoch"], "rootId": actor["rootId"],
            "threadId": actor.get("threadId"), "accountKey": account,
            "connectionId": rt.connection_ids.get(account)}


def _retire_unsubmitted_inspection(rt: "Runtime", agent: "AgentRecord", phase: str,
                                  now: float) -> bool:
    """Retire a failed read inspection without claiming native closure."""
    saved = agent.get("nativeRelease") or {}
    account = agent.get("accountKey", "default")
    identity = (agent["epoch"], agent.get("threadId"), account, rt.connection_ids.get(account))
    began = saved.get("at")
    superseded = saved.get("phase") is None and saved.get("inspectionPhase") == "superseded"
    if (phase not in {"superseded", "resumed"} or rt.closed or agent.get("deletedAt")
            or agent.get("provider", "codex") != "codex" or not agent.get("autoWake")
            or rt.servers.get(account) is None
            or (saved.get("phase") != "blocked" and not (phase == "resumed" and superseded))
            or saved.get("resetPending") is not False
            or "submittedAt" in saved or "closedAt" in saved
            or not isinstance(saved.get("id"), str) or not saved["id"]
            or any(name not in saved for name in ("targetEpoch", "targetRootId", "targetParentId",
                                                  "threadId", "accountKey", "connectionId"))
            or type(saved.get("targetEpoch")) is not int
            or not all(isinstance(value, str) and value for value in identity[1:])
            or (saved.get("targetEpoch"), saved.get("threadId"), saved.get("accountKey"),
                saved.get("connectionId")) != identity
            or saved.get("targetRootId") != agent.get("rootId")
            or saved.get("targetParentId") != agent.get("parentId")
            or not isinstance(began, (int, float)) or isinstance(began, bool)
            or not 0 <= began <= now):
        return False
    if saved.get("error") is not None:
        saved.setdefault("inspectionError", saved["error"])
    saved.update(phase="resumed" if phase == "resumed" else None, error=None, resetPending=False)  # type: ignore[call-arg]  # typed-update
    if phase == "superseded":
        saved["inspectionPhase"] = "superseded"
    saved["supersededAt" if phase == "superseded" else "resumedAt"] = now
    return True


def _inspection_current(rt: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord",
                        release: "NativeReleaseRecord") -> tuple[Any, ...]:
    saved = agent.get("nativeRelease") or {}
    identity = (release["targetEpoch"], release["threadId"], release["accountKey"],
                release["connectionId"])
    if (rt.closed or saved.get("id") != release["id"] or saved.get("submittedAt")
            or saved.get("phase") != "checking"
            or not _same(rt, agent, identity)
            or agent.get("rootId") != release["targetRootId"]
            or agent.get("parentId") != release["targetParentId"]):
        raise ValueError("agent changed during native inspection")
    if release.get("resetBy") is not None:
        from codex_agent_management import _authorize
        actor = _authorize(rt, db, release["resetBy"], release["resetActorEpoch"], agent)
        if _actor_scope(rt, actor) != release["resetActorScope"]:
            raise ValueError("reset owner changed during native inspection")
    return identity


def _cancel_inspection(rt: "Runtime", agent_id: str, release: "NativeReleaseRecord",
                       error: BaseException | str) -> None:
    # Revoke only this request's unsubmitted inspection, even after Stop.
    with rt.lock, rt.db() as db:
        agent = rt.agent(agent_id, db)
        saved = agent.get("nativeRelease") or {}
        if (saved.get("id") == release["id"] and not saved.get("submittedAt")
                and saved.get("phase") == "checking"):
            saved.update(phase="blocked", resetPending=False, error=str(error)[:300])  # type: ignore[call-arg]  # typed-update
            rt.put(db, "agents", agent)


def _release_checked(rt: "Runtime", agent_id: str, identity: tuple[Any, ...], server: Any,
                     release: "NativeReleaseRecord") -> dict[str, Any]:
    from codex_runtime import ResponseTimeout

    try:
        native = server.call("thread/read", {"threadId": identity[1], "includeTurns": False}, timeout=10)["thread"]
        native_status = native.get("status", {}).get("type")
        if native.get("id") != identity[1] or native_status not in {"idle", "notLoaded"}:
            raise ValueError("native thread is active or its status is unknown")
        if native_status == "notLoaded":
            if _mark(rt, agent_id, identity, "released", _release_id=release["id"],
                     _inspection_only=True,
                     nativeStatus="notLoaded", releasedAt=time.time(), closedAt=time.time(),
                     resetPending=False, error=None):
                return {"status": "released", "nativeStatus": "notLoaded",
                        "note": "Native session closure is confirmed."}
            _cancel_inspection(rt, agent_id, release, "agent changed after native inspection")
            return {"status": "unknown", "reason": "agent changed after native inspection"}
        terminals = server.call("thread/backgroundTerminals/list",
                                {"threadId": identity[1], "limit": 1}, timeout=10)
        if not isinstance(terminals.get("data"), list) or terminals["data"] or terminals.get("nextCursor"):
            raise ValueError("native background command is active")
        queued = server.call("thread/queue/list", {"threadId": identity[1]}, timeout=10)
        if not isinstance(queued.get("data"), list) or queued["data"] or queued.get("nextCursor"):
            raise ValueError("native input is pending")
    except Exception as error:
        if release.get("resetPending") and isinstance(error, (ResponseTimeout, TimeoutError)):
            failures = int(release.get("inspectionFailures", 0)) + 1
            delay = min(300, SCAN_SECONDS * 2 ** min(failures - 1, 4))
            if _mark(rt, agent_id, identity, "checking", _release_id=release["id"],
                     _inspection_only=True,
                     error=str(error)[:300], resetPending=True, inspectionFailures=failures,
                     inspectionPhase="inspection_wait", nextAttemptAt=time.time() + delay):
                return {"status": "waiting", "reason": str(error)[:300]}
        _cancel_inspection(rt, agent_id, release, error)
        return {"status": "blocked", "reason": str(error)[:300]}
    try:
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            _inspection_current(rt, db, agent, release)
            blocker = _local_blocker(rt, db, agent)
            if blocker:
                raise ValueError(blocker)
            agent["nativeRelease"].update(phase="unsubscribing", submittedAt=time.time())  # type: ignore[call-arg]  # typed-update
            agent["nativeRelease"].pop("inspectionPhase", None)
            rt.put(db, "agents", agent)
    except ValueError as error:
        _cancel_inspection(rt, agent_id, release, error)
        return {"status": "blocked", "reason": str(error)[:300]}
    try:
        result = server.call("thread/unsubscribe", {"threadId": identity[1]}, timeout=10)
        if result.get("status") not in {"unsubscribed", "notSubscribed", "notLoaded"}:
            raise RuntimeError("native unsubscribe response is not confirmed")
    except Exception as error:
        _mark(rt, agent_id, identity, "unknown", _release_id=release["id"], error=str(error)[:300])
        return {"status": "unknown", "reason": str(error)[:300]}
    if not _mark(rt, agent_id, identity, "released", _release_id=release["id"],
                 releasedAt=time.time(), nativeStatus=result["status"], error=None,
                 resetPending=bool(release.get("resetPending") and result["status"] != "notLoaded"),
                 **({"closedAt": time.time()} if result["status"] == "notLoaded" else {})):
        return {"status": "unknown", "reason": "agent changed after unsubscribe"}
    return {"status": "released", "nativeStatus": result["status"],
            "note": ("Native session closure is confirmed." if result["status"] == "notLoaded"
                     else "Codex can keep this idle session and its tool processes until its configured idle window ends (60 seconds by default).")}


def release_agent(rt: "Runtime", agent_id: str, *, reason: str | None = None,
                  actor_id: str | None = None, actor_epoch: int | None = None,
                  now: float | None = None) -> dict[str, Any]:
    """Check both Studio and native work, then unsubscribe on the owning connection."""
    now = time.time() if now is None else now
    with rt.lock:
        guard = rt.prepare_locks.setdefault(agent_id, threading.Lock())
    if not guard.acquire(blocking=False):
        return {"status": "blocked", "reason": "thread preparation is active"}
    try:
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            actor = None
            if actor_id is not None:
                from codex_agent_management import _authorize
                actor = _authorize(rt, db, actor_id, actor_epoch, agent)
            previous = agent.get("nativeRelease") or {}
            if previous.get("phase") in {"unknown", "unsubscribing"}:
                return {"status": "unknown", "reason": previous.get("error") or "native release acknowledgement pending"}
            account = agent.get("accountKey", "default")
            identity = (agent["epoch"], agent.get("threadId"), account,
                        rt.connection_ids.get(account))
            already_released = (reason is not None and agent_id not in rt.loaded
                                and (agent.get("nativeRelease") or {}).get("phase") == "released"
                                and (agent.get("nativeRelease") or {}).get("threadId") == identity[1]
                                and not previous.get("closedAt"))
            if (agent.get("provider", "codex") != "codex" or agent.get("deletedAt")
                    or (agent_id not in rt.loaded and not already_released)
                    or not identity[1] or not identity[3]):
                closed_at, began = previous.get("closedAt"), previous.get("at")
                if (reason is not None and not rt.closed and agent_id not in rt.loaded
                        and agent.get("provider", "codex") == "codex" and not agent.get("deletedAt")
                        and identity[1] and identity[3] and previous.get("phase") == "blocked"
                        and isinstance(previous.get("id"), str) and previous["id"]
                        and "submittedAt" not in previous and previous.get("resetPending") is False
                        and (previous.get("targetEpoch"), previous.get("threadId"),
                             previous.get("accountKey"), previous.get("connectionId")) == identity
                        and previous.get("targetRootId") == agent.get("rootId")
                        and previous.get("targetParentId") == agent.get("parentId")
                        and isinstance(closed_at, (int, float)) and not isinstance(closed_at, bool)
                        and isinstance(began, (int, float)) and not isinstance(began, bool)
                        and 0 <= began <= closed_at <= now and not _local_blocker(rt, db, agent)):
                    # The current native close settles only this unsubmitted
                    # inspection. It does not settle a command or input receipt.
                    if previous.get("error") is not None:
                        previous.setdefault("inspectionError", previous["error"])
                    previous.update(phase="released", nativeStatus="notLoaded", releasedAt=closed_at,
                                    resetPending=False, error=None)  # type: ignore[call-arg]  # typed-update
                    rt.put(db, "agents", agent)
                elif (reason is not None and agent_id not in rt.loaded
                        and not _local_blocker(rt, db, agent)
                        and _retire_unsubmitted_inspection(rt, agent, "superseded", now)):
                    rt.put(db, "agents", agent)
                return {"status": "not_loaded"}
            blocker = _local_blocker(rt, db, agent)
            if blocker:
                return {"status": "blocked", "reason": blocker}
            if reason is None and now - _idle_since(agent) < IDLE_SECONDS:
                return {"status": "not_due"}
            server = rt.servers.get(account)
            if server is None:
                return {"status": "blocked", "reason": "owning account is offline"}
            release: "NativeReleaseRecord" = {"id": uuid.uuid4().hex, "phase": "checking",
                       "threadId": identity[1], "accountKey": account, "connectionId": identity[3], "at": now,
                       "targetEpoch": identity[0], "targetRootId": agent.get("rootId"),
                       "targetParentId": agent.get("parentId")}
            if reason is not None:
                release.update(resetReason=reason, resetBy=actor_id, resetPending=True,  # type: ignore[call-arg]  # typed-update
                               inspectionPhase="inspecting",
                               resetActorEpoch=actor["epoch"] if actor else None,
                               resetActorScope=_actor_scope(rt, actor) if actor else None)
            agent["nativeRelease"] = release
            rt.put(db, "agents", agent)
        return _release_checked(rt, agent_id, identity, server, release)
    finally:
        guard.release()


def reconcile_unknown(rt: "Runtime", agent: "AgentRecord") -> None:
    """Settle captured resets from closure proof; retain legacy unsubscribe retries."""
    release = agent.get("nativeRelease") or {}
    if release.get("phase") not in {"unknown", "checking", "unsubscribing"}:
        return
    if "resetActorEpoch" in release:
        from codex_runtime import ResponseTimeout
        with rt.lock, rt.db() as db:
            current = rt.agent(agent["id"], db)
            saved = current.get("nativeRelease") or {}
            if (isinstance(release.get("id"), str) and release["id"]
                    and saved.get("id") == release["id"]):
                if saved.get("phase") in {"released", "resumed"}:
                    return
                identity = (saved.get("targetEpoch"), saved.get("threadId"),
                            saved.get("accountKey"), saved.get("connectionId"))
                closed_at = saved.get("closedAt")
                if (not rt.closed and _same(rt, current, identity)
                        and current.get("rootId") == saved.get("targetRootId")
                        and current.get("parentId") == saved.get("targetParentId")
                        and not saved.get("resetPending")
                        and isinstance(closed_at, (int, float)) and not isinstance(closed_at, bool)
                        and closed_at >= max(saved.get("at", 0), saved.get("submittedAt", 0))):
                    saved.update(phase="released", nativeStatus="notLoaded", releasedAt=closed_at, error=None)  # type: ignore[call-arg]  # typed-update
                    rt.put(db, "agents", current)
                    rt.loaded.discard(agent["id"])
                    return
        raise ResponseTimeout("Native release outcome remains unknown; wait for exact native closure")
    if release.get("phase") == "checking" and release.get("inspectionPhase"):
        return
    account = agent.get("accountKey", "default")
    identity = (agent["epoch"], agent.get("threadId"), account, rt.connection_ids.get(account))
    if (release.get("threadId"), release.get("connectionId")) != (identity[1], identity[3]):
        return
    server = rt.servers[account]
    result = server.call("thread/unsubscribe", {"threadId": identity[1]}, timeout=10)
    if result.get("status") not in {"unsubscribed", "notSubscribed", "notLoaded"}:
        raise ValueError("Native release outcome remains unknown")
    _mark(rt, agent["id"], identity, "released", releasedAt=time.time(),
          nativeStatus=result["status"], error=None)


def _probe_reset(rt: "Runtime", agent_id: str) -> None:
    with rt.lock, rt.db() as db:
        agent = rt.agent(agent_id, db)
        release = agent.get("nativeRelease") or {}
        account = agent.get("accountKey", "default")
        identity = (agent["epoch"], agent.get("threadId"), account,
                    rt.connection_ids.get(account))
        server = rt.servers.get(account)
        if (not release.get("resetPending") or release.get("phase") != "released"
                or release.get("threadId") != identity[1] or server is None):
            return
    try:
        native = server.call("thread/read", {"threadId": identity[1], "includeTurns": False}, timeout=10)["thread"]
    except Exception:
        return
    if native.get("id") == identity[1] and native.get("status", {}).get("type") == "notLoaded":
        if not _mark(rt, agent_id, identity, "released", resetPending=False, closedAt=time.time()):
            with rt.lock, rt.db() as db:
                current = rt.agent(agent_id, db)
                saved = current.get("nativeRelease") or {}
                if (current["epoch"], current.get("threadId"), current.get("accountKey", "default")) == identity[:3] and saved.get("resetPending"):
                    saved.update(resetPending=False, closedAt=time.time(), nativeStatus="notLoaded")  # type: ignore[call-arg]  # typed-update
                    rt.put(db, "agents", current)
        rt.changed.set()


def _retry_reset(rt: "Runtime", agent_id: str) -> None:
    with rt.lock:
        guard = rt.prepare_locks.setdefault(agent_id, threading.Lock())
    if not guard.acquire(blocking=False):
        return
    try:
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            release = agent.get("nativeRelease") or {}
            if not release.get("resetPending"):
                return
            if "resetActorEpoch" in release and release.get("submittedAt"):
                return
            inspection = (release.get("phase") == "checking" and
                          release.get("inspectionPhase") in {"inspecting", "inspection_wait"})
            if inspection:
                if release.get("nextAttemptAt", 0) > time.time():
                    return
                try:
                    identity = _inspection_current(rt, db, agent, release)
                    blocker = _local_blocker(rt, db, agent)
                    if blocker:
                        raise ValueError(blocker)
                    server = rt.servers.get(identity[2])
                    if server is None or agent_id not in rt.loaded or agent.get("deletedAt"):
                        raise ValueError("owning native session is no longer available")
                except ValueError as error:
                    release.update(phase="blocked", resetPending=False, error=str(error)[:300])  # type: ignore[call-arg]  # typed-update
                    rt.put(db, "agents", agent)
                    return
            elif release.get("phase") not in {"unknown", "checking", "unsubscribing"}:
                return
        if inspection:
            _release_checked(rt, agent_id, identity, server, release)
            return
        try:
            reconcile_unknown(rt, agent)
        except Exception:
            return
    finally:
        guard.release()


def tick(rt: "Runtime", now: float | None = None) -> None:
    now = time.time() if now is None else now
    if now - getattr(rt, "_native_release_last_scan", 0) < SCAN_SECONDS:
        return
    rt._native_release_last_scan = now  # type: ignore[attr-defined]  # typed-narrowing: Module owns scan cursor state
    from codex_native_sweep import tick as native_sweep_tick
    native_sweep_tick(rt, now)
    with rt.lock, rt.db() as db:
        pending: set[str] = getattr(rt, "_native_release_pending", set())
        rt._native_release_pending = pending  # type: ignore[attr-defined]  # typed-narrowing: Module owns scan cursor state
        candidates = []
        reset_filter = "json_extract(record,'$.nativeRelease.resetPending')=1"
        reset_count = db.execute(f"SELECT COUNT(*) FROM runtime_agents WHERE {reset_filter}").fetchone()[0]
        reset_position = getattr(rt, "_native_reset_cursor", 0) % max(1, reset_count)
        reset_rows = db.execute(f"SELECT id FROM runtime_agents WHERE {reset_filter} ORDER BY id "
                                "LIMIT ? OFFSET ?", (MAX_PER_TICK, reset_position)).fetchall()
        if len(reset_rows) < min(MAX_PER_TICK, reset_count):
            reset_rows += db.execute(f"SELECT id FROM runtime_agents WHERE {reset_filter} ORDER BY id "
                                     "LIMIT ?", (MAX_PER_TICK - len(reset_rows),)).fetchall()
        rt._native_reset_cursor = reset_position + len(reset_rows)  # type: ignore[attr-defined]  # typed-narrowing: Module owns scan cursor state
        resets = [row[0] for row in reset_rows if row[0] not in pending]
        loaded = sorted(rt.loaded)
        position = getattr(rt, "_native_release_cursor", 0) % max(1, len(loaded))
        keys = [loaded[(position + offset) % len(loaded)]
                for offset in range(min(MAX_SCAN_PER_TICK, len(loaded)))]
        rt._native_release_cursor = position + len(keys)  # type: ignore[attr-defined]  # typed-narrowing: Module owns scan cursor state
        for key in keys:
            if key in pending:
                continue
            agent = rt.agent(key, db)
            if (agent.get("provider", "codex") == "codex" and not agent.get("deletedAt")
                    and agent.get("threadId") and now - _idle_since(agent) >= IDLE_SECONDS):
                if _local_blocker(rt, db, agent):
                    continue
                candidates.append((_idle_since(agent), key))
        selected = resets[:MAX_PER_TICK]
        selected += [key for _, key in sorted(candidates)[:MAX_PER_TICK - len(selected)]]
        pending.update(selected)
    for key in selected:
        try:
            if key in resets:
                release = rt.agent(key).get("nativeRelease") or {}
                future = rt.recovery_pool.submit(
                    _probe_reset if release.get("phase") == "released" else _retry_reset, rt, key)
            else:
                future = rt.recovery_pool.submit(release_agent, rt, key, now=now)  # type: ignore[arg-type]  # typed-narrowing: Callback result is intentionally discarded
        except RuntimeError:
            with rt.lock:
                pending.discard(key)
            continue
        def done(_future, agent_id=key):  # type: (Any, str) -> None
            with rt.lock:
                pending.discard(agent_id)
            rt.changed.set()
        future.add_done_callback(done)
