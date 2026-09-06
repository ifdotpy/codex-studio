#!/usr/bin/env python3
"""Install command links without replacing unrelated executables."""

import argparse
import os
from pathlib import Path
import sys
import uuid

COMMANDS = (
    "codex-canvas",
    "codex-chat",
    "codex-control",
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


def install(project, destination, replace_from=None):
    project = Path(project).resolve(strict=True)
    destination = Path(destination).expanduser().absolute()
    old = Path(replace_from).expanduser().resolve() if replace_from else None
    links = []
    for name in COMMANDS:
        source = project / "scripts" / name
        if not source.is_file() or not os.access(source, os.X_OK):
            raise ValueError(f"Missing executable: {source}")
        target = destination / name
        if target.exists() or target.is_symlink():
            if not target.is_symlink():
                raise ValueError(
                    f"Refusing to replace an existing executable: {target}"
                )
            resolved = target.resolve()
            if resolved != source and (
                old is None or resolved != old / "scripts" / name
            ):
                raise ValueError(
                    f"Refusing to replace an unrelated command link: {target}"
                )
        links.append((source, target))
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
    args = parser.parse_args()
    try:
        paths = install(
            Path(__file__).resolve().parents[1], args.bin_dir, args.replace_from
        )
    except (ValueError, OSError) as error:
        parser.exit(1, str(error) + "\n")
    print(f"Installed {len(paths)} command links in {Path(paths[0]).parent}")


if __name__ == "__main__":
    main()
