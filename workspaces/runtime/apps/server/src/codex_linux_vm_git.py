"""Transfer only the host's global Git author identity into the guest home."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import subprocess
import threading
import uuid
from typing import Protocol

MAX_IDENTITY_BYTES = 8192


class RuntimePort(Protocol):
    root: Path


class TransferClient(Protocol):
    def request(self, method: str, params: dict[str, object], *, request_id: str, timeout: int) -> object: ...


def read_git_identity() -> dict[str, str] | None:
    values = {}
    for key in ('user.name', 'user.email'):
        result = subprocess.run(['git', 'config', '--global', '--null', '--get', key],
                                capture_output=True, timeout=5)
        if result.returncode == 1:
            continue
        if result.returncode or not result.stdout.endswith(b'\0'):
            raise ValueError('Cannot read the host global Git identity')
        value = result.stdout[:-1].decode('utf-8')
        if len(result.stdout) > MAX_IDENTITY_BYTES or any(c in value for c in ('\n', '\r', '\0')):
            raise ValueError('The host Git identity has an invalid size, newline, or NUL')
        if value.strip():
            values[key] = value
    if not values:
        return None
    text = '[user]\n'
    for key, value in values.items():
        # Git config quoting, not shell quoting. Always quote spaces, comments,
        # section characters and quotes; escape only supported Git sequences.
        value = value.replace('\\', '\\\\').replace('"', '\\"').replace('\t', '\\t').replace('\b', '\\b')
        text += '\t' + key.removeprefix('user.') + ' = "' + value + '"\n'
    return {'path': '.gitconfig', 'data': base64.b64encode(text.encode('utf-8')).decode('ascii')}


def sync_git_identity(runtime: RuntimePort, client: TransferClient, *, force: bool = False) -> None:
    """Persist each transfer identity before sending it; preserve lost replies."""
    from codex_linux_vm import _atomic_json
    lock = runtime.__dict__.setdefault('_linux_git_identity_lock', threading.Lock())
    with lock:
        file = read_git_identity()
        if file is None:
            return
        digest = hashlib.sha256(file['data'].encode()).hexdigest()
        path = Path(runtime.root) / 'linux-git-identity.json'
        try:
            state = json.loads(path.read_text())
            if not isinstance(state, dict):
                raise ValueError()
        except FileNotFoundError:
            state = {}
        except ValueError:
            raise ValueError('Cannot read the guest Git identity transfer state') from None
        pending = state.get('pending')
        if pending:
            client.request('credentials.put', {'files': [pending['file']]},
                           request_id=pending['requestId'], timeout=15)
            state = {'digest': pending['digest']}
            _atomic_json(path, state)
            # A same-content retry already satisfies a forced initial sync.
            if state['digest'] == digest:
                return
        elif state.get('digest') == digest and not force:
            return
        pending = {'file': file, 'digest': digest, 'requestId': 'git-identity:' + uuid.uuid4().hex}
        _atomic_json(path, {**state, 'pending': pending})
        client.request('credentials.put', {'files': [file]}, request_id=pending['requestId'], timeout=15)
        _atomic_json(path, {'digest': digest})
