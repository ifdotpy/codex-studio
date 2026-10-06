"""Select public snapshot fields before strict response validation."""

from __future__ import annotations

from typing import cast

from studio_api.models import JsonValue
from studio_api.sync.models import SnapshotAgentDto, SnapshotNativeRelease, SnapshotStartAttempt


def snapshot_agent(record: JsonValue) -> JsonValue:
    """Exclude private runtime bookkeeping from the public agent snapshot."""
    if not isinstance(record, dict):
        return record
    result = {key: value for key, value in record.items() if key in SnapshotAgentDto.model_fields}
    for field, model in (("nativeRelease", SnapshotNativeRelease), ("startAttempt", SnapshotStartAttempt)):
        nested = result.get(field)
        if isinstance(nested, dict):
            result[field] = {key: value for key, value in nested.items() if key in model.model_fields}
    return result


def project_snapshot(snapshot: dict[str, JsonValue]) -> dict[str, JsonValue]:
    """Use one field selection for threads, graph nodes, and runtime agents."""
    result = dict(snapshot)
    for field in ("threads", "nodes"):
        values = result.get(field)
        if isinstance(values, list):
            result[field] = [
                snapshot_agent(value) if isinstance(value, dict) and value.get("kind") == "agent" else value
                for value in values
            ]
    runtime = result.get("runtime")
    if isinstance(runtime, dict) and isinstance(runtime.get("agents"), list):
        result["runtime"] = {**runtime, "agents": [snapshot_agent(value) for value in cast(list[JsonValue], runtime["agents"])]}
    return result
