"""Shared safe API error responses."""
from __future__ import annotations

from collections.abc import Mapping

from starlette.requests import Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.models import ErrorResponse, JsonValue


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
