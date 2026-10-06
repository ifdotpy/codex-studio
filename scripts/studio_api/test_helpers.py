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
class TestState:
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


def read_test_state(get_json: JsonReader) -> TestState:
    """Read the session token and every current entity through HTTP."""
    token = read_session_token(get_json)
    documents: list[dict[str, Any]] = []
    after = 0
    while True:
        query = urlencode({"scope": ENTITY_SCOPE, "after": after, "limit": ENTITY_PAGE_SIZE})
        pull = get_json(f"/api/sync/pull?{query}")
        for raw in pull.get("documents", []):
            payload = raw.get("payload") if isinstance(raw, dict) else None
            if not isinstance(payload, str):
                continue
            entity = json.loads(payload)
            if isinstance(entity, dict) and not raw.get("_deleted"):
                documents.append(entity)
        checkpoint = pull.get("checkpoint", {})
        next_after = checkpoint.get("seq", after) if isinstance(checkpoint, dict) else after
        max_seq = pull.get("maxSeq", next_after)
        if not isinstance(next_after, int) or not isinstance(max_seq, int) or next_after >= max_seq:
            break
        if next_after <= after:
            raise AssertionError("Entity pull did not advance its checkpoint")
        after = next_after
    return TestState(token=token, documents=tuple(documents))


def read_session_token(get_json: JsonReader) -> str:
    """Read the CSRF token from the session endpoint."""
    return get_json("/api/session")["token"]


def read_legacy_snapshot_field(read_snapshot: SnapshotReader, *path: str | int) -> Any:
    """Read a snapshot-only assertion through the one retained legacy reader."""
    value: Any = read_snapshot()
    for key in path:
        value = value[key]
    return value
