"""Recover native browser discovery at an idle turn boundary, without action replay."""
import time
import threading
import uuid

DISCOVERY_ERRORS = {"No browser is available", "Browser is not available: chrome",
                    "Browser is not available: extension"}
BUSY = {"running", "starting", "approval"}
HOLD = {"pending", "reconnecting"}


def text_results(item):
    result = item.get("result") or {}
    if not isinstance(result, dict):
        return []
    return [v["text"].strip() for v in result.get("content", [])
            if isinstance(v, dict) and v.get("type") == "text" and isinstance(v.get("text"), str)]


def observe(runtime, db, actor, item, turn_id, connection_id):
    # Read error results, never model arguments, quoted page text, or arbitrary timeouts.
    if (item.get("type") != "mcpToolCall" or item.get("server") != "node_repl"
            or item.get("tool") != "js" or not item.get("id")):
        return
    texts = text_results(item)
    previous = actor.get("browserRecovery") or {}
    if previous and not same_identity(runtime, actor, previous):
        previous = {}
    if item.get("status") == "completed":
        if (previous.get("stage") == "verify" and any(
                t.startswith("# Selected Browser\n- Name: Chrome\n- Type: extension\n") for t in texts)):
            previous.update(stage="verified", verifiedAt=time.time(), verificationItem=item["id"])
            actor["browserRecovery"] = previous
        return
    reason = next((t for t in texts if t in DISCOVERY_ERRORS), None)
    if item.get("status") != "failed" or reason is None:
        return
    if previous.get("itemId") == item["id"] or previous.get("stage") in HOLD:
        return
    if previous.get("stage") in {"verify", "failed"}:
        # One automatic attempt per failure episode. A failed probe must not loop.
        previous.update(stage="failed", error=reason, failedAt=time.time())
        actor["browserRecovery"] = previous
        return
    actor["browserRecovery"] = {
        "id": str(uuid.uuid4()), "stage": "pending", "itemId": item["id"],
        "turnId": turn_id, "threadId": actor.get("threadId"), "epoch": actor["epoch"],
        "accountKey": actor.get("accountKey", "default"), "connectionId": connection_id,
        "reason": reason, "createdAt": time.time(),
    }


def same_identity(runtime, actor, recovery):
    return (actor.get("threadId") == recovery["threadId"] and actor["epoch"] == recovery["epoch"]
            and actor.get("accountKey", "default") == recovery["accountKey"]
            and runtime.connection_current(recovery["accountKey"], recovery["connectionId"]))


def busy(runtime, db, actor):
    if (actor.get("inFlight") or actor.get("activeTools") or actor["status"] in BUSY
            or actor.get("workspaceOperation") or actor.get("accountTransferId")):
        return True
    for table in ("runtime_monitors", "runtime_tasks"):
        if db.execute(f"SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=? "
                      "AND json_extract(record,'$.status') IN ('starting','running','approval') LIMIT 1",
                      (actor["id"],)).fetchone():
            return True
    return False


def tick(runtime, db, actors):
    if runtime.closed:
        return
    for actor in actors:
        recovery = actor.get("browserRecovery") or {}
        if recovery.get("stage") not in HOLD:
            continue
        if not same_identity(runtime, actor, recovery):
            recovery.update(stage="failed", error="Browser recovery belongs to an earlier session")
            runtime.put(db, "agents", actor)
            continue
        if recovery["stage"] == "reconnecting":
            # After a process restart a persisted operation has no in-memory owner.
            jobs = getattr(runtime, "_browser_recovery_jobs", set())
            if recovery["id"] not in jobs:
                fail(runtime, db, actor, "Browser reconnection outcome is unknown after restart")
            continue
        if (not actor.get("autoWake") or actor.get("nativeFailureHold") or actor.get("deletedAt")
                or busy(runtime, db, actor)):
            continue
        jobs = runtime.__dict__.setdefault("_browser_recovery_jobs", set())
        if len(jobs) >= 2:
            continue
        recovery.update(stage="reconnecting", startedAt=time.time())
        runtime.put(db, "agents", actor)
        jobs.add(recovery["id"])
        try:
            # Native reconnection must not occupy the coordination/status pool.
            threading.Thread(target=reconnect, args=(runtime, actor["id"], recovery.copy()),
                             name="studio-browser-recovery", daemon=True).start()
        except Exception:
            jobs.discard(recovery["id"])
            recovery["stage"] = "pending"
            runtime.put(db, "agents", actor)
            raise


def fail(runtime, db, actor, error):
    recovery = actor["browserRecovery"]
    recovery.update(stage="failed", error=error, failedAt=time.time())
    actor.update(error=error, nativeFailureHold=True, status="failed")
    runtime.put(db, "agents", actor)


def recovery_current(runtime, actor, operation):
    return (not runtime.closed and not actor.get("deletedAt") and actor.get("autoWake")
            and not actor.get("nativeFailureHold")
            and actor.get("browserRecovery", {}).get("id") == operation["id"]
            and actor.get("browserRecovery", {}).get("stage") == "reconnecting"
            and same_identity(runtime, actor, operation))


def native_call(runtime, server, agent_id, operation, method, params, timeout):
    # Preserve the exact native request ID before waiting. Do not replay an
    # ambiguous mutation just because its response has not arrived.
    with runtime.lock, runtime.db() as db:
        actor = runtime.agent(agent_id, db)
        if not recovery_current(runtime, actor, operation) or busy(runtime, db, actor):
            raise RuntimeError("Agent changed before native browser request submission")
        submitted = runtime.submit_reserved(server, method, params)
        actor["browserRecovery"]["nativeRequest"] = {"method": method, "id": submitted[0], "submittedAt": time.time()}
        runtime.put(db, "agents", actor)
    result = server.wait(submitted, timeout)
    with runtime.lock, runtime.db() as db:
        actor = runtime.agent(agent_id, db)
        if not recovery_current(runtime, actor, operation):
            raise RuntimeError("Agent changed while waiting for native browser response")
        actor["browserRecovery"]["nativeRequest"].update(receivedAt=time.time(), outcome="received")
        runtime.put(db, "agents", actor)
    return result


def reconnect(runtime, agent_id, operation):
    try:
        with runtime.lock:
            guard = runtime.prepare_locks.setdefault(agent_id, threading.Lock())
        with guard:
            with runtime.lock, runtime.db() as db:
                actor = runtime.agent(agent_id, db)
                if (runtime.closed or actor.get("browserRecovery", {}).get("id") != operation["id"]
                        or not same_identity(runtime, actor, operation) or not actor.get("autoWake")):
                    return
                if busy(runtime, db, actor):
                    actor["browserRecovery"]["stage"] = "pending"
                    runtime.put(db, "agents", actor)
                    return
            from codex_browser import browser_config
            config = browser_config(runtime.accounts.home(operation["accountKey"]), runtime.accounts.base_home)
            if not config:
                raise RuntimeError("Native browser integration is disabled or its runtime files are unavailable")
            server = runtime.connect(operation["accountKey"])
            params = runtime.new_thread_params(actor)
            params.pop("dynamicTools", None)
            params.update(threadId=operation["threadId"], excludeTurns=True)
            # This change forces a fresh native session after unsubscribe, even when
            # the runtime configuration itself has not changed. Never fork history.
            params["developerInstructions"] += "\nBrowser connection generation: " + operation["id"]
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(agent_id, db)
                if not same_identity(runtime, current, operation) or busy(runtime, db, current):
                    raise RuntimeError("Agent changed before browser reconnection")
            native_call(runtime, server, agent_id, operation, "thread/unsubscribe", {"threadId": operation["threadId"]}, 20)
            response = native_call(runtime, server, agent_id, operation, "thread/resume", params, 60)
            if response.get("thread", {}).get("id") != operation["threadId"]:
                raise RuntimeError("Browser reconnection returned a different thread; outcome unknown")
            drained = threading.Event()
            server.after_events(drained.set)
            if not drained.wait(20):
                raise RuntimeError("Browser reconnection callback delivery is unconfirmed")
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(agent_id, db)
                if not recovery_current(runtime, current, operation):
                    return
                previous = runtime.preparations.get(agent_id)
                if previous and previous["future"].done():
                    runtime.preparations.pop(agent_id, None)
                runtime.loaded.add(agent_id)
                current["browserRecovery"].update(stage="verify", resumedAt=time.time())
                # This is new diagnostic input. It does not replay the failed tool,
                # any earlier user message, or the last browser command.
                message = (
                    "Studio refreshed this thread's native browser connection after a discovery failure. "
                    "The thread history is preserved; previous JavaScript browser bindings may be gone. "
                    "Read the native Chrome skill and reconnect through node_repl. First verify access "
                    "with read-only browser discovery and inspect the existing task tab. Do not open or "
                    "navigate tabs for this check. Do not replay prior clicks, submissions, signatures, "
                    "or other actions with unknown outcomes. If verification fails, report the exact "
                    "error and finish the turn; automatic recovery will not repeat. If it succeeds, "
                    "continue only the user's already authorized work, preserving all review requirements."
                )
                runtime.enqueue(db, current, "browser_recovery", message, "browser-recovery:" + operation["id"])
                current["status"] = "queued"
                runtime.put(db, "agents", current)
    except Exception as error:
        with runtime.lock, runtime.db() as db:
            current = runtime.agent(agent_id, db)
            if recovery_current(runtime, current, operation):
                fail(runtime, db, current, "Browser reconnection failed: " + str(error))
    finally:
        with runtime.lock:
            getattr(runtime, "_browser_recovery_jobs", set()).discard(operation["id"])
        runtime.changed.set()
