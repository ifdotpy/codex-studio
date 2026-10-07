"""Select public snapshot fields before strict response validation."""

from __future__ import annotations

from typing import cast

from studio_api.models import JsonValue
from studio_api.sync.models import RuleSnapshotDto, SnapshotAgentDto, SnapshotNativeRelease, SnapshotStartAttempt


PRIVATE_AGENT_FIELDS = frozenset({
    "contextRepair", "contextRepairHistory", "lastContextRepairCheck", "lastContextRepairWait",
})


def snapshot_agent(record: JsonValue) -> JsonValue:
    """Exclude private runtime bookkeeping from the public agent snapshot."""
    if not isinstance(record, dict):
        return record
    result = {key: value for key, value in record.items()
              if key in SnapshotAgentDto.model_fields and key not in PRIVATE_AGENT_FIELDS}
    for field, model in (("nativeRelease", SnapshotNativeRelease), ("startAttempt", SnapshotStartAttempt)):
        nested = result.get(field)
        if isinstance(nested, dict):
            result[field] = {key: value for key, value in nested.items() if key in model.model_fields}
    return result


def snapshot_rule(record: JsonValue) -> JsonValue:
    """Exclude private rule bookkeeping, such as restart notification markers."""
    if not isinstance(record, dict):
        return record
    return {key: value for key, value in record.items() if key in RuleSnapshotDto.model_fields}


def project_snapshot(snapshot: dict[str, JsonValue]) -> dict[str, JsonValue]:
    """Use one field selection for threads, graph nodes, and runtime agents."""
    result = dict(snapshot)
    agents: dict[int, JsonValue] = {}

    def agent(record: JsonValue) -> JsonValue:
        identity = id(record)
        if identity not in agents:
            agents[identity] = snapshot_agent(record)
        return agents[identity]

    for field in ("threads", "nodes"):
        values = result.get(field)
        if isinstance(values, list):
            result[field] = [
                agent(value) if isinstance(value, dict) and value.get("kind") == "agent" else value
                for value in values
            ]
    runtime = result.get("runtime")
    if isinstance(runtime, dict):
        projected = dict(runtime)
        if isinstance(runtime.get("agents"), list):
            projected["agents"] = [agent(value) for value in cast(list[JsonValue], runtime["agents"])]
        if isinstance(runtime.get("rules"), list):
            projected["rules"] = [snapshot_rule(value) for value in cast(list[JsonValue], runtime["rules"])]
        result["runtime"] = projected
    return result
