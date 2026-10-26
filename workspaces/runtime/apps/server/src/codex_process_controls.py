"""One identity and receipt boundary for monitor and native process controls."""
from __future__ import annotations

import base64
import hashlib
import json
import time
from typing import Any, cast

from codex_entity_contracts import ACTIVE_MONITOR_STATUSES
from codex_shared_values import NATIVE_TERMINAL_ITEMS_PER_PAGE, NATIVE_TERMINAL_MAX_PAGES
from codex_workspace import active_monitors, active_task_records

# Providers whose adapter implements `thread/commandExecution/background`.
# Codex app-server has no request that ends a running exec wait early.
BACKGROUND_COMMAND_PROVIDERS = frozenset({"claude"})


def _json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _digest(value: object) -> str:
    return hashlib.sha256(_json(value).encode()).hexdigest()


def _key(source: str, agent: str, opaque_id: str) -> str:
    raw = _json([source, agent, opaque_id]).encode()
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def _agent_rows(db: Any, scope: str) -> tuple[str, list[dict[str, Any]]]:
    root = db.execute("SELECT record FROM runtime_agents WHERE id=?", (scope,)).fetchone()
    if root is None:
        raise ValueError("Unknown process owner")
    owner = json.loads(root[0])
    if owner.get("deletedAt"):
        raise ValueError("This process owner was deleted")
    rows = db.execute(
        "SELECT record FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL"
    )
    all_agents = [json.loads(row[0]) for row in rows]
    by_parent: dict[str, list[dict[str, Any]]] = {}
    for agent in all_agents:
        parent = agent.get("parentId")
        if isinstance(parent, str):
            by_parent.setdefault(parent, []).append(agent)
    agents = [owner]
    seen = {owner["id"]}
    index = 0
    while index < len(agents):
        parent_id = agents[index]["id"]
        index += 1
        for child in by_parent.get(parent_id, []):
            if child["id"] not in seen:
                seen.add(child["id"])
                agents.append(child)
    return owner["id"], agents


def _record_process(source: str, raw: dict[str, Any], owner: dict[str, Any],
                    current_connection: str | None = None) -> dict[str, Any]:
    output = ""
    agent_id = str(raw.get("agent") or owner["id"])
    source_account = raw.get("sourceAccountKey")
    source_connection = raw.get("sourceConnectionId")
    source_thread = raw.get("sourceThreadId")
    if source == "monitor":
        operation = raw.get("operation") or {}
        pending = raw.get("status") in {"starting", "approval"}
        source_account = operation.get("accountKey") or (owner.get("accountKey", "default") if pending else None)
        saved_connection = operation.get("connectionId")
        saved_thread = operation.get("threadId")
        reattached_connection = raw.get("reattachedConnectionId")
        reattached = bool(raw.get("status") == "running" and reattached_connection
                          and reattached_connection == current_connection)
        source_connection = reattached_connection if reattached else saved_connection
        source_thread = owner.get("threadId") if reattached else saved_thread
        if not source_thread and saved_connection == current_connection:
            source_thread = owner.get("threadId")
        native_generation: object = [raw.get("id"), raw.get("epoch"),
                                     source_account, source_connection, source_thread]
        opaque_id = str(raw["id"])
        tail = raw.get("tail")
        output = tail if isinstance(tail, str) else ""
        same_owner = bool(
            source_account == owner.get("accountKey", "default")
            and raw.get("epoch") == owner.get("epoch")
            and source_connection and source_connection == current_connection
            and source_thread and source_thread == owner.get("threadId")
        )
        pending_stop = bool(pending and raw.get("epoch") == owner.get("epoch")
                            and source_account == owner.get("accountKey", "default"))
        can_input = bool(raw.get("interactive") and raw.get("status") == "running"
                         and raw.get("epoch") == owner.get("epoch")
                         and owner.get("autoWake") and same_owner)
        capabilities = {"readOutput": True, "sendInput": can_input,
                        "resize": can_input, "stop": same_owner or pending_stop}
        name = str(raw.get("command") or raw.get("name") or "Command monitor")
    else:
        item_id = str(raw.get("itemId") or "")
        provider = str(raw.get("sourceProvider") or owner.get("provider") or "")
        if source == "claude_task":
            source_account = source_account or None
            source_thread = source_thread or None
            opaque_id = item_id.removeprefix("task:")
            native_generation = [opaque_id, raw.get("ownerEpoch"), source_account,
                                 source_connection, source_thread, raw.get("turnId")]
            output = ""
            same_owner = bool(
                raw.get("ownerEpoch") == owner.get("epoch")
                and source_account == owner.get("accountKey", "default")
                and source_connection and source_connection == current_connection
                and source_thread and source_thread == owner.get("threadId")
            )
            capabilities = {"readOutput": False, "sendInput": False,
                            "resize": False, "stop": same_owner}
            name = str(raw.get("name") or raw.get("command") or "Claude background task")
        else:
            source_account = source_account or None
            source_thread = source_thread or None
            opaque_id = str(raw.get("processId") or item_id)
            native_generation = [opaque_id, item_id, raw.get("ownerEpoch"), source_account,
                                 source_connection, source_thread, raw.get("turnId")]
            tail = raw.get("tail")
            output = tail if isinstance(tail, str) else ""
            same_owner = bool(
                raw.get("ownerEpoch") == owner.get("epoch")
                and source_account == owner.get("accountKey", "default")
                and source_connection and source_connection == current_connection
                and source_thread and source_thread == owner.get("threadId")
            )
            capabilities = {"readOutput": True,
                            "sendInput": bool(raw.get("processId") and same_owner and owner.get("autoWake")),
                            "resize": False, "stop": same_owner}
            name = str(raw.get("command") or raw.get("name") or "Background command")
    generation = _digest([source, agent_id, native_generation])
    raw_bytes = raw.get("bytes")
    output_bytes = int(raw_bytes) if isinstance(raw_bytes, (int, float)) else len(output.encode("utf-8"))
    return {
        "id": str(raw.get("id") or opaque_id),
        "key": _key(source, agent_id, opaque_id),
        "generation": generation,
        "agent": agent_id,
        "accountKey": str(owner.get("accountKey", "default")),
        "epoch": int(owner.get("epoch", 0)),
        "source": source,
        "status": str(raw.get("status") or "unknown"),
        "cancelRequested": bool(raw.get("cancelRequested", False)),
        "created": float(raw.get("created") or 0),
        "name": name,
        "command": raw.get("command") if isinstance(raw.get("command"), str) else None,
        "cwd": raw.get("cwd") if isinstance(raw.get("cwd"), str) else None,
        "log": raw.get("log") if isinstance(raw.get("log"), str) else None,
        "output": output,
        "error": raw.get("error") if isinstance(raw.get("error"), str) else None,
        "stdinError": raw.get("stdinError") if isinstance(raw.get("stdinError"), str) else None,
        "stdinClosed": bool(raw.get("stdinClosed", False)),
        "stdinCloseRequested": bool(raw.get("stdinCloseRequested", False)),
        "bytes": output_bytes,
        "outputTruncated": bool(raw.get(
            "outputTruncated", output_bytes > len(output.encode("utf-8")))),
        "capabilities": capabilities,
        "_raw": raw,
        "_sourceAccountKey": source_account,
        "_sourceConnectionId": source_connection,
        "_sourceThreadId": source_thread,
        "_sourceEpoch": raw.get("epoch") if source == "monitor" else raw.get("ownerEpoch"),
    }


def _listed_processes(rt: Any, db: Any, scope: str) -> list[dict[str, Any]]:
    _, agents = _agent_rows(db, scope)
    owners = {agent["id"]: agent for agent in agents}
    processes: list[dict[str, Any]] = []
    for monitor in active_monitors(db):
        owner = owners.get(monitor.get("agent"))
        if owner and monitor.get("status") in ACTIVE_MONITOR_STATUSES:
            process = _record_process("monitor", monitor, owner, rt.agent_connection(owner))
            processes.append(process)
    for task in active_task_records(db, statuses=("running",)):
        owner = owners.get(task.get("agent"))
        if not owner:
            continue
        item_id = task.get("itemId")
        account_key = task.get("sourceAccountKey", owner.get("accountKey", "default"))
        account = rt.accounts.get(account_key)
        provider = task.get("sourceProvider") or (
            account.get("provider", owner.get("provider", "codex"))
            if account else owner.get("provider", "codex")
        )
        task_source = None
        if task.get("kind") == "command" and task.get("processId"):
            task_source = "codex_task"
        elif (provider == "claude" and task.get("kind") == "tool"
              and isinstance(item_id, str) and item_id.startswith("task:")
              and item_id.removeprefix("task:")):
            task_source = "claude_task"
        if task_source:
            task = {**task, "sourceProvider": provider}
            process = _record_process(task_source, task, owner, rt.agent_connection(owner))
            processes.append(process)
    processes.sort(key=lambda process: (-process["created"], process["key"]))
    return processes


def listing(rt: Any, scope: str) -> dict[str, Any]:
    with rt.read_db() as db:
        _, owners = _agent_rows(db, scope)
        root = next(agent for agent in owners if agent["id"] == scope)
        response_target = None
        connection_id = rt.agent_connection(root)
        if root.get("inFlight") and root.get("threadId") and root.get("turnId") and connection_id:
            response_target = {"agent": scope, "accountKey": root.get("accountKey", "default"),
                               "epoch": root.get("epoch"), "connectionId": connection_id,
                               "threadId": root["threadId"], "turnId": root["turnId"]}
        return {"responseTarget": response_target, "processes": [
            {key: value for key, value in process.items() if not key.startswith("_")}
            for process in _listed_processes(rt, db, scope)
        ]}


def _identity_matches(process: dict[str, Any], identity: dict[str, Any]) -> bool:
    return all(process.get(key) == identity.get(key)
               for key in ("key", "generation", "agent", "accountKey", "epoch"))


def _snapshot(rt: Any, db: Any, request: dict[str, Any]) -> tuple[list[dict[str, Any]], list[dict[str, Any]], dict[str, Any] | None]:
    scope = request["agent"]
    root_id, owners = _agent_rows(db, scope)
    root = next((agent for agent in owners if agent["id"] == scope), None)
    if root is None:
        root = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (scope,)).fetchone()[0])
    if (root.get("accountKey", "default") != request["accountKey"]
            or root.get("epoch") != request["epoch"]):
        raise ValueError("The process owner changed. Refresh the process list")
    if request["action"] in {"stop_response", "background_command"}:
        identity = request["responseTarget"]
        current = {"agent": scope, "accountKey": root.get("accountKey", "default"),
                   "epoch": root.get("epoch"), "connectionId": rt.agent_connection(root),
                   "threadId": root.get("threadId"), "turnId": root.get("turnId")}
        if not root.get("inFlight") or not all(current.get(key) == identity.get(key) for key in current):
            raise ValueError("The response changed. Refresh the process list")
        return [], [], current
    selected = [request["process"]] if request["action"] == "stop_selected" else request.get("processes", [])
    if request["action"] == "stop_selected" and not request.get("process"):
        raise ValueError("Select a process to stop")
    processes = _listed_processes(rt, db, scope)
    owner_map = {agent["id"]: agent for agent in owners}
    targets: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    seen: set[str] = set()
    for identity in selected:
        identity_key = str(identity.get("key", ""))
        if identity_key in seen:
            rejected.append({"key": identity_key, "agent": identity.get("agent", ""),
                             "status": "stale", "error": "A process identity was repeated"})
            continue
        seen.add(identity_key)
        match = next((process for process in processes
                      if _identity_matches(process, identity)), None)
        owner = owner_map.get(identity.get("agent"))
        if (request["action"] == "stop_owned" and not request.get("includeDescendants")
                and identity.get("agent") != scope):
            rejected.append({"key": identity.get("key", ""), "agent": identity.get("agent", ""),
                             "status": "stale", "error": "Worker processes are outside this stop scope"})
            continue
        if (request["action"] == "stop_selected" and not request.get("includeDescendants")
                and identity.get("agent") != scope):
            rejected.append({"key": identity.get("key", ""), "agent": identity.get("agent", ""),
                             "status": "stale", "error": "Worker processes are outside this stop scope"})
            continue
        if match is None or owner is None:
            rejected.append({"key": identity.get("key", ""), "agent": identity.get("agent", ""),
                             "status": "stale", "error": "This process changed or finished. Refresh the process list"})
            continue
        if (owner.get("accountKey", "default") != identity.get("accountKey")
                or owner.get("epoch") != identity.get("epoch")
                or not match["capabilities"]["stop"]):
            rejected.append({"key": match["key"], "agent": match["agent"], "status": "stale",
                             "error": "The process owner changed. Refresh the process list"})
            continue
        targets.append(match)
    return targets, rejected, None


def _server(rt: Any, process: dict[str, Any]) -> Any:
    account_key = process.get("_sourceAccountKey")
    connection_id = process.get("_sourceConnectionId")
    if not isinstance(account_key, str) or not account_key or not isinstance(connection_id, str):
        raise ValueError("The native process has no saved connection identity")
    with rt.lock, rt.db() as db:
        owner = rt.agent(process["agent"], db)
        if (account_key != owner.get("accountKey", "default")
                or owner.get("accountKey", "default") != process["accountKey"]
                or owner.get("epoch") != process["epoch"]):
            raise ValueError("The native process owner changed. Refresh the process list")
        if owner.get("threadId") != process.get("_sourceThreadId"):
            raise ValueError("The native process thread changed. Inspect the process state")
        if rt.agent_connection(owner) != connection_id:
            raise ValueError("The native process connection changed. Inspect the process state")
        server = rt.server_for(account_key, connection_id)
    if server is None:
        raise ValueError("The native process connection is unavailable")
    return server


def _stop_process(rt: Any, process: dict[str, Any]) -> dict[str, Any]:
    raw = process["_raw"]
    if process["source"] == "monitor":
        if raw.get("status") in {"starting", "approval"}:
            try:
                result = rt.cancel_monitor(
                    raw["id"], owner=process["agent"],
                    expected_account_key=process["_sourceAccountKey"],
                    expected_epoch=process["epoch"],
                    expected_monitor_epoch=process["_sourceEpoch"],
                    expected_status=raw["status"],
                )
            except ValueError as error:
                return {"status": "stale", "error": str(error)}
            except Exception as error:
                return {"status": "uncertain", "error": str(error)}
            return {"status": "stopped" if result.get("status") == "cancelled" else "already_finished"}
        if not all(isinstance(process.get(field), str) and process[field]
                   for field in ("_sourceAccountKey", "_sourceConnectionId", "_sourceThreadId")):
            raise ValueError("The monitor has no complete saved native identity")
        try:
            result = rt.cancel_monitor(
                raw["id"], owner=process["agent"],
                expected_account_key=process["_sourceAccountKey"],
                expected_epoch=process["epoch"],
                expected_connection_id=process["_sourceConnectionId"],
                expected_thread_id=process["_sourceThreadId"],
                expected_monitor_epoch=process["_sourceEpoch"],
                expected_status="running",
            )
        except ValueError as error:
            return {"status": "stale", "error": str(error)}
        except Exception as error:
            return {"status": "uncertain", "error": str(error)}
        if result.get("status") == "cancelled":
            return {"status": "stopped"}
        if result.get("cancelRequested"):
            if result.get("error"):
                return {"status": "uncertain", "error": result["error"]}
            return {"status": "requested", "error": result.get("error")}
        if result.get("status") in {"completed", "failed"}:
            return {"status": "already_finished"}
        if result.get("status") == "lost":
            return {"status": "uncertain", "error": result.get("error") or "The monitor outcome is unknown"}
        return {"status": "uncertain", "error": result.get("error") or "The monitor stop is not confirmed"}
    server = _server(rt, process)
    thread_id = process.get("_sourceThreadId")
    if not isinstance(thread_id, str) or not thread_id:
        raise ValueError("The native process has no saved thread identity")
    if process["source"] == "claude_task":
        task_id = str(raw.get("itemId", "")).removeprefix("task:")
        state = server.call("claude/state", {"threadId": thread_id})
        active = any(task.get("task_id") == task_id for task in state.get("tasks", []))
        if not active:
            return {"status": "already_finished"}
        if _server(rt, process) is not server:
            raise ValueError("The native process connection changed before stop")
        try:
            server.call("claude/stopTask", {"threadId": thread_id, "taskId": task_id}, timeout=20)
        except Exception as error:
            return {"status": "uncertain", "error": str(error)}
        try:
            state = server.call("claude/state", {"threadId": thread_id})
        except Exception as error:
            return {"status": "uncertain", "error": str(error)}
        if any(task.get("task_id") == task_id for task in state.get("tasks", [])):
            return {"status": "requested"}
        return {"status": "stopped"}
    process_id = str(raw.get("processId") or "")
    item_id = str(raw.get("itemId") or "")
    params: dict[str, Any] = {"threadId": thread_id, "limit": NATIVE_TERMINAL_ITEMS_PER_PAGE}
    found = False
    cursors: set[str] = set()
    for _ in range(NATIVE_TERMINAL_MAX_PAGES):
        page = server.call("thread/backgroundTerminals/list", params)
        if not isinstance(page.get("data"), list) or len(page["data"]) > NATIVE_TERMINAL_ITEMS_PER_PAGE:
            raise ValueError("Invalid native terminal list")
        if any(task.get("itemId") == item_id and str(task.get("processId")) == process_id
               for task in page.get("data", [])):
            found = True
            break
        cursor = page.get("nextCursor")
        if not cursor:
            break
        if not isinstance(cursor, str) or cursor in cursors:
            raise ValueError("Native terminal listing did not advance")
        cursors.add(cursor)
        params["cursor"] = cursor
    else:
        raise ValueError("Native terminal listing did not advance")
    if not found:
        return {"status": "already_finished"}
    if _server(rt, process) is not server:
        raise ValueError("The native process connection changed before stop")
    try:
        result = server.call("thread/backgroundTerminals/terminate",
                             {"threadId": thread_id, "processId": process_id})
    except Exception as error:
        return {"status": "uncertain", "error": str(error)}
    if result.get("terminated") is True:
        return {"status": "stopped"}
    return {"status": "uncertain", "error": "The provider did not confirm that the process stopped"}


def _background_command(rt: Any, request: dict[str, Any], target: dict[str, Any]) -> dict[str, Any]:
    """Ask the provider to move one running command of the current turn to the background."""
    item_id = request["itemId"]
    with rt.lock, rt.db() as db:
        current = rt.agent(request["agent"], db)
        account_key = current.get("accountKey", "default")
        connection_id = rt.agent_connection(current)
        if (account_key != target["accountKey"] or current.get("epoch") != target["epoch"]
                or connection_id != target["connectionId"]
                or current.get("threadId") != target["threadId"]
                or current.get("turnId") != target["turnId"] or not current.get("inFlight")):
            return {"status": "stale", "error": "The response changed. Refresh the conversation"}
        account = rt.accounts.get(account_key)
        provider = account.get("provider") if account else None
        if provider not in BACKGROUND_COMMAND_PROVIDERS:
            return {"status": "failed",
                    "error": "This provider cannot move a running command to the background"}
        server = rt.server_for(account_key, connection_id)
    if server is None:
        return {"status": "failed", "error": "The response connection is unavailable"}
    from codex_native_errors import NativeRpcError
    try:
        result = server.call("thread/commandExecution/background",
                             {"threadId": target["threadId"], "turnId": target["turnId"],
                              "itemId": item_id}, timeout=20)
    except NativeRpcError as error:
        # An explicit rejection did not move the command.
        message = error.error.get("message") if isinstance(error.error, dict) else None
        return {"status": "failed", "error": str(message or error)}
    except Exception as error:
        return {"status": "uncertain", "error": "The background move outcome is unknown: " + str(error)}
    status = result.get("status") if isinstance(result, dict) else None
    if status not in {"backgrounded", "not_running"}:
        return {"status": "uncertain", "error": "The provider returned an unknown background move result"}
    return {"status": status}


def _receipt_table(db: Any) -> None:
    db.execute("CREATE TABLE IF NOT EXISTS runtime_process_actions (id TEXT PRIMARY KEY, signature TEXT NOT NULL, record TEXT NOT NULL)")


def _save_receipt(rt: Any, request_id: str, record: dict[str, Any]) -> None:
    with rt.lock, rt.db() as db:
        _receipt_table(db)
        db.execute("UPDATE runtime_process_actions SET record=? WHERE id=?",
                   (_json(record), request_id))
    rt.changed.set()


def receipt(rt: Any, query: dict[str, Any]) -> dict[str, Any]:
    """Read an existing action receipt without creating tables or dispatching work."""
    with rt.lock, rt.read_db() as db:
        exists = db.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_process_actions'"
        ).fetchone()
        if not exists:
            return {"found": False, "result": None}
        row = db.execute(
            "SELECT record FROM runtime_process_actions WHERE id=?", (query["requestId"],)
        ).fetchone()
        if not row:
            return {"found": False, "result": None}
        record = json.loads(row[0])
        if (record.get("scopeAgent") != query["agent"]
                or record.get("scopeAccountKey") != query["accountKey"]
                or record.get("scopeEpoch") != query["epoch"]):
            return {"found": False, "result": None}
        return {"found": True, "result": record["result"]}


def action(rt: Any, request: dict[str, Any]) -> dict[str, Any]:
    request_id = request["requestId"]
    signature = _digest(request)
    with rt.lock, rt.db() as db:
        _receipt_table(db)
        prior = db.execute("SELECT signature,record FROM runtime_process_actions WHERE id=?",
                            (request_id,)).fetchone()
        if prior:
            if prior[0] != signature:
                raise ValueError("This process action ID has different content")
            record = json.loads(prior[1])
            if record["phase"] == "dispatching":
                for item in record["processes"]:
                    if item["status"] in {"dispatching", "pending"}:
                        item.update(status="uncertain", error="The previous stop reply is unknown. Do not repeat it")
                record["phase"] = "uncertain"
                record["result"]["processes"] = record["processes"]
                db.execute("UPDATE runtime_process_actions SET record=? WHERE id=?",
                           (_json(record), request_id))
                rt.changed.set()
                return cast(dict[str, Any], record["result"])
            return cast(dict[str, Any], record["result"])
        targets, rejected, response_target = _snapshot(rt, db, request)
        record = {
            "requestId": request_id,
            "signature": signature,
            "phase": "dispatching",
            "scopeAgent": request["agent"],
            "scopeAccountKey": request["accountKey"],
            "scopeEpoch": request["epoch"],
            "action": request["action"],
            "responseTarget": response_target,
            "processes": [{"key": item["key"], "agent": item["agent"], "source": item["source"],
                           "generation": item["generation"], "status": "pending"} for item in targets]
                        + rejected,
            "created": time.time(),
        }
        result = {"requestId": request_id, "status": "uncertain", "processes": record["processes"]}
        record["result"] = result
        db.execute("INSERT INTO runtime_process_actions(id,signature,record) VALUES(?,?,?)",
                   (request_id, signature, _json(record)))
    rt.changed.set()
    if request["action"] == "background_command":
        if response_target is None:
            raise ValueError("The response identity is unavailable")
        try:
            moved = _background_command(rt, request, response_target)
        except Exception as exception:
            moved = {"status": "uncertain", "error": str(exception)}
        status = moved["status"]
        result = {
            "requestId": request_id,
            "status": "uncertain" if status == "uncertain" else "completed",
            "processes": [],
            "response": {"agent": request["agent"], "itemId": request["itemId"], **moved},
        }
        record.update(phase="uncertain" if status == "uncertain" else "completed", result=result)
        _save_receipt(rt, request_id, record)
        return result
    if request["action"] == "stop_response":
        if response_target is None:
            raise ValueError("The response identity is unavailable")
        try:
            stopped = rt.stop_response(request["agent"],
                                       expected_account_key=request["accountKey"],
                                       expected_epoch=request["epoch"],
                                       expected_connection_id=response_target["connectionId"],
                                       expected_thread_id=response_target["threadId"],
                                       expected_turn_id=response_target["turnId"])
        except ValueError as exception:
            stopped = {"status": "stale", "error": str(exception)}
        except Exception as exception:
            stopped = {"status": "uncertain", "error": str(exception)}
        status = stopped.get("status", "uncertain")
        result = {
            "requestId": request_id,
            "status": "uncertain" if status == "uncertain" else "completed",
            "processes": [],
            "response": {"agent": request["agent"], "status": status,
                         **({"error": stopped["error"]} if stopped.get("error") else {})},
        }
        record.update(phase="uncertain" if status == "uncertain" else "completed", result=result)
        _save_receipt(rt, request_id, record)
        return result
    for target in targets:
        entry = next(item for item in record["processes"] if item["key"] == target["key"])
        entry["status"] = "dispatching"
        record["result"]["processes"] = record["processes"]
        _save_receipt(rt, request_id, record)
        try:
            dispatched = _stop_process(rt, target)
            entry.update(dispatched)
        except Exception as error:
            entry.update(status="failed", error=str(error))
        record["result"]["processes"] = record["processes"]
        _save_receipt(rt, request_id, record)
    statuses = [item["status"] for item in record["processes"]]
    if any(status == "uncertain" for status in statuses):
        result_status = "uncertain"
        record["phase"] = "uncertain"
    elif any(status in {"failed", "stale"} for status in statuses) and any(
        status in {"stopped", "requested", "already_finished"} for status in statuses
    ):
        result_status = "partial"
        record["phase"] = "completed"
    elif any(status in {"failed", "stale"} for status in statuses):
        result_status = "partial"
        record["phase"] = "completed"
    else:
        result_status = "completed"
        record["phase"] = "completed"
    result = {"requestId": request_id, "status": result_status,
              "processes": record["processes"]}
    record["result"] = result
    _save_receipt(rt, request_id, record)
    return result
