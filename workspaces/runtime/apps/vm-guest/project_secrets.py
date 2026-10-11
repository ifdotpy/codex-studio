"""Root broker methods of project secrets (docs/project-secrets.md): named values that every
agent process of one project gets in its environment when it starts (project_env.py).

The values of a project stay in one root-only file (mode 0600, folder 0700) in the broker state,
outside every layr line. No method returns a value, and no error message contains one."""
from __future__ import annotations

import json
from pathlib import Path

from common import GuestError, atomic_bytes, private_dir, require
from layr_agents import LAUNCH_ENV_NAMES, PROVIDER_ENV
from layr_share import project_id
from project_env import FD_VARIABLE, NAME, read_secrets, secrets_path

READ_METHODS = {"secrets.list"}
METHODS = READ_METHODS | {"secrets.set", "secrets.remove"}
MAX_VALUE_BYTES = 16 * 1024
MAX_SECRETS = 64
MAX_TOTAL_BYTES = 256 * 1024
# The launch and the providers set these names; Studio and the providers own the prefixes.
RESERVED = LAUNCH_ENV_NAMES.union(*PROVIDER_ENV.values(), {FD_VARIABLE, "CLAUDECODE"})
RESERVED_PREFIXES = ("STUDIO_", "LAYR_", "CODEX_", "CLAUDE_")


def check_name(value):
    require(isinstance(value, str) and NAME.fullmatch(value),
            "The secret name must match ^[A-Z_][A-Z0-9_]{0,63}$")
    require(value not in RESERVED and not value.startswith(RESERVED_PREFIXES),
            f"The name {value} is reserved: Studio or the provider sets it")
    return value


def check_value(value):
    require(isinstance(value, str) and value, "The secret value must be a string that is not empty")
    require("\0" not in value, "The secret value must not contain a NUL character")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeEncodeError:
        raise GuestError("invalid_params", "The secret value must be UTF-8 text") from None
    require(size <= MAX_VALUE_BYTES, "The secret value exceeds 16 KiB")
    return value


class SecretHandlers:
    def __init__(self, state, root):
        self.state, self.root = Path(state), Path(root)
        private_dir(self.state / "secrets")

    def save(self, project, secrets):
        # atomic_bytes writes a new 0600 file, syncs it and renames it over the old one.
        private_dir(self.state / "secrets")
        atomic_bytes(secrets_path(self.state, project),
                     json.dumps({"version": 1, "secrets": secrets}, sort_keys=True).encode())

    async def dispatch(self, request_id, method, params, emit):
        project = project_id(params.get("projectId"))
        if not (self.root / "projects" / project / "lines/main").is_dir():
            raise GuestError("not_found", "The layr project does not exist")
        try:
            secrets = read_secrets(secrets_path(self.state, project))
        except (OSError, ValueError):
            raise GuestError("operation_failed", "The project secret file is unreadable") from None
        # No await between the read and the write: one change of a project at a time.
        if method == "secrets.set":
            name, value = check_name(params.get("name")), check_value(params.get("value"))
            secrets = {**secrets, name: value}
            require(len(secrets) <= MAX_SECRETS, f"A project has at most {MAX_SECRETS} secrets")
            require(sum(len(key) + len(item.encode()) for key, item in secrets.items()) <= MAX_TOTAL_BYTES,
                    "The secrets of a project exceed 256 KiB")
            # Always write: the reply must not tell whether the value was the same.
            self.save(project, secrets)
        elif method == "secrets.remove":
            name = params.get("name")
            require(isinstance(name, str) and NAME.fullmatch(name),
                    "The secret name must match ^[A-Z_][A-Z0-9_]{0,63}$")
            if secrets.pop(name, None) is not None:
                self.save(project, secrets)
        else:
            require(method == "secrets.list", "The secret method is not supported")
        return {"projectId": project, "names": sorted(secrets)}
