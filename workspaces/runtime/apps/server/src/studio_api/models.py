"""Strict shared models for the FastAPI boundary."""
from __future__ import annotations

from enum import StrEnum
import math
import re
from typing import Annotated, Literal, TypeAlias, cast

from pydantic import (
    BaseModel,
    AfterValidator,
    BeforeValidator,
    ConfigDict,
    Field,
    GetCoreSchemaHandler,
    JsonValue as PydanticJsonValue,
    StrictInt,
)
from pydantic_core import CoreSchema, core_schema


def _validate_finite_json(value: object) -> object:
    """Keep arbitrary JSON numbers portable across JavaScript and Rust."""
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("JSON numbers must be finite")
    if isinstance(value, list):
        for item in value:
            _validate_finite_json(item)
    elif isinstance(value, dict):
        for item in value.values():
            _validate_finite_json(item)
    return value


# JavaScript's exact integer range is the cross-runtime wire limit. API integer
# fields default to non-negative values; signed values use SignedSafeInt only
# where the domain gives negative numbers meaning.
MAX_SAFE_INTEGER = 9_007_199_254_740_991
MIN_SAFE_INTEGER = -MAX_SAFE_INTEGER


def _validate_safe_int(value: int) -> int:
    if value < 0 or value > MAX_SAFE_INTEGER:
        raise ValueError(f"Integer must be between 0 and {MAX_SAFE_INTEGER}")
    return value


def _validate_signed_safe_int(value: int) -> int:
    if value < MIN_SAFE_INTEGER or value > MAX_SAFE_INTEGER:
        raise ValueError(
            f"Integer must be between {MIN_SAFE_INTEGER} and {MAX_SAFE_INTEGER}"
        )
    return value


def require_strict_integer(value: object) -> object:
    """Reject bool before integer Literals, where Python otherwise equates True with 1."""
    if type(value) is not int:
        raise ValueError("Value must be an integer")
    return value


# Keep the shared range check in a validator so a field's narrower Field bounds
# compose with it instead of being shadowed by nested Annotated metadata.
SafeInt: TypeAlias = Annotated[StrictInt, AfterValidator(_validate_safe_int)]
SignedSafeInt: TypeAlias = Annotated[
    StrictInt, AfterValidator(_validate_signed_safe_int)
]


def _parse_safe_query_integer(value: object) -> object:
    """Convert a lexically valid query integer to the strict shared int type."""
    if isinstance(value, str) and re.fullmatch(r"[0-9]+", value):
        try:
            return int(value)
        except ValueError:
            return value
    return value


SafeQueryInt: TypeAlias = Annotated[SafeInt, BeforeValidator(_parse_safe_query_integer)]


def _parse_safe_integer_header(value: object) -> object:
    """Parse decimal HTTP header text before applying the shared SafeInt bounds."""
    if isinstance(value, str) and re.fullmatch(r"[0-9]+", value):
        try:
            return int(value)
        except ValueError:
            return value
    return value


SafeIntHeader: TypeAlias = Annotated[SafeInt, BeforeValidator(_parse_safe_integer_header)]
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
    generation: SafeInt


class SyncEntity(ContractModel):
    """One durable sync entity change attached to a successful response."""

    id: str
    seq: SafeInt
    payload: str
    deleted: bool = Field(alias="_deleted")


class ResponseModel(ContractModel):
    """Explicit JSON response with the centrally managed sync envelope."""

    sync_entities: list[SyncEntity] | None = Field(
        default=None,
        alias="_syncEntities",
        serialization_alias="_syncEntities",
    )
    sync_entities_after: SafeInt | None = Field(
        default=None,
        alias="_syncEntitiesAfter",
        serialization_alias="_syncEntitiesAfter",
    )

    def wire_dump(self) -> dict[str, JsonValue]:
        """Serialize set fields only, retaining omitted-versus-null semantics."""
        value = self.model_dump(mode="json", by_alias=True, exclude_unset=True)
        return cast(dict[str, JsonValue], value)


class ErrorResponse(ResponseModel):
    """Standard API error envelope with an optional first-problem location."""

    error: str
    code: str | None = None
    outcome: Literal["not_applied"] | None = None
    location: list[str] | None = None
    metadata: dict[str, JsonValue] | None = None
    catalogPending: bool | None = None
    protocolVersion: SafeInt | None = None
    supportedVersions: list[SafeInt] | None = None
