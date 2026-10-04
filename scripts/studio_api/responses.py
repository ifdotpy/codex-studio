"""Shared safe API error responses."""
from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, cast

from starlette.requests import Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.models import ErrorResponse, JsonValue

if TYPE_CHECKING:
    from fastapi import FastAPI

ERROR_STATUS_DESCRIPTIONS = {
    400: "Invalid request",
    403: "Local origin and session token required",
    408: "Request body timed out",
    409: "The server workspace changed. Reload before sending.",
    413: "Invalid request size",
    415: "JSON required",
}
WRITE_METHODS = frozenset({"POST", "PUT", "PATCH"})
OPENAPI_COMPONENTS_EXTENSION = "x-studio-components"


def error_response(
    context: ApiContext,
    request: Request,
    error: str,
    status: int,
    *,
    details: JsonValue | None = None,
    headers: Mapping[str, str] | None = None,
) -> Response:
    """Return the existing `{error: ...}` shape through the common validator."""
    value = ErrorResponse(error=error, details=details) if details is not None else ErrorResponse(error=error)
    response = context.send(request, value, status=status)
    if headers:
        response.headers.update(headers)
    return response


def install_error_response_docs(app: FastAPI) -> None:
    """Add shared boundary failures to generated OpenAPI route responses."""
    original_openapi = app.openapi

    def openapi() -> dict[str, JsonValue]:
        if app.openapi_schema is not None:
            return cast(dict[str, JsonValue], app.openapi_schema)
        schema = original_openapi()
        components = schema.setdefault("components", {})
        if not isinstance(components, dict):
            raise TypeError("OpenAPI components must be an object")
        model_schemas = components.setdefault("schemas", {})
        if not isinstance(model_schemas, dict):
            raise TypeError("OpenAPI schemas must be an object")
        model_schemas["ErrorResponse"] = ErrorResponse.model_json_schema(
            ref_template="#/components/schemas/{model}"
        )

        def normalize_refs(value: JsonValue) -> None:
            if isinstance(value, dict):
                definitions = value.pop("$defs", None)
                if isinstance(definitions, dict):
                    for name, definition in definitions.items():
                        if isinstance(name, str):
                            model_schemas.setdefault(name, definition)
                            normalize_refs(definition)
                for key, entry in list(value.items()):
                    if key == "$ref" and isinstance(entry, str):
                        value[key] = entry.replace("#/$defs/", "#/components/schemas/").replace(
                            "#/definitions/", "#/components/schemas/"
                        )
                    else:
                        normalize_refs(entry)
            elif isinstance(value, list):
                for entry in value:
                    normalize_refs(entry)

        normalize_refs(cast(JsonValue, schema))
        response_schema: JsonValue = {"$ref": "#/components/schemas/ErrorResponse"}
        content: JsonValue = {"application/json": {"schema": response_schema}}
        for path_item in schema.get("paths", {}).values():
            if not isinstance(path_item, dict):
                continue
            for method, operation in path_item.items():
                if method not in {"get", "head", "post", "put", "patch", "delete", "options"}:
                    continue
                if not isinstance(operation, dict):
                    continue
                extra_components = operation.pop(OPENAPI_COMPONENTS_EXTENSION, {})
                if isinstance(extra_components, dict):
                    for name, definition in extra_components.items():
                        if isinstance(name, str):
                            model_schemas[name] = definition
                            normalize_refs(definition)
                responses = operation.setdefault("responses", {})
                if not isinstance(responses, dict):
                    continue
                for status, description in ERROR_STATUS_DESCRIPTIONS.items():
                    if status in {408, 409, 413, 415} and method.upper() not in WRITE_METHODS:
                        continue
                    responses.setdefault(str(status), {"description": description, "content": content})
                generated_validation = responses.get("422")
                if isinstance(generated_validation, dict) and "model" not in operation.get("responses", {}).get("422", {}):
                    schema_value = generated_validation.get("content", {})
                    application_json = schema_value.get("application/json", {}) if isinstance(schema_value, dict) else {}
                    detail_schema = application_json.get("schema", {}) if isinstance(application_json, dict) else {}
                    if isinstance(detail_schema, dict) and detail_schema.get("$ref", "").endswith("#/HTTPValidationError"):
                        responses.pop("422", None)
        app.openapi_schema = schema
        return cast(dict[str, JsonValue], schema)

    setattr(app, "openapi", openapi)


def register_route_components(route: object, components: Mapping[str, JsonValue]) -> None:
    """Attach named schema components to one FastAPI route for app assembly."""
    current = getattr(route, "openapi_extra", None)
    extras = dict(current) if isinstance(current, dict) else {}
    existing = extras.get(OPENAPI_COMPONENTS_EXTENSION)
    registered = dict(existing) if isinstance(existing, dict) else {}
    registered.update(components)
    extras[OPENAPI_COMPONENTS_EXTENSION] = registered
    setattr(route, "openapi_extra", extras)
