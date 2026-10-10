"""Identify the backend source loaded at process startup."""
import hashlib
from pathlib import Path

from codex_source_inventory import source_files
from codex_layout import SERVER_SOURCE_ROOT


def backend_build(scripts=None):
    scripts = Path(scripts) if scripts is not None else SERVER_SOURCE_ROOT
    files = source_files(scripts)
    if not any(name == "codex-canvas" for name, _ in files):
        raise ValueError("The backend entry point is missing")
    digest = hashlib.sha256()
    for name, path in files:
        digest.update((name + "\0" + hashlib.sha256(path.read_bytes()).hexdigest() + "\n").encode())
    return digest.hexdigest()


# Never recalculate this value for an identity request. Installed files can
# change while the existing process continues to run its original methods.
BACKEND_BUILD = backend_build()
