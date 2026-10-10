"""Small shared helpers for decoding HTTP request values."""

from __future__ import annotations

from typing import cast

from fastapi import Request

from studio_api.models import ContractModel, JsonValue


def body_data(model: ContractModel) -> dict[str, JsonValue]:
    """Dump supplied request fields in JSON mode using model default aliases."""
    return cast(dict[str, JsonValue], model.model_dump(mode="json", exclude_unset=True))


def first_nonempty_query(
    request: Request,
    name: str,
    default: str | None = None,
) -> str | None:
    """Return the first nonempty repeated query value, as legacy parse_qs did."""
    return next(
        (value for value in request.query_params.getlist(name) if value != ""),
        default,
    )
