"""Durable dynamic-tool receipts and cancellation before execution."""

import hashlib
import json
import math
import time


def request_tools(tool, text):
    return [tool(
        "orchestration_request",
        "Recover your tool request by request_id or callId before retrying an uncertain mutation. "
        "List returns the latest 50 requests. Cancel prevents queued work from starting; "
        "running work continues and remains pending until its receipt arrives. "
        "A missing request has unknown outcome, not proof that it did not execute.",
        {"action": {"type": "string", "enum": ["list", "get", "cancel"]},
         "request_id": text}, ["action"],
    )]


def _arguments(message):
    params = message.get("params", {})
    args = params.get("arguments", {})
    if isinstance(args, str):
        args = json.loads(args)
    if not isinstance(args, dict):
        raise ValueError("Tool arguments must be an object")
    return params, args


def _identity_command(params, args):
    tool = params.get("tool")
    if tool == "orchestration_send" and args.get("agent_id") == "workspace":
        try:
            bridge = json.loads(args.get("text", ""))
            if isinstance(bridge, dict) and bridge.get("tool") == "orchestration_send" and isinstance(bridge.get("arguments"), dict):
                return bridge["tool"], bridge["arguments"]
        except (ValueError, TypeError):
            pass
    return tool, args


def _prefix(account, thread):
    return (str(account) + ":" if account != "default" else "") + str(thread) + ":"


def request_prefixes(actor):
    """Scopes recorded by the server, including completed account transfers."""
    scopes = [actor, *actor.get("accountHistory", [])]
    return list(dict.fromkeys(_prefix(scope.get("accountKey", "default"), scope["threadId"])
                             for scope in scopes if isinstance(scope, dict) and scope.get("threadId")))


_READ_TOOLS = {"orchestration_read", "orchestration_peers", "orchestration_search", "orchestration_chat_read"}
# These exact failures originate before mutation in model_page/model_work,
# work_action, model_read, and chat_message. Transport errors are excluded.
_WORK_REJECTIONS = {
    "limit must be 1 to 50", "List changed or cursor is invalid. Read the first page again.",
    "Unknown task in this team", "Unknown work item", "This work item changed. Reload before editing",
    "Only the lead can create work", "Only the lead can change work assignments",
    "Only the assigned worker can submit its result", "Only the lead can accept or reject a result",
    "This result is already accepted", "Submit a result before review", "This work item is not ready",
    "Another agent owns this work item", "Dependencies have not been accepted", "Unknown managed agent",
    "The file is outside this agent workspace", "Supply a source revision with 1 to 200 characters",
    "Supply a result with 1 to 32000 characters", "Supply test evidence with 1 to 32000 characters",
    "Work under review cannot be reassigned; reject its result before editing",
    "Review or accepted work cannot be reassigned; reject a result before editing",
}
_MESSAGE_REJECTIONS = {
    "Versioned progress requires a progress_key and nonnegative integer progress_version",
    "Unknown message importance", "Message must have 1 to 12000 characters", "Select another agent",
    "Unknown managed agent",
}


def request_read_only(tool, args):
    if tool == "orchestration_send" and args.get("agent_id") == "workspace":
        try:
            bridge = json.loads(args.get("text", ""))
            tool, args = bridge["tool"], bridge.get("arguments", {})
        except (ValueError, TypeError, KeyError):
            return False
    if not isinstance(args, dict):
        return False
    return (tool in _READ_TOOLS
            or tool in {"orchestration_task", "orchestration_result"} and args.get("action", "list") in {"list", "get", "read", "history"}
            or tool == "orchestration_request" and args.get("action", "list") in {"list", "get"})


def request_result_outcome(record, result):
    if result.get("success") is True:
        return "applied"
    if result.get("success") is not False:
        return "unknown"
    tool = record.get("tool")
    if record.get("readOnly") is True or tool in _READ_TOOLS:
        return "not_applied"
    texts = [item.get("text") for item in result.get("contentItems", [])
             if isinstance(item, dict) and item.get("type") == "inputText"]
    error = texts[0] if texts and isinstance(texts[0], str) else None
    if tool in {"orchestration_task", "orchestration_result"} and error in _WORK_REJECTIONS:
        return "not_applied"
    if tool == "orchestration_message" and error in _MESSAGE_REJECTIONS:
        return "not_applied"
    rejections = {
        "orchestration_monitor": {"Command timeout must be 1 second to 24 hours", "Supply a command with 1 to 12000 characters"},
        "orchestration_monitor_input": {"This interactive monitor is not active"},
        "orchestration_user_task": {"Wait for the user to check this task before accepting it"},
        "orchestration_watch": {"The file is outside this agent workspace", "Choose a date within the next year", "Interval must be 10 seconds to one year"},
        "orchestration_context": {"Unknown monitor in this team", "Unknown context topic"},
    }
    if error in rejections.get(tool, set()):
        return "not_applied"
    if tool == "orchestration_title" and error == "Only a lead can set a title of 1 to 80 characters":
        return "not_applied"
    # The legacy workspace bridge routes these errors only from read/validation
    # paths. Do not infer safety from arbitrary exception words or JSON output.
    if tool == "orchestration_send" and error in {
        "Output reference is not owned by this agent", "Unknown context topic",
        "There is no active turn to steer. Choose queue", "A workspace operation is active in this directory",
        "Unknown managed agent", "Unknown workspace tool", "'agent_id'",
        "request_id is not used by list", "limit must be 1 to 50",
        "List changed or cursor is invalid. Read the first page again.", "Unknown task in this team",
    }:
        return "not_applied"
    return "unknown"


def _cancel_result(record, message):
    return {"success": False, "contentItems": [{"type": "inputText", "text": json.dumps({
        "requestId": record["id"], "stage": "cancelled", "outcome": "not_applied",
        "cancelRequested": record.get("cancelRequested", False), "message": message,
    })}]}


def _spawned_ids(result):
    ids = []
    if not isinstance(result, dict):
        return ids
    for item in result.get("contentItems", []):
        if item.get("type") != "inputText":
            continue
        try:
            value = json.loads(item.get("text", ""))
        except (ValueError, TypeError):
            continue
        if not isinstance(value, dict):
            continue
        agents = value.get("agents", [])
        if isinstance(agents, list):
            ids.extend(a["id"] for a in agents if isinstance(a, dict) and isinstance(a.get("id"), str))
        for field in ("agentIds", "agent_ids"):
            if isinstance(value.get(field), list):
                ids.extend(a for a in value[field] if isinstance(a, str))
    return list(dict.fromkeys(ids))


def operation_receipt_evidence(db, key):
    """Read committed operation evidence without asserting tool completion."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_operation_receipts'").fetchone():
        return None
    row = db.execute("SELECT result FROM runtime_operation_receipts WHERE id=?", (key,)).fetchone()
    if row is None:
        return None
    return {"evidenceSource": "operation_receipt", "operationApplied": True,
            "operationResult": json.loads(row[0])}


class RequestMixin:
    def setup_tool_requests(self, db):
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_tool_request_actor
                ON runtime_tool_requests(json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC);
            CREATE TABLE IF NOT EXISTS runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
        """)
        # This setup runs once at server startup, before requests can execute.
        rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.stage') IN ('queued','running')").fetchall()
        for row in rows:
            record = json.loads(row[0])
            cached = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (record["id"],)).fetchone()
            if cached:
                self.finish_tool_request(record["id"], json.loads(cached[0]), db=db)
                continue
            record["updated"] = time.time()
            if record["stage"] == "queued":
                record.update(stage="cancelled", outcome="not_applied", error="Server restarted before execution")
                record["result"] = _cancel_result(record, record["error"])
            else:
                record.update(stage="interrupted", outcome="unknown", error="Server restarted before the execution receipt")
            self.put(db, "tool_requests", record)

    @staticmethod
    def tool_request_key(message, account_key="default"):
        params, args = _arguments(message)
        call = str(params.get("callId", message.get("id")))
        name, identity_args = _identity_command(params, args)
        if name in {"orchestration_spawn", "orchestration_send"} and "request_id" in identity_args:
            request_id = identity_args["request_id"]
            if (not isinstance(request_id, str) or not 1 <= len(request_id) <= 200
                    or request_id != request_id.strip() or any(ord(c) < 32 for c in request_id)):
                raise ValueError("Supply request_id with 1 to 200 characters and no surrounding whitespace")
            call = ("spawn:" if name == "orchestration_spawn" else "send:") + request_id
        return _prefix(account_key, params.get("threadId")) + call

    def tool_request(self, key, db=None):
        if db is None:
            with self.lock, self.db() as own:
                return self.tool_request(key, own)
        row = db.execute("SELECT record FROM runtime_tool_requests WHERE id=?", (key,)).fetchone()
        return json.loads(row[0]) if row else None

    def transcript_tool_result(self, db, actor, item):
        """Recover a missing native completion from the exact durable receipt."""
        if item.get("title") != "dynamicToolCall":
            return
        try:
            payload = json.loads(item["text"])
        except (ValueError, TypeError):
            return
        if not isinstance(payload, dict) or not isinstance(payload.get("id"), str):
            return
        projected = False
        # Native in-progress calls can explicitly carry contentItems=null.
        contents = payload.get("contentItems")
        for content in contents if isinstance(contents, list) else []:
            if not isinstance(content, dict) or content.get("type") != "inputText":
                continue
            try:
                value = json.loads(content.get("text", ""))
                projected = projected or (isinstance(value, dict) and value.get("truncated") is True and isinstance(value.get("outputRef"), str))
            except (ValueError, TypeError):
                pass
        if not projected and (payload.get("status") not in (None, "inProgress") or payload.get("success") is not None):
            return
        aliases = db.execute(
            "SELECT request FROM runtime_tool_request_aliases WHERE agent=? AND alias=? LIMIT 2",
            (actor["id"], payload["id"]),
        ).fetchall()
        if len(aliases) > 1:
            return
        receipt_id = aliases[0][0] if aliases else (
            _prefix(actor.get("accountKey", "default"), actor.get("threadId")) + payload["id"])
        row = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (receipt_id,)).fetchone()
        if row is None:
            return
        result = json.loads(row[0])
        if type(result.get("success")) is not bool:
            return
        # Tool completion does not prove that a mutation was applied. Preserve
        # its exact response, including any uncertainty and recovery instructions.
        item["toolStatus"] = "completed" if result["success"] else "failed"
        content = result.get("contentItems", [])
        encoded = json.dumps(content, ensure_ascii=False)
        # Model projection must not reduce the existing UI transcript allowance.
        limit = 20000 if projected else 12000
        if len(encoded) > limit:
            content = [{"type": "inputText", "text": encoded[:limit] + "\n[Result truncated]"}]
            item["truncated"] = True
        payload.update(status=item["toolStatus"], success=result["success"],
                       contentItems=content)
        item["text"] = json.dumps(payload, ensure_ascii=False)

    def reserve_tool_request(self, message, account_key="default", connection_id=None):
        params, args = _arguments(message)
        key = self.tool_request_key(message, account_key)
        identity_tool, identity_args = _identity_command(params, args)
        signature = hashlib.sha256(json.dumps(
            {"tool": identity_tool, "arguments": identity_args}, sort_keys=True,
            separators=(",", ":"), ensure_ascii=False, allow_nan=False,
        ).encode()).hexdigest()
        with self.lock, self.db() as db:
            if self.closed or not self.connection_current(account_key, connection_id):
                raise ValueError("The caller connection changed before request reservation")
            actor = next((a for a in self.records(db, "agents")
                          if params.get("threadId") and a.get("threadId") == params["threadId"]
                          and a.get("accountKey", "default") == account_key), None)
            if actor is None or actor.get("deletedAt"):
                raise ValueError("Unknown managed agent")
            record = self.tool_request(key, db)
            if record and (record["signature"] != signature or record["agent"] != actor["id"]):
                raise ValueError("This request id has different content")
            call_id = str(params.get("callId", message.get("id")))
            if not record:
                now = time.time()
                record = {"id": key, "agent": actor["id"], "threadId": params["threadId"],
                          "callId": call_id, "tool": params.get("tool"), "signature": signature,
                          "rpcId": message.get("id"), "turnId": params.get("turnId"), "epoch": actor.get("epoch"),
                          "accountKey": account_key, "connectionId": connection_id,
                          "created": now, "updated": now, "stage": "queued", "outcome": "pending",
                          "cancelRequested": False, "readOnly": request_read_only(params.get("tool"), args)}
                received = message.get("_studioReceivedAt")
                if type(received) in (int, float) and math.isfinite(received):
                    record["wireReceivedAt"] = received
                    record["admissionDelayMs"] = max(0, now - received) * 1000
                dispatched = message.get("_studioDispatchedAt")
                if type(dispatched) in (int, float) and math.isfinite(dispatched):
                    record["callbackStartedAt"] = dispatched
                    record["reservationDelayMs"] = max(0, now - dispatched) * 1000
                    if "wireReceivedAt" in record:
                        record["callbackQueueDelayMs"] = max(0, dispatched - received) * 1000
                if identity_tool in {"orchestration_spawn", "orchestration_send"} and "request_id" in identity_args:
                    record["request_id"] = identity_args["request_id"]
                self.put(db, "tool_requests", record)
                cached = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (key,)).fetchone()
                if cached:
                    record = self.finish_tool_request(key, json.loads(cached[0]), db=db)
            aliases = [call_id, _prefix(account_key, params["threadId"]) + call_id]
            if record.get("request_id"):
                aliases.append(record["request_id"])
            for alias in aliases:
                # Preserve aliases across retries. An ambiguous short identifier
                # requires the canonical key instead of selecting another operation.
                db.execute("INSERT OR IGNORE INTO runtime_tool_request_aliases VALUES (?,?,?)",
                           (actor["id"], alias, key))
            return record

    def begin_tool_request(self, key):
        with self.lock, self.db() as db:
            record = self.tool_request(key, db)
            if not record or record["stage"] != "queued" or record.get("cancelRequested"):
                return False
            now = time.time()
            record.update(stage="running", updated=now, started=now)
            record["executionQueueDelayMs"] = max(0, now - record["created"]) * 1000
            if "wireReceivedAt" in record:
                record["queueDelayMs"] = max(0, now - record["wireReceivedAt"]) * 1000
            self.put(db, "tool_requests", record)
            return True

    def finish_tool_request(self, key, result, outcome=None, db=None):
        if db is None:
            with self.lock, self.db() as own:
                return self.finish_tool_request(key, result, outcome, own)
        if outcome not in (None, "applied", "not_applied", "unknown"):
            raise ValueError("Invalid request outcome")
        record = self.tool_request(key, db)
        if record is None:
            raise ValueError("Unknown tool request")
        # Definitive receipts cannot be replaced by a late transport error.
        if record["outcome"] in {"applied", "not_applied"}:
            return record
        if outcome is None:
            outcome = request_result_outcome(record, result)
            if outcome == "not_applied" and operation_receipt_evidence(db, key):
                # A committed operation overrides an inferred rejection. Keep its
                # final error and uncertainty; recovery exposes the commit.
                outcome = "unknown"
        if record["outcome"] == outcome and record.get("result") == result:
            return record
        now = time.time()
        record.update(stage="completed" if outcome == "applied" else "failed",
                      outcome=outcome, result=result, updated=now, finished=record.get("finished", now))
        record.pop("error", None)
        if record.get("tool") == "orchestration_spawn":
            ids = _spawned_ids(result)
            if ids:
                record["agentIds"] = ids
        self.put(db, "tool_requests", record)
        return record

    def _legacy_tool_request(self, db, actor, request_id):
        prefixes = request_prefixes(actor)
        candidates = ([request_id] if any(request_id.startswith(prefix) for prefix in prefixes)
                      else [prefix + request_id for prefix in prefixes])
        found = []
        for key in candidates:
            owner = self.tool_request(key, db)
            if owner and owner.get("agent") != actor["id"]:
                continue
            row = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (key,)).fetchone()
            evidence = operation_receipt_evidence(db, key) if not row else None
            if row or evidence:
                found.append((key, row, evidence))
        if len(found) != 1:
            if found:
                return {"id": request_id, "agent": actor["id"], "legacy": True,
                        "stage": "ambiguous", "outcome": "unknown",
                        "message": "Use the full request id; this call id exists in several account histories."}
            return None
        key, row, evidence = found[0]
        prefix = next(prefix for prefix in prefixes if key.startswith(prefix))
        if not row:
            return {"id": key, "agent": actor["id"], "threadId": actor.get("threadId"),
                    "callId": key[len(prefix):], "stage": "unknown", "outcome": "unknown",
                    "cancelRequested": False, "legacy": True, **evidence,
                    "message": "The operation receipt is committed. The final tool result is unavailable; do not repeat the operation."}
        result = json.loads(row[0])
        return {"id": key, "agent": actor["id"], "threadId": actor.get("threadId"),
                "callId": key[len(prefix):], "stage": "completed" if result.get("success") is True else "failed",
                "outcome": "applied" if result.get("success") is True else "unknown", "result": result,
                "cancelRequested": False, "legacy": True, "agentIds": _spawned_ids(result)}

    def _refresh_tool_request(self, db, record):
        if record["outcome"] not in {"applied", "not_applied"}:
            cached = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (record["id"],)).fetchone()
            result = json.loads(cached[0]) if cached else record.get("result")
            if isinstance(result, dict):
                return self.finish_tool_request(record["id"], result, db=db)
        return record

    def reconcile_tool_requests(self, db, agent_id):
        rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "AND json_extract(record,'$.outcome') NOT IN ('applied','not_applied')", (agent_id,)).fetchall()
        reconciled, unknown = [], []
        for row in rows:
            before = json.loads(row[0])
            after = self._refresh_tool_request(db, before)
            if after["outcome"] in {"applied", "not_applied"}:
                reconciled.append(after["id"])
            elif after["outcome"] == "unknown":
                unknown.append(after["id"])
        return {"reconciled": reconciled, "unknown": unknown}

    def _request_agent_states(self, db, record):
        agents = []
        for agent_id in record.get("agentIds", []):
            row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
            agent = json.loads(row[0]) if row else None
            if not agent or agent.get("parentId") != record["agent"]:
                agents.append({"id": agent_id, "absent": True})
                continue
            agents.append({**{field: agent.get(field) for field in ("id", "name", "status", "turnId", "model")},
                           "deleted": bool(agent.get("deletedAt"))})
        return agents

    def request_action(self, actor_id, data):
        if not isinstance(data, dict) or set(data) - {"action", "request_id"}:
            raise ValueError("Unsupported request fields")
        action = data.get("action", "list")
        if action not in {"list", "get", "cancel"}:
            raise ValueError("Unknown request action")
        request_id = data.get("request_id")
        if action != "list" and (not isinstance(request_id, str) or not 1 <= len(request_id) <= 1000):
            raise ValueError("Supply request_id")
        if action == "list" and request_id is not None:
            raise ValueError("request_id is not used by list")
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id)
            if action == "list":
                rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                                  "ORDER BY json_extract(record,'$.updated') DESC LIMIT 50", (actor_id,)).fetchall()
                return {"requests": [{k: v for k, v in self._refresh_tool_request(db, json.loads(row[0])).items()
                                      if k not in {"result", "signature"}} for row in rows]}
            record = self.tool_request(request_id, db)
            if record and record["agent"] != actor_id:
                record = None
            if record is None:
                aliases = db.execute("SELECT request FROM runtime_tool_request_aliases WHERE agent=? AND alias=? LIMIT 2",
                                     (actor_id, request_id)).fetchall()
                if len(aliases) > 1:
                    return {"id": request_id, "stage": "ambiguous", "outcome": "unknown",
                            "message": "Use the canonical request id from list; this short id names several requests."}
                record = self.tool_request(aliases[0][0], db) if aliases else None
            if record is None:
                record = self._legacy_tool_request(db, actor, request_id)
            if record is None:
                return {"id": request_id, "stage": "not_found", "outcome": "unknown",
                        "message": "No receipt found. This does not prove that the operation did not execute."}
            if not record.get("legacy"):
                record = self._refresh_tool_request(db, record)
            if action == "cancel" and record["stage"] in {"queued", "running"}:
                record.update(cancelRequested=True, updated=time.time())
                if record["stage"] == "queued":
                    record.update(stage="cancelled", outcome="not_applied", finished=time.time())
                    record["result"] = _cancel_result(record, "Cancelled before execution")
                self.put(db, "tool_requests", record)
            result = {k: v for k, v in record.items() if k != "signature"}
            if action == "get" and record.get("outcome") not in {"applied", "not_applied"} and "operationResult" not in record:
                result.update(operation_receipt_evidence(db, record["id"]) or {})
            if action == "get" and (record.get("tool") == "orchestration_spawn"
                                    or (record.get("legacy") and record.get("agentIds"))):
                # The receipt remains immutable. These observations show the
                # current registry state, not the state when creation committed.
                result["agents"] = self._request_agent_states(db, record)
                result["registryObservedAt"] = time.time()
            return result
