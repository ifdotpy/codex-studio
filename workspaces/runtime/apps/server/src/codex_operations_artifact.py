"""Own native operations artifact paths for checkout tests and installation."""

import os
from pathlib import Path
import platform
from typing import Literal

OPERATIONS_BUILD_COMMAND = "cargo build --release -p studio-operations-python"
NATIVE_MODULE = "studio_operations_native"


def operations_extension(project: Path | str, packaged_source: Path | None = None, *,
                         profile: Literal["debug", "release"] = "release") -> tuple[Path, str]:
    project = Path(project).resolve(strict=True)
    packaged = Path(packaged_source) if packaged_source else None
    configured_target = Path(os.environ.get("CARGO_TARGET_DIR", project / "target"))
    if not configured_target.is_absolute():
        configured_target = project / configured_target
    system = platform.system()
    if system == "Linux":
        names = ("libstudio_operations_native.so", "studio_operations_native.abi3.so")
        installed_name = "studio_operations_native.abi3.so"
    elif system == "Darwin":
        names = ("libstudio_operations_native.dylib", "studio_operations_native.abi3.so")
        installed_name = "studio_operations_native.abi3.so"
    elif system == "Windows":
        names = ("studio_operations_native.dll", "studio_operations_native.pyd")
        installed_name = "studio_operations_native.pyd"
    else:
        raise RuntimeError(f"Unsupported operations module platform: {system}")
    candidates = [configured_target / profile / name for name in names]
    if profile == "release":
        candidates.append(project / "resources/workspace/bin" / installed_name)
        if packaged is not None:
            candidates.append(packaged.parent / "bin" / installed_name)
            candidates.append(packaged.parent.parent / "bin" / installed_name)
    for candidate in candidates:
        if candidate.is_file():
            return candidate, installed_name
    if profile == "debug":
        raise RuntimeError(
            "The checkout requires its native operations module; run "
            "`cargo build --locked -p studio-operations-python` with the same CARGO_TARGET_DIR."
        )
    raise RuntimeError(
        f"The backend requires {NATIVE_MODULE}; build it with `{OPERATIONS_BUILD_COMMAND}`."
    )
