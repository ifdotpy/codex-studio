"""Fail-fast import and version check for the native operations boundary."""

from __future__ import annotations

import importlib
from types import ModuleType

PROTOCOL_VERSION = 2

try:
    native: ModuleType = importlib.import_module("studio_operations_native")
except ImportError as error:
    raise RuntimeError(
        "The backend requires studio_operations_native. Build it with "
        "`cargo build --release -p studio-operations-python`."
    ) from error

def check_protocol(module: ModuleType) -> None:
    if getattr(module, "PROTOCOL_VERSION", None) == PROTOCOL_VERSION:
        return
    raise RuntimeError(
        "studio_operations_native protocol mismatch; rebuild it with "
        "`cargo build --release -p studio-operations-python`."
    )


check_protocol(native)
