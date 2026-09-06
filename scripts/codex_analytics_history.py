"""Bounded, resumable metrics import from each managed thread's native rollout.

The runtime is the only writer. File reads happen outside its transaction lock.
Checkpoints contain identities and counters, never prompts or tool output.
"""

import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import threading
import time


TOKEN_FIELDS = {
    "input_tokens": "inputTokens", "cached_input_tokens": "cachedInputTokens",
    "cache_write_input_tokens": "cacheWriteInputTokens", "output_tokens": "outputTokens",
    "reasoning_output_tokens": "reasoningOutputTokens", "total_tokens": "totalTokens",
}
ITEM_FIELDS = {
    "process_id": "processId", "parsed_cmd": "commandActions",
    "aggregated_output": "aggregatedOutput", "exit_code": "exitCode",
    "content_items": "contentItems", "client_id": "clientId",
    "summary_text": "summary", "raw_content": "content",
    "formatted_output": "formattedOutput",
}
MAX_LINE_BYTES = 64 * 1024 * 1024


def timestamp(value, fallback):
    if isinstance(value, (float, int)) and not isinstance(value, bool):
        return value if math.isfinite(value) and value >= 0 else fallback
    if isinstance(value, str):
        try:
            parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
            return parsed.timestamp() if parsed.tzinfo is not None else fallback
        except ValueError:
            pass
    return fallback


def tokens(value):
    return {TOKEN_FIELDS.get(k, k): v for k, v in (value or {}).items()}


def duration_ms(value):
    if isinstance(value, dict):
        return (value.get("secs", 0) * 1000) + value.get("nanos", 0) / 1_000_000
    return value


def normalize_item(item):
    result = {ITEM_FIELDS.get(k, k): v for k, v in item.items()}
    kind = result.get("type", "")
    result["type"] = kind[:1].lower() + kind[1:]
    if "duration" in result:
        result["durationMs"] = duration_ms(result.pop("duration"))
    if result["type"] == "agentMessage" and "text" not in result:
        content = result.get("content", [])
        if isinstance(content, str):
            result["text"] = content
        elif isinstance(content, list):
            result["text"] = "\n".join(c.get("text", "") for c in content if isinstance(c, dict))
    return result


def rollout_actions(record, context, identity, fallback_at):
    """Return collector operations. Context is small and checkpointed atomically."""
    if not isinstance(record, dict) or not isinstance(record.get("payload"), dict):
        return []
    kind, p = record.get("type"), record["payload"]
    at = timestamp(record.get("timestamp"), fallback_at)
    time_source = "record" if timestamp(record.get("timestamp"), None) is not None else "fileModifiedEstimate"
    actions = []
    if kind == "session_meta":
        context["window"] = p.get("context_window", context.get("window"))
    elif kind == "turn_context":
        for src, dest in (("turn_id", "turnId"), ("model", "model"), ("effort", "effort")):
            if p.get(src) is not None:
                context[dest] = p[src]
    elif kind == "token_usage_record":
        if p.get("thread_id") and p["thread_id"] != context["threadId"]:
            return [("coverage", "wrongThreadRecord", {}, at)]
        context["pendingUsage"] = {"responseId": p.get("response_id"), "turnId": p.get("turn_id") or context.get("turnId"),
                                   "usage": tokens(p.get("usage"))}
        context["requestUsageAvailable"] = True
        usage = {"total": tokens(p.get("thread_token_usage")),
                 "last": tokens(p.get("usage")), "modelContextWindow": context.get("window")}
        actions.append(("event", "thread/tokenUsage/updated", {
            "threadId": context["threadId"], "turnId": p.get("turn_id") or context.get("turnId"),
            "responseId": p.get("response_id"), "tokenUsage": usage,
            "turnUsage": tokens(p.get("turn_token_usage")), "requestUsage": tokens(p.get("usage")),
            "rawTokenUsageRecord": p, "usageSource": "responseRecord", "_analyticsUsageKind": "response",
        }, at))
    elif kind == "response_item":
        metadata = p.get("internal_chat_message_metadata_passthrough") or {}
        body = {**p, "_analyticsId": identity}
        body["_analyticsTurnId"] = metadata.get("turn_id") or context.get("turnId")
        actions.append(("payload", None, body, timestamp(metadata.get("create_time"), at)))
    elif kind == "compacted":
        metadata = {k: p.get(k) for k in ("window_number", "first_window_id", "previous_window_id", "window_id", "compaction_response_id")}
        actions.append(("event", "analytics/compaction", {
            "threadId": context["threadId"], "turnId": context.get("turnId"),
            "id": identity, **metadata,
        }, at))
        for key, category in (("replacement_history", "compactionReplacement"), ("guardian_history", "compactionGuardian")):
            for index, item in enumerate(p.get(key) or []):
                if isinstance(item, dict):
                    actions.append(("payload", None, {**item, "_analyticsCategory": category,
                        "_analyticsId": f"{identity}:{key}:{index}", "_analyticsTurnId": context.get("turnId")}, at))
        if isinstance(p.get("latest_token_usage_record"), dict):
            actions.extend(rollout_actions({"type": "token_usage_record", "payload": p["latest_token_usage_record"],
                                           "timestamp": record.get("timestamp")}, context, identity + ":usage", at))
    elif kind == "event_msg":
        event = p.get("type")
        if p.get("thread_id") and p["thread_id"] != context["threadId"]:
            return [("coverage", "wrongThreadRecord", {}, at)]
        if event == "task_started":
            context["turnId"] = p.get("turn_id")
            context["window"] = p.get("model_context_window", context.get("window"))
            actions.append(("event", "turn/started", {"threadId": context["threadId"],
                "turnId": p.get("turn_id"), "turn": {"id": p.get("turn_id")}}, at))
        elif event in ("task_complete", "turn_aborted"):
            actions.append(("event", "turn/completed", {"threadId": context["threadId"],
                "turnId": p.get("turn_id"), "turn": {"id": p.get("turn_id"),
                "status": "interrupted" if event == "turn_aborted" else "completed"},
                "durationMs": p.get("duration_ms"), "timeToFirstTokenMs": p.get("time_to_first_token_ms")}, at))
        elif event in ("item_started", "item_completed") and isinstance(p.get("item"), dict):
            item = normalize_item(p["item"])
            normalized = {"threadId": context["threadId"], "turnId": p.get("turn_id") or context.get("turnId"), "item": item}
            if p.get("started_at_ms") is not None:
                normalized["startedAt"] = p["started_at_ms"] / 1000
            if p.get("completed_at_ms") is not None:
                normalized["completedAt"] = p["completed_at_ms"] / 1000
            actions.append(("event", "item/started" if event == "item_started" else "item/completed", normalized, at))
        elif event == "token_count":
            info = p.get("info")
            if isinstance(info, dict):
                total, last = tokens(info.get("total_token_usage")), tokens(info.get("last_token_usage"))
                pending = context.get("pendingUsage") or {}
                previous = context.get("noticeAssociation") or {}
                response_id = None
                # A notice follows the exact request record. Match only this
                # pending response, then consume it; equal-size later requests
                # must keep their own provider response IDs.
                fields = ("inputTokens", "cachedInputTokens", "outputTokens", "reasoningOutputTokens", "totalTokens")
                matches = (pending.get("responseId") and pending.get("turnId") == context.get("turnId")
                           and all(field in last and last[field] == pending.get("usage", {}).get(field) for field in fields))
                if matches:
                    response_id = pending["responseId"]
                    context["pendingUsage"] = None
                    context["noticeAssociation"] = {"responseId": response_id, "turnId": context.get("turnId"), "total": total, "last": last}
                elif previous.get("turnId") == context.get("turnId") and previous.get("total") == total and previous.get("last") == last:
                    response_id = previous.get("responseId")
                actions.append(("event", "thread/tokenUsage/updated", {
                    "threadId": context["threadId"], "turnId": context.get("turnId"),
                    "responseId": response_id, "usageSource": "tokenCount", "_analyticsUsageKind": "notice",
                    "requestUsageAvailable": context.get("requestUsageAvailable", False),
                    "tokenUsage": {"total": total, "last": last,
                                   "modelContextWindow": info.get("model_context_window", context.get("window"))},
                }, at))
            if p.get("rate_limits") is not None:
                actions.append(("event", "analytics/rateLimits", {"rateLimits": p["rate_limits"]}, at))
    for _, _, body, event_at in actions:
        body.setdefault("_analyticsTimestampSource", "metadata" if event_at != at else time_source)
    return actions


class AnalyticsHistoryMixin:
    def analytics_history_init(self, db):
        db.execute("CREATE TABLE IF NOT EXISTS analytics_history (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)")
        self._analytics_history_guard = threading.Lock()
        self._analytics_history_cursor = 0
        self._analytics_history_paths = {}

    def analytics_history_start(self):
        def run():
            while not self.closed:
                try:
                    advanced = self.analytics_history_step()
                except Exception:
                    # An importer failure cannot interrupt agents or commands.
                    # Persist a safe error, without exception text containing data.
                    with self.lock, self.db() as db:
                        db.execute("INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)", (
                            "importer", "", json.dumps({"status": "error", "error": "History importer failed", "updated": time.time()})))
                    advanced = False
                if not advanced and self._analytics_history_cursor == 0:
                    # Small bounded waits permit prompt shutdown.
                    for _ in range(20):
                        if self.closed:
                            return
                        time.sleep(0.25)
        self.analytics_history_thread = threading.Thread(target=run, daemon=True, name="analytics-history")
        self.analytics_history_thread.start()

    def _analytics_rollout_path(self, home, thread_id):
        now = time.monotonic()
        cache = self._analytics_history_paths.get(str(home))
        if cache is None or now - cache[0] > 30:
            paths = {}
            for folder in ("sessions", "archived_sessions"):
                root = home / folder
                if root.is_dir():
                    for path in root.rglob("*.jsonl"):
                        # Native UUID suffix. Do not infer another account home.
                        suffix = path.stem[-36:]
                        paths.setdefault(suffix, []).append(path)
            cache = (now, paths)
            self._analytics_history_paths[str(home)] = cache
        matches = cache[1].get(thread_id, [])
        if len(matches) != 1:
            return None, "ambiguous" if matches else "missing"
        path = matches[0]
        try:
            if not path.resolve().is_relative_to(home.resolve()):
                return None, "outsideProfile"
        except OSError:
            return None, "unreadable"
        return path, None

    def analytics_history_step(self, max_bytes=1024 * 1024, max_records=128):
        """Import one fair batch. Return whether complete lines advanced."""
        if not self._analytics_history_guard.acquire(blocking=False):
            return False
        try:
            with self.lock, self.db() as db:
                agents = [a for a in self.records(db, "agents") if a.get("threadId")]
            if not agents:
                return False
            self._analytics_history_cursor %= len(agents)
            a = agents[self._analytics_history_cursor]
            self._analytics_history_cursor = (self._analytics_history_cursor + 1) % len(agents)
            key = a["id"] + ":" + a.get("accountKey", "default") + ":" + a["threadId"]
            with self.lock, self.db() as db:
                row = db.execute("SELECT record FROM analytics_history WHERE id=?", (key,)).fetchone()
            state = json.loads(row[0]) if row else {
                "id": key, "agent": a["id"], "accountKey": a.get("accountKey", "default"),
                "threadId": a["threadId"], "deletedAt": a.get("deletedAt"), "offset": 0, "importedRecords": 0, "malformedLines": 0,
                "context": {"threadId": a["threadId"]},
            }
            state["deletedAt"] = a.get("deletedAt")
            try:
                try:
                    home = Path(self.accounts.home(a.get("accountKey", "default")))
                except ValueError:
                    state.update(status="profileUnavailable", error="The managed account profile is unavailable")
                    return self._analytics_history_save(key, a, state)
                path, problem = self._analytics_rollout_path(home, a["threadId"])
                if problem:
                    state.update(status=problem, error="Native rollout " + problem)
                    return self._analytics_history_save(key, a, state)
                info = path.stat()
                identity = [info.st_dev, info.st_ino]
                if state.get("identity") and (state["identity"] != identity or info.st_size < state["offset"]):
                    state.update(status="identityChanged", error="Native rollout was replaced or truncated; previous checkpoint retained")
                    return self._analytics_history_save(key, a, state)
                state.update(path=str(path), identity=identity, fileBytes=info.st_size)
                records, consumed, partial, oversized = [], 0, False, False
                with path.open("rb") as handle:
                    opened = os.fstat(handle.fileno())
                    if [opened.st_dev, opened.st_ino] != identity:
                        state.update(status="identityChanged", error="Native rollout changed during open")
                        return self._analytics_history_save(key, a, state)
                    if not state.get("validated"):
                        first = handle.readline(MAX_LINE_BYTES + 1)
                        try:
                            header = json.loads(first)
                            payload = header.get("payload", {})
                            valid = header.get("type") == "session_meta" and (payload.get("id") or payload.get("session_id")) == a["threadId"]
                        except (ValueError, AttributeError):
                            valid = False
                        if not valid:
                            state.update(status="wrongThread", error="Native rollout header does not match the managed thread")
                            return self._analytics_history_save(key, a, state)
                        state["validated"] = True
                    if state.get("anchor"):
                        handle.seek(max(0, state["offset"] - 256))
                        anchor = hashlib.sha256(handle.read(min(state["offset"], 256))).hexdigest()
                        if anchor != state["anchor"]:
                            state.update(status="identityChanged", error="Native rollout checkpoint bytes changed")
                            return self._analytics_history_save(key, a, state)
                    handle.seek(state["offset"])
                    while len(records) < max_records and consumed < max_bytes:
                        offset = handle.tell()
                        line = handle.readline(MAX_LINE_BYTES + 1)
                        if not line:
                            break
                        if len(line) > MAX_LINE_BYTES:
                            oversized = True
                            break
                        if not line.endswith(b"\n"):
                            partial = True
                            break
                        consumed += len(line)
                        try:
                            record = json.loads(line)
                            if not isinstance(record, dict):
                                raise ValueError()
                        except (ValueError, UnicodeDecodeError):
                            state["malformedLines"] += 1
                            record = None
                        records.append((offset, record))
                    next_offset = state["offset"] + consumed
                    handle.seek(max(0, next_offset - 256))
                    state["anchor"] = hashlib.sha256(handle.read(min(next_offset, 256))).hexdigest()
                # Parsing and metrics computation can include large results; the
                # collector runs in bounded record groups to release the writer.
                context = state["context"]
                collected = []
                for offset, record in records:
                    if record is not None:
                        identity_key = hashlib.sha256((key + ":" + str(identity) + ":" + str(offset)).encode()).hexdigest()
                        for action in rollout_actions(record, context, identity_key, info.st_mtime):
                            collected.append((action, dict(context)))
                with self.lock, self.db() as db:
                    current = self.agent(a["id"], db)
                    if current.get("threadId") != a["threadId"] or current.get("accountKey", "default") != a.get("accountKey", "default"):
                        return False
                    for (action, method, p, at), event_context in collected:
                        event_agent = {**a, "turnId": event_context.get("turnId"),
                                       "model": event_context.get("model"), "effort": event_context.get("effort")}
                        if action == "event":
                            self.analytics_event(db, event_agent, method, p, at=at, source="rollout")
                        elif action == "payload":
                            self.analytics_model_payload(db, event_agent, p, at=at,
                                turn_id=p.get("_analyticsTurnId"), source="rollout")
                        elif action == "coverage":
                            state[method] = state.get(method, 0) + 1
                    state.update(offset=next_offset, importedRecords=state["importedRecords"] + len(records),
                                 updated=time.time(), status="oversizedLine" if oversized else "partialLine" if partial else "catchingUp" if next_offset < info.st_size else "current")
                    state["error"] = "Native rollout line exceeds 64 MiB; checkpoint retained" if oversized else None
                    if state["malformedLines"] or state.get("wrongThreadRecord"):
                        state["coverage"] = "partial"
                    else:
                        state["coverage"] = "availableRecords"
                    db.execute("INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)", (key, a["id"], json.dumps(state)))
                return bool(consumed)
            except OSError:
                state.update(status="unreadable", error="Cannot read the managed account's native rollout")
                return self._analytics_history_save(key, a, state)
        finally:
            self._analytics_history_guard.release()

    def _analytics_history_save(self, key, a, state):
        state["updated"] = time.time()
        with self.lock, self.db() as db:
            db.execute("INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)", (key, a["id"], json.dumps(state)))
        return False
