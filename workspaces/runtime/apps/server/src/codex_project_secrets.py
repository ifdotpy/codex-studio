"""Project secrets (docs/project-secrets.md): named values that Studio keeps in the VM, outside the
project tree, and gives to every agent process of one layr project as environment variables.

Run it with the Studio Python from the installed app scripts while the VM runs:

    python3 codex_project_secrets.py list PROJECT_FOLDER
    python3 codex_project_secrets.py set PROJECT_FOLDER NAME < value-file
    python3 codex_project_secrets.py remove PROJECT_FOLDER NAME

`set` reads the value from stdin; at a terminal it asks for it without echo. The value never
comes from the command line, and nothing prints it. `set` and `remove` print their operation ID
first; after a lost reply, run the same command again with --operation-id ID.
"""
from __future__ import annotations

import argparse
import getpass
import os
from pathlib import Path
import re
import sys
from typing import Any, Protocol
import uuid

from codex_shared_values import (
    MAX_PROJECT_SECRET_VALUE_BYTES,
    project_secret_value_limit_label,
)

# The guest checks the name again and refuses the names that the agent launch sets.
NAME = re.compile(r"[A-Z_][A-Z0-9_]{0,63}")


class GuestClient(Protocol):
    def call(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
             timeout: float = 60) -> Any: ...


def _names(project_id: str, result: Any) -> dict[str, Any]:
    names = result.get('names') if isinstance(result, dict) else None
    if not isinstance(names, list) or not all(isinstance(name, str) for name in names):
        from codex_linux_vm import LinuxVMError
        raise LinuxVMError('The VM returned an invalid list of secret names.', uncertain=True)
    return {'projectId': project_id, 'names': names}


def list_names(client: GuestClient, project_id: str) -> dict[str, Any]:
    """The names of the project's secrets. No call returns a value."""
    return _names(project_id, client.call('secrets.list', {'projectId': project_id}, timeout=30))


def set_secret(client: GuestClient, project_id: str, name: str, value: str, request_id: str) -> dict[str, Any]:
    """Set one secret. A repeated request ID returns the saved reply and does not set it again."""
    return _names(project_id, client.call('secrets.set', {'projectId': project_id, 'name': name, 'value': value},
                                          request_id='secrets-set:' + request_id, timeout=30))


def remove_secret(client: GuestClient, project_id: str, name: str, request_id: str) -> dict[str, Any]:
    return _names(project_id, client.call('secrets.remove', {'projectId': project_id, 'name': name},
                                          request_id='secrets-remove:' + request_id, timeout=30))


def read_value(stdin: Any = None) -> str:
    """The value from stdin: hidden at a terminal, else the whole input without one final newline."""
    stream = stdin or sys.stdin
    if stream.isatty():
        value = getpass.getpass('Value (hidden): ')
    else:
        value = stream.read(MAX_PROJECT_SECRET_VALUE_BYTES * 4 + 2)
        value = value[:-1] if value.endswith('\n') else value
        value = value[:-1] if value.endswith('\r') else value
    if not value:
        raise ValueError('The value is empty')
    if len(value.encode('utf-8', 'surrogatepass')) > MAX_PROJECT_SECRET_VALUE_BYTES:
        raise ValueError(f'The value exceeds {project_secret_value_limit_label()}')
    return value


def main(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser(prog='codex_project_secrets.py',
                                     description='List, set or remove the secrets of a layr project.')
    parser.add_argument('command', choices=('list', 'set', 'remove'))
    parser.add_argument('project', type=Path, help='the project folder on this Mac')
    parser.add_argument('name', nargs='?', help='the variable name, for set and remove')
    parser.add_argument('--operation-id', default=None, help='repeat an earlier set or remove')
    options = parser.parse_args(arguments)
    if options.command != 'list' and not (options.name and NAME.fullmatch(options.name)):
        parser.error('give a NAME that matches ^[A-Z_][A-Z0-9_]{0,63}$')
    from codex_layr_migration import project_id
    from codex_linux_vm import Client, LinuxVMError
    project = project_id(Path(os.path.abspath(options.project.expanduser())))
    try:
        client = Client()
        if options.command == 'list':
            result = list_names(client, project)
        else:
            value = read_value() if options.command == 'set' else ''
            operation = options.operation_id or str(uuid.uuid4())
            print(f'operation {operation}', file=sys.stderr, flush=True)
            if options.command == 'set':
                result = set_secret(client, project, options.name, value, operation)
            else:
                result = remove_secret(client, project, options.name, operation)
    except LinuxVMError as error:
        print('project secrets: ' + (error.detail or str(error)), file=sys.stderr)
        if error.uncertain:
            print('The change may have applied. Run list to check, or repeat with --operation-id.', file=sys.stderr)
        return 1
    except ValueError as error:
        print(f'project secrets: {error}', file=sys.stderr)
        return 2
    for name in result['names']:
        print(name)
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
