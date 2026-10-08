"""Apply private permissions to Studio state paths."""
from __future__ import annotations

import os
from pathlib import Path
import subprocess
import csv


def protect(path: str | Path, *, directory: bool = False) -> None:
    """Restrict a private path to its owner and the system on each platform."""
    target = Path(path)
    if os.name != "nt":
        os.chmod(target, 0o700 if directory else 0o600)
        return

    identity = subprocess.run(
        ["whoami.exe", "/user", "/fo", "csv", "/nh"], capture_output=True,
        text=True, check=True, timeout=10,
    ).stdout
    fields = next(csv.reader(identity.splitlines()), [])
    sid = fields[1] if len(fields) > 1 else ""
    if not sid.startswith("S-"):
        raise RuntimeError("Cannot determine the Windows account for private state ACLs")
    inheritance = "(OI)(CI)F" if directory else "F"
    subprocess.run(
        ["icacls.exe", str(target), "/inheritance:r", "/grant:r", f"*{sid}:{inheritance}"],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )


def ensure_private_dir(path: str | Path) -> Path:
    target = Path(path)
    target.mkdir(parents=True, exist_ok=True, mode=0o700)
    if os.name == "nt":
        protect(target, directory=True)
    return target


def protect_temp_file(path: str | Path) -> Path:
    target = Path(path)
    protect(target)
    return target
