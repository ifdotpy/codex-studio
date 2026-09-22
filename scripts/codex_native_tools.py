"""Refresh persisted native tools only after the account process becomes idle."""
import base64
import concurrent.futures
import copy
import hashlib
import json
import os
from pathlib import Path
import stat
import threading
import time
import uuid


ACTIVE = {"running", "starting", "approval"}
TABLE = "native_catalog_updates"


def catalog(tools):
    """Use the current native tagged tool representation for exact comparisons."""
    return [{"type": "function", **copy.deepcopy(tool)} for tool in tools]


def digest(tools):
    data = json.dumps(catalog(tools), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(data.encode()).hexdigest()


def mark_current(agent, tools):
    agent["nativeToolCatalog"] = {"threadId": agent.get("threadId"), "digest": digest(tools)}
    notice = agent.pop("nativeToolUpdate", None)
    if notice and agent.get("error") == notice.get("message"):
        agent["error"] = None


def needs_refresh(agent, tools):
    return (agent.get("provider", "codex") == "codex" and bool(agent.get("threadId"))
            and agent.get("nativeToolCatalog") != {"threadId": agent["threadId"], "digest": digest(tools)})


def account_reserved(rt, key):
    retiring = getattr(rt, "_native_tools_retiring", {}).get(key)
    if retiring is not None:
        if retiring["server"].proc.poll() is None:
            return True
        rt._native_tools_retiring.pop(key, None)
    return key in getattr(rt, "_native_tools_refreshing", set())


def assert_connect_allowed(rt, key):
    retiring = getattr(rt, "_native_tools_retiring", {}).get(key)
    if retiring is not None:
        if retiring["server"].proc.poll() is None:
            raise ValueError("The previous native process has not stopped. The account tool update is blocked.")
        rt._native_tools_retiring.pop(key, None)


def _schema(db):
    db.execute("CREATE TABLE IF NOT EXISTS runtime_native_catalog_updates "
               "(id TEXT PRIMARY KEY, record TEXT NOT NULL)")


def _save(rt, record):
    with rt.lock, rt.db() as db:
        _schema(db)
        record["updated"] = time.time()
        rt.put(db, TABLE, record)


def _same_source(agent):
    return {key: agent.get(key) for key in
            ("id", "epoch", "threadId", "accountKey", "cwd", "provider", "deletedAt")}


def _local_idle(rt, db, key, server):
    if rt.closed:
        return None, "Studio is stopped"
    agents = [a for a in rt.records(db, "agents") if a.get("accountKey", "default") == key]
    ids = {a["id"] for a in agents}
    for agent in agents:
        if (agent.get("inFlight") or agent.get("status") in ACTIVE or agent.get("activeTools")
                or agent.get("workspaceOperation") or agent.get("accountTransferId")):
            return None, "Waiting for account work to finish"
    if any(p.get("accountKey", "default") == key and not p["future"].done()
           for p in rt.preparations.values()):
        return None, "Waiting for native thread preparation"
    tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    ids_sql = ",".join("?" for _ in ids) or "NULL"
    for name, statuses in (("monitors", ACTIVE), ("tasks", ACTIVE | {"pending"}),
                           ("requests", {"pending", "answering"})):
        if "runtime_" + name in tables:
            statuses_sql = ",".join("?" for _ in statuses)
            native_only = (" AND COALESCE(json_extract(record,'$.method'),'')!='agent/asyncQuestion'"
                           if name == "requests" else "")
            if db.execute(f"SELECT 1 FROM runtime_{name} WHERE "
                          f"json_extract(record,'$.status') IN ({statuses_sql}){native_only} AND "
                          f"(json_extract(record,'$.agent') IN ({ids_sql}) OR "
                          "json_extract(record,'$.accountKey')=?) LIMIT 1", (*statuses, *ids, key)).fetchone():
                return None, "Waiting for a command or request receipt"
    if "runtime_tool_requests" in tables:
        for aid in ids:
            if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "AND json_extract(record,'$.stage') IN ('queued','running') LIMIT 1", (aid,)).fetchone():
                return None, "Waiting for a tool receipt"
    if "runtime_account_transfers" in tables:
        transfers = getattr(rt, "_account_transfers", None)
        running = transfers.running if transfers else set()
        futures = transfers.futures if transfers else {}
        rows = db.execute("SELECT record FROM runtime_account_transfers "
                          "WHERE json_extract(record,'$.targetAccountKey')=?", (key,))
        for row in rows:
            operation = json.loads(row[0])
            operation_id = operation["id"]
            members = operation.get("members", {})
            if any(aid in running or (operation_id, aid) in futures for aid in members):
                return None, "Waiting for an account transfer receipt"
            # An unclaimed historical waiting row cannot launch a transfer. Keep
            # submitted work reserved even when its agent pointer was removed.
            for member in members.values():
                phase = member.get("phase")
                if phase == "reading" or (phase in {"submitted", "unknown", "ready"}
                        and any(member.get(field) for field in
                                ("submittedAt", "nativeMethod", "nativeParams", "result"))):
                    return None, "Waiting for an account transfer receipt"
            if operation.get("status") != "pending":
                continue
            if db.execute("SELECT 1 FROM runtime_agents WHERE "
                          "json_extract(record,'$.accountTransferId')=? OR "
                          "(id=? AND json_extract(record,'$.accountTransferId') IS NULL "
                          "AND json_extract(record,'$.accountTransfer.id')=? "
                          "AND json_extract(record,'$.accountTransfer.status')='pending') LIMIT 1",
                          (operation_id, operation.get("leadId"), operation_id)).fetchone():
                return None, "Waiting for an account transfer receipt"
    if "runtime_events" in tables and ids:
        placeholders = ",".join("?" for _ in ids)
        if db.execute(f"SELECT 1 FROM runtime_events WHERE agent IN ({placeholders}) "
                      "AND status IN ('reserved','dispatching') LIMIT 1", tuple(ids)).fetchone():
            return None, "Waiting for confirmed message delivery"
    if "voice_sessions" in tables:
        if any(db.execute("SELECT 1 FROM voice_sessions WHERE agent=? AND ended IS NULL LIMIT 1",
                          (aid,)).fetchone() for aid in ids):
            return None, "Waiting for the voice session to end"
    if server is not None:
        if server.pending:
            return None, "Waiting for native request receipts"
        for name in ("callbacks", "clock_replies"):
            queue = getattr(server, name)
            with queue.mutex:
                if queue.unfinished_tasks:
                    return None, "Waiting for native events"
    return agents, None


def _native_idle(server):
    """Inspect every loaded thread, including native children outside Studio."""
    loaded = []
    cursor = None
    seen = set()
    while True:
        params = {"limit": 100}
        if cursor:
            params["cursor"] = cursor
        page = server.call("thread/loaded/list", params, timeout=10)
        if not isinstance(page.get("data"), list):
            raise ValueError("Native loaded-thread state is unavailable")
        loaded.extend(page["data"])
        cursor = page.get("nextCursor")
        if not cursor:
            break
        if cursor in seen:
            raise ValueError("Native loaded-thread pagination repeated")
        seen.add(cursor)
    if any(not isinstance(tid, str) for tid in loaded):
        raise ValueError("Native loaded-thread identity is unavailable")
    sources = {}
    for tid in dict.fromkeys(loaded):
        native = server.call("thread/read", {"threadId": tid, "includeTurns": False}, timeout=10)["thread"]
        if native.get("id") != tid or native.get("status", {}).get("type") not in {"idle", "notLoaded"}:
            raise ValueError("Waiting for a native turn to finish")
        queue = server.call("thread/queue/list", {"threadId": tid}, timeout=10)
        if not isinstance(queue.get("data"), list) or queue["data"] or queue.get("nextCursor"):
            raise ValueError("Waiting for the native input queue")
        if native["status"]["type"] == "idle":
            jobs = server.call("thread/backgroundTerminals/list", {"threadId": tid}, timeout=10)
            if not isinstance(jobs.get("data"), list) or jobs["data"] or jobs.get("nextCursor"):
                raise ValueError("Waiting for native background commands")
        sources[tid] = native
    return sources


def _identity(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _header(path, thread_id):
    with path.open("rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("Native rollout is not a regular file")
        first = stream.readline(16 * 1024 * 1024)
    if not first.endswith(b"\n"):
        raise ValueError("Native session metadata is incomplete")
    value = json.loads(first)
    if value.get("type") != "session_meta" or value.get("payload", {}).get("id") != thread_id:
        raise ValueError("Native session metadata has a different identity")
    return first, value, info


def replace_header(path, thread_id, tools, persist, *, allow_growth=False):
    """Stream the exact tail. Save the replacement receipt before atomic rename."""
    path = Path(path)
    if path.is_symlink() or path.suffix != ".jsonl":
        raise ValueError("Native tool refresh requires a canonical JSONL rollout")
    first, value, info = _header(path, thread_id)
    wanted = catalog(tools)
    if value["payload"].get("dynamic_tools", []) == wanted:
        return {"path": str(path), "threadId": thread_id, "status": "unchanged"}
    value["payload"]["dynamic_tools"] = wanted
    new_header = (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
    if len(new_header) > len(first) and not allow_growth:
        raise ValueError("The current tool catalog does not fit the native header. Existing history offsets are preserved.")
    # Descendant history_base records use byte offsets into this file.
    new_header = new_header[:-1] + b" " * (len(first) - len(new_header)) + b"\n"
    temporary = path.with_name("." + path.name + ".studio-tools-" + str(uuid.uuid4()) + ".tmp")
    record = {"path": str(path), "threadId": thread_id, "temporary": str(temporary), "status": "copying",
              "originalHeader": base64.b64encode(first).decode(),
              "newHeader": base64.b64encode(new_header).decode(), "sourceIdentity": _identity(info)}
    persist(copy.deepcopy(record))
    old_hash, new_hash, tail_hash = hashlib.sha256(first), hashlib.sha256(new_header), hashlib.sha256()
    try:
        with path.open("rb") as source, temporary.open("xb") as target:
            if _identity(os.fstat(source.fileno())) != _identity(info) or source.readline(len(first) + 1) != first:
                raise ValueError("Native rollout changed before the copy")
            os.chmod(temporary, stat.S_IMODE(info.st_mode))
            target.write(new_header)
            final = first[-1:]
            tail_bytes = 0
            while block := source.read(1024 * 1024):
                target.write(block)
                old_hash.update(block)
                new_hash.update(block)
                tail_hash.update(block)
                tail_bytes += len(block)
                final = block[-1:]
            if final != b"\n" or _identity(os.fstat(source.fileno())) != _identity(info):
                raise ValueError("Native rollout changed or has an incomplete final record")
            target.flush()
            os.fsync(target.fileno())
        record.update(status="prepared", originalSha256=old_hash.hexdigest(), newSha256=new_hash.hexdigest(),
                      tailSha256=tail_hash.hexdigest(), tailBytes=tail_bytes)
        persist(copy.deepcopy(record))
        if _identity(path.stat()) != _identity(info):
            raise ValueError("Native rollout changed before the replacement")
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        record["status"] = "completed"
        persist(copy.deepcopy(record))
        return record
    finally:
        # This exact temporary belongs to this attempt. Never remove other files.
        temporary.unlink(missing_ok=True)


def _file_hash(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while block := stream.read(1024 * 1024):
            value.update(block)
    return value.hexdigest()


def _ticket(rt, key):
    with rt.db() as db:
        row = db.execute("SELECT record FROM runtime_native_catalog_updates WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("The native tool fork receipt is missing")
        return json.loads(row[0])


def _receive_fork(rt, key, future):
    """Save late native receipts even after the caller's wait expires."""
    try:
        result = future.result()
        native = result.get("thread", {})
        if not native.get("id") or not native.get("path"):
            raise ValueError("The native fork returned no destination identity")
        with rt.lock:
            ticket = _ticket(rt, key)
            if ticket.get("result"):
                if ticket["result"] != result:
                    raise ValueError("The native fork returned conflicting receipts")
                return
        path = Path(native["path"])
        _, metadata, _ = _header(path, native["id"])
        if metadata["payload"].get("forked_from_id") != ticket["source"]["threadId"]:
            raise ValueError("The native fork has a different source")
        destination_hash = _file_hash(path)
        with rt.lock:
            ticket = _ticket(rt, key)
            if ticket.get("result"):
                if ticket["result"] != result:
                    raise ValueError("The native fork returned conflicting receipts")
                return
            ticket.update(status="ready", result=result, destinationSha256=destination_hash, received=time.time())
            _save(rt, ticket)
        with rt.lock, rt.db() as db:
            current = rt.agent(ticket["agent"], db)
            if current.get("nativeToolRefreshId") == key and _same_source(current) == ticket["source"]:
                _wait_notice(rt, db, current, rt.tool_definitions(current), "waiting", "The native tool fork receipt arrived")
        rt.changed.set()
    except Exception as error:
        with rt.lock:
            ticket = _ticket(rt, key)
            if not ticket.get("result"):
                ticket.update(status="unknown", reason=str(error))
                _save(rt, ticket)


def _growth_target(rt, server, agent):
    with rt.lock, rt.db() as db:
        current = rt.agent(agent["id"], db)
        if _same_source(current) != _same_source(agent):
            raise ValueError("The native tool source changed before its fork")
        key = current.get("nativeToolRefreshId")
        if key:
            row = db.execute("SELECT record FROM runtime_native_catalog_updates WHERE id=?", (key,)).fetchone()
            if row is None:
                raise ValueError("The native tool fork receipt is missing")
            ticket = json.loads(row[0])
            if ticket["source"] != _same_source(current):
                raise ValueError("The saved native tool fork belongs to another source")
            if ticket.get("result") is None:
                raise ValueError("The native tool fork outcome is unknown. Its request was not repeated.")
        else:
            params = rt.new_thread_params(current)
            params.pop("dynamicTools", None)
            params.update(threadId=current["threadId"], excludeTurns=True, deferGoalContinuation=True)
            ticket = {"id": str(uuid.uuid4()), "kind": "tool_catalog_fork", "agent": current["id"],
                      "accountKey": current.get("accountKey", "default"), "source": _same_source(current),
                      "status": "submitted", "params": params, "created": time.time(),
                      "connectionId": rt.connection_ids[current.get("accountKey", "default")]}
            _schema(db)
            rt.put(db, TABLE, ticket)
            current["nativeToolRefreshId"] = ticket["id"]
            rt.put(db, "agents", current)
            db.commit()
            submitted = rt.submit_reserved(server, "thread/fork", params)
            server.on_result(submitted, lambda future: _receive_fork(rt, ticket["id"], future))
    if ticket.get("result") is None:
        try:
            result = server.wait(submitted, timeout=30)
        except Exception as error:
            with rt.lock:
                latest = _ticket(rt, ticket["id"])
                if latest.get("result") is None:
                    latest.update(status="unknown", reason=str(error))
                    _save(rt, latest)
                    raise ValueError("The native tool fork outcome is unknown. Its request was not repeated.") from error
        else:
            completed = concurrent.futures.Future()
            completed.set_result(result)
            _receive_fork(rt, ticket["id"], completed)
        ticket = _ticket(rt, ticket["id"])
    if ticket.get("status") != "ready" or not ticket.get("result"):
        raise ValueError("The native tool fork receipt is not ready: " + ticket.get("reason", "Waiting for confirmation"))
    native = ticket["result"]["thread"]
    path = Path(native["path"])
    if _file_hash(path) != ticket["destinationSha256"]:
        # A prior attempt can replace the header before its final DB transaction.
        replaced = ticket.get("replacement") or {}
        if replaced.get("newSha256") != _file_hash(path):
            raise ValueError("The native tool fork changed after its receipt")
    return path, ticket


def _assert_no_descendants(home, thread_id):
    for directory in ("sessions", "archived_sessions"):
        for path in (home / directory).glob("**/*.jsonl"):
            with path.open("rb") as stream:
                first = stream.readline(16 * 1024 * 1024)
            metadata = json.loads(first).get("payload", {})
            if metadata.get("history_base", {}).get("thread_id") == thread_id:
                raise ValueError("The native tool fork has a descendant; its history offsets are preserved")


def refresh_account(rt, account_key="default", tools_for_agent=None, *, agent_ids=None):
    """Return a durable result. Never stop an account with unconfirmed idle state."""
    tools_for_agent = tools_for_agent or rt.tool_definitions
    if rt.accounts.get(account_key).get("provider", "codex") != "codex":
        return {"status": "not_applicable", "accountKey": account_key}
    operation = {"id": str(uuid.uuid4()), "accountKey": account_key, "status": "checking", "files": {},
                 "created": time.time()}
    with rt.start_lock:
        with rt.lock, rt.db() as db:
            server = rt.servers.get(account_key)
            agents, reason = _local_idle(rt, db, account_key, server)
            if reason:
                return {**operation, "status": "waiting", "reason": reason}
            if account_reserved(rt, account_key):
                return {**operation, "status": "waiting", "reason": "The account tool update is already reserved"}
            rt._native_tools_refreshing = getattr(rt, "_native_tools_refreshing", set())
            rt._native_tools_refreshing.add(account_key)
        try:
            # Callers must not enter through Runtime.connect while start_lock is held.
            if server is None:
                return {**operation, "status": "waiting", "reason": "Connect the account before its tool update"}
            sources = _native_idle(server)
            requested = [a for a in agents if a.get("threadId") and not a.get("deletedAt")
                         and (agent_ids is None or a["id"] in agent_ids or
                              (a.get("status") == "queued" and a.get("autoWake") and
                               needs_refresh(a, tools_for_agent(a))))]
            home = Path(rt.accounts.home(account_key)).resolve()
            targets = []
            operation["threadErrors"] = {}
            for agent in requested:
                try:
                    native = sources.get(agent["threadId"])
                    if native is None:
                        native = server.call("thread/read", {"threadId": agent["threadId"],
                                             "includeTurns": False}, timeout=10)["thread"]
                        if (native.get("id") != agent["threadId"]
                                or native.get("status", {}).get("type") != "notLoaded"):
                            raise ValueError("The native thread changed during its idle check")
                    path = Path(native.get("path") or "").resolve()
                    relative = path.relative_to(home)
                    if not relative.parts or relative.parts[0] not in {"sessions", "archived_sessions"}:
                        raise ValueError("Native rollout is outside the account session directory")
                    _, metadata, _ = _header(path, agent["threadId"])
                    definitions = tools_for_agent(agent)
                    changed = metadata["payload"].get("dynamic_tools", []) != catalog(definitions)
                    ticket = None
                    if changed:
                        first, replacement, _ = _header(path, agent["threadId"])
                        replacement["payload"]["dynamic_tools"] = catalog(definitions)
                        required = len((json.dumps(replacement, ensure_ascii=False, separators=(",", ":")) + "\n").encode())
                        if required > len(first):
                            path, ticket = _growth_target(rt, server, agent)
                    targets.append((agent, path, definitions, changed, ticket))
                except Exception as error:
                    operation["threadErrors"][agent["id"]] = str(error)
            if not targets and operation["threadErrors"]:
                operation.update(status="blocked", reason="The requested native thread cannot be updated")
                _record_target_errors(rt, requested, tools_for_agent, operation["threadErrors"])
                _save(rt, operation)
                return operation
            barrier = threading.Event()
            if any(ticket for _, _, _, _, ticket in targets):
                _native_idle(server)
            server.after_events(barrier.set)
            if not barrier.wait(10):
                raise ValueError("Waiting for native events to finish")
            with rt.lock, rt.db() as db:
                current, reason = _local_idle(rt, db, account_key, server)
                if reason or [_same_source(a) for a in current] != [_same_source(a) for a in agents]:
                    raise ValueError(reason or "The account changed during its tool check")
                operation["agents"] = [_same_source(a) for a in agents]
                operation["connectionId"] = rt.connection_ids.get(account_key)
                operation["status"] = "reserved"
                _schema(db)
                rt.put(db, TABLE, operation)
                db.commit()
                if any(changed for _, _, _, changed, _ in targets):
                    # Old disconnect callbacks must not create restart recovery events.
                    rt._native_tools_retiring = getattr(rt, "_native_tools_retiring", {})
                    rt._native_tools_retiring[account_key] = {"server": server, "attempt": operation["id"]}
                    rt.connection_ids[account_key] = "retired:" + operation["id"]
                    rt.servers.pop(account_key, None)
                    rt.loaded.difference_update(a["id"] for a in agents)
                    if account_key == "default":
                        rt.server = None
            if any(changed for _, _, _, changed, _ in targets):
                server.close()
                if server.proc.poll() is None:
                    raise RuntimeError("The native process did not stop; tool metadata was not changed")
                with rt.lock, rt.db() as db:
                    rt._native_tools_retiring.pop(account_key, None)
                    operation["status"] = "retired"
                    rt.put(db, TABLE, operation)
                    db.commit()
            for agent, path, tools, changed, ticket in targets:
                if changed:
                    def persist(file_record):
                        operation["files"][agent["id"]] = file_record
                        _save(rt, operation)
                        if ticket:
                            ticket["replacement"] = file_record
                            _save(rt, ticket)
                    native_id = ticket["result"]["thread"]["id"] if ticket else agent["threadId"]
                    if ticket:
                        _assert_no_descendants(home, native_id)
                    replace_header(path, native_id, tools, persist, allow_growth=ticket is not None)
            with rt.lock, rt.db() as db:
                for before, _, tools, _, ticket in targets:
                    agent = rt.agent(before["id"], db)
                    if _same_source(agent) != _same_source(before):
                        raise ValueError("The Studio chat changed during the tool update")
                    if ticket:
                        native_id = ticket["result"]["thread"]["id"]
                        agent.setdefault("accountHistory", []).append({
                            "toolRefreshId": ticket["id"], "accountKey": account_key,
                            "threadId": agent["threadId"], "targetAccountKey": account_key,
                            "targetThreadId": native_id, "provider": "codex", "at": time.time()})
                        agent.update(threadId=native_id, turnId=None)
                        agent.pop("nativeToolRefreshId", None)
                        agent.pop("prepareAttempt", None)
                        agent.pop("preparedContext", None)
                        ticket.update(status="completed", completed=time.time())
                        rt.put(db, TABLE, ticket)
                    mark_current(agent, tools)
                    rt.put(db, "agents", agent)
                operation["status"] = "partial" if operation["threadErrors"] else "completed"
                operation["completed"] = time.time()
                rt.put(db, TABLE, operation)
            _record_target_errors(rt, requested, tools_for_agent, operation["threadErrors"])
            return operation
        except Exception as error:
            operation.update(status="blocked" if operation["status"] in {"reserved", "retired"} else "waiting",
                             reason=str(error))
            if operation["status"] == "blocked":
                _save(rt, operation)
            return operation
        finally:
            with rt.lock:
                rt._native_tools_refreshing.discard(account_key)
            rt.changed.set()


def _record_target_errors(rt, requested, tools_for_agent, errors):
    with rt.lock, rt.db() as db:
        for before in requested:
            if before["id"] not in errors:
                continue
            current = rt.agent(before["id"], db)
            if _same_source(current) == _same_source(before):
                _wait_notice(rt, db, current, tools_for_agent(current), "blocked", errors[current["id"]])


def gate(rt, db, agent, tools):
    """Pre-turn admission. Existing active turns never enter this gate."""
    key = agent.get("accountKey", "default")
    if account_reserved(rt, key):
        return False
    if not needs_refresh(agent, tools):
        _wait_notice(rt, db, agent, tools, None, None)
        return True
    prior_notice = agent.get("nativeToolUpdate") or {}
    identity = {"threadId": agent.get("threadId"), "epoch": agent.get("epoch"), "digest": digest(tools)}
    if prior_notice.get("status") == "blocked" and prior_notice.get("source") == identity:
        ticket_id = agent.get("nativeToolRefreshId")
        row = db.execute("SELECT record FROM runtime_native_catalog_updates WHERE id=?", (ticket_id,)).fetchone() if ticket_id else None
        if not row or json.loads(row[0]).get("status") != "ready":
            return False
    # Catalog maintenance must not hold ordinary continuations behind another
    # agent's long turn. A saved fork is already a mutation and must settle first.
    _, busy = _local_idle(rt, db, key, rt.servers.get(key))
    if busy:
        if not agent.get("nativeToolRefreshId"):
            _wait_notice(rt, db, agent, tools, None, None)
            return True
        return False
    jobs = getattr(rt, "_native_tools_jobs", None)
    if jobs is None:
        jobs = rt._native_tools_jobs = {}
    previous = jobs.get(key, {})
    if previous.get("running"):
        return False
    if previous.get("nextCheck", 0) > time.monotonic():
        if not agent.get("nativeToolRefreshId"):
            _wait_notice(rt, db, agent, tools, None, None)
            return True
        return False
    _wait_notice(rt, db, agent, tools, None, None)
    jobs[key] = {"running": True}
    def run():
        try:
            rt.connect(key)
            result = refresh_account(rt, key, agent_ids=[agent["id"]])
        except Exception as error:
            result = {"status": "blocked", "reason": str(error)}
        try:
            with rt.lock, rt.db() as own:
                jobs[key] = {"running": False, "nextCheck": time.monotonic() + 2, "result": result}
                current = rt.agent(agent["id"], own)
                if _same_source(current) == _same_source(agent):
                    status = result["status"]
                    _wait_notice(rt, own, current, tools, None if status in {"completed", "waiting"} else status,
                                 result.get("threadErrors", {}).get(agent["id"]) or result.get("reason"))
        finally:
            with rt.lock:
                rt._native_tools_workers.discard(threading.current_thread())
            rt.changed.set()
    db.commit()
    rt._native_tools_workers = getattr(rt, "_native_tools_workers", set())
    worker = threading.Thread(target=run, daemon=True, name="studio-native-tools")
    rt._native_tools_workers.add(worker)
    worker.start()
    return False


def wait_updates(rt):
    """The Runtime lease must outlive every metadata writer."""
    with rt.lock:
        workers = list(getattr(rt, "_native_tools_workers", ()))
    deadline = time.monotonic() + 30
    for worker in workers:
        worker.join(max(0, deadline - time.monotonic()))
    if any(worker.is_alive() for worker in workers):
        raise RuntimeError("Native tool metadata update is still running; runtime lease retained")


def _wait_notice(rt, db, agent, tools, status, reason):
    previous = agent.get("nativeToolUpdate") or {}
    if status is None:
        if not previous:
            return
        if agent.get("error") == previous.get("message"):
            agent["error"] = None
        agent.pop("nativeToolUpdate", None)
    else:
        message = reason or "Waiting for the account tool catalog update"
        notice = {"status": status, "message": message, "source": {
            "threadId": agent.get("threadId"), "epoch": agent.get("epoch"), "digest": digest(tools)}}
        if previous == notice:
            return
        if not agent.get("error") or agent.get("error") == previous.get("message"):
            agent["error"] = message
        agent["nativeToolUpdate"] = notice
    rt.put(db, "agents", agent)
