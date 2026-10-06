"""Test helpers for reading server state through the public entity sync API."""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlencode


JsonReader = Callable[[str], dict[str, Any]]
SnapshotReader = Callable[[], dict[str, Any]]
ENTITY_SCOPE = "state:entities:v1"
ENTITY_PAGE_SIZE = 100


@dataclass(frozen=True)
class StateReadResult:
    """A token and raw entity values, without rebuilding the old snapshot."""

    token: str
    documents: tuple[dict[str, Any], ...]

    def values(self, collection: str) -> list[dict[str, Any]]:
        """Return the values for one collection from the entity pull."""
        values = []
        for document in self.documents:
            value = document.get("value")
            if document.get("collection") == collection and isinstance(value, dict):
                values.append(value)
        return values

    def value(self, collection: str, entity_id: str) -> dict[str, Any] | None:
        """Return one entity value by collection and id."""
        for document in self.documents:
            value = document.get("value")
            if (
                document.get("collection") == collection
                and document.get("id") == entity_id
                and isinstance(value, dict)
            ):
                return value
        return None


def read_test_state(get_json: JsonReader) -> StateReadResult:
    """Read the session token and every current entity through HTTP."""
    token = read_session_token(get_json)
    documents: list[dict[str, Any]] = []
    after = 0
    while True:
        query = urlencode({"scope": ENTITY_SCOPE, "after": after, "limit": ENTITY_PAGE_SIZE})
        pull = get_json(f"/api/sync/pull?{query}")
        if not isinstance(pull, dict):
            raise AssertionError("Entity pull response must be an object")
        page = pull.get("documents")
        checkpoint = pull.get("checkpoint")
        if not isinstance(page, list):
            raise AssertionError("Entity pull response is missing documents")
        if not isinstance(checkpoint, dict) or not isinstance(checkpoint.get("seq"), int):
            raise AssertionError("Entity pull response is missing checkpoint.seq")
        if not isinstance(pull.get("maxSeq"), int):
            raise AssertionError("Entity pull response is missing maxSeq")
        for raw in page:
            if not isinstance(raw, dict):
                raise AssertionError("Entity pull document must be an object")
            payload = raw.get("payload")
            if not isinstance(payload, str):
                raise AssertionError("Entity pull document payload must be a string")
            entity = json.loads(payload)
            if not isinstance(entity, dict) or not isinstance(entity.get("value"), dict):
                raise AssertionError("Entity pull payload must contain an object value")
            if not raw.get("_deleted"):
                documents.append(entity)
        next_after = checkpoint["seq"]
        max_seq = pull["maxSeq"]
        if next_after >= max_seq:
            break
        if next_after <= after:
            raise AssertionError("Entity pull did not advance its checkpoint")
        after = next_after
    return StateReadResult(token=token, documents=tuple(documents))


def read_runtime_state(runtime: Any, *, include_work: bool = True, db: Any = None) -> dict[str, Any]:
    """Read in-process state through the legacy Runtime.snapshot source.

    This is the single compatibility point to replace when the legacy snapshot
    implementation is removed.
    """
    snapshot: dict[str, Any] = runtime.snapshot(include_work=include_work, db=db)
    return snapshot


def read_session_token(get_json: JsonReader) -> str:
    """Read the CSRF token from the session endpoint."""
    token = get_json("/api/session")["token"]
    if not isinstance(token, str):
        raise AssertionError("Session response is missing a string token")
    return token


def read_legacy_snapshot_field(read_snapshot: SnapshotReader, *path: str | int) -> Any:
    """Read a snapshot-only assertion through the one retained legacy reader."""
    value: Any = read_snapshot()
    for key in path:
        value = value[key]
    return value
