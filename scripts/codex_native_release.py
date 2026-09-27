"""Release idle Codex subscriptions without claiming native session shutdown."""

import threading
import time
from datetime import datetime


IDLE_SECONDS = 15 * 60
SCAN_SECONDS = 30
MAX_PER_TICK = 2
MAX_SCAN_PER_TICK = 32
ACTIVE = {"queued", "starting", "running", "approval"}


def _idle_since(agent):
    value = agent.get("lastEvent")
    if value:
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
        except ValueError:
            return time.time()
    return agent.get("created", time.time())


def _local_blocker(rt, db, agent):
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
    if db.execute("SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=? "
                  "AND json_extract(record,'$.status') IN ('running','starting','approval') LIMIT 1",
                  (key,)).fetchone():
        return "active command monitor"
    if active_task_records(db, ("running", "starting", "pending", "unknown"), agent=key):
        return "active or unknown task"
    if db.execute("SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
                  "AND json_extract(record,'$.status') IN ('pending','answering') LIMIT 1",
                  (key,)).fetchone():
        return "pending request"
    if db.execute("SELECT 1 FROM runtime_work WHERE json_extract(record,'$.owner')=? "
                  "AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') LIMIT 1",
                  (key,)).fetchone():
        return "active assigned work"
    if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                  "AND (json_extract(record,'$.stage') IN ('queued','running') "
                  "OR json_extract(record,'$.outcome')='unknown') LIMIT 1", (key,)).fetchone():
        return "active or unknown tool request"
    return None


def _same(rt, agent, identity):
    return (agent["epoch"], agent.get("threadId"), agent.get("accountKey", "default"),
            rt.connection_ids.get(agent.get("accountKey", "default"))) == identity


def _mark(rt, agent_id, identity, phase, **details):
    with rt.lock, rt.db() as db:
        agent = rt.agent(agent_id, db)
        if not _same(rt, agent, identity):
            return False
        release = agent.get("nativeRelease") or {}
        release.update(phase=phase, **details)
        agent["nativeRelease"] = release
        rt.put(db, "agents", agent)
        if phase == "released":
            rt.loaded.discard(agent_id)
        return True


def release_agent(rt, agent_id, *, reason=None, actor_id=None, actor_epoch=None, now=None):
    """Check both Studio and native work, then unsubscribe on the owning connection."""
    now = time.time() if now is None else now
    with rt.lock:
        guard = rt.prepare_locks.setdefault(agent_id, threading.Lock())
    if not guard.acquire(blocking=False):
        return {"status": "blocked", "reason": "thread preparation is active"}
    try:
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            if actor_id is not None:
                from codex_agent_management import _authorize
                _authorize(rt, db, actor_id, actor_epoch, agent)
            account = agent.get("accountKey", "default")
            identity = (agent["epoch"], agent.get("threadId"), account,
                        rt.connection_ids.get(account))
            already_released = (reason is not None and agent_id not in rt.loaded
                                and (agent.get("nativeRelease") or {}).get("phase") == "released"
                                and (agent.get("nativeRelease") or {}).get("threadId") == identity[1])
            if (agent.get("provider", "codex") != "codex" or agent.get("deletedAt")
                    or (agent_id not in rt.loaded and not already_released)
                    or not identity[1] or not identity[3]):
                return {"status": "not_loaded"}
            blocker = _local_blocker(rt, db, agent)
            if blocker:
                return {"status": "blocked", "reason": blocker}
            if reason is None and now - _idle_since(agent) < IDLE_SECONDS:
                return {"status": "not_due"}
            server = rt.servers.get(account)
            if server is None:
                return {"status": "blocked", "reason": "owning account is offline"}
            release = {"phase": "checking", "threadId": identity[1], "accountKey": account,
                       "connectionId": identity[3], "at": now}
            if reason is not None:
                release.update(resetReason=reason, resetBy=actor_id, resetPending=True)
            agent["nativeRelease"] = release
            rt.put(db, "agents", agent)
        try:
            native = server.call("thread/read", {"threadId": identity[1], "includeTurns": False}, timeout=10)["thread"]
            native_status = native.get("status", {}).get("type")
            if native.get("id") != identity[1] or native_status not in {"idle", "notLoaded"}:
                raise ValueError("native thread is active or its status is unknown")
            if native_status == "notLoaded":
                if _mark(rt, agent_id, identity, "released", nativeStatus="notLoaded",
                         releasedAt=time.time(), closedAt=time.time(), resetPending=False, error=None):
                    return {"status": "released", "nativeStatus": "notLoaded",
                            "note": "Native session closure is confirmed."}
                return {"status": "unknown", "reason": "agent changed after native inspection"}
            if native_status == "idle":
                terminals = server.call("thread/backgroundTerminals/list",
                                        {"threadId": identity[1], "limit": 1}, timeout=10)
                if not isinstance(terminals.get("data"), list) or terminals["data"] or terminals.get("nextCursor"):
                    raise ValueError("native background command is active")
            queued = server.call("thread/queue/list", {"threadId": identity[1]}, timeout=10)
            if not isinstance(queued.get("data"), list) or queued["data"] or queued.get("nextCursor"):
                raise ValueError("native input is pending")
        except Exception as error:
            _mark(rt, agent_id, identity, "blocked", error=str(error)[:300], resetPending=False)
            return {"status": "blocked", "reason": str(error)[:300]}
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            if actor_id is not None:
                from codex_agent_management import _authorize
                _authorize(rt, db, actor_id, actor_epoch, agent)
            blocker = _local_blocker(rt, db, agent)
            if not _same(rt, agent, identity) or blocker:
                return {"status": "blocked", "reason": blocker or "agent changed during native inspection"}
            agent["nativeRelease"].update(phase="unsubscribing", submittedAt=time.time())
            rt.put(db, "agents", agent)
        try:
            result = server.call("thread/unsubscribe", {"threadId": identity[1]}, timeout=10)
            if result.get("status") not in {"unsubscribed", "notSubscribed", "notLoaded"}:
                raise RuntimeError("native unsubscribe response is not confirmed")
        except Exception as error:
            _mark(rt, agent_id, identity, "unknown", error=str(error)[:300])
            return {"status": "unknown", "reason": str(error)[:300]}
        if not _mark(rt, agent_id, identity, "released", releasedAt=time.time(),
                     nativeStatus=result["status"], error=None,
                     resetPending=bool(reason is not None and result["status"] != "notLoaded"),
                     **({"closedAt": time.time()} if result["status"] == "notLoaded" else {})):
            return {"status": "unknown", "reason": "agent changed after unsubscribe"}
        return {"status": "released", "nativeStatus": result["status"],
                "note": ("Native session closure is confirmed." if result["status"] == "notLoaded"
                         else "Codex can keep this idle session and its tool processes until its configured idle window ends (60 seconds by default).")}
    finally:
        guard.release()


def reconcile_unknown(rt, agent):
    """Retry only the idempotent unsubscribe before a later thread/resume."""
    release = agent.get("nativeRelease") or {}
    if release.get("phase") not in {"unknown", "checking", "unsubscribing"}:
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


def _probe_reset(rt, agent_id):
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
                    saved.update(resetPending=False, closedAt=time.time(), nativeStatus="notLoaded")
                    rt.put(db, "agents", current)
        rt.changed.set()


def _retry_reset(rt, agent_id):
    with rt.lock:
        guard = rt.prepare_locks.setdefault(agent_id, threading.Lock())
    if not guard.acquire(blocking=False):
        return
    try:
        with rt.lock, rt.db() as db:
            agent = rt.agent(agent_id, db)
            release = agent.get("nativeRelease") or {}
            if not release.get("resetPending") or release.get("phase") not in {"unknown", "checking", "unsubscribing"}:
                return
        try:
            reconcile_unknown(rt, agent)
        except Exception:
            return
    finally:
        guard.release()


def tick(rt, now=None):
    now = time.time() if now is None else now
    if now - getattr(rt, "_native_release_last_scan", 0) < SCAN_SECONDS:
        return
    rt._native_release_last_scan = now
    with rt.lock, rt.db() as db:
        pending = getattr(rt, "_native_release_pending", set())
        rt._native_release_pending = pending
        candidates = []
        reset_filter = "json_extract(record,'$.nativeRelease.resetPending')=1"
        reset_count = db.execute(f"SELECT COUNT(*) FROM runtime_agents WHERE {reset_filter}").fetchone()[0]
        reset_position = getattr(rt, "_native_reset_cursor", 0) % max(1, reset_count)
        reset_rows = db.execute(f"SELECT id FROM runtime_agents WHERE {reset_filter} ORDER BY id "
                                "LIMIT ? OFFSET ?", (MAX_PER_TICK, reset_position)).fetchall()
        if len(reset_rows) < min(MAX_PER_TICK, reset_count):
            reset_rows += db.execute(f"SELECT id FROM runtime_agents WHERE {reset_filter} ORDER BY id "
                                     "LIMIT ?", (MAX_PER_TICK - len(reset_rows),)).fetchall()
        rt._native_reset_cursor = reset_position + len(reset_rows)
        resets = [row[0] for row in reset_rows if row[0] not in pending]
        loaded = sorted(rt.loaded)
        position = getattr(rt, "_native_release_cursor", 0) % max(1, len(loaded))
        keys = [loaded[(position + offset) % len(loaded)]
                for offset in range(min(MAX_SCAN_PER_TICK, len(loaded)))]
        rt._native_release_cursor = position + len(keys)
        for key in keys:
            if key in pending:
                continue
            agent = rt.agent(key, db)
            if (agent.get("provider", "codex") == "codex" and not agent.get("deletedAt")
                    and agent.get("threadId") and now - _idle_since(agent) >= IDLE_SECONDS
                    and not _local_blocker(rt, db, agent)):
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
                future = rt.recovery_pool.submit(release_agent, rt, key, now=now)
        except RuntimeError:
            with rt.lock:
                pending.discard(key)
            continue
        def done(_future, agent_id=key):
            with rt.lock:
                pending.discard(agent_id)
            rt.changed.set()
        future.add_done_callback(done)
