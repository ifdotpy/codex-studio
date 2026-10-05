"""Generate renderer API types from the FastAPI OpenAPI contract."""
from __future__ import annotations

import argparse
import json
import os
import re
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
    api_schema_hash,
    entity_schema_names,
    normalize_json_value_schema,
    openapi_document,
    remove_orphan_fastapi_validation_schemas,
    validate_contract_schemas,
    validate_error_responses,
)


ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "web" / "src" / "generated" / "api.ts"
SCHEMA_OUTPUT = ROOT / "web" / "src" / "generated" / "apiSchema.ts"
DEFAULT_CACHE_ROOT = Path.home() / ".cache"
CACHE_ROOT_ENV = "XDG_CACHE_HOME"
OPENAPI_TEMP_DIRECTORY = "codex-studio-openapi-types"
GENERATOR_PATH = ROOT / "node_modules" / ".bin" / "openapi-typescript"
FORMATTER_PATH = ROOT / "node_modules" / ".bin" / "oxfmt"
JSON_VALUE_TS_ALIAS = (
    "export type JsonValue = null | boolean | number | string | JsonValue[] "
    "| { [key: string]: JsonValue };"
)


def _entity_aliases(names: list[str]) -> str:
    return "".join(
        f'\nexport type {name} = components["schemas"]["{name}"];'
        for name in names
    )


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
        if not GENERATOR_PATH.is_file():
            raise FileNotFoundError("Run npm ci to install the pinned openapi-typescript generator")
        subprocess.run(
            [
                str(GENERATOR_PATH),
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
    generated += "\n\n" + JSON_VALUE_TS_ALIAS + _entity_aliases(entities) + "\n"
    if not FORMATTER_PATH.is_file():
        raise FileNotFoundError("Run npm ci to install the pinned Oxfmt formatter")
    with tempfile.TemporaryDirectory(prefix="format-", dir=temp_root) as temp_name:
        output_path = Path(temp_name) / "api.ts"
        output_path.write_text(generated, encoding="utf-8")
        subprocess.run(
            [
                str(FORMATTER_PATH),
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
                print(f"{path.relative_to(ROOT)} is stale; run npm run api:generate", file=sys.stderr)
                return 1
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
