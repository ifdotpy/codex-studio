"""Shared safe API error responses."""
from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, Any, cast

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
_API_ERROR_RESPONSE_DESCRIPTIONS = {
    **ERROR_STATUS_DESCRIPTIONS,
    404: "Not found",
    401: "Unauthorized",
    410: "Gone",
    426: "API schema mismatch",
    500: "Internal server error",
    502: "Bad gateway",
    503: "Service unavailable",
    429: "Too many requests",
}
WRITE_METHODS = frozenset({"POST", "PUT", "PATCH"})
OPENAPI_COMPONENTS_EXTENSION = "x-studio-components"
OPENAPI_413_DESCRIPTION = "Content Too Large"
PYTHON_413_DESCRIPTIONS = frozenset({"Request Entity Too Large", OPENAPI_413_DESCRIPTION})

_HEADER_SCHEMA = {"schema": {"type": "string"}}
_HEADER_COMPONENT_NAMES = {
    "Access-Control-Allow-Origin": "AccessControlAllowOrigin",
    "Access-Control-Expose-Headers": "AccessControlExposeHeaders",
    "Cache-Control": "CacheControl",
    "Content-Length": "ContentLength",
    "Content-Type": "ContentType",
    "Content-Encoding": "ContentEncoding",
    "Vary": "Vary",
    "X-Content-Type-Options": "XContentTypeOptions",
    "Referrer-Policy": "ReferrerPolicy",
    "Content-Security-Policy": "ContentSecurityPolicy",
    "Server-Timing": "ServerTiming",
    "X-Studio-API-Schema": "StudioApiSchema",
    "X-Studio-API-Schema-Mismatch": "StudioApiSchemaMismatch",
    "X-Studio-Server": "StudioServer",
    "Content-Disposition": "ContentDisposition",
    "Accept-Ranges": "AcceptRanges",
    "Content-Range": "ContentRange",
    "X-Log-Truncated": "LogTruncated",
    "X-Accel-Buffering": "AccelBuffering",
    "ETag": "ETag",
}
_HEADER_COMPONENTS = {
    name: {"schema": dict(_HEADER_SCHEMA["schema"])}
    for name in _HEADER_COMPONENT_NAMES.values()
}


def response_headers(*names: str) -> dict[str, dict[str, JsonValue]]:
    """Reference shared string-valued HTTP header declarations."""
    return {
        name: {"$ref": f"#/components/headers/{_HEADER_COMPONENT_NAMES[name]}"}
        for name in names
    }


API_CONTEXT_RESPONSE_HEADERS = response_headers(
    "Cache-Control",
    "Content-Length",
    "Content-Type",
    "Content-Encoding",
    "Vary",
    "X-Content-Type-Options",
    "Referrer-Policy",
    "Content-Security-Policy",
    "Server-Timing",
    "X-Studio-API-Schema",
)
API_BOUNDARY_RESPONSE_HEADERS = response_headers(
    "X-Studio-API-Schema",
    "X-Studio-Server",
    "Access-Control-Allow-Origin",
    "Access-Control-Expose-Headers",
)


def error_responses(
    statuses: Sequence[int], *, schema_mismatch: bool = False
) -> dict[int | str, dict[str, Any]]:
    """Reference shared error responses; ApiError426 carries the mismatch header."""
    if schema_mismatch and 426 not in statuses:
        statuses = (*statuses, 426)
    return {
        status: {"$ref": f"#/components/responses/ApiError{status}"}
        for status in statuses
    }


def api_read_responses(
    *, errors: Sequence[int] = (), headers: Sequence[str] = (), etag: bool = False
) -> dict[int | str, dict[str, Any]]:
    """Document a JSON read's common boundary errors and emitted headers."""
    success_headers = {**API_CONTEXT_RESPONSE_HEADERS, **API_BOUNDARY_RESPONSE_HEADERS}
    success_headers.update(response_headers(*headers))
    error_statuses = tuple(dict.fromkeys((400, 401, 403, 429, 500, 503, *errors)))
    responses: dict[int | str, dict[str, Any]] = {
        200: {"headers": success_headers},
        **error_responses(error_statuses),
    }
    if etag:
        responses[200]["headers"] = {**success_headers, **response_headers("ETag")}
        responses[304] = {
            "description": "Not Modified",
            "headers": {
                **API_CONTEXT_RESPONSE_HEADERS,
                **API_BOUNDARY_RESPONSE_HEADERS,
                **response_headers("ETag"),
            },
        }
    return responses


def api_write_responses(
    *, errors: Sequence[int] = (), headers: Sequence[str] = ()
) -> dict[int | str, dict[str, Any]]:
    """Document a JSON write's common boundary errors and emitted headers."""
    success_headers = {**API_CONTEXT_RESPONSE_HEADERS, **API_BOUNDARY_RESPONSE_HEADERS}
    success_headers.update(response_headers(*headers))
    return {
        200: {"headers": success_headers},
        **error_responses(
            tuple(dict.fromkeys((400, 401, 403, 408, 409, 410, 413, 415, 426, 429, 500, 502, 503, *errors)))
        ),
    }


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
        header_schemas = components.setdefault("headers", {})
        if not isinstance(header_schemas, dict):
            raise TypeError("OpenAPI headers must be an object")
        header_schemas.update(_HEADER_COMPONENTS)
        response_components = components.setdefault("responses", {})
        if not isinstance(response_components, dict):
            raise TypeError("OpenAPI responses must be an object")
        error_headers = {**API_CONTEXT_RESPONSE_HEADERS, **API_BOUNDARY_RESPONSE_HEADERS}
        for status, description in _API_ERROR_RESPONSE_DESCRIPTIONS.items():
            headers = dict(error_headers)
            if status == 426:
                headers.update(response_headers("X-Studio-API-Schema-Mismatch"))
            response_components[f"ApiError{status}"] = {
                "description": description,
                "headers": headers,
                "content": {
                    "application/json": {
                        "schema": {"$ref": "#/components/schemas/ErrorResponse"}
                    }
                },
            }

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
                response_413 = responses.get("413")
                if isinstance(response_413, dict) and (
                    method.upper() in WRITE_METHODS
                    or response_413.get("description") in PYTHON_413_DESCRIPTIONS
                ):
                    response_413["description"] = OPENAPI_413_DESCRIPTION
                for status in ERROR_STATUS_DESCRIPTIONS:
                    if status in {408, 409, 413, 415} and method.upper() not in WRITE_METHODS:
                        continue
                    documented_response: dict[str, JsonValue] = {
                        "$ref": f"#/components/responses/ApiError{status}"
                    }
                    if status == 413 and method.upper() in WRITE_METHODS:
                        documented_response["description"] = OPENAPI_413_DESCRIPTION
                    responses.setdefault(
                        str(status),
                        documented_response,
                    )
                generated_validation = responses.get("422")
                if isinstance(generated_validation, dict) and "model" not in operation.get("responses", {}).get("422", {}):
                    schema_value = generated_validation.get("content", {})
                    application_json = schema_value.get("application/json", {}) if isinstance(schema_value, dict) else {}
                    detail_schema = application_json.get("schema", {}) if isinstance(application_json, dict) else {}
                    if isinstance(detail_schema, dict) and detail_schema.get("$ref") == "#/components/schemas/HTTPValidationError":
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
