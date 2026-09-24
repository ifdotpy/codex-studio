"""Parse Claude Code assistant usage rows for account and team estimates."""
import json
from datetime import datetime


def parse_claude_usage(path):
    rows = []
    with open(path, "r", encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            message = row.get("message") if isinstance(row, dict) else None
            usage = message.get("usage") if isinstance(message, dict) else None
            model = message.get("model") if isinstance(message, dict) else None
            if row.get("type") != "assistant" or not isinstance(usage, dict):
                continue
            if not isinstance(model, str) or not model or model == "<synthetic>":
                continue
            identity = message.get("id") or row.get("requestId") or row.get("uuid")
            if not isinstance(identity, str) or not identity:
                continue
            timestamp = row.get("timestamp")
            if not isinstance(timestamp, str):
                continue
            try:
                at = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp()
            except ValueError:
                continue
            values = [usage.get(key, 0) for key in (
                "input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")]
            if not all(isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0
                       for value in values):
                input_tokens = None
            else:
                input_tokens = sum(values)
            rows.append({
                "id": identity,
                "model": model,
                "at": at,
                "usage": {
                    "inputTokens": input_tokens,
                    "cachedInputTokens": usage.get("cache_read_input_tokens", 0),
                    "cacheWriteInputTokens": usage.get("cache_creation_input_tokens", 0),
                    "outputTokens": usage.get("output_tokens"),
                },
            })
    return rows
