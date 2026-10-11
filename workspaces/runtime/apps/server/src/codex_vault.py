"""The project vault from the Mac (docs/vault.md): configure the VM writer, push, read the status.

    python3 codex_vault.py configure PROJECT_FOLDER < config.json   (vault-deploy pipes it)
    python3 codex_vault.py push PROJECT_FOLDER
    python3 codex_vault.py status PROJECT_FOLDER

The configuration holds the writer's AWS key; it goes from stdin to the root broker and is never
printed or written on the Mac.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys
import uuid

from codex_layr_migration import project_id

FIELDS = ("bucket", "region", "prefix", "recipient", "accessKeyId", "secretAccessKey")


def main(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="codex_vault.py")
    parser.add_argument("command", choices=("configure", "push", "status"))
    parser.add_argument("project", type=Path)
    options = parser.parse_args(arguments)
    from codex_linux_vm import Client
    client = Client()
    project = project_id(Path(os.path.abspath(options.project.expanduser())))
    if options.command == "configure":
        given = json.load(sys.stdin)
        missing = [field for field in FIELDS if not isinstance(given.get(field), str)]
        if missing:
            parser.error("the configuration lacks " + ", ".join(missing))
        result = client.call("vault.configure", {"projectId": project, **{f: given[f] for f in FIELDS}},
                             request_id="vault-configure:" + str(uuid.uuid4()), timeout=60)
    elif options.command == "push":
        result = client.call("vault.push", {"projectId": project}, request_id="vault-push:" + str(uuid.uuid4()),
                             timeout=1800)
    else:
        result = client.call("vault.status", {"projectId": project}, timeout=60)
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
