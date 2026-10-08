"""Resolve platform executables and launch Windows command shims safely."""
from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
from typing import Mapping


def which(
    name: str, *, environment: Mapping[str, str] | None = None
) -> str | None:
    values = os.environ if environment is None else environment
    found = shutil.which(name, path=values.get("PATH"))
    if found or os.name != "nt":
        return found
    extensions = values.get("PATHEXT", ".COM;.EXE;.BAT;.CMD").split(";")
    path_exts = {suffix.casefold() for suffix in extensions}
    candidate = Path(name)
    if candidate.suffix.casefold() in path_exts:
        return None
    for suffix in extensions:
        found = shutil.which(name + suffix, path=values.get("PATH"))
        if found:
            return found
    return None


def tailscale() -> str | None:
    found = which("tailscale")
    if found:
        return found
    if os.name == "nt":
        for root in (os.environ.get("ProgramFiles"), os.environ.get("LOCALAPPDATA")):
            if root:
                candidate = Path(root) / ("Tailscale" if root == os.environ.get("ProgramFiles") else "Tailscale") / "tailscale.exe"
                if candidate.is_file():
                    return str(candidate)
    return None


def command(executable: str | Path, arguments: list[str] | tuple[str, ...]) -> list[str] | str:
    path = str(executable)
    if os.name == "nt" and Path(path).suffix.casefold() in {".cmd", ".bat"}:
        # cmd expands percent variables even in quotes. Reject shell syntax in
        # arguments; callers must use a native executable for arbitrary input.
        if any(any(char in item for char in "%!&|<>^\"\n\r") for item in (path, *arguments)):
            raise ValueError("Command shim arguments contain shell control characters")
        def quote(item: str) -> str:
            return f'"{item}"' if any(char.isspace() for char in item) else item

        comspec = os.environ.get("COMSPEC", "cmd.exe")
        invocation = " ".join(quote(item) for item in (path, *arguments))
        return f'{quote(comspec)} /d /c call {invocation}'
    return [path, *arguments]
