"""Offline OpenAPI construction and schema coverage checks."""
from __future__ import annotations

import json
from typing import cast

from studio_api.models import JsonValue


HTTP_METHODS = frozenset({"get", "put", "post", "delete", "options", "head", "patch", "trace"})
NO_BODY_SUCCESS_STATUS = "204"


def openapi_document() -> dict[str, JsonValue]:
    """Build OpenAPI using the inert schema context; never opens runtime state."""
    from studio_api.app import create_app
    from studio_api.context import ApiContext

    app = create_app(ApiContext.for_schema())
    return cast(dict[str, JsonValue], json.loads(json.dumps(app.openapi())))


def _object(value: JsonValue) -> dict[str, JsonValue] | None:
    if isinstance(value, dict):
        return value
    return None


def _has_schema_shape(schema: dict[str, JsonValue]) -> bool:
    known_shape_keys = {
        "$ref",
        "type",
        "enum",
        "const",
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "properties",
        "items",
        "additionalProperties",
    }
    if not schema.keys() & known_shape_keys:
        return False
    if schema.get("additionalProperties") is True:
        return False
    if schema.get("type") == "object":
        properties = _object(schema.get("properties"))
        additional = schema.get("additionalProperties")
        if not properties and additional is None:
            return False
    return True


def validate_success_responses(document: dict[str, JsonValue]) -> None:
    """Reject undocumented and opaque JSON success paths in the API contract."""
    paths = _object(document.get("paths"))
    if paths is None or not paths:
        raise ValueError("OpenAPI document has no paths")

    failures: list[str] = []
    for path, raw_path_item in paths.items():
        path_item = _object(raw_path_item)
        if path_item is None:
            failures.append(f"{path}: path item is not an object")
            continue
        for method, raw_operation in path_item.items():
            if method not in HTTP_METHODS:
                continue
            operation = _object(raw_operation)
            responses = _object(operation.get("responses")) if operation else None
            if responses is None or not responses:
                failures.append(f"{method.upper()} {path}: no response declarations")
                continue
            for status, raw_response in responses.items():
                if not status.startswith("2") or status == NO_BODY_SUCCESS_STATUS:
                    continue
                response = _object(raw_response)
                content = _object(response.get("content")) if response else None
                if content is None:
                    failures.append(f"{method.upper()} {path} {status}: response has no content schema")
                    continue
                json_media_type = next(
                    (
                        media_type
                        for media_type in content
                        if media_type == "application/json" or media_type.endswith("+json")
                    ),
                    None,
                )
                json_media = _object(content.get(json_media_type)) if json_media_type else None
                if json_media is None:
                    # Non-JSON responses (for example an image or download) have
                    # their own explicit media type and are outside this schema.
                    if not content:
                        failures.append(f"{method.upper()} {path} {status}: empty response content")
                    continue
                schema = _object(json_media.get("schema"))
                if schema is None or not _has_schema_shape(schema):
                    failures.append(f"{method.upper()} {path} {status}: opaque JSON response schema")

    if failures:
        raise ValueError("\n".join(failures))


def validate_error_responses(document: dict[str, JsonValue]) -> None:
    """Require the shared legacy 400 shape and reject FastAPI's default 422."""
    paths = _object(document.get("paths"))
    if paths is None:
        raise ValueError("OpenAPI document has no paths")
    failures: list[str] = []
    for path, raw_path_item in paths.items():
        path_item = _object(raw_path_item)
        if path_item is None:
            continue
        for method, raw_operation in path_item.items():
            if method not in HTTP_METHODS:
                continue
            operation = _object(raw_operation)
            responses = _object(operation.get("responses")) if operation else None
            error_response = _object(responses.get("400")) if responses else None
            content = _object(error_response.get("content")) if error_response else None
            media = _object(content.get("application/json")) if content else None
            schema = _object(media.get("schema")) if media else None
            if schema is None or schema.get("$ref") != "#/components/schemas/ErrorResponse":
                failures.append(f"{method.upper()} {path}: 400 must use ErrorResponse")
            generated_422 = _object(responses.get("422")) if responses else None
            generated_content = _object(generated_422.get("content")) if generated_422 else None
            generated_media = _object(generated_content.get("application/json")) if generated_content else None
            generated_schema = _object(generated_media.get("schema")) if generated_media else None
            if generated_schema and generated_schema.get("$ref") == "#/components/schemas/HTTPValidationError":
                failures.append(f"{method.upper()} {path}: default FastAPI 422 conflicts with legacy 400 validation")
    if failures:
        raise ValueError("\n".join(failures))


def entity_schema_names(document: dict[str, JsonValue]) -> list[str]:
    """Return stable, named entity DTO schemas exposed by the API."""
    components = _object(document.get("components"))
    schemas = _object(components.get("schemas")) if components else None
    if schemas is None:
        raise ValueError("OpenAPI document has no component schemas")
    names = sorted(name for name in schemas if name.endswith("EntityDto"))
    if not names:
        raise ValueError("OpenAPI document has no *EntityDto component schemas")
    return names


def require_json_value_schema(document: dict[str, JsonValue]) -> None:
    components = _object(document.get("components"))
    schemas = _object(components.get("schemas")) if components else None
    if schemas is None or "JsonValue" not in schemas:
        raise ValueError("OpenAPI document does not expose the shared JsonValue component")
