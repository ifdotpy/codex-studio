#!/usr/bin/env python3
"""Install command links without replacing unrelated executables."""

import argparse
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import uuid
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from codex_python import prepare_environment, resolve_python

OPERATIONS_BUILD_COMMAND = "cargo build --release -p studio-operations-python"
NATIVE_MODULE = "studio_operations_native"


def operations_extension(project, packaged_source=None):
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
    candidates = [configured_target / "release" / name for name in names]
    candidates.append(project / "resources/workspace/bin" / installed_name)
    if packaged is not None:
        candidates.append(packaged.parent / "bin" / installed_name)
        candidates.append(packaged.parent.parent / "bin" / installed_name)
    for candidate in candidates:
        if candidate.is_file():
            return candidate, installed_name
    raise RuntimeError(
        f"The backend requires {NATIVE_MODULE}; build it with `{OPERATIONS_BUILD_COMMAND}`."
    )


def install_operations_extension(extension, installed_name, python):
    site_packages = subprocess.run(
        [str(python), "-c", "import sysconfig; print(sysconfig.get_paths()['platlib'])"],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout.strip()
    destination = Path(site_packages) / installed_name
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name("." + destination.name + "." + uuid.uuid4().hex)
    try:
        shutil.copy2(extension, temporary)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    check = subprocess.run(
        [str(python), "-c", "import studio_operations_native as m; assert m.PROTOCOL_VERSION == 1"],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )
    if check.returncode:
        raise RuntimeError(
            "The installed studio_operations_native protocol is incompatible; "
            f"rebuild it with `{OPERATIONS_BUILD_COMMAND}`. {check.stderr.strip()}"
        )
    return destination

COMMANDS = (
    "codex-canvas",
    "codex-chat",
    "codex-control",
    "codex-supervisor",
    "codex-daemon",
    "codex-graph",
    "codex-models",
    "codex-report",
    "codex-steer",
    "codex-stop",
    "codex-swarm.mjs",
    "codex-watch",
    "luna",
)


def diagnostics_binary(project, packaged_source=None):
    project = Path(project).resolve(strict=True)
    packaged = Path(packaged_source) if packaged_source else None
    configured_target = Path(os.environ.get("CARGO_TARGET_DIR", project / "target"))
    if not configured_target.is_absolute():
        configured_target = project / configured_target
    if os.environ.get("CARGO_TARGET_DIR"):
        candidates = [configured_target / "release/codex-diagnostics"]
    else:
        candidates = [project / "target/release/codex-diagnostics"]
    candidates.append(project / "resources/workspace/bin/codex-diagnostics")
    if packaged is not None:
        candidates.append(packaged.parent / "bin/codex-diagnostics")
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def install(project, destination, replace_from=None, *, python=None, packaged_source=None):
    project = Path(project).resolve(strict=True)
    destination = Path(destination).expanduser().absolute()
    old = Path(replace_from).expanduser().resolve() if replace_from else None
    source_root = (
        SERVER_SOURCE_ROOT
        if project == REPOSITORY_ROOT.resolve()
        else project / "workspaces/runtime/apps/server/src"
    )
    if not source_root.is_dir():
        source_root = project / "scripts"
    links = []
    for name in COMMANDS:
        source = source_root / name
        if not source.is_file() or not os.access(source, os.X_OK):
            raise ValueError(f"Missing executable: {source}")
        target = destination / name
        if target.exists() or target.is_symlink():
            if not target.is_symlink():
                raise ValueError(
                    f"Refusing to replace an existing executable: {target}"
                )
            resolved = target.resolve()
            old_sources = () if old is None else (
                old / "scripts" / name,
                old / "workspaces/runtime/apps/server/src" / name,
            )
            if resolved != source and all(resolved != path.resolve() for path in old_sources):
                raise ValueError(
                    f"Refusing to replace an unrelated command link: {target}"
                )
        links.append((source, target))
    diagnostic_source = diagnostics_binary(project, source_root)
    extension_source, extension_name = operations_extension(
        project, packaged_source or source_root
    )
    diagnostic_target = destination / "codex-diagnostics"
    if diagnostic_source is None:
        print(
            "codex-diagnostics not installed: build it with `cargo build --locked --release -p studio-diagnostics`.",
            file=sys.stderr,
        )
    elif diagnostic_target.exists() or diagnostic_target.is_symlink():
        if not diagnostic_target.is_symlink():
            raise ValueError(
                f"Refusing to replace an existing executable: {diagnostic_target}"
            )
        resolved = diagnostic_target.resolve()
        old_sources = () if old is None else (
            old / "scripts" / "codex-diagnostics",
            old / "workspaces/runtime/apps/server/src" / "codex-diagnostics",
            old / "target/release/codex-diagnostics",
        )
        if resolved != diagnostic_source and all(
            resolved != path.resolve() for path in old_sources
        ):
            raise ValueError(
                f"Refusing to replace an unrelated command link: {diagnostic_target}"
            )
    if diagnostic_source is not None:
        links.append((diagnostic_source, diagnostic_target))
    if python is not None:
        install_operations_extension(extension_source, extension_name, python)
    destination.mkdir(parents=True, exist_ok=True)
    for source, target in links:
        if target.is_symlink() and target.resolve() == source:
            continue
        temporary = destination / ("." + target.name + "." + uuid.uuid4().hex)
        try:
            temporary.symlink_to(source)
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)
    return [str(target) for _, target in links]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bin-dir", default=os.environ.get("CODEX_AGENTS_BIN_DIR", "~/.local/bin")
    )
    parser.add_argument(
        "--replace-from",
        help="Previous project root whose matching command links can be replaced",
    )
    parser.add_argument(
        "--dev",
        action="store_true",
        help="Also prepare the isolated development and mypy environment",
    )
    args = parser.parse_args()
    try:
        project = REPOSITORY_ROOT
        operations_extension(REPOSITORY_ROOT, SERVER_SOURCE_ROOT)
        if os.environ.get("CODEX_AGENTS_PYTHON"):
            python = resolve_python(SERVER_SOURCE_ROOT)
        else:
            python = prepare_environment(project)
        if args.dev:
            prepare_environment(project, development=True)
        paths = install(
            project, args.bin_dir, args.replace_from, python=python
        )
    except (ValueError, OSError, RuntimeError) as error:
        parser.exit(1, str(error) + "\n")
    print(f"Installed {len(paths)} command links in {Path(paths[0]).parent}")


if __name__ == "__main__":
    main()
