"""Delivery boundaries for Python consumers of the Rust-owned contract.

Legacy spellings are accepted only while reading existing records or requests.
All new metadata and internal decisions use DeliveryMoment.
"""
from __future__ import annotations

import json
import sqlite3
from collections.abc import Mapping
from typing import Any

from codex_delivery_moment import DeliveryMoment as DeliveryMoment, LEGACY_DELIVERY_MOMENTS


def normalize_delivery(value: object = None, *, default: DeliveryMoment = DeliveryMoment.CURRENT_TURN) -> DeliveryMoment:
    if value is None:
        return default
    if isinstance(value, str) and value in LEGACY_DELIVERY_MOMENTS:
        return LEGACY_DELIVERY_MOMENTS[value]
    if not isinstance(value, str):
        raise ValueError("Delivery moment must be a string")
    try:
        return DeliveryMoment(value)
    except (TypeError, ValueError) as error:
        raise ValueError("Choose a delivery moment from " + ", ".join(DeliveryMoment)) from error


def parse_delivery(value: object) -> DeliveryMoment:
    """Validate an explicitly supplied request field, including its JSON type."""
    if not isinstance(value, str):
        raise ValueError("Delivery moment must be a string")
    return normalize_delivery(value)


def default_delivery(kind: str, text: str) -> DeliveryMoment:
    if kind == "agent_message":
        try:
            value = json.loads(text)
            if isinstance(value, dict) and value.get("importance") == "progress":
                return DeliveryMoment.NEXT_TURN
        except (ValueError, TypeError):
            pass
    return DeliveryMoment.CURRENT_TURN


def _event_metadata(db: sqlite3.Connection, key: str) -> dict[str, Any]:
    saved = db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()
    metadata = json.loads(saved[0]) if saved else None
    if metadata is None:
        return {}
    if not isinstance(metadata, dict):
        raise ValueError("Event metadata must be an object or null")
    return metadata


def event_delivery(db: sqlite3.Connection, event: Mapping[str, Any]) -> DeliveryMoment:
    metadata = _event_metadata(db, event["id"])
    return normalize_delivery(metadata.get("delivery"), default=default_delivery(event["kind"], event["text"]))


def stamp_delivery(db: sqlite3.Connection, key: str, kind: str, text: str, delivery: DeliveryMoment | None = None) -> None:
    # Existing events own their timing, including a user's later promotion.
    if db.execute("SELECT 1 FROM runtime_events WHERE id=?", (key,)).fetchone():
        return
    metadata = _event_metadata(db, key)
    metadata["delivery"] = normalize_delivery(delivery if delivery is not None else metadata.get("delivery"),
                                               default=default_delivery(kind, text))
    db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)", (key, json.dumps(metadata)))
