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


def _prefix(account, thread):
    return (str(account) + ":" if account != "default" else "") + str(thread) + ":"


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
        if params.get("tool") == "orchestration_spawn" and "request_id" in args:
            request_id = args["request_id"]
            if (not isinstance(request_id, str) or not 1 <= len(request_id) <= 200
                    or request_id != request_id.strip() or any(ord(c) < 32 for c in request_id)):
                raise ValueError("Supply request_id with 1 to 200 characters and no surrounding whitespace")
            call = "spawn:" + request_id
        return _prefix(account_key, params.get("threadId")) + call

    def tool_request(self, key, db=None):
        if db is None:
            with self.lock, self.db() as own:
                return self.tool_request(key, own)
        row = db.execute("SELECT record FROM runtime_tool_requests WHERE id=?", (key,)).fetchone()
        return json.loads(row[0]) if row else None

    def reserve_tool_request(self, message, account_key="default", connection_id=None):
        params, args = _arguments(message)
        key = self.tool_request_key(message, account_key)
        signature = hashlib.sha256(json.dumps(
            {"tool": params.get("tool"), "arguments": args}, sort_keys=True,
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
                          "cancelRequested": False}
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
                if params.get("tool") == "orchestration_spawn" and "request_id" in args:
                    record["request_id"] = args["request_id"]
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
        outcome = outcome or ("applied" if result.get("success") is True else "unknown")
        if record["outcome"] == outcome and record.get("result") == result:
            return record
        record.update(stage="completed" if outcome == "applied" else "failed",
                      outcome=outcome, result=result, updated=time.time(), finished=time.time())
        record.pop("error", None)
        if record.get("tool") == "orchestration_spawn":
            ids = _spawned_ids(result)
            if ids:
                record["agentIds"] = ids
        self.put(db, "tool_requests", record)
        return record

    def _legacy_tool_request(self, db, actor, request_id):
        prefix = _prefix(actor.get("accountKey", "default"), actor.get("threadId"))
        key = request_id if request_id.startswith(prefix) else prefix + request_id
        row = db.execute("SELECT result FROM runtime_tool_results WHERE id=?", (key,)).fetchone()
        if not row:
            evidence = operation_receipt_evidence(db, key)
            if evidence is None:
                return None
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
            if cached:
                return self.finish_tool_request(record["id"], json.loads(cached[0]), db=db)
        return record

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
            if action == "get" and "result" not in record and "operationResult" not in record:
                result.update(operation_receipt_evidence(db, record["id"]) or {})
            if action == "get" and (record.get("tool") == "orchestration_spawn"
                                    or (record.get("legacy") and record.get("agentIds"))):
                # The receipt remains immutable. These observations show the
                # current registry state, not the state when creation committed.
                result["agents"] = self._request_agent_states(db, record)
                result["registryObservedAt"] = time.time()
            return result
