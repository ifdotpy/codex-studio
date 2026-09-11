"""Identify the backend source loaded at process startup."""
import hashlib
from pathlib import Path


def backend_build(scripts=None):
    scripts = Path(scripts) if scripts is not None else Path(__file__).parent
    files = sorted(path for path in scripts.iterdir()
                   if path.is_file() and (path.suffix == ".py" or path.name == "codex-canvas"))
    if not any(path.name == "codex-canvas" for path in files):
        raise ValueError("The backend entry point is missing")
    digest = hashlib.sha256()
    for path in files:
        digest.update((path.name + "\0" + hashlib.sha256(path.read_bytes()).hexdigest() + "\n").encode())
    return digest.hexdigest()


# Never recalculate this value for an identity request. Installed files can
# change while the existing process continues to run its original methods.
BACKEND_BUILD = backend_build()
