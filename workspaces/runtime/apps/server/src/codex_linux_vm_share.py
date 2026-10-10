"""Import a project once and mount its authenticated, read-only guest share."""
from __future__ import annotations

import base64
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import time
from typing import Any, Iterator, TYPE_CHECKING, cast

if TYPE_CHECKING:
    from codex_linux_vm import Client


def _id(value: str) -> str:
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,99}', value):
        raise ValueError('projectId must start with an ASCII letter or digit and contain letters, digits, hyphens, or underscores.')
    return value


def _save(path: Path, value: dict[str, Any]) -> None:
    from codex_linux_vm import _atomic_json
    _atomic_json(path, value)
    path.chmod(0o600)


@contextmanager
def _project_lock(directory: Path) -> Iterator[None]:
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (directory / 'import.lock').open('a') as lease:
        deadline = time.monotonic() + 1800
        while True:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeError('The project import lock exceeded its deadline.')
                time.sleep(0.1)
        try:
            yield
        finally:
            fcntl.flock(lease, fcntl.LOCK_UN)


def _credential(client: Client) -> str:
    path = client.state_dir / 'smb-credential.json'
    # The client lock also serializes initial credential creation across projects.
    with client._lock():
        if path.exists():
            if path.is_symlink() or path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o077:
                raise RuntimeError('The SMB credential file must be private and owned by this user.')
            password = json.loads(path.read_text())['password']
        else:
            password = secrets.token_hex(32)
            _save(path, {'password': password})
    if not isinstance(password, str) or not re.fullmatch(r'[a-f0-9]{64}', password):
        raise RuntimeError('The saved SMB credential is invalid.')
    client.call('share.configure', {'password': password},
                request_id='share-configure:' + hashlib.sha256(password.encode()).hexdigest(), timeout=60)
    return password


def _mount_entries() -> list[dict[str, str]]:
    output = subprocess.run(['/sbin/mount'], check=True, capture_output=True, text=True, timeout=10).stdout
    result = []
    for line in output.splitlines():
        match = re.fullmatch(r'(.+) on (.+) \((.+)\)', line)
        if match and 'smbfs' in match[3].split(', '):
            result.append({'source': match[1], 'path': match[2], 'options': match[3]})
    return result


# mount_smbfs asks for the password only on its controlling terminal; with a pipe or a
# terminal that is not controlling, it skips the prompt and the server rejects the login.
# A fresh single-threaded helper makes the terminal with pty.fork (the backend has threads)
# and types the password from its stdin. The password never appears in an argument.
_MOUNT_HELPER = r"""
import os, pty, select, sys, time
password = sys.stdin.readline().rstrip("\n")
pid, fd = pty.fork()
if pid == 0:
    try:
        os.execv(sys.argv[1], sys.argv[1:])
    finally:
        os._exit(127)
sent, seen, mark, deadline = False, b"", 0, time.monotonic() + 25
while time.monotonic() < deadline:
    if not select.select([fd], [], [], 0.5)[0]:
        continue
    try:
        chunk = os.read(fd, 1024)
    except OSError:
        break
    if not chunk:
        break
    seen += chunk
    if not sent and b"assword" in seen:
        os.write(fd, password.encode() + b"\n")
        sent, mark = True, len(seen)
else:
    os.kill(pid, 9)
os.close(fd)
# Only what follows the prompt explains a failure.
sys.stdout.write(seen[mark:].decode(errors="replace").replace(password, "***"))
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]) or (0 if sent else 2))
"""


def _mount_smbfs(source: str, destination: Path, password: str,
                 executable: str = '/sbin/mount_smbfs') -> subprocess.CompletedProcess[str]:
    import sys
    return subprocess.run([sys.executable, '-I', '-c', _MOUNT_HELPER, executable, '-o',
                           'rdonly,soft,nodatacache,nomdatacache,sessionencrypt', source, str(destination)],
                          input=password + '\n', capture_output=True, text=True,
                          start_new_session=True, timeout=30)


def mount_project(client: Client, project: dict[str, Any], password: str) -> dict[str, Any]:
    from codex_linux_vm import LinuxVMError
    identity = _id(project['projectId'])
    share = project['share']
    if share.get('state') != 'ready':
        raise LinuxVMError('The guest SMB share is not ready.')
    address = share['address']
    import ipaddress
    if not ipaddress.ip_address(address).is_private:
        raise LinuxVMError('The share address must identify the internal VM network.')
    destination = Path.home() / 'Studio' / identity
    destination.parent.mkdir(mode=0o700, exist_ok=True)
    if destination.parent.is_symlink() or destination.is_symlink():
        raise LinuxVMError('The Studio share path must not contain symlinks.')
    entries = [row for row in _mount_entries() if row['path'] == str(destination)]
    expected = f'//studio-view@{address}/{identity}'
    if entries:
        row = entries[0]
        if row['source'] != expected or 'read-only' not in row['options'].split(', '):
            raise LinuxVMError('A different filesystem already uses the project share path.')
        _save(client.state_dir / 'layr-projects' / identity / 'mount.json',
              {'projectId': identity, 'path': str(destination), 'address': address})
        return {'state': 'mounted', 'path': str(destination), 'readOnly': True}
    destination.mkdir(mode=0o700, exist_ok=True)
    if any(destination.iterdir()):
        raise LinuxVMError('The project share mount folder is not empty.')
    result = _mount_smbfs(expected, destination, password)
    if result.returncode:
        lines = (result.stdout + result.stderr).replace(password, '***').strip().splitlines()
        detail = lines[-1].strip()[:300] if lines else 'exit code ' + str(result.returncode)
        raise LinuxVMError('The authenticated SMB mount failed: ' + detail)
    rows = [row for row in _mount_entries() if row['path'] == str(destination)]
    if len(rows) != 1 or 'read-only' not in rows[0]['options'].split(', '):
        raise LinuxVMError('The SMB mount has no proven read-only result.', uncertain=True)
    _save(client.state_dir / 'layr-projects' / identity / 'mount.json',
          {'projectId': identity, 'path': str(destination), 'address': address})
    return {'state': 'mounted', 'path': str(destination), 'readOnly': True}


def _mount_or_report(client: Client, project: dict[str, Any], password: str) -> dict[str, Any]:
    """The Finder view is a convenience: agents work in the VM without it. A failed mount
    stays visible in mount_status and in the project result, and the chat continues.
    For example, macOS blocks a background Python without Local Network permission."""
    from codex_linux_vm import LinuxVMError
    directory = client.state_dir / 'layr-projects' / _id(project['projectId'])
    report = directory / 'mount-error.json'
    try:
        mounted = mount_project(client, project, password)
    except LinuxVMError as error:
        _save(report, {'projectId': _id(project['projectId']), 'error': str(error)[:500], 'at': time.time()})
        return {'state': 'failed', 'readOnly': True, 'error': str(error)[:500]}
    report.unlink(missing_ok=True)
    return mounted


def import_project(client: Client, project_id: str, source: str | Path, owner: str | None,
                   request_id: str | None, *, incremental: bool,
                   expected_state_id: str | None) -> dict[str, Any]:
    from codex_linux_vm import LinuxVMError
    from codex_linux_workspace_sync import archive_source
    project_id = _id(project_id)
    if incremental and (not request_id or not expected_state_id):
        raise ValueError('Explicit re-import requires a request ID and the expected main state.')
    identity = request_id or 'layr-import:' + project_id
    # UUID-derived subrequest identities stay bounded for the guest protocol.
    operation = hashlib.sha256(identity.encode()).hexdigest()
    directory = client.state_dir / 'layr-projects' / project_id
    with _project_lock(directory):
        if not incremental:
            try:
                existing = client.call('project.ensure', {'projectId': project_id}, timeout=30)
            except LinuxVMError as error:
                if error.code != 'not_found':
                    raise
            else:
                password = _credential(client)
                existing['share'] = client.call('share.status', {}, timeout=30)
                existing['mount'] = _mount_or_report(client, existing, password)
                return cast(dict[str, Any], existing)
        state_path = directory / ('import-' + operation + '.json')
        archive = directory / ('source-' + operation + '.tar.gz')
        source_path = str(Path(source).expanduser().resolve())
        association = {'source': source_path, 'projectId': project_id, 'owner': owner,
                       'incremental': incremental, 'expectedStateId': expected_state_id}
        if state_path.exists():
            pending = json.loads(state_path.read_text())
            if pending['association'] != association:
                raise LinuxVMError('The import request ID has different content.', code='id_conflict')
            if 'result' in pending:
                project = pending['result']
                password = _credential(client)
                project['share'] = client.call('share.status', {}, timeout=30)
                project['mount'] = _mount_or_report(client, project, password)
                return cast(dict[str, Any], project)
            if not archive.exists():
                raise LinuxVMError('The pending import archive is unavailable; inspect the guest receipt.', uncertain=True)
        else:
            metadata = archive_source(source_path, archive)
            pending = {**metadata, 'association': association, 'uploadId': operation,
                       'root': '/var/lib/codex-studio/projects/layr-import-' + operation}
            _save(state_path, pending)
        begin = {key: pending[key] for key in ('uploadId', 'root', 'totalBytes', 'sha256', 'mode', 'deletePaths')}
        client.call('upload.begin', begin, request_id=operation + ':begin', timeout=30)
        with archive.open('rb') as stream:
            sequence = 0
            while data := stream.read(512 * 1024):
                client.call('upload.chunk', {'uploadId': operation, 'seq': sequence,
                    'data': base64.b64encode(data).decode()}, request_id=operation + ':chunk:' + str(sequence), timeout=30)
                sequence += 1
        client.call('upload.commit', {'uploadId': operation}, request_id=operation + ':upload', timeout=1810)
        params = {'projectId': project_id, 'source': pending['root'], 'owner': owner,
                  'incremental': incremental, 'expectedStateId': expected_state_id}
        project = client.call('project.import', params, request_id=operation + ':import', timeout=1810)
        password = _credential(client)
        project['share'] = client.call('share.status', {}, timeout=30)
        project['mount'] = _mount_or_report(client, project, password)
        _save(directory / 'project.json', {'projectId': project_id, 'source': source_path})
        # Keep the small receipt association. Never remove an uncertain archive.
        pending['result'] = project
        _save(state_path, pending)
        archive.unlink(missing_ok=True)
        return cast(dict[str, Any], project)


def remount_registered(client: Client) -> None:
    records = list((client.state_dir / 'layr-projects').glob('*/mount.json'))
    if not records:
        return
    password = _credential(client)
    for record in records:
        identity = _id(json.loads(record.read_text())['projectId'])
        project = client.call('project.ensure', {'projectId': identity}, timeout=30)
        project['share'] = client.call('share.status', {}, timeout=30)
        _mount_or_report(client, project, password)


def mount_status(state_dir: Path) -> dict[str, Any]:
    records = list((state_dir / 'layr-projects').glob('*/mount.json'))
    failures = [json.loads(path.read_text()) for path in sorted((state_dir / 'layr-projects').glob('*/mount-error.json'))]
    failed = [{'projectId': row['projectId'], 'state': 'failed', 'error': row['error']} for row in failures]
    if not records:
        return {'state': 'failed' if failed else 'unmounted', 'projects': failed}
    entries = _mount_entries()
    result = []
    for record in records:
        saved = json.loads(record.read_text())
        entry = next((row for row in entries if row['path'] == saved['path']), None)
        result.append({'projectId': saved['projectId'], 'path': saved['path'],
                       'state': 'mounted' if entry else 'unmounted',
                       'readOnly': bool(entry and 'read-only' in entry['options'].split(', '))})
    failed = [row for row in failed if row['projectId'] not in {item['projectId'] for item in result}]
    state = 'failed' if failed else 'mounted' if all(row['state'] == 'mounted' for row in result) else 'unmounted'
    return {'state': state, 'projects': result + failed}
