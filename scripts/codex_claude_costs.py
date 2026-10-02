"""Parse Claude Code assistant usage rows for account and team estimates."""
import json
from datetime import datetime


def _claude_usage_row(row):
    if not isinstance(row, dict):
        return None
    message = row.get("message")
    usage = message.get("usage") if isinstance(message, dict) else None
    model = message.get("model") if isinstance(message, dict) else None
    if row.get("type") != "assistant" or not isinstance(usage, dict):
        return None
    if not isinstance(model, str) or not model or model == "<synthetic>":
        return None
    identity = message.get("id") or row.get("requestId") or row.get("uuid")
    if not isinstance(identity, str) or not identity:
        return None
    timestamp = row.get("timestamp")
    if not isinstance(timestamp, str):
        return None
    try:
        at = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None
    values = [usage.get(key, 0) for key in (
        "input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")]
    if not all(isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0
               for value in values):
        input_tokens = None
    else:
        input_tokens = sum(values)
    return {"id": identity, "model": model, "at": at,
            "usage": {"inputTokens": input_tokens,
                      "cachedInputTokens": usage.get("cache_read_input_tokens", 0),
                      "cacheWriteInputTokens": usage.get("cache_creation_input_tokens", 0),
                      "outputTokens": usage.get("output_tokens")}}


def parse_claude_usage(path):
    rows = []
    with open(path, "r", encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                row = _claude_usage_row(json.loads(line))
            except ValueError:
                continue
            if row is not None:
                rows.append(row)
    return rows


def read_claude_usage_tail(path, cached=None):
    """Reuse complete rows only after verifying every byte in their source prefix."""
    import hashlib
    import os

    with open(path, "rb") as source:
        stat = os.fstat(source.fileno())
        identity = (stat.st_dev, stat.st_ino)
        rows, offset, complete_count = [], 0, 0
        digest = hashlib.sha256()
        if (cached and cached["identity"] == identity
                and stat.st_size >= cached["offset"]):
            remaining = cached["offset"]
            while remaining:
                block = source.read(min(remaining, 1024 * 1024))
                if not block:
                    break
                digest.update(block)
                remaining -= len(block)
            if not remaining and digest.hexdigest() == cached["prefixHash"]:
                offset = cached["offset"]
                complete_count = cached["completeCount"]
                rows = cached["rows"][:complete_count]
            else:
                source.seek(0)
                digest = hashlib.sha256()
        for line in source:
            try:
                row = _claude_usage_row(json.loads(line.decode("utf-8", errors="replace")))
            except ValueError:
                row = None
            if row is not None:
                rows.append(row)
            if line.endswith(b"\n"):
                digest.update(line)
                offset += len(line)
                complete_count = len(rows)
        # A valid final row without a newline is visible now and read again on append.
        return {"identity": identity, "offset": offset, "completeCount": complete_count,
                "prefixHash": digest.hexdigest(), "rows": rows}
