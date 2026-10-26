"""Shared entity DTO response rules for HTTP pull and direct resource updates."""

from __future__ import annotations

import json
import logging

from codex_sync_entities import (
    _report_bad_entity,
    response_entity_payload_fail_open,
    validate_stored_entity_payload,
)


def response_entity_document(document: dict[str, object]) -> dict[str, object] | None:
    """Sanitize one readable entity; return None for an unreadable envelope."""
    payload = document.get("payload")
    row_id = str(document.get("id", ""))
    if not isinstance(payload, str):
        _report_bad_entity("response", row_id, TypeError("payload is not a string"))
        return None
    was_deleted = bool(document.get("_deleted"))
    if was_deleted:
        try:
            envelope = json.loads(payload)
            if not isinstance(envelope, dict):
                raise ValueError("invalid entity envelope")
            collection = envelope.get("collection")
            entity_id = envelope.get("id")
            if not isinstance(collection, str) or not isinstance(entity_id, str):
                raise ValueError("invalid entity envelope")
            validate_stored_entity_payload(payload, collection, entity_id, True)
        except (ValueError, TypeError) as error:
            _report_bad_entity("response", row_id, error)
            return None
        # Tombstones carry no DTO value. Never validate or expose
        # any stale fields that may remain in a deleted row.
        document["payload"] = json.dumps(
            {"collection": collection, "id": entity_id, "value": {}},
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        )
        document["_deleted"] = True
        return document
    try:
        # SyncStore rejects unreadable envelopes. For readable rows,
        # strip DTO extras but deliver other mismatches so one stale
        # row cannot block the checkpoint or later entity updates.
        (document["payload"], alias_deleted, dropped_paths,
         remaining_mismatch) = response_entity_payload_fail_open(payload)
    except (ValueError, TypeError) as error:
        # SyncStore skips unreadable envelopes before they reach this
        # route; retain the guard for alternate stores and test doubles.
        _report_bad_entity("response", row_id, error)
        return None
    document["_deleted"] = was_deleted or alias_deleted
    if dropped_paths:
        logging.getLogger("studio_api.sync.router").warning(
            "Sync entity contract extras removed for %s: %s",
            row_id, ", ".join(dropped_paths),
        )
    if remaining_mismatch:
        logging.getLogger("studio_api.sync.router").error(
            "Sync entity contract mismatch for %s: %s",
            row_id, remaining_mismatch,
        )
    return document
