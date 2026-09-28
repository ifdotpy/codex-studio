"""Release idle native threads that no current Studio agent owns."""

import json
import time

from codex_native_release import IDLE_SECONDS, MAX_PER_TICK, _idle_since, _local_blocker


RECHECK_SECONDS = 90


def _ticket_id(ticket):
    return ticket[0] if isinstance(ticket, tuple) else f"fixture:{id(ticket)}"


def _ticket_future(ticket):
    return ticket[2] if isinstance(ticket, tuple) else ticket


def _receipt_id(account, connection, thread):
    return json.dumps([account, connection, thread], separators=(",", ":"))


def _receipt(db, key):
    row = db.execute("SELECT record FROM runtime_native_sweeps WHERE id=?", (key,)).fetchone()
    return json.loads(row[0]) if row else {}


def _save(db, key, **values):
    record = _receipt(db, key)
    record.update(values)
    db.execute("INSERT OR REPLACE INTO runtime_native_sweeps VALUES (?,?)",
               (key, json.dumps(record)))


def _account_busy(rt, db, account):
    # A fork can create a subscribed thread before Studio learns its ID.
    if (account in getattr(rt, "_native_tools_refreshing", set())
            or account in getattr(rt, "_native_runtime_reservations", {})):
        return True
    for agent in rt.records(db, "agents"):
        if agent.get("provider", "codex") != "codex":
            continue
        if agent.get("accountKey", "default") != account:
            continue
        repair = agent.get("contextRepair") or {}
        if repair.get("phase") in {"preparing", "submitted", "unknown", "ready"}:
            return True
        if agent.get("nativeToolRefreshId"):
            return True
        preparation = rt.preparations.get(agent["id"])
        if preparation and not preparation["future"].done():
            return True
    has_transfers = db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_account_transfers'").fetchone()
    for transfer in (rt.records(db, "account_transfers") if has_transfers else []):
        if transfer.get("status") != "pending":
            continue
        for member in transfer.get("members", {}).values():
            if (member.get("phase") in {"reading", "submitted", "unknown", "ready"}
                    and account in {member.get("sourceAccountKey"), transfer.get("targetAccountKey")}):
                return True
    return False


def _protected(rt, db, account, thread, now):
    if db.execute("SELECT 1 FROM runtime_agents WHERE "
                  "json_extract(record,'$.contextRepair.sourceCleanup.phase') IN ('planned','submitted') "
                  "AND json_extract(record,'$.contextRepair.sourceCleanup.accountKey')=? "
                  "AND json_extract(record,'$.contextRepair.sourceCleanup.threadId')=? LIMIT 1",
                  (account, thread)).fetchone():
        return True
    rows = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? "
                      "AND (json_extract(record,'$.accountKey')=? OR "
                      "(?='default' AND json_type(record,'$.accountKey') IS NULL))",
                      (thread, account, account)).fetchall()
    for row in rows:
        agent = json.loads(row[0])
        recent_at = max(_idle_since(agent), agent.get("deletedAt") or 0)
        if ((agent["id"] in rt.loaded and not agent.get("deletedAt"))
                or now - recent_at < IDLE_SECONDS
                or _local_blocker(rt, db, agent)):
            return True
        recovery = agent.get("browserRecovery") or {}
        if recovery.get("stage") in {"pending", "reconnecting"}:
            return True
        has_voice = db.execute("SELECT 1 FROM sqlite_master WHERE name='voice_sessions'").fetchone()
        if has_voice and db.execute("SELECT 1 FROM voice_sessions WHERE agent=? AND ended IS NULL LIMIT 1",
                                    (agent["id"],)).fetchone():
            return True
    return False


def _loaded(server):
    threads = []
    cursor = None
    seen = set()
    while True:
        params = {"limit": 100}
        if cursor:
            params["cursor"] = cursor
        page = server.call("thread/loaded/list", params, timeout=10)
        if not isinstance(page.get("data"), list) or any(
                not isinstance(tid, str) or not tid for tid in page["data"]):
            raise ValueError("Native loaded-thread list is invalid")
        threads.extend(page["data"])
        cursor = page.get("nextCursor")
        if not cursor:
            return list(dict.fromkeys(threads))
        if cursor in seen:
            raise ValueError("Native loaded-thread pagination repeated")
        seen.add(cursor)


def _native_idle(server, thread):
    native = server.call("thread/read", {"threadId": thread, "includeTurns": False}, timeout=10)["thread"]
    if native.get("id") != thread or native.get("status", {}).get("type") != "idle":
        return False
    terminals = server.call("thread/backgroundTerminals/list", {"threadId": thread, "limit": 1}, timeout=10)
    if not isinstance(terminals.get("data"), list) or terminals["data"] or terminals.get("nextCursor"):
        return False
    queued = server.call("thread/queue/list", {"threadId": thread}, timeout=10)
    return isinstance(queued.get("data"), list) and not queued["data"] and not queued.get("nextCursor")


def _release(rt, account, connection, server, thread, now):
    key = _receipt_id(account, connection, thread)
    try:
        if not _native_idle(server, thread):
            return False
    except Exception:
        return False
    with rt.lock, rt.db() as db:
        if (rt.closed or rt.servers.get(account) is not server
                or not rt.connection_current(account, connection)
                or _account_busy(rt, db, account) or _protected(rt, db, account, thread, time.time())):
            return False
        receipt = _receipt(db, key)
        if receipt.get("phase") == "submitted":
            return False
        # A prior unsubscribe can be awaiting Codex's idle unload window.
        phase = receipt.get("phase")
        wait = IDLE_SECONDS if phase == "notSubscribed" else RECHECK_SECONDS
        if phase in {"unsubscribed", "notSubscribed"} and now - receipt.get("checkedAt", 0) < wait:
            return False
        _save(db, key, phase="submitting", accountKey=account, connectionId=connection,
              threadId=thread, checkedAt=now)
        db.commit()
        try:
            ticket = rt.submit_reserved(server, "thread/unsubscribe", {"threadId": thread})
        except Exception as error:
            _save(db, key, phase="unknown", error=str(error)[:300])
            return True
        _save(db, key, phase="submitted", requestId=_ticket_id(ticket), submittedAt=time.time())
        rt.__dict__.setdefault("_native_sweep_tickets", {})[key] = ticket
    try:
        response = server.wait(ticket, timeout=10)
        status = response.get("status")
        if status not in {"unsubscribed", "notSubscribed", "notLoaded"}:
            raise ValueError("Native unsubscribe response is invalid")
        phase = status
        error = None
    except Exception as cause:
        phase = "unknown"
        error = str(cause)[:300]
    with rt.lock, rt.db() as db:
        if _ticket_future(ticket).done():
            rt.__dict__.setdefault("_native_sweep_tickets", {}).pop(key, None)
        if _receipt(db, key).get("requestId") == _ticket_id(ticket):
            _save(db, key, phase=phase, error=error, checkedAt=time.time())
    return True


def _settle_ticket(rt, db, key, ticket):
    try:
        result = _ticket_future(ticket).result()
        status = result.get("status")
        if status not in {"unsubscribed", "notSubscribed", "notLoaded"}:
            raise ValueError("Native unsubscribe response is invalid")
        error = None
    except Exception as cause:
        status = "unknown"
        error = str(cause)[:300]
    receipt = _receipt(db, key)
    if receipt.get("requestId") == _ticket_id(ticket) and receipt.get("phase") != "closed":
        _save(db, key, phase=status, error=error, checkedAt=time.time())
    rt.__dict__.setdefault("_native_sweep_tickets", {}).pop(key, None)


def sweep(rt, now=None):
    """Inspect existing account connections only; use at most two unsubscribes."""
    now = time.time() if now is None else now
    with rt.lock:
        accounts = sorted((key, server, rt.connection_ids.get(key))
                          for key, server in rt.servers.items()
                          if rt.accounts.get(key).get("provider", "codex") == "codex"
                          and rt.connection_current(key, rt.connection_ids.get(key)))
        offset = getattr(rt, "_native_sweep_cursor", 0) % max(1, len(accounts))
        rt._native_sweep_cursor = offset + 1
    count = 0
    for account, server, connection in accounts[offset:] + accounts[:offset]:
        try:
            loaded = _loaded(server)
        except Exception:
            continue
        loaded_set = set(loaded)
        with rt.lock, rt.db() as db:
            rows = db.execute("SELECT id,record FROM runtime_native_sweeps WHERE "
                              "json_extract(record,'$.accountKey')=? AND "
                              "json_extract(record,'$.connectionId')=? AND "
                              "json_extract(record,'$.phase') IN ('unsubscribed','notSubscribed','unknown','notLoaded')",
                              (account, connection)).fetchall()
            for row in rows:
                record = json.loads(row[1])
                if record.get("threadId") not in loaded_set:
                    _save(db, row[0], phase="closed", closedAt=time.time(), error=None)
            if _account_busy(rt, db, account):
                continue
        for thread in loaded:
            if count >= MAX_PER_TICK:
                return count
            key = _receipt_id(account, connection, thread)
            with rt.lock, rt.db() as db:
                receipt = _receipt(db, key)
                ticket = getattr(rt, "_native_sweep_tickets", {}).get(key)
                if ticket and not _ticket_future(ticket).done():
                    if now - receipt.get("submittedAt", now) < RECHECK_SECONDS:
                        continue
                    _save(db, key, phase="unknown", error="Native unsubscribe receipt remains pending")
                    rt._native_sweep_tickets.pop(key, None)
                    ticket = None
                if ticket:
                    _settle_ticket(rt, db, key, ticket)
                    receipt = _receipt(db, key)
                if receipt.get("phase") in {"submitted", "submitting"} and ticket is None:
                    # The process restarted after submission. Unsubscribe is idempotent.
                    _save(db, key, phase="unknown", error="Native receipt lost after restart")
                if (rt.servers.get(account) is not server
                        or _protected(rt, db, account, thread, now)):
                    continue
            if _release(rt, account, connection, server, thread, now):
                count += 1
    return count


def tick(rt, now):
    if getattr(rt, "_native_sweep_running", False):
        return
    rt._native_sweep_running = True
    try:
        future = rt.recovery_pool.submit(sweep, rt, now)
    except RuntimeError:
        rt._native_sweep_running = False
        return

    def done(_future):
        rt._native_sweep_running = False
        rt.changed.set()

    future.add_done_callback(done)
