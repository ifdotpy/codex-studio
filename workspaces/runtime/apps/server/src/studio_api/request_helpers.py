"""Small shared helpers for decoding HTTP request values."""

from __future__ import annotations

from typing import cast

from studio_api.models import ContractModel, JsonValue


def body_data(model: ContractModel) -> dict[str, JsonValue]:
    """Dump supplied request fields in JSON mode using model default aliases."""
    return cast(dict[str, JsonValue], model.model_dump(mode="json", exclude_unset=True))
