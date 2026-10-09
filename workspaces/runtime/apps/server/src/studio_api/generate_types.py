"""Generate renderer API types from the FastAPI OpenAPI contract."""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import cast

from studio_api.models import JsonValue
from studio_api.schema import (
    API_SCHEMA_HASH_HEADER,
    API_SCHEMA_HASH_PARAM,
    API_SCHEMA_MISMATCH_HEADER,
    API_SCHEMA_MISMATCH_FIELD,
    api_schema_hash,
    entity_schema_names,
    normalize_json_value_schema,
    openapi_document,
    remove_orphan_fastapi_validation_schemas,
    validate_contract_schemas,
    validate_error_responses,
)
from codex_layout import REPOSITORY_ROOT, WEB_ROOT


ROOT = REPOSITORY_ROOT
OUTPUT = WEB_ROOT / "src" / "generated" / "api.ts"
SCHEMA_OUTPUT = WEB_ROOT / "src" / "generated" / "apiSchema.ts"
DEFAULT_CACHE_ROOT = Path.home() / ".cache"
CACHE_ROOT_ENV = "XDG_CACHE_HOME"
OPENAPI_TEMP_DIRECTORY = "codex-studio-openapi-types"
JSON_VALUE_TS_ALIAS = (
    "export type JsonValue = null | boolean | number | string | JsonValue[] "
    "| { [key: string]: JsonValue };"
)


def _entity_aliases(names: list[str], include_sync_entity_payload: bool = False) -> str:
    aliases = "".join(
        f'\nexport type {name} = components["schemas"]["{name}"];'
        for name in names
    )
    if include_sync_entity_payload:
        aliases += '\nexport type SyncEntityPayload = components["schemas"]["SyncEntityPayload"];'
    return aliases


def _normalize_recursive_json_value(generated: str) -> str:
    """Break openapi-typescript's indexed self-reference for the recursive JSON schema."""
    pattern = re.compile(
        r'(?m)^([ \t]*)JsonValue: null \| boolean \| number \| string \| '
        r'components\["schemas"\]\["JsonValue"\]\[\] \| \{\r?\n'
        r'[ \t]*\[key: string\]: components\["schemas"\]\["JsonValue"\];\r?\n'
        r'([ \t]*)\};$'
    )
    normalized, replacements = pattern.subn(r"\1JsonValue: JsonValue;", generated)
    if replacements != 1:
        raise ValueError("openapi-typescript output has no unique recursive JsonValue component")
    return normalized


def render(document: dict[str, JsonValue]) -> str:
    remove_orphan_fastapi_validation_schemas(document)
    normalize_json_value_schema(document)
    validate_contract_schemas(document)
    validate_error_responses(document)
    entities = entity_schema_names(document)
    components = document.get("components")
    schemas = components.get("schemas") if isinstance(components, dict) else None
    include_sync_entity_payload = isinstance(schemas, dict) and "SyncEntityPayload" in schemas
    cache_root = Path(os.environ.get(CACHE_ROOT_ENV, DEFAULT_CACHE_ROOT))
    temp_root = cache_root / OPENAPI_TEMP_DIRECTORY
    temp_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="generation-", dir=temp_root) as temp_name:
        temp = Path(temp_name)
        input_path = temp / "openapi.json"
        output_path = temp / "api.ts"
        input_path.write_text(
            json.dumps(document, ensure_ascii=False, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        if shutil.which("pnpm") is None:
            raise FileNotFoundError("Run `pnpm install --frozen-lockfile` at the repository root to install pinned API tools")
        subprocess.run(
            [
                "pnpm",
                "exec",
                "openapi-typescript",
                str(input_path),
                "--output",
                str(output_path),
                "--alphabetize",
                "--default-non-nullable=false",
            ],
            cwd=ROOT,
            check=True,
            text=True,
        )
        generated = output_path.read_text(encoding="utf-8")
    if "export interface components" not in generated and "export type components" not in generated:
        raise ValueError("openapi-typescript output is missing the components export")
    generated = _normalize_recursive_json_value(generated)
    generated += "\n\n" + JSON_VALUE_TS_ALIAS + _entity_aliases(
        entities, include_sync_entity_payload
    ) + "\n"
    with tempfile.TemporaryDirectory(prefix="format-", dir=temp_root) as temp_name:
        output_path = Path(temp_name) / "api.ts"
        output_path.write_text(generated, encoding="utf-8")
        subprocess.run(
            [
                "pnpm",
                "exec",
                "oxfmt",
                "--write",
                f"--config={ROOT / '.oxfmtrc.json'}",
                str(output_path),
            ],
            cwd=ROOT,
            check=True,
            text=True,
        )
        return output_path.read_text(encoding="utf-8")


def render_schema_identity(schema_hash: str) -> str:
    return (
        "export const API_SCHEMA_HASH =\n"
        f'  "{schema_hash}";\n'
        f'export const API_SCHEMA_HASH_HEADER = "{API_SCHEMA_HASH_HEADER}";\n'
        f'export const API_SCHEMA_HASH_PARAM = "{API_SCHEMA_HASH_PARAM}";\n'
        f'export const API_SCHEMA_MISMATCH_HEADER = "{API_SCHEMA_MISMATCH_HEADER}";\n'
        f'export const API_SCHEMA_MISMATCH_FIELD = "{API_SCHEMA_MISMATCH_FIELD}";\n'
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if generated API types are stale")
    options = parser.parse_args(argv)
    try:
        document = openapi_document()
        schema_hash = api_schema_hash(document)
        expected_files = {
            OUTPUT: render(document),
            SCHEMA_OUTPUT: render_schema_identity(schema_hash),
        }
    except (FileNotFoundError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"API type generation failed: {error}", file=sys.stderr)
        return 1
    for path, expected in expected_files.items():
        current = path.read_text(encoding="utf-8") if path.exists() else ""
        if options.check:
            if current != expected:
                print(f"{path.relative_to(ROOT)} is stale; run `pnpm --workspace-root run api:generate`", file=sys.stderr)
                return 1
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
