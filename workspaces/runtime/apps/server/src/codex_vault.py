"""The project vault from the Mac (docs/vault.md): configure the VM writer, push, read the status.

    python3 codex_vault.py configure PROJECT_FOLDER < config.json   (vault-deploy pipes it)
    python3 codex_vault.py push PROJECT_FOLDER
    python3 codex_vault.py status PROJECT_FOLDER
    python3 codex_vault.py site PROJECT_FOLDER https://desk.example.com/

The configuration holds the writer's AWS key; it goes from stdin to the root broker and is never
printed or written on the Mac. Its optional "site", the Desk site of the vault, stays on the Mac
for the sidebar's Desk link; `site` sets it for a vault configured without one.
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
    parser.add_argument("command", choices=("configure", "push", "status", "site"))
    parser.add_argument("project", type=Path)
    parser.add_argument("site", nargs="?")
    options = parser.parse_args(arguments)
    from codex_linux_vm import Client
    client = Client()
    project = project_id(Path(os.path.abspath(options.project.expanduser())))
    if options.command == "site":
        if not options.site:
            parser.error("site needs the Desk site URL")
        from codex_vm_vault import store_site
        store_site(client.state_dir, project, options.site)
        print(json.dumps({"site": options.site}))
        return 0
    if options.command == "configure":
        given = json.load(sys.stdin)
        missing = [field for field in FIELDS if not isinstance(given.get(field), str)]
        if missing:
            parser.error("the configuration lacks " + ", ".join(missing))
        result = client.call("vault.configure", {"projectId": project, **{f: given[f] for f in FIELDS}},
                             request_id="vault-configure:" + str(uuid.uuid4()), timeout=60)
        if isinstance(given.get("site"), str):
            from codex_vm_vault import store_site
            store_site(client.state_dir, project, given["site"])
    elif options.command == "push":
        result = client.call("vault.push", {"projectId": project}, request_id="vault-push:" + str(uuid.uuid4()),
                             timeout=1800)
    else:
        result = client.call("vault.status", {"projectId": project}, timeout=60)
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
