#!/usr/bin/env python3
"""Vendor one committed revision of layr or layr-desk. Never read its working tree."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

# The paths that each vendored source takes from its repository (docs/vm-layr.md, docs/vault.md).
SOURCES = {
    "layr": ("src", "Cargo.toml", "Cargo.lock", "LICENSE"),
    "layr-desk": ("Cargo.toml", "Cargo.lock", "crates/desk-core", "crates/layr-desk", "LICENSE"),
}


def update(source: Path, revision: str, destination: Path, paths: tuple[str, ...] = SOURCES["layr"]) -> None:
    def git(*args: str) -> bytes:
        return subprocess.run(["git", "-C", str(source), *args], check=True,
                              capture_output=True, timeout=30).stdout

    commit = git("rev-parse", revision + "^{commit}").decode().strip()
    tree = git("rev-parse", commit + "^{tree}").decode().strip()
    payload = git("archive", commit, *paths)
    destination.mkdir(parents=True, exist_ok=True)
    # Only files recorded by the preceding vendor manifest can be removed.
    manifest = destination / "manifest.json"
    previous = json.loads(manifest.read_text()) if manifest.exists() else {}
    for name in previous.get("files", {}):
        path = destination / name
        if path.is_relative_to(destination) and ".." not in Path(name).parts:
            path.unlink(missing_ok=True)
    files = {}
    with tarfile.open(fileobj=io.BytesIO(payload)) as archive:
        for member in archive:
            if not member.isfile() or ".." in Path(member.name).parts or member.name.startswith("/"):
                continue
            stream = archive.extractfile(member)
            assert stream is not None
            data = stream.read()
            path = destination / member.name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            files[member.name] = hashlib.sha256(data).hexdigest()
    identity = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    manifest.write_text(json.dumps({"revision": commit, "tree": tree,
                                   "sourceSha256": identity, "files": files}, indent=2) + "\n")
    print(json.dumps({"revision": commit, "sourceSha256": identity, "files": len(files)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", choices=sorted(SOURCES), default="layr")
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--destination", type=Path,
                        help="the vendored folder; by default workspaces/runtime/apps/<name>")
    arguments = parser.parse_args()
    destination = arguments.destination or Path(__file__).resolve().parents[2] / arguments.name
    update(arguments.source.resolve(), arguments.revision, destination.resolve(), SOURCES[arguments.name])
