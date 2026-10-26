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
import stat
import subprocess
import tarfile
import time
from typing import IO, Any, Callable, Iterator, TYPE_CHECKING, cast

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


@contextmanager
def _import_attempt(directory: Path, project_id: str, started: Callable[[], None] | None) -> Iterator[None]:
    """One import attempt under the project lock. import-error.json describes the last
    finished attempt: this attempt removes it, and writes it again if the attempt fails.
    import_readiness shows that reason until the next attempt."""
    with _project_lock(directory):
        report = directory / 'import-error.json'
        report.unlink(missing_ok=True)
        if started is not None:
            started()
        try:
            yield
        except Exception as error:
            try:
                _save(report, {'projectId': project_id, 'error': (str(error) or type(error).__name__)[:500],
                               'at': time.time()})
            except OSError:
                pass  # The import error stays the visible result.
            raise


def _lock_held(path: Path) -> bool:
    """Whether another holder has the project lock now. Never creates the lock file.
    A shared probe delays a waiting _project_lock by one 0.1 s retry at most."""
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
    except FileNotFoundError:
        return False
    try:
        fcntl.flock(descriptor, fcntl.LOCK_SH | fcntl.LOCK_NB)
    except BlockingIOError:
        return True
    else:
        fcntl.flock(descriptor, fcntl.LOCK_UN)
        return False
    finally:
        os.close(descriptor)


_RECEIPT = re.compile(r'import-[0-9a-f]{64}\.json')


def import_readiness(state_dir: Path, project_id: str) -> dict[str, str]:
    """Whether a new layr chat of this project starts without an import. It reads only the
    host records of import_project: no VM call and no mount table.

    - ready: project.json exists, so the VM had the project at the last import or check.
    - importing: another thread or process holds import.lock and project.json is missing.
    - failed: import-error.json has the reason of the last attempt, or an import receipt
      has no result after its attempt ended (for example, the backend stopped).
    - not_imported: no record; the first layr chat imports the Mac folder.
    """
    directory = state_dir / 'layr-projects' / _id(project_id)
    try:
        if (directory / 'project.json').exists():
            # A held lock here is a chat start or a remount of the existing VM project.
            return {'state': 'ready'}
        if _lock_held(directory / 'import.lock'):
            return {'state': 'importing'}
        report = directory / 'import-error.json'
        if report.exists():
            return {'state': 'failed', 'error': str(json.loads(report.read_text())['error'])[:500]}
        if directory.is_dir() and any(_RECEIPT.fullmatch(path.name) and 'result' not in json.loads(path.read_text())
                                      for path in directory.iterdir()):
            return {'state': 'failed', 'error': 'The last import stopped before it finished. '
                                                'The next layr chat continues it.'}
    except (OSError, ValueError, KeyError, TypeError) as error:
        return {'state': 'failed', 'error': 'The import records are unreadable: ' + str(error)[:300]}
    return {'state': 'not_imported'}


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
# An existing authenticated session can mount without a prompt; the exit code decides.
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
"""


def _mount_smbfs(source: str, destination: Path, password: str,
                 executable: str = '/sbin/mount_smbfs') -> subprocess.CompletedProcess[str]:
    import sys
    return subprocess.run([sys.executable, '-I', '-c', _MOUNT_HELPER, executable, '-o',
                           # nobrowse: the folder in ~/Studio is the view; no "127.0.0.1" server in the sidebar.
                           'nobrowse,rdonly,soft,nodatacache,nomdatacache,sessionencrypt', source, str(destination)],
                          input=password + '\n', capture_output=True, text=True,
                          start_new_session=True, timeout=30)


def share_hint(source: str | Path | None) -> str:
    """The preferred share name: the Mac project folder name in plain ASCII. Finder shows
    the mounted share by this name; the guest makes it unique."""
    name = re.sub(r'[^A-Za-z0-9._-]+', '-', Path(source or '').name).strip('._-')[:40].strip('._-')
    return name if re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', name) and name.lower() not in {
        'global', 'homes', 'printers'} else 'project'


def _ours(row: dict[str, str], names: set[str]) -> bool:
    """A read-only mount of one of this project's share names, at any address or port."""
    return ('read-only' in row['options'].split(', ') and any(
        re.fullmatch(r'//studio-view@[^/]+/' + re.escape(name), row['source']) for name in names))


def _release(path: Path) -> None:
    from codex_linux_vm import LinuxVMError
    released = subprocess.run(['/sbin/umount', str(path)], capture_output=True, text=True, timeout=30)
    if released.returncode:
        raise LinuxVMError('The previous share mount is busy: ' + (released.stderr.strip()[:200] or 'umount failed'))


def mount_project(client: Client, project: dict[str, Any], password: str, hint: str) -> dict[str, Any]:
    from codex_linux_vm import LinuxVMError
    identity = _id(project['projectId'])
    share = project['share']
    if share.get('state') != 'ready':
        raise LinuxVMError('The guest SMB share is not ready.')
    # The helper bridges 127.0.0.1 to the guest over vsock: no VM network address, and
    # no macOS Local Network permission for the background backend (docs/vm-layr.md).
    bridge = client.status().get('share') or {}
    port = bridge.get('port')
    if bridge.get('state') != 'listening' or type(port) is not int:
        raise LinuxVMError('The VM share bridge is unavailable: ' + str(bridge.get('error') or 'not listening'))
    name = client.call('share.name', {'projectId': identity, 'name': hint}, timeout=30)['name']
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,63}', name):
        raise LinuxVMError('The guest returned an invalid share name.')
    record = client.state_dir / 'layr-projects' / identity / 'mount.json'
    previous = json.loads(record.read_text()) if record.exists() else {}
    names = {identity, name} | ({previous['name']} if previous.get('name') else set())
    destination = Path.home() / 'Studio' / name
    destination.parent.mkdir(mode=0o700, exist_ok=True)
    if destination.parent.is_symlink() or destination.is_symlink():
        raise LinuxVMError('The Studio share path must not contain symlinks.')
    entries = _mount_entries()
    # A rename moves the view: release this project's mount at its former path.
    for former in {Path(previous['path'])} if previous.get('path') else set():
        if former != destination and former.parent == destination.parent and not former.is_symlink():
            if any(row['path'] == str(former) and _ours(row, names) for row in entries):
                _release(former)
            if former.is_dir() and not any(former.iterdir()):
                former.rmdir()
    expected = f'//studio-view@127.0.0.1:{port}/{name}'
    for row in [row for row in _mount_entries() if row['path'] == str(destination)]:
        if row['source'] == expected and 'read-only' in row['options'].split(', '):
            _save(record, {'projectId': identity, 'name': name, 'path': str(destination), 'port': port})
            return {'state': 'mounted', 'path': str(destination), 'name': name, 'readOnly': True}
        # Only this share at an older address, port or name is replaced; anything else stays.
        if not _ours(row, names):
            raise LinuxVMError('A different filesystem already uses the project share path.')
        _release(destination)
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
    _save(record, {'projectId': identity, 'name': name, 'path': str(destination), 'port': port})
    return {'state': 'mounted', 'path': str(destination), 'name': name, 'readOnly': True}


def _mount_or_report(client: Client, project: dict[str, Any], password: str, hint: str) -> dict[str, Any]:
    """The Finder view is a convenience: agents work in the VM without it. A failed mount
    stays visible in mount_status and in the project result, and the chat continues.
    For example, macOS blocks a background Python without Local Network permission."""
    from codex_linux_vm import LinuxVMError
    directory = client.state_dir / 'layr-projects' / _id(project['projectId'])
    report = directory / 'mount-error.json'
    try:
        mounted = mount_project(client, project, password, hint)
    except LinuxVMError as error:
        _save(report, {'projectId': _id(project['projectId']), 'error': str(error)[:500], 'at': time.time()})
        return {'state': 'failed', 'readOnly': True, 'error': str(error)[:500]}
    report.unlink(missing_ok=True)
    return mounted


# The guest accepts at most 1 MiB of decoded data in one request (vm-guest/common.py MAX_INPUT).
CHUNK = 1024 * 1024


def seed_paths(source: str | Path) -> list[str]:
    """The folders that `.layrattributes` at the project root marks `regenerable seed`: a seeded
    layer comes from this folder once instead of a fetch or build in the VM (docs/vm-layr.md,
    "Seeded layers"). The last line for a path wins; only plain paths are seeded."""
    root = Path(source).expanduser().resolve()
    try:
        lines = (root / '.layrattributes').read_text(encoding='utf-8').splitlines()
    except (FileNotFoundError, NotADirectoryError):
        return []
    marks: dict[str, dict[str, bool]] = {}
    for line in lines:
        words = line.split()
        if not words or words[0].startswith('#'):
            continue
        path = words[0].strip('/')
        attributes = marks.setdefault(path, {})
        for word in words[1:]:
            name = word.lstrip('-')
            if name in {'regenerable', 'seed'}:
                attributes[name] = not word.startswith('-')
    folders = []
    for path, attributes in marks.items():
        parts = path.split('/')
        if (not attributes.get('regenerable') or not attributes.get('seed') or not path
                or any(part in {'', '.', '..'} for part in parts) or any(sign in path for sign in '*?[!\\')):
            continue
        folder = root / path
        if folder.is_dir() and not folder.is_symlink() and not any(
                (root / '/'.join(parts[:index])).is_symlink() for index in range(1, len(parts))):
            folders.append(path)
    return folders


def _layer_tar(folder: Path, output: IO[bytes]) -> None:
    """A tar of the folder that is the same in every pass over unchanged files: sorted names, no
    owners, whole-second times, no hard links. Files that are not regular files, folders or
    symbolic links (sockets, pipes) stay out; the guest accepts no other entry."""
    # Reads and writes of 1 MiB: tarfile copies in 16 KiB blocks, and reads of that size gave
    # about 50 MB/s from the Mac disk; the file buffer makes each disk read 1 MiB.
    with tarfile.open(fileobj=output, mode='w|', format=tarfile.PAX_FORMAT, bufsize=CHUNK) as archive:
        def add(path: Path, name: str) -> None:
            meta = path.lstat()
            info = tarfile.TarInfo(name)
            info.mode = stat.S_IMODE(meta.st_mode)
            info.mtime = int(meta.st_mtime)
            if stat.S_ISDIR(meta.st_mode):
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
            elif stat.S_ISLNK(meta.st_mode):
                info.type, info.linkname = tarfile.SYMTYPE, os.readlink(path)
                archive.addfile(info)
            elif stat.S_ISREG(meta.st_mode):
                info.size = meta.st_size
                with path.open('rb', buffering=CHUNK) as stream:
                    archive.addfile(info, stream)
        add(folder, '.')
        for directory, folders, files in os.walk(folder):
            folders.sort()
            base = Path(directory)
            for entry in sorted(folders + files):
                add(base / entry, (base / entry).relative_to(folder).as_posix())


class _Measure:
    def __init__(self) -> None:
        self.size, self.digest = 0, hashlib.sha256()

    def write(self, data: bytes) -> int:
        self.size += len(data)
        self.digest.update(data)
        return len(data)


class _Chunks:
    """Send the tar stream in chunks; skip what an earlier attempt already delivered."""

    def __init__(self, client: Client, upload_id: str, operation: str, received: int) -> None:
        self.client, self.upload_id, self.operation = client, upload_id, operation
        self.skip, self.sequence, self.buffer = received, received // CHUNK, bytearray()

    def write(self, data: bytes) -> int:
        size = len(data)
        if self.skip:
            dropped = min(self.skip, size)
            self.skip -= dropped
            data = data[dropped:]
        self.buffer.extend(data)
        while len(self.buffer) >= CHUNK:
            self.send(bytes(self.buffer[:CHUNK]))
            del self.buffer[:CHUNK]
        return size

    def send(self, data: bytes) -> None:
        self.client.call('upload.chunk', {'uploadId': self.upload_id, 'seq': self.sequence,
            'data': base64.b64encode(data).decode()},
            request_id=self.operation + ':chunk:' + str(self.sequence), timeout=60)
        self.sequence += 1

    def close(self) -> None:
        if self.buffer:
            self.send(bytes(self.buffer))
            self.buffer.clear()


def plan_seeds(client: Client, project_id: str, source: str | Path) -> None:
    """Record the seeded layers of a new import before its receipt. Only these are sent: after
    the import the VM main, not the Mac folder, is the project."""
    record = client.state_dir / 'layr-projects' / _id(project_id) / 'seeds.json'
    if not record.exists():
        _save(record, {path: None for path in seed_paths(source)})


def seed_layers(client: Client, project_id: str, source: str | Path, identity: str) -> list[dict[str, Any]]:
    """Give the VM main each planned layer once: send the Mac folder as a tar stream, then the
    guest expands it and makes it a warm layer of main (project.seed). The tar is made twice,
    first for its size and SHA-256, so the Mac keeps no copy."""
    from codex_linux_vm import LinuxVMError
    project_id = _id(project_id)
    root = Path(source).expanduser().resolve()
    record = client.state_dir / 'layr-projects' / project_id / 'seeds.json'
    if not record.exists():
        return []
    done = json.loads(record.read_text())
    results = []
    # A path waits while its value is None or a failed attempt ({"pending": true, "attempts": n}).
    for path in [path for path, value in done.items() if value is None or value.get('pending')]:
        attempt = int((done[path] or {}).get('attempts', 0))
        folder = root / path
        if not folder.is_dir() or folder.is_symlink():
            raise LinuxVMError(f'The layer folder {path} is no longer on the Mac; Studio cannot send it to the VM.',
                               code='not_found')
        measure = _Measure()
        _layer_tar(folder, cast(IO[bytes], measure))
        digest = measure.digest.hexdigest()
        # A folder that changed before its upload ended gets a new upload, not an ID conflict.
        operation = hashlib.sha256((identity + ':seed:' + path + ':' + digest).encode()).hexdigest()
        upload_id = 'seed-' + operation[:40]
        begin = {'uploadId': upload_id, 'root': '/var/lib/codex-studio/projects/layr-' + upload_id,
                 'totalBytes': measure.size, 'sha256': digest, 'mode': 'full', 'deletePaths': []}
        # The guest returns the saved answer of a request ID again, also a failure whose outcome is
        # unknown. Each step is safe to repeat (the guest keeps chunks by sequence number and content;
        # the upload state and main/<path> decide project.seed), so after a failure the next attempt
        # uses new IDs; the guest serializes it behind an earlier one that may still run.
        try:
            status = client.call('upload.begin', begin, request_id=operation + ':begin:' + str(attempt), timeout=30)
            if status.get('state') == 'receiving' and status.get('receivedBytes') != measure.size:
                chunks = _Chunks(client, upload_id, operation + ':' + str(attempt), int(status.get('receivedBytes') or 0))
                _layer_tar(folder, cast(IO[bytes], chunks))
                chunks.close()
            result = client.call('project.seed', {'projectId': project_id, 'path': path, 'uploadId': upload_id},
                                 request_id=operation + ':seed:' + str(attempt), timeout=3600)
        except LinuxVMError:
            done[path] = {'pending': True, 'attempts': attempt + 1}
            _save(record, done)
            raise
        done[path] = {'stateId': result.get('stateId'), 'seeded': result.get('seeded'), 'bytes': measure.size,
                      'at': time.time()}
        _save(record, done)
        results.append(result)
    return results


def upload_archive(client: Client, archive: Path, begin: dict[str, Any], operation: str) -> None:
    """Send a tar archive with the guest tree upload protocol; each call keeps its request ID."""
    client.call('upload.begin', begin, request_id=operation + ':begin', timeout=30)
    with archive.open('rb') as stream:
        sequence = 0
        while data := stream.read(CHUNK):
            client.call('upload.chunk', {'uploadId': begin['uploadId'], 'seq': sequence,
                'data': base64.b64encode(data).decode()}, request_id=operation + ':chunk:' + str(sequence), timeout=30)
            sequence += 1
    client.call('upload.commit', {'uploadId': begin['uploadId']}, request_id=operation + ':upload', timeout=1810)


def import_project(client: Client, project_id: str, source: str | Path, owner: str | None,
                   request_id: str | None, *, incremental: bool,
                   expected_state_id: str | None,
                   started: Callable[[], None] | None = None) -> dict[str, Any]:
    """`started` runs once this attempt holds the project lock; it must not raise."""
    from codex_linux_vm import LinuxVMError
    from codex_linux_workspace_sync import archive_source
    project_id = _id(project_id)
    if incremental and (not request_id or not expected_state_id):
        raise ValueError('Explicit re-import requires a request ID and the expected main state.')
    identity = request_id or 'layr-import:' + project_id
    # UUID-derived subrequest identities stay bounded for the guest protocol.
    operation = hashlib.sha256(identity.encode()).hexdigest()
    directory = client.state_dir / 'layr-projects' / project_id
    with _import_attempt(directory, project_id, started):
        if not incremental:
            try:
                existing = client.call('project.ensure', {'projectId': project_id}, timeout=30)
            except LinuxVMError as error:
                if error.code != 'not_found':
                    raise
            else:
                # An import that ended before its receipt left no project.json; the
                # Mac folder still names the Finder view after a VM restart.
                if not (directory / 'project.json').exists():
                    _save(directory / 'project.json',
                          {'projectId': project_id, 'source': str(Path(source).expanduser().resolve())})
                password = _credential(client)
                existing['share'] = client.call('share.status', {}, timeout=30)
                existing['mount'] = _mount_or_report(client, existing, password, share_hint(source))
                seed_layers(client, project_id, source, identity)
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
                project['mount'] = _mount_or_report(client, project, password, share_hint(source))
                seed_layers(client, project_id, source, identity)
                return cast(dict[str, Any], project)
            if not archive.exists():
                raise LinuxVMError('The pending import archive is unavailable; inspect the guest receipt.', uncertain=True)
        else:
            metadata = archive_source(source_path, archive)
            pending = {**metadata, 'association': association, 'uploadId': operation,
                       'root': '/var/lib/codex-studio/projects/layr-import-' + operation}
            _save(state_path, pending)
        begin = {key: pending[key] for key in ('uploadId', 'root', 'totalBytes', 'sha256', 'mode', 'deletePaths')}
        upload_archive(client, archive, begin, operation)
        params = {'projectId': project_id, 'source': pending['root'], 'owner': owner,
                  'incremental': incremental, 'expectedStateId': expected_state_id}
        project = client.call('project.import', params, request_id=operation + ':import', timeout=1810)
        password = _credential(client)
        project['share'] = client.call('share.status', {}, timeout=30)
        project['mount'] = _mount_or_report(client, project, password, share_hint(source))
        # The state right after the import is the base of the first Mac sync round.
        _save(directory / 'project.json', {'projectId': project_id, 'source': source_path,
                                           'importStateId': project.get('stateId')})
        # Keep the small receipt association. Never remove an uncertain archive.
        plan_seeds(client, project_id, source_path)
        pending['result'] = project
        _save(state_path, pending)
        archive.unlink(missing_ok=True)
        # After the receipt: a failed seed repeats alone on the next ensure. The seed does not
        # move the head of main (its folder is a cache), so the import state stays the sync base.
        seed_layers(client, project_id, source, identity)
        return cast(dict[str, Any], project)


# How long a remount waits for the layr admin channel. It restarts once after a boot that built
# new layr tools, so the first remount after an upgrade can find it unavailable.
ADMIN_WAIT = 120.0


def _ensure_project(client: Client, identity: str) -> dict[str, Any]:
    """project.ensure, again while the guest says that the layr admin channel is unavailable:
    such a request has not reached the broker, so a repeat cannot run it twice."""
    from codex_linux_vm import LinuxVMError
    deadline = time.monotonic() + ADMIN_WAIT
    while True:
        try:
            return cast(dict[str, Any], client.call('project.ensure', {'projectId': identity}, timeout=30))
        except LinuxVMError as error:
            if error.code != 'unavailable' or time.monotonic() > deadline:
                raise
        time.sleep(2)


def remount_registered(client: Client) -> None:
    records = list((client.state_dir / 'layr-projects').glob('*/mount.json'))
    if not records:
        return
    password = _credential(client)
    for record in records:
        saved = json.loads(record.read_text())
        identity = _id(saved['projectId'])
        project = _ensure_project(client, identity)
        project['share'] = client.call('share.status', {}, timeout=30)
        source = record.with_name('project.json')
        hint = saved.get('name') or share_hint(json.loads(source.read_text())['source'] if source.exists() else None)
        _mount_or_report(client, project, password, hint)


def mount_status(state_dir: Path) -> dict[str, Any]:
    records = list((state_dir / 'layr-projects').glob('*/mount.json'))
    failures = [json.loads(path.read_text()) for path in sorted((state_dir / 'layr-projects').glob('*/mount-error.json'))]
    failed = [{'projectId': row['projectId'], 'state': 'failed', 'error': row['error']} for row in failures]
    if not records:
        return {'state': 'failed' if failed else 'unmounted', 'projects': failed}
    entries = _mount_entries()
    reasons = {row['projectId']: row['error'] for row in failed}
    result = []
    for record in records:
        saved = json.loads(record.read_text())
        entry = next((row for row in entries if row['path'] == saved['path']), None)
        row = {'projectId': saved['projectId'], 'path': saved['path'],
               'state': 'mounted' if entry else 'unmounted',
               'readOnly': bool(entry and 'read-only' in entry['options'].split(', '))}
        # A remount that failed after an earlier success keeps its reason.
        if not entry and saved['projectId'] in reasons:
            row['error'] = reasons[saved['projectId']]
        result.append(row)
    failed = [row for row in failed if row['projectId'] not in {item['projectId'] for item in result}]
    state = 'failed' if failed else 'mounted' if all(row['state'] == 'mounted' for row in result) else 'unmounted'
    return {'state': state, 'projects': result + failed}
