"""Fail-fast import and version check for the native operations boundary."""

from __future__ import annotations

from importlib.machinery import EXTENSION_SUFFIXES
from pathlib import Path
import sys

from codex_shared_values import OPERATIONS_PYTHON_PROTOCOL_VERSION

PROTOCOL_VERSION = OPERATIONS_PYTHON_PROTOCOL_VERSION

# A packaged backend must use the extension shipped with its own sources,
# rather than a potentially older copy installed in a shared Python environment.
# Checkout tests prepend the artifact selected by codex_operations_artifact in
# the server runner; they never update the shared interpreter's site-packages.
_source = Path(__file__).resolve().parent
_packaged_bin = _source.parent / "bin" if _source.name == "scripts" else None
if _packaged_bin is not None:
    if not any((_packaged_bin / f"studio_operations_native{suffix}").is_file()
               for suffix in EXTENSION_SUFFIXES):
        raise RuntimeError("The Studio package is missing its native operations module.")
    sys.path.insert(0, str(_packaged_bin))
try:
    import studio_operations_native as native
except ImportError as error:
    raise RuntimeError(
        "The backend requires studio_operations_native. Build it with "
        "`cargo build --release -p studio-operations-python`."
    ) from error
finally:
    if _packaged_bin is not None:
        sys.path.remove(str(_packaged_bin))

if _packaged_bin is not None and Path(native.__file__).resolve().parent != _packaged_bin:
    raise RuntimeError("The loaded operations module does not belong to this Studio package.")


def check_protocol(module: object) -> None:
    if getattr(module, "PROTOCOL_VERSION", None) == PROTOCOL_VERSION:
        return
    raise RuntimeError(
        "studio_operations_native protocol mismatch; rebuild it with "
        "`cargo build --release -p studio-operations-python`."
    )


check_protocol(native)
