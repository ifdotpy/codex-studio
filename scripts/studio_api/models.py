"""Strict shared models for the FastAPI boundary."""
from __future__ import annotations

from enum import StrEnum
import math
from typing import Annotated, Literal, TypeAlias, cast

from pydantic import (
    AfterValidator,
    BaseModel,
    ConfigDict,
    Field,
    GetCoreSchemaHandler,
    JsonValue as PydanticJsonValue,
)
from pydantic_core import CoreSchema, core_schema


def _validate_finite_json(value: object) -> object:
    """Reject non-standard NaN and infinity values from extensible JSON data."""
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("JSON numbers must be finite")
    if isinstance(value, list):
        for item in value:
            _validate_finite_json(item)
    elif isinstance(value, dict):
        for item in value.values():
            _validate_finite_json(item)
    return value


JsonValue: TypeAlias = Annotated[PydanticJsonValue, AfterValidator(_validate_finite_json)]


class ContractStrEnum(StrEnum):
    """String enum accepting its JSON string value, with no other coercions."""

    @classmethod
    def __get_pydantic_core_schema__(
        cls,
        source: type[StrEnum],
        handler: GetCoreSchemaHandler,
    ) -> CoreSchema:
        # FastAPI validates parsed JSON through validate_python, where Pydantic's
        # strict enum mode accepts only enum instances. JSON wire values must be
        # strings; disabling strictness for this enum schema accepts those exact
        # values while enum validation continues to reject numbers and booleans.
        schema = handler(source)
        if not isinstance(schema, dict) or schema.get("type") != "enum":
            raise TypeError("ContractStrEnum must use Pydantic's enum schema")
        schema["strict"] = False

        def validate_string_value(value: object) -> object:
            if isinstance(value, (str, cls)):
                return value
            raise ValueError("String enum values must be provided as strings")

        return core_schema.no_info_before_validator_function(validate_string_value, schema)


class ContractModel(BaseModel):
    """Input and nested contract model: reject unknown fields and coercions."""

    model_config = ConfigDict(
        extra="forbid",
        strict=True,
        validate_assignment=True,
        populate_by_name=True,
    )


class SupervisorIdentity(ContractModel):
    stateDir: str
    handle: str
    generation: int


class SyncEntity(ContractModel):
    """One durable sync entity change attached to a successful response."""

    id: str
    seq: int
    payload: str
    deleted: bool = Field(alias="_deleted")


class ResponseModel(ContractModel):
    """Explicit JSON response with the centrally managed sync envelope."""

    sync_entities: list[SyncEntity] | None = Field(
        default=None,
        alias="_syncEntities",
        serialization_alias="_syncEntities",
    )
    sync_entities_after: int | None = Field(
        default=None,
        alias="_syncEntitiesAfter",
        serialization_alias="_syncEntitiesAfter",
    )

    def wire_dump(self) -> dict[str, JsonValue]:
        """Serialize set fields only, retaining omitted-versus-null semantics."""
        value = self.model_dump(mode="json", by_alias=True, exclude_unset=True)
        return cast(dict[str, JsonValue], value)


class ErrorResponse(ResponseModel):
    """Existing API error shape plus explicitly documented optional metadata."""

    error: str
    code: str | None = None
    outcome: Literal["not_applied"] | None = None
    details: JsonValue | None = None
    metadata: dict[str, JsonValue] | None = None
    catalogPending: bool | None = None
    protocolVersion: int | None = None
    supportedVersions: list[int] | None = None
