"""Read-only validation of persisted account and usage-limit state."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sqlite3
import sys
from typing import Any

from pydantic import ValidationError

from studio_api.persisted_models import AccountRegistryRecord, UsageLimitsWorkspaceEnvelope


def _pointer(parts: tuple[object, ...]) -> str:
    if not parts:
        return ""
    return "/" + "/".join(str(part).replace("~", "~0").replace("/", "~1") for part in parts)


def _keyword(error_type: str) -> str:
    return {
        "missing": "required",
        "extra_forbidden": "additionalProperties",
        "literal_error": "enum",
        "unicode_scalar": "unicode-scalar",
        "string_type": "type",
        "int_type": "type",
        "float_type": "type",
        "bool_type": "type",
        "dict_type": "type",
        "list_type": "type",
    }.get(error_type, error_type)


def _model_violations(
    source: dict[str, str], key: str, pointer: str, model: type[Any], value: object,
) -> list[dict[str, str]]:
    try:
        model.model_validate(value)
    except ValidationError as error:
        result = []
        for item in error.errors(include_url=False):
            parts = tuple(item.get("loc", ()))
            suffix = _pointer(parts)
            item_key = key
            if source.get("file") and len(parts) > 1 and parts[0] in {
                "accounts", "logins", "deleteReceipts", "nameReceipts",
            }:
                item_key = str(parts[1])
            result.append({
                **source,
                "key": item_key,
                "pointer": pointer + suffix,
                "keyword": _keyword(str(item.get("type", "validation"))),
            })
        return result
    return []


def _json_violation(source: dict[str, str], key: str, pointer: str = "") -> dict[str, str]:
    return {**source, "key": key, "pointer": pointer, "keyword": "json"}


def _check_registry(state_dir: Path) -> list[dict[str, str]]:
    path = state_dir / "accounts" / "registry.json"
    if not path.exists():
        return []
    source = {"file": "accounts/registry.json"}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return [_json_violation(source, "registry")]
    return _model_violations(source, "registry", "", AccountRegistryRecord, value)


def _check_workspace_usage_limits(state_dir: Path) -> list[dict[str, str]]:
    path = state_dir / "canvas.sqlite3"
    if not path.exists():
        return []
    source = {"table": "sync_entities"}
    try:
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
            exists = db.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities'"
            ).fetchone()
            if not exists:
                return []
            row = db.execute(
                "SELECT payload FROM sync_entities "
                "WHERE collection='workspace' AND id='current' AND deleted=0"
            ).fetchone()
    except (OSError, sqlite3.DatabaseError):
        return [{**source, "key": "workspace/current", "pointer": "", "keyword": "sqlite"}]
    if not row:
        return []
    try:
        envelope = json.loads(row[0])
    except (TypeError, json.JSONDecodeError):
        return [_json_violation(source, "workspace/current", "/payload")]
    if not isinstance(envelope, dict) or "value" not in envelope:
        return [_json_violation(source, "workspace/current", "/payload")]
    return _model_violations(
        source, "workspace/current", "/value", UsageLimitsWorkspaceEnvelope, envelope["value"],
    )


def check_state_directory(state_dir: Path) -> list[dict[str, str]]:
    """Inspect only the declared persisted formats without opening a writer."""
    return _check_registry(state_dir) + _check_workspace_usage_limits(state_dir)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    violations = (
        check_state_directory(args.state_dir)
        if args.state_dir.is_dir()
        else [{"file": "<state-directory>", "key": "state", "pointer": "", "keyword": "directory"}]
    )
    print(json.dumps({
        "violations": violations,
        "note": "Rust sync-table comparison is not performed by this command.",
    }, ensure_ascii=False, indent=2))
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main())
