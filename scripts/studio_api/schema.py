"""Offline OpenAPI construction and strict schema coverage checks."""
from __future__ import annotations

import hashlib
import json
import threading
from typing import cast

from studio_api.models import JsonValue


HTTP_METHODS = frozenset({"get", "put", "post", "delete", "options", "head", "patch", "trace"})
NO_BODY_SUCCESS_STATUS = "204"
JSON_VALUE_REF = "#/components/schemas/JsonValue"
JSON_VALUE_SCHEMA: dict[str, JsonValue] = {
    "title": "JsonValue",
    "anyOf": [
        {"type": "null"},
        {"type": "boolean"},
        {"type": "integer"},
        {"type": "number"},
        {"type": "string"},
        {"type": "array", "items": {"$ref": JSON_VALUE_REF}},
        {"type": "object", "additionalProperties": {"$ref": JSON_VALUE_REF}},
    ],
}
SCHEMA_SHAPE_KEYS = frozenset(
    {
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
        "prefixItems",
        "additionalProperties",
    }
)
SCHEMA_COMBINATORS = ("oneOf", "anyOf", "allOf")
API_SCHEMA_HASH_HEADER = "X-Studio-API-Schema"
API_SCHEMA_HASH_PARAM = "apiSchema"
API_SCHEMA_MISMATCH_HEADER = "X-Studio-API-Schema-Mismatch"
API_SCHEMA_MISMATCH_FIELD = "mismatch"
_HASH_LOCK = threading.Lock()
_CACHED_API_SCHEMA_HASH: str | None = None


def openapi_document() -> dict[str, JsonValue]:
    """Build OpenAPI using the inert schema context; never opens runtime state."""
    from studio_api.app import create_app
    from studio_api.context import ApiContext

    app = create_app(ApiContext.for_schema())
    return cast(dict[str, JsonValue], json.loads(json.dumps(app.openapi())))


_NAME_KEYED_MAPS = frozenset({
    "properties", "$defs", "definitions", "patternProperties", "dependentSchemas",
    "paths", "schemas", "parameters", "responses", "requestBodies", "headers",
    "links", "callbacks", "securitySchemes", "content", "encoding", "mapping",
    "webhooks",
})


def canonical_openapi_json(document: dict[str, JsonValue]) -> bytes:
    """Canonicalize wire-shape OpenAPI fields; ignore documentation-only metadata."""
    def strip_documentation(value: JsonValue) -> JsonValue:
        if isinstance(value, list):
            return [strip_documentation(item) for item in value]
        if isinstance(value, dict):
            canonical: dict[str, JsonValue] = {}
            for key, item in value.items():
                if key in {"description", "summary", "examples", "externalDocs"}:
                    continue
                if key == "default":
                    # A default is arbitrary JSON data, not OpenAPI metadata.
                    canonical[key] = item
                elif key in _NAME_KEYED_MAPS:
                    if isinstance(item, dict):
                        canonical[key] = {
                            name: strip_documentation(child)
                            for name, child in item.items()
                        }
                    else:
                        canonical[key] = strip_documentation(item)
                elif key == "components" and isinstance(item, dict):
                    # Both component categories and component names are named
                    # maps. Preserve them at each level, then clean schema bodies.
                    canonical[key] = {
                        category: (
                            {
                                name: strip_documentation(component)
                                for name, component in collection.items()
                            }
                            if category in _NAME_KEYED_MAPS and isinstance(collection, dict)
                            else strip_documentation(collection)
                        )
                        for category, collection in item.items()
                    }
                else:
                    canonical[key] = strip_documentation(item)
            return canonical
        return value

    return json.dumps(
        strip_documentation(document),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def api_schema_hash(document: dict[str, JsonValue] | None = None) -> str:
    """Hash the canonical OpenAPI document shared by codegen and the server."""
    if document is not None:
        return hashlib.sha256(canonical_openapi_json(document)).hexdigest()
    global _CACHED_API_SCHEMA_HASH
    if _CACHED_API_SCHEMA_HASH is None:
        with _HASH_LOCK:
            if _CACHED_API_SCHEMA_HASH is None:
                _CACHED_API_SCHEMA_HASH = hashlib.sha256(
                    canonical_openapi_json(openapi_document())
                ).hexdigest()
    return _CACHED_API_SCHEMA_HASH


def _object(value: JsonValue) -> dict[str, JsonValue] | None:
    if isinstance(value, dict):
        return value
    return None


def _resolve_pointer(document: dict[str, JsonValue], reference: str) -> JsonValue | None:
    if not reference.startswith("#/"):
        return None
    current: JsonValue = document
    for encoded_part in reference[2:].split("/"):
        part = encoded_part.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict):
            if part not in current:
                return None
            current = current[part]
        elif isinstance(current, list):
            try:
                index = int(part)
            except ValueError:
                return None
            if not 0 <= index < len(current):
                return None
            current = current[index]
        else:
            return None
    return current


def _reference_has_concrete_schema(document: dict[str, JsonValue], reference: str) -> bool:
    """Check that a pure `$ref` cycle eventually reaches a concrete schema."""
    seen: set[str] = set()
    current_reference = reference
    while current_reference not in seen:
        seen.add(current_reference)
        target = _object(_resolve_pointer(document, current_reference))
        if target is None:
            return False
        if target.keys() & (SCHEMA_SHAPE_KEYS - {"$ref"}):
            return True
        next_reference = target.get("$ref")
        if not isinstance(next_reference, str):
            return False
        current_reference = next_reference
    return False


def normalize_json_value_schema(document: dict[str, JsonValue]) -> None:
    """Replace Pydantic's empty JsonValue schema with its recursive JSON union."""
    components = _object(document.get("components"))
    schemas = _object(components.get("schemas")) if components else None
    if schemas is None or "JsonValue" not in schemas:
        raise ValueError("OpenAPI document does not expose the shared JsonValue component")
    schemas["JsonValue"] = JSON_VALUE_SCHEMA


def remove_orphan_fastapi_validation_schemas(document: dict[str, JsonValue]) -> None:
    """Drop FastAPI's unused default-422 models after routes document legacy 400s.

    FastAPI may leave these component definitions behind after the app removes
    its generated 422 response declarations. They are not part of the wire
    contract when no path references them, and Pydantic's validation-error
    context is intentionally an unconstrained framework payload.
    """
    components = _object(document.get("components"))
    schemas = _object(components.get("schemas")) if components else None
    paths = _object(document.get("paths"))
    if schemas is None or paths is None:
        return

    reachable: set[str] = set()
    traversed_references: set[str] = set()

    def visit(value: JsonValue) -> None:
        if isinstance(value, dict):
            reference = value.get("$ref")
            if isinstance(reference, str) and reference.startswith("#/components/schemas/"):
                encoded_name = reference[len("#/components/schemas/") :].split("/", 1)[0]
                name = encoded_name.replace("~1", "/").replace("~0", "~")
                reachable.add(name)
                if reference not in traversed_references:
                    traversed_references.add(reference)
                    target = _resolve_pointer(document, reference)
                    if target is not None:
                        visit(target)
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    visit(paths)
    for name in ("HTTPValidationError", "ValidationError"):
        if name not in reachable:
            schemas.pop(name, None)


def _schema_errors(
    schema_value: JsonValue,
    document: dict[str, JsonValue],
    location: str,
    reference_stack: frozenset[str] = frozenset(),
) -> list[str]:
    schema = _object(schema_value)
    if schema is None:
        return [f"{location}: schema is not an object"]

    errors: list[str] = []
    reference = schema.get("$ref")
    if reference is not None:
        if not isinstance(reference, str):
            errors.append(f"{location}: $ref is not a string")
        else:
            target = _resolve_pointer(document, reference)
            if target is None:
                errors.append(f"{location}: unresolved schema reference {reference}")
            elif reference not in reference_stack:
                errors.extend(
                    _schema_errors(
                        target,
                        document,
                        f"{location} -> {reference}",
                        reference_stack | {reference},
                    )
                )
            elif not _reference_has_concrete_schema(document, reference):
                errors.append(f"{location}: reference cycle has no concrete schema")

    if not schema.keys() & SCHEMA_SHAPE_KEYS:
        errors.append(f"{location}: schema has no type or validation shape")
        return errors
    if schema.get("additionalProperties") is True:
        errors.append(f"{location}: unconstrained additionalProperties")
    if schema.get("type") == "object":
        properties_value = schema.get("properties")
        properties = _object(properties_value) if properties_value is not None else None
        additional = schema.get("additionalProperties")
        if not properties and additional is None:
            errors.append(f"{location}: object has no declared properties or additionalProperties schema")
    if schema.get("type") == "array" and "items" not in schema and "prefixItems" not in schema:
        errors.append(f"{location}: array has no items schema")

    properties_value = schema.get("properties")
    if properties_value is not None:
        properties = _object(properties_value)
        if properties is None:
            errors.append(f"{location}: properties is not an object")
        else:
            for name, child in properties.items():
                errors.extend(_schema_errors(child, document, f"{location}.properties.{name}", reference_stack))

    items = schema.get("items")
    if items is False:
        pass
    elif items is True:
        errors.append(f"{location}.items: unconstrained item schema")
    elif items is not None:
        errors.extend(_schema_errors(items, document, f"{location}.items", reference_stack))

    prefix_items = schema.get("prefixItems")
    if prefix_items is not None:
        if not isinstance(prefix_items, list) or not prefix_items:
            errors.append(f"{location}: prefixItems must contain at least one schema")
        else:
            for index, item_schema in enumerate(prefix_items):
                errors.extend(
                    _schema_errors(
                        item_schema,
                        document,
                        f"{location}.prefixItems[{index}]",
                        reference_stack,
                    )
                )
            if items is None:
                max_items = schema.get("maxItems")
                if type(max_items) is not int or max_items > len(prefix_items):
                    errors.append(
                        f"{location}: prefixItems permits an untyped array tail without items or a sufficient maxItems bound"
                    )

    additional = schema.get("additionalProperties")
    if isinstance(additional, dict):
        errors.extend(
            _schema_errors(additional, document, f"{location}.additionalProperties", reference_stack)
        )
    elif additional is not None and not isinstance(additional, bool):
        errors.append(f"{location}: additionalProperties is not a schema or boolean")

    for keyword in SCHEMA_COMBINATORS:
        raw_branches = schema.get(keyword)
        if raw_branches is None:
            continue
        if not isinstance(raw_branches, list) or not raw_branches:
            errors.append(f"{location}: {keyword} must contain at least one schema")
            continue
        for index, branch in enumerate(raw_branches):
            errors.extend(
                _schema_errors(branch, document, f"{location}.{keyword}[{index}]", reference_stack)
            )

    for keyword in ("not", "if", "then", "else", "contains"):
        child = schema.get(keyword)
        if child is not None:
            errors.extend(_schema_errors(child, document, f"{location}.{keyword}", reference_stack))
    return errors


def _parameter_errors(
    raw_parameter: JsonValue,
    document: dict[str, JsonValue],
    location: str,
    reference_stack: frozenset[str] = frozenset(),
) -> list[str]:
    parameter = _object(raw_parameter)
    if parameter is None:
        return [f"{location}: parameter is not an object"]
    reference = parameter.get("$ref")
    if reference is not None:
        if not isinstance(reference, str):
            return [f"{location}: parameter $ref is not a string"]
        target = _resolve_pointer(document, reference)
        if target is None:
            return [f"{location}: unresolved parameter reference {reference}"]
        if reference in reference_stack:
            return [f"{location}: cyclic parameter reference {reference}"]
        return _parameter_errors(target, document, f"{location} -> {reference}", reference_stack | {reference})

    errors: list[str] = []
    if not isinstance(parameter.get("name"), str):
        errors.append(f"{location}: parameter has no string name")
    parameter_location = parameter.get("in")
    if not isinstance(parameter_location, str) or parameter_location not in {"path", "query", "header", "cookie"}:
        errors.append(f"{location}: parameter has invalid or missing location")

    schema = parameter.get("schema")
    if schema is not None:
        errors.extend(_schema_errors(schema, document, f"{location}.schema"))
    else:
        content = _object(parameter.get("content"))
        if not content:
            errors.append(f"{location}: parameter has no schema")
        else:
            for media_type, raw_media in content.items():
                media = _object(raw_media)
                media_schema = media.get("schema") if media else None
                if media_schema is None:
                    errors.append(f"{location}.content.{media_type}: missing schema")
                else:
                    errors.extend(_schema_errors(media_schema, document, f"{location}.content.{media_type}"))
    return errors


def _request_body_errors(
    raw_body: JsonValue,
    document: dict[str, JsonValue],
    location: str,
    reference_stack: frozenset[str] = frozenset(),
) -> list[str]:
    body = _object(raw_body)
    if body is None:
        return [f"{location}: request body is not an object"]
    reference = body.get("$ref")
    if reference is not None:
        if not isinstance(reference, str):
            return [f"{location}: request body $ref is not a string"]
        target = _resolve_pointer(document, reference)
        if target is None:
            return [f"{location}: unresolved request body reference {reference}"]
        if reference in reference_stack:
            return [f"{location}: cyclic request body reference {reference}"]
        return _request_body_errors(target, document, f"{location} -> {reference}", reference_stack | {reference})

    content = _object(body.get("content"))
    if not content:
        return [f"{location}: request body has no content schemas"]
    errors: list[str] = []
    for media_type, raw_media in content.items():
        media = _object(raw_media)
        schema = media.get("schema") if media else None
        if schema is None:
            errors.append(f"{location}.content.{media_type}: missing schema")
        else:
            errors.extend(_schema_errors(schema, document, f"{location}.content.{media_type}"))
    return errors


def _operation_errors(
    path: str,
    method: str,
    path_item: dict[str, JsonValue],
    operation: dict[str, JsonValue],
    document: dict[str, JsonValue],
) -> list[str]:
    prefix = f"{method.upper()} {path}"
    errors: list[str] = []
    for parameter_scope, raw_parameters in (
        ("path", path_item.get("parameters")),
        ("operation", operation.get("parameters")),
    ):
        if raw_parameters is None:
            continue
        if not isinstance(raw_parameters, list):
            errors.append(f"{prefix}: {parameter_scope} parameters are not an array")
            continue
        for index, parameter in enumerate(raw_parameters):
            errors.extend(
                _parameter_errors(parameter, document, f"{prefix}.{parameter_scope}.parameters[{index}]")
            )

    request_body = operation.get("requestBody")
    if request_body is not None:
        errors.extend(_request_body_errors(request_body, document, f"{prefix}.requestBody"))

    responses = _object(operation.get("responses"))
    if responses is None:
        errors.append(f"{prefix}: no response declarations")
        return errors
    for status, raw_response in responses.items():
        response = _object(raw_response)
        if response is None:
            errors.append(f"{prefix} {status}: response is not an object")
            continue
        content = _object(response.get("content"))
        if content is None:
            if status.startswith("2") and status != NO_BODY_SUCCESS_STATUS:
                errors.append(f"{prefix} {status}: response has no content schema")
            continue
        for media_type, raw_media in content.items():
            media = _object(raw_media)
            schema = media.get("schema") if media else None
            is_json = media_type == "application/json" or media_type.endswith("+json")
            if schema is None:
                if status.startswith("2") and status != NO_BODY_SUCCESS_STATUS and is_json:
                    errors.append(f"{prefix} {status} {media_type}: missing JSON response schema")
                continue
            if status.startswith("2") or is_json:
                errors.extend(_schema_errors(schema, document, f"{prefix}.{status}.{media_type}"))
    return errors


def validate_contract_schemas(document: dict[str, JsonValue]) -> None:
    """Recursively validate route parameters, request bodies, and response schemas."""
    paths = _object(document.get("paths"))
    if paths is None or not paths:
        raise ValueError("OpenAPI document has no paths")

    errors: list[str] = []
    for path, raw_path_item in paths.items():
        path_item = _object(raw_path_item)
        if path_item is None:
            errors.append(f"{path}: path item is not an object")
            continue
        for method, raw_operation in path_item.items():
            if method not in HTTP_METHODS:
                continue
            operation = _object(raw_operation)
            if operation is None:
                errors.append(f"{method.upper()} {path}: operation is not an object")
                continue
            errors.extend(_operation_errors(path, method, path_item, operation, document))

    components = _object(document.get("components"))
    schemas = _object(components.get("schemas")) if components else None
    if schemas is None:
        errors.append("OpenAPI document has no component schemas")
    else:
        for name, schema in schemas.items():
            errors.extend(_schema_errors(schema, document, f"components.schemas.{name}"))
    if errors:
        raise ValueError("\n".join(errors))


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
