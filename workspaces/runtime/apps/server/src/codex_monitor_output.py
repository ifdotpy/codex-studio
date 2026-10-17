"""Bounded literal matching and wake summaries for command monitor output."""

from __future__ import annotations

import base64
import codecs
import time
from typing import Any

from codex_shared_values import (
    MONITOR_OUTPUT_MAX_PATTERNS as MAX_PATTERNS,
    MONITOR_OUTPUT_MAX_PATTERN_BYTES as MAX_PATTERN_BYTES,
    MONITOR_OUTPUT_MAX_MATCH_COUNT as MAX_MATCH_COUNT,
    MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS as OUTPUT_WAKE_COOLDOWN_SECONDS,
    MONITOR_OUTPUT_RETAINED_EVENT_IDS as MAX_RETAINED_OUTPUT_EVENT_IDS,
    MONITOR_OUTPUT_TAIL_CODEPOINTS,
)


def validate_output_patterns(value: Any) -> list[str]:
    if value is None:
        return []
    if (not isinstance(value, list) or not 1 <= len(value) <= MAX_PATTERNS
            or any(not isinstance(item, str) or not item for item in value)):
        raise ValueError("output_patterns must contain up to 16 non-empty literal strings")
    if len(set(value)) != len(value):
        raise ValueError("output_patterns must not contain duplicate strings")
    patterns = list(value)
    if any(len(item.encode("utf-8")) > MAX_PATTERN_BYTES for item in patterns):
        raise ValueError("Each output pattern must be at most 256 UTF-8 bytes")
    return patterns


def _decode_state(mapping: dict[str, str], key: str) -> bytes:
    encoded = mapping.get(key, "")
    return base64.b64decode(encoded) if encoded else b""


def _encode_state(mapping: dict[str, str], key: str, value: bytes) -> None:
    if value:
        mapping[key] = base64.b64encode(value).decode("ascii")
    else:
        mapping.pop(key, None)


def _increment(mapping: dict[str, int], key: str, amount: int) -> None:
    mapping[key] = min(MAX_MATCH_COUNT, mapping.get(key, 0) + amount)


def append_output(monitor: dict[str, Any], stream: str, chunk: bytes, *, match: bool = True) -> None:
    """Append complete UTF-8 text and find literals across bounded chunk edges."""
    if stream not in {"stdout", "stderr"}:
        raise ValueError("Invalid monitor output stream")
    utf8_state = monitor.setdefault("outputUtf8Pending", {})
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    decoder.setstate((_decode_state(utf8_state, stream), 0))
    text = decoder.decode(chunk, final=False)
    _encode_state(utf8_state, stream, decoder.getstate()[0])
    if text:
        monitor["tail"] = (monitor.get("tail", "") + text)[-MONITOR_OUTPUT_TAIL_CODEPOINTS:]

    patterns = monitor.get("outputPatterns", [])
    if not match or not patterns or not chunk:
        return
    matcher_state = monitor.setdefault("outputMatcherTail", {})
    previous = _decode_state(matcher_state, stream)
    combined = previous + chunk
    longest = 0
    pending = monitor.setdefault("outputMatchesPending", {})
    totals = monitor.setdefault("outputMatchesTotal", {})
    for index, pattern in enumerate(patterns):
        needle = pattern.encode("utf-8")
        longest = max(longest, len(needle))
        start = max(0, len(previous) - len(needle) + 1)
        count = 0
        while True:
            found = combined.find(needle, start)
            if found < 0:
                break
            count += 1
            start = found + 1
        if count:
            key = f"{stream}:{index}"
            _increment(pending, key, count)
            _increment(totals, key, count)
    _encode_state(matcher_state, stream, combined[-max(0, longest - 1):] if longest > 1 else b"")


def flush_utf8_tail(monitor: dict[str, Any]) -> None:
    states = monitor.setdefault("outputUtf8Pending", {})
    for stream in tuple(states):
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        decoder.setstate((_decode_state(states, stream), 0))
        text = decoder.decode(b"", final=True)
        if text:
            monitor["tail"] = (monitor.get("tail", "") + text)[-MONITOR_OUTPUT_TAIL_CODEPOINTS:]
        states.pop(stream, None)


def _describe_counts(monitor: dict[str, Any], counts: dict[str, int]) -> list[dict[str, Any]]:
    patterns = monitor.get("outputPatterns", [])
    result = []
    for key in sorted(counts):
        stream, raw_index = key.split(":", 1)
        index = int(raw_index)
        if 0 <= index < len(patterns) and counts[key]:
            result.append({"stream": stream, "pattern": patterns[index], "count": counts[key]})
    return result


def pending_output_event(monitor: dict[str, Any], *, final: bool = False,
                         now: float | None = None) -> dict[str, Any] | None:
    """Turn pending counts into a stable event under a renewable cooldown."""
    pending = monitor.get("outputMatchesPending", {})
    if not pending:
        return None
    now = time.time() if now is None else now
    event_count = monitor.get("outputEventCount", 0)
    if (not final
            and now - monitor.get("outputWakeAt", 0) < OUTPUT_WAKE_COOLDOWN_SECONDS):
        return None

    sequence = monitor.get("outputEventSequence", 0) + 1
    event_id = f"monitor-output:{monitor['id']}:{monitor['epoch']}:{sequence}"
    monitor["outputEventSequence"] = sequence
    monitor["outputEventCount"] = min(MAX_MATCH_COUNT, event_count + 1)
    monitor["outputWakeAt"] = now
    monitor["outputEventIds"] = (
        monitor.get("outputEventIds", []) + [event_id]
    )[-MAX_RETAINED_OUTPUT_EVENT_IDS:]
    monitor["outputMatchesPending"] = {}
    return {
        "id": event_id,
        "payload": {
            "monitorId": monitor["id"], "epoch": monitor["epoch"],
            "sequence": sequence, "matches": _describe_counts(monitor, pending),
            "final": final,
        },
    }


def output_match_summary(monitor: dict[str, Any]) -> dict[str, Any] | None:
    if not monitor.get("outputPatterns"):
        return None
    return {
        "events": monitor.get("outputEventCount", 0),
        "matches": _describe_counts(monitor, monitor.get("outputMatchesTotal", {})),
        "pending": _describe_counts(monitor, monitor.get("outputMatchesPending", {})),
    }
