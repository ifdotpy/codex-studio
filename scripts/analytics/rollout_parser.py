"""Translate decoded native rollout records into analytics collector actions.

No filesystem, database, runtime, or provider dependencies. rollout_actions
updates the caller-owned context in record order; use a fresh context per replay.
Input records are not modified. Returned payloads may share nested input values.
"""
import datetime
import math


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
        if p.get("thread_id") and p["thread_id"] != context["threadId"] and p["thread_id"] not in context.get("allowedSourceThreadIds", []):
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
        if p.get("thread_id") and p["thread_id"] != context["threadId"] and p["thread_id"] not in context.get("allowedSourceThreadIds", []):
            return [("coverage", "wrongThreadRecord", {}, at)]
        if event == "task_started":
            context["turnId"] = p.get("turn_id")
            context["window"] = p.get("model_context_window", context.get("window"))
            actions.append(("event", "turn/started", {"threadId": context["threadId"],
                "turnId": p.get("turn_id"), "turn": {"id": p.get("turn_id")}}, at))
        elif event in ("task_complete", "turn_aborted"):
            actions.append(("event", "turn/completed", {"threadId": context["threadId"],
                "turnId": p.get("turn_id") or context.get("turnId"),
                "turn": {"id": p.get("turn_id") or context.get("turnId"),
                "status": "failed" if p.get("error") else "interrupted" if event == "turn_aborted" else p.get("status") or "completed",
                "error": p.get("error")},
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


