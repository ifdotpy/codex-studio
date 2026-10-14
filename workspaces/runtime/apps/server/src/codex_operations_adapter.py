"""Fail-fast import and version check for the native operations boundary."""

from __future__ import annotations

from codex_shared_values import OPERATIONS_PYTHON_PROTOCOL_VERSION

PROTOCOL_VERSION = OPERATIONS_PYTHON_PROTOCOL_VERSION

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
