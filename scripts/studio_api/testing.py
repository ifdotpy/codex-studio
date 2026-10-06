"""Test helpers for reading server state through the public entity sync API."""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlencode


JsonReader = Callable[[str], dict[str, Any]]
ENTITY_SCOPE = "state:entities:v1"
ENTITY_PAGE_SIZE = 100
_STATE_KEYS = {
    "agent": "agents",
    "chat": "chats",
    "complaint": "complaints",
    "edge": "edges",
    "event": "events",
    "monitor": "monitors",
    "peerTeam": "peerTeams",
    "project": "projects",
    "request": "requests",
    "room": "rooms",
    "rule": "rules",
    "task": "tasks",
    "work": "work",
}


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
    """Read state after the entity store has applied its current maintenance."""
    if db is None:
        sync_store = getattr(runtime, "sync_store", None)
        if sync_store is None:
            from codex_canvas import Canvas
            from codex_sync import SyncStore

            canvas = Canvas(runtime.root)
            canvas.runtime = runtime
            sync_store = SyncStore(canvas.connect, canvas.transcript, runtime=runtime, canvas=canvas)
            runtime.sync_store = sync_store
        sync_store.pull("state:entities:v1", fresh=True)
        with runtime.read_db() as own:
            return read_runtime_state(runtime, include_work=include_work, db=own)
    state: dict[str, Any] = {name: [] for name in _STATE_KEYS.values()}
    state["workspace"] = {}
    rows = db.execute(
        "SELECT collection,id,payload FROM sync_entities "
        "WHERE deleted=0 AND collection NOT LIKE 'transcript:%' ORDER BY seq"
    )
    for collection, entity_id, payload in rows:
        entity = json.loads(payload)
        if (not isinstance(entity, dict) or entity.get("collection") != collection
                or entity.get("id") != entity_id or not isinstance(entity.get("value"), dict)):
            raise AssertionError(f"Invalid stored entity collection={collection} id={entity_id}")
        value = entity["value"]
        state_key = _STATE_KEYS.get(collection)
        if state_key is not None:
            if include_work or collection != "work":
                state[state_key].append(value)
        elif collection == "workspace":
            state["workspace"] = value
            state.update(value)
    if not include_work:
        state.pop("work")
    return state


def read_session_token(get_json: JsonReader) -> str:
    """Read the CSRF token from the session endpoint."""
    token = get_json("/api/session")["token"]
    if not isinstance(token, str):
        raise AssertionError("Session response is missing a string token")
    return token
