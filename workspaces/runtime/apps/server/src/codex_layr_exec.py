"""Run a command as the owner of a project's VM main line, in main (docs/vm-layr.md, "Operator commands").

Run it with the Studio Python from the installed app scripts while the VM runs:

    python3 codex_layr_exec.py [--subpath PATH] [--timeout SECONDS] [--operation-id ID] PROJECT_FOLDER -- COMMAND...

It prints the operation ID first. After a lost reply, run it again with --operation-id: the
guest returns the saved result and does not run the command a second time.
"""
from __future__ import annotations

import argparse
import base64
import os
import sys
from pathlib import Path
import uuid

from codex_layr_migration import project_id


def main(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser(prog='codex_layr_exec.py')
    parser.add_argument('--subpath', default='.', help='folder in main to run in')
    parser.add_argument('--timeout', type=float, default=300, help='seconds, at most 1800')
    parser.add_argument('--operation-id', default=None)
    parser.add_argument('project', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args(arguments)
    command = options.command[1:] if options.command[:1] == ['--'] else options.command
    if not command:
        parser.error('give the command after --')
    from codex_linux_vm import Client
    operation = options.operation_id or 'project-exec:' + str(uuid.uuid4())
    print(f'operation {operation}', file=sys.stderr, flush=True)
    folder = Path(os.path.abspath(options.project.expanduser()))
    result = Client().call('project.exec', {
        'projectId': project_id(folder), 'argv': command, 'subpath': options.subpath,
        'timeoutSeconds': options.timeout}, request_id=operation, timeout=options.timeout + 30)
    sys.stdout.buffer.write(base64.b64decode(result['stdout']))
    sys.stderr.buffer.write(base64.b64decode(result['stderr']))
    if result['exitCode'] is None:
        print(f'stopped: {result["reason"]}', file=sys.stderr)
        return 1
    return int(result['exitCode'])


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
