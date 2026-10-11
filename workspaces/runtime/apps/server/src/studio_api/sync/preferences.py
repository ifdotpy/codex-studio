"""Field versions for persistent user interface choices."""
from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
import time
from typing import Literal

from pydantic import Field, field_validator, model_validator
from studio_api.models import ContractModel, JsonValue, ResponseModel


class PreferenceField(ContractModel):
    value: JsonValue
    timestamp: int = Field(ge=0, le=9_007_199_254_740_991)
    writer: str = Field(min_length=1, max_length=128)


@dataclass(frozen=True)
class AppearancePreferenceRule:
    """Server acceptance domain for one canonical appearance preference."""

    kind: Literal["choices", "integer", "boolean", "string"]
    choices: frozenset[str] = field(default_factory=frozenset)
    minimum: int | None = None
    maximum: int | None = None
    max_length: int | None = None

    def accepts(self, value: JsonValue) -> bool:
        if self.kind == "choices":
            return isinstance(value, str) and value in self.choices
        if self.kind == "integer":
            return (
                type(value) is int
                and self.minimum is not None
                and self.maximum is not None
                and self.minimum <= value <= self.maximum
            )
        if self.kind == "boolean":
            return type(value) is bool
        return (
            isinstance(value, str)
            and self.max_length is not None
            and len(value) <= self.max_length
        )

    def valid_example(self) -> JsonValue:
        if self.kind == "choices":
            return min(self.choices)
        if self.kind == "integer":
            if self.minimum is None:
                raise ValueError("Integer appearance rules need a minimum")
            return self.minimum
        if self.kind == "boolean":
            return False
        return ""


# `fields` stays a JSON-keyed map because it also carries dynamic visual and
# server preference keys. This table is the single server authority for the
# known appearance fields and their accepted value domains.
APPEARANCE_PREFERENCE_RULES: dict[str, AppearancePreferenceRule] = {
    "theme": AppearancePreferenceRule(
        "choices", choices=frozenset({"auto", "light", "dark"})
    ),
    "typography": AppearancePreferenceRule(
        "choices", choices=frozenset({"original", "custom"})
    ),
    "contentLayout": AppearancePreferenceRule(
        "choices", choices=frozenset({"original", "custom"})
    ),
    "sidebarFontSize": AppearancePreferenceRule("integer", minimum=12, maximum=24),
    "mainFontSize": AppearancePreferenceRule("integer", minimum=12, maximum=24),
    "fontFamily": AppearancePreferenceRule(
        "choices",
        choices=frozenset({"system", "arial", "inter", "georgia", "verdana"}),
    ),
    "contentWidth": AppearancePreferenceRule("integer", minimum=60, maximum=100),
    # Match the server's previous acceptance rule; shortcut parsing remains a
    # client behavior and is not tightened by this sync repair.
    "sidebarShortcut": AppearancePreferenceRule("string", max_length=32),
    "showMessageAvatars": AppearancePreferenceRule("boolean"),
    "hideOldChatsThreshold": AppearancePreferenceRule(
        "integer", minimum=1, maximum=50
    ),
}


class UiPreferencesDto(ContractModel):
    fields: dict[str, PreferenceField] = Field(default_factory=dict)

    @field_validator("fields")
    @classmethod
    def validate_fields(cls, fields: dict[str, PreferenceField]) -> dict[str, PreferenceField]:
        for name, field in fields.items():
            if len(name) > 2048:
                raise ValueError("Preference key is too long")
            try:
                parts = json.loads(name)
            except ValueError:
                raise ValueError("Invalid preference key") from None
            if (not isinstance(parts, list) or len(parts) not in (1, 2)
                    or not all(isinstance(part, str) for part in parts)):
                raise ValueError("Invalid preference key")
            if name != json.dumps(parts, separators=(",", ":"), ensure_ascii=False):
                raise ValueError("Preference key must use canonical JSON")
            key = parts[0]
            appearance = key == "codex-studio-preferences-v1"
            visual = key.startswith(("codex-project-tree:", "codex-project-compact:",
                "codex-progress-hidden:", "studio-prompt-bookmarks:")) or key in {
                "codex-worker-disclosures", "studio-logical-project-collapsed",
                "studio-logical-project-compact", "studio-server-project-compact-v1"}
            shell = key == "server-order" or key.startswith(("server-alias:", "server-label:"))
            if not (appearance or visual or shell):
                raise ValueError("Unknown preference key")
            value = field.value
            if appearance:
                if len(parts) != 2:
                    raise ValueError("Appearance requires a field")
                rule = APPEARANCE_PREFERENCE_RULES.get(parts[1])
                if rule is None or not rule.accepts(value):
                    raise ValueError("Invalid Appearance value")
            elif visual and value is not None and type(value) is not bool:
                raise ValueError("Visual state must be a boolean")
            elif shell:
                if key == "server-order":
                    if not isinstance(value, list) or len(value) > 256 or not all(isinstance(v, str) and len(v) <= 256 for v in value):
                        raise ValueError("Invalid server order")
                elif not isinstance(value, str) or not value.strip() or len(value) > 128:
                    raise ValueError("Invalid server label")
                elif key.startswith("server-alias:") and (not value.isascii() or not value.isalpha() or not value.isupper() or len(value) > 3):
                    raise ValueError("Invalid server alias")
        return fields


class PreferencePushResponse(ResponseModel):
    fields: dict[str, PreferenceField]


class PreferencePushRequest(UiPreferencesDto):
    scope: Literal["user", "server"]

    @model_validator(mode="after")
    def validate_scope(self) -> PreferencePushRequest:
        for name in self.fields:
            key = json.loads(name)[0]
            user_key = key == "codex-studio-preferences-v1" or key.startswith("server-") or key.startswith("studio-logical-") or key == "studio-server-project-compact-v1"
            if user_key != (self.scope == "user"):
                raise ValueError("Preference belongs to another scope")
        return self


def read_preferences(db: sqlite3.Connection, scope: str) -> UiPreferencesDto:
    row = db.execute("SELECT payload FROM sync_entities WHERE collection='uiPreferences' AND id=? AND deleted=0", (scope,)).fetchone()
    if not row:
        return UiPreferencesDto()
    value = json.loads(row[0])["value"]
    value["fields"] = {name: field for name, field in value["fields"].items()
                       if not json.loads(name)[0].startswith("studio-turns:")}
    return UiPreferencesDto.model_validate(value)


ITEM_FIELD_CAP = 3000
MAX_CLOCK_SKEW_MS = 5 * 60 * 1000


def prune_fields(fields: dict[str, PreferenceField]) -> dict[str, PreferenceField]:
    protected: dict[str, PreferenceField] = {}
    items: list[tuple[str, PreferenceField]] = []
    for name, field in fields.items():
        key = json.loads(name)[0]
        if key == "codex-studio-preferences-v1" or key.startswith("server-"):
            protected[name] = field
        else:
            items.append((name, field))
    items.sort(key=lambda item: (item[1].timestamp, item[0]), reverse=True)
    return {**protected, **dict(items[:ITEM_FIELD_CAP])}


def merge_preferences(db: sqlite3.Connection, request: PreferencePushRequest) -> UiPreferencesDto:
    from codex_sync_entities import put
    if not db.in_transaction:
        db.execute("BEGIN IMMEDIATE")
    fields = read_preferences(db, request.scope).fields
    now = int(time.time() * 1000)
    for key, incoming in request.fields.items():
        if incoming.timestamp > now + MAX_CLOCK_SKEW_MS:
            incoming = incoming.model_copy(update={"timestamp": now})
        prior = fields.get(key)
        # Writer and canonical JSON make equal timestamps independent of arrival order.
        def rank(item: PreferenceField) -> tuple[int, str, str]:
            return (item.timestamp, item.writer, json.dumps(item.value, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
        if prior is None or rank(incoming) > rank(prior):
            fields[key] = incoming
    result = UiPreferencesDto(fields=prune_fields(fields))
    put(db, "uiPreferences", request.scope, result.model_dump(mode="json"))
    return result


def bootstrap_preferences(db: sqlite3.Connection, workspace_id: str | None = None) -> bytes:
    """Inert JSON seeds a new browser before React creates its first view."""
    values: dict[str, object] = {scope: read_preferences(db, scope).model_dump(mode="json") for scope in ("user", "server")}
    if workspace_id:
        values["workspaceId"] = workspace_id
    payload = json.dumps(values, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c")
    return ('<script id="studio-ui-preferences" type="application/json">' + payload + '</script>').encode("utf-8")
