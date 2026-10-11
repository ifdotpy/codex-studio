"""Start an agent process with its project's secrets in its environment (docs/project-secrets.md).

The root broker starts every agent process of a project through this file:

    python3 -I project_env.py root SECRETS_FILE -- runuser -u OWNER -- python3 -I project_env.py user -- COMMAND...

The root step reads the project's secret file, gives the values to the user step in an
anonymous memory file, and runs runuser. After runuser drops privilege, the user step reads
that file, adds the values to its environment, and runs the command. The values never go into
a command line, a launch record, or the environment of a root process. The command line stays
the same when a secret changes, so a running provider keeps its launch identity; a process
started later reads the current file.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import sys
import tempfile

FD_VARIABLE = "STUDIO_PROJECT_SECRETS_FD"
NAME = re.compile(r"[A-Z_][A-Z0-9_]{0,63}")
RUNUSER = "/usr/sbin/runuser"
MAX_FILE_BYTES = 4 * 1024 * 1024


def secrets_path(state: Path, project: str) -> Path:
    """The root-only secret file of one project, in the broker state outside every layr line."""
    return Path(state) / "secrets" / (project + ".json")


def wrap(state: Path, project: str, owner: str, command: list[str]) -> list[str]:
    """The launch argv of an agent command of `project` that runs as `owner`."""
    script = str(Path(__file__).resolve())
    return [sys.executable, "-I", script, "root", str(secrets_path(state, project)), "--",
            RUNUSER, "-u", owner, "--", sys.executable, "-I", script, "user", "--", *command]


def read_secrets(path: Path) -> dict[str, str]:
    try:
        with open(path, "rb") as stream:
            data = json.loads(stream.read(MAX_FILE_BYTES + 1))
    except FileNotFoundError:
        return {}
    secrets = data.get("secrets") if isinstance(data, dict) else None
    if not isinstance(secrets, dict) or not all(
            isinstance(name, str) and NAME.fullmatch(name) and isinstance(value, str) and "\0" not in value
            for name, value in secrets.items()):
        raise ValueError("The project secret file is invalid")
    return secrets


def anonymous(data: bytes) -> int:
    """An inheritable descriptor of a file with `data` that has no name."""
    if hasattr(os, "memfd_create"):
        descriptor = os.memfd_create("studio-project-secrets", 0)
    else:  # A test host without memfd: an unlinked private file.
        descriptor, name = tempfile.mkstemp()
        os.unlink(name)
    view = memoryview(data)
    while view:
        view = view[os.write(descriptor, view):]
    os.lseek(descriptor, 0, os.SEEK_SET)
    os.set_inheritable(descriptor, True)
    return descriptor


def root(path: Path, command: list[str]) -> None:
    descriptor = anonymous(json.dumps(read_secrets(path)).encode())
    os.execve(command[0], command, {**os.environ, FD_VARIABLE: str(descriptor)})


def user(command: list[str]) -> None:
    environment = dict(os.environ)
    descriptor = int(environment.pop(FD_VARIABLE))
    with os.fdopen(descriptor, "rb") as stream:
        secrets = json.loads(stream.read(MAX_FILE_BYTES + 1))
    if not isinstance(secrets, dict):
        raise ValueError("The project secrets are invalid")
    for name, value in secrets.items():
        # The launch sets its own names; a secret never replaces them.
        if isinstance(name, str) and NAME.fullmatch(name) and isinstance(value, str) and name not in environment:
            environment[name] = value
    os.execvpe(command[0], command, environment)


def main(arguments: list[str]) -> int:
    try:
        step = arguments[0]
        if step == "root" and arguments[2] == "--" and len(arguments) > 3:
            root(Path(arguments[1]), arguments[3:])
        elif step == "user" and arguments[1] == "--" and len(arguments) > 2:
            user(arguments[2:])
        else:
            raise ValueError("Use: root SECRETS_FILE -- COMMAND..., or user -- COMMAND...")
    except (OSError, ValueError, KeyError, IndexError) as error:
        # The message names the failure and at most a path or a name, never a value.
        print(f"project_env: {type(error).__name__}: {error}", file=sys.stderr)
        return 126
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
