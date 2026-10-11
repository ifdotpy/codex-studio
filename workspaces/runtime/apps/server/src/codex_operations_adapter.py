"""Fail-fast import and version check for the native operations boundary."""

from __future__ import annotations

PROTOCOL_VERSION = 3

try:
    import studio_operations_native as native
except ImportError as error:
    raise RuntimeError(
        "The backend requires studio_operations_native. Build it with "
        "`cargo build --release -p studio-operations-python`."
    ) from error


def check_protocol(module: object) -> None:
    if getattr(module, "PROTOCOL_VERSION", None) == PROTOCOL_VERSION:
        return
    raise RuntimeError(
        "studio_operations_native protocol mismatch; rebuild it with "
        "`cargo build --release -p studio-operations-python`."
    )


check_protocol(native)


def check_settlement_api(module: object) -> None:
    required = ("settle_tool_request", "missing_tool_request_outcome")
    missing = [name for name in required if not callable(getattr(module, name, None))]
    if not missing:
        return
    raise RuntimeError(
        "studio_operations_native is missing " + ", ".join(missing) + "; rebuild it with "
        "`cargo build --release -p studio-operations-python`."
    )


check_settlement_api(native)
