"""Mac folder sync for layr projects, the Mac side (docs/vm-mac-sync.md).

The manifest records, for each synced path, the content the Mac and the VM agreed on
and the layr state that content came from. A round sends the Mac's changes with those
bases, lets the guest fold them into the `mac` line and merge it into main, and then
writes main's changes into the Mac folder where the Mac file still has its agreed
content. Nothing is written over a Mac edit, and no conflict markers reach the Mac.
"""
from __future__ import annotations

import base64
from contextlib import contextmanager
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tarfile
import tempfile
import time
from typing import Any, Iterator
import uuid

MAX_FILE = 256 * 1024 * 1024
ALWAYS_SKIPPED = {".git", ".worktrees", "node_modules", ".venv", "__pycache__", ".pytest_cache",
                  ".mypy_cache", ".DS_Store"}
DELETE_GUARD = (0.25, 500)


def _save(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    temporary = path.with_name('.' + path.name + '.' + uuid.uuid4().hex)
    temporary.write_text(json.dumps(value, sort_keys=True))
    temporary.chmod(0o600)
    os.replace(temporary, path)


def _load(path: Path, default: Any) -> Any:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return default


class Rules:
    """Which Mac paths sync: not Git metadata, caches, dependency folders, or folders Git
    ignores. Files Git ignores, such as .env, do sync."""

    def __init__(self, root: Path):
        self.root = root
        self.ignored: set[str] = set()
        if (root / '.git').exists():
            listing = subprocess.run(['git', '-C', str(root), 'ls-files', '--others', '--ignored',
                                      '--exclude-standard', '--directory', '-z'],
                                     capture_output=True, timeout=120)
            if listing.returncode == 0:
                self.ignored = {item.decode(errors='surrogateescape').rstrip('/')
                                for item in listing.stdout.split(b'\0') if item.endswith(b'/')}

    def skipped(self, rel: str) -> bool:
        parts = rel.split('/')
        if any(part in ALWAYS_SKIPPED for part in parts):
            return True
        return any('/'.join(parts[:index]) in self.ignored for index in range(1, len(parts) + 1))


def _describe(path: Path, info: os.stat_result, *, digest: bool) -> dict[str, Any]:
    if stat.S_ISLNK(info.st_mode):
        data = os.readlink(path).encode()
        return {'k': 'link', 's': len(data), 'm': info.st_mtime_ns, 'x': 0,
                'h': hashlib.sha256(data).hexdigest()}
    entry = {'k': 'file', 's': info.st_size, 'm': info.st_mtime_ns, 'x': stat.S_IMODE(info.st_mode) & 0o777}
    if digest:
        entry['h'] = _file_hash(path)
    return entry


def _file_hash(path: Path) -> str:
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def scan(root: Path, rules: Rules, manifest: dict[str, dict[str, Any]]) -> tuple[dict[str, dict[str, Any]], list[str]]:
    """The Mac's synced paths. A file whose size and time match the manifest keeps its
    recorded hash; others are hashed. Returns (entries, notices)."""
    entries: dict[str, dict[str, Any]] = {}
    notices = []
    for directory, folders, files in os.walk(root, followlinks=False):
        base = Path(directory)
        relative_dir = base.relative_to(root).as_posix()
        prefix = '' if relative_dir == '.' else relative_dir + '/'
        folders[:] = [name for name in folders if not rules.skipped(prefix + name)
                      and not (base / name).is_symlink()]
        links = [name for name in os.listdir(base) if (base / name).is_symlink() and name not in files]
        for name in files + links:
            rel = prefix + name
            if rules.skipped(rel):
                continue
            path = base / name
            try:
                info = path.lstat()
            except FileNotFoundError:
                continue
            if not (stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)):
                notices.append(rel + ': a special file does not sync')
                continue
            if stat.S_ISREG(info.st_mode) and info.st_size > MAX_FILE:
                notices.append(rel + ': files over 256 MiB do not sync')
                continue
            known = manifest.get(rel)
            same = known is not None and known.get('k') == ('link' if stat.S_ISLNK(info.st_mode) else 'file') \
                and known.get('s') == info.st_size and known.get('m') == info.st_mtime_ns
            entry = _describe(path, info, digest=not same)
            if same and known is not None:
                entry['h'] = known['h']
            entries[rel] = entry
    return entries, notices


def _inside(root: Path, rel: str) -> Path:
    path = root / rel
    if path.is_absolute() is False or '..' in rel.split('/') or not path.resolve(strict=False).is_relative_to(root.resolve()):
        raise ValueError('The sync path leaves the project folder: ' + rel)
    for parent in path.parents:
        if parent == root:
            break
        if parent.is_symlink():
            raise ValueError('The sync path crosses a symbolic link: ' + rel)
    return path


def write_mac(root: Path, rel: str, kind: str, data: bytes, mode: int) -> dict[str, Any] | None:
    """Replace one Mac path atomically; returns its new manifest entry (None after a delete)."""
    path = _inside(root, rel)
    if kind == 'absent':
        if path.is_symlink() or path.is_file():
            path.unlink()
        return None
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name('.' + path.name + '.studio-sync-' + uuid.uuid4().hex[:8])
    if kind == 'link':
        os.symlink(data.decode(), temporary)
    else:
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode or 0o644)
    os.replace(temporary, path)
    entry = _describe(path, path.lstat(), digest=True)
    return entry


class ProjectSync:
    def __init__(self, client: Any, project_id: str, folder: Path, *, import_state: str | None = None):
        self.client, self.project_id = client, project_id
        self.root = Path(folder).expanduser().resolve()
        self.directory = client.state_dir / 'layr-projects' / project_id / 'sync'
        self.import_state = import_state

    @contextmanager
    def lock(self) -> Iterator[None]:
        self.directory.mkdir(parents=True, mode=0o700, exist_ok=True)
        with (self.directory / 'round.lock').open('a') as lease:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            yield

    def status(self, **values: Any) -> dict[str, Any]:
        current: dict[str, Any] = dict(_load(self.directory / 'status.json', {}))
        current.update(values, at=time.time())
        _save(self.directory / 'status.json', current)
        return current

    def bootstrap(self, rules: Rules) -> dict[str, dict[str, Any]]:
        """The first manifest. With the import state, its content is what the Mac and the
        VM agreed on: later Mac edits then go in, and agent work comes out. Without it,
        only files equal to main are agreed; every other difference waits for a choice."""
        main_state = self.client.call('project.ensure', {'projectId': self.project_id}, timeout=30)['stateId']
        base = self.import_state or main_state
        known: dict[str, dict[str, Any]] = {}
        after = ''
        while True:
            page = self.client.call('sync.mac.hashes', {'projectId': self.project_id, 'stateId': base,
                                                         'after': after}, timeout=600)
            known.update(page['items'])
            if not page.get('next'):
                break
            after = page['next']
        recorded = {rel: {'k': item['kind'], 's': item.get('size', 0), 'm': 0, 'x': item.get('mode', 0),
                          'h': item['sha256'], 'b': base}
                    for rel, item in known.items() if item.get('kind') in {'file', 'link'} and not rules.skipped(rel)}
        if self.import_state is not None:
            return recorded
        current, _ = scan(self.root, rules, {})
        manifest = {rel: entry for rel, entry in recorded.items()
                    if rel in current and current[rel]['k'] == entry['k'] and current[rel]['h'] == entry['h']}
        waiting = sorted((set(current) | set(recorded)) - set(manifest))
        if waiting:
            self.status(choices=waiting)
        return manifest

    def round(self) -> dict[str, Any]:
        with self.lock():
            return self._round()

    def _round(self) -> dict[str, Any]:
        rules = Rules(self.root)
        manifest_path = self.directory / 'manifest.json'
        manifest = _load(manifest_path, None)
        if manifest is None:
            manifest = self.bootstrap(rules)
            _save(manifest_path, manifest)
        waiting = set(_load(self.directory / 'status.json', {}).get('choices', []))
        # Sent content that is not agreed yet: a conflict, or Mac work waiting in the mac
        # line for a merge. It is not sent again until the Mac file changes.
        held = _load(self.directory / 'held.json', {})
        current, notices = scan(self.root, rules, manifest)
        changes: list[tuple[str, dict[str, Any] | None, str | None]] = []
        for rel, entry in current.items():
            known = manifest.get(rel)
            if rel in waiting or held.get(rel, {}).get('h') == entry['h']:
                continue
            if known is None or known['k'] != entry['k'] or known['h'] != entry['h']:
                changes.append((rel, entry, known.get('b') if known else None))
        deleted = [(rel, None, known.get('b')) for rel, known in manifest.items()
                   if rel not in current and not rules.skipped(rel) and rel not in waiting
                   and held.get(rel, {}).get('h') != 'absent']
        if len(deleted) > 3 and (len(deleted) > DELETE_GUARD[1] or len(deleted) > len(manifest) * DELETE_GUARD[0]):
            return self.status(state='paused', notices=notices,
                               reason=f'{len(deleted)} files were deleted on the Mac; confirm before they sync')
        changes += deleted
        pending = _load(self.directory / 'pending.json', None)
        if pending is not None:
            # A lost reply repeats the same operation; the guest answers from its receipt.
            operation, params, sent = pending['operation'], pending['params'], pending['hashes']
        else:
            operation = 'mac-' + uuid.uuid4().hex
            params, sent = self.upload(operation, changes, manifest)
            _save(self.directory / 'pending.json', {'operation': operation, 'params': params, 'hashes': sent})
        result = self.client.call('sync.mac.apply', params, request_id=operation, timeout=1800)
        merged = result['merge'] in {'merged', 'none'}
        conflicts = set(result['conflicts']) | set(result['mergeConflicts'])
        agreed, written, skipped = self.outbound(manifest, result, sent, conflicts)
        for rel in result['applied']:
            if rel in agreed or rel in written or rel in skipped:
                continue
            if merged:
                # main took the Mac content unchanged for this path.
                found = current.get(rel)
                if found is None:
                    manifest.pop(rel, None)
                else:
                    manifest[rel] = {**found, 'b': result['mainStateId']}
            else:
                held[rel] = {'h': sent.get(rel, 'absent'), 'why': 'waiting for a merge into main'}
        for rel in conflicts:
            if rel in sent:
                held[rel] = {'h': sent[rel], 'why': 'conflict'}
        if merged:
            for rel, known in manifest.items():
                if rel not in skipped and rel not in conflicts:
                    known['b'] = result['mainStateId']
        held = {rel: value for rel, value in held.items()
                if manifest.get(rel, {}).get('h') != value['h']
                and (current.get(rel, {}).get('h', 'absent') == value['h'] or rel in conflicts)}
        _save(manifest_path, manifest)
        _save(self.directory / 'held.json', held)
        (self.directory / 'pending.json').unlink(missing_ok=True)
        in_conflict = sorted(rel for rel, value in held.items() if value['why'] == 'conflict')
        state = ('conflict' if in_conflict or result['mergeConflicts']
                 else 'waiting' if result['merge'] == 'refused' or held else 'synced')
        return self.status(state=state, mainStateId=result['mainStateId'], sent=len(params['entries']),
                           received=len(written), conflicts=in_conflict, mergeConflicts=result['mergeConflicts'],
                           mergeError=result.get('mergeError'), notices=notices, reason=None)

    def upload(self, operation: str, changes: list[tuple[str, dict[str, Any] | None, str | None]],
               manifest: dict[str, dict[str, Any]]) -> tuple[dict[str, Any], dict[str, str]]:
        since = sorted({known['b'] for known in manifest.values() if known.get('b')})[:16]
        params: dict[str, Any] = {'projectId': self.project_id, 'operationId': operation, 'since': since, 'entries': []}
        hashes: dict[str, str] = {}
        if not changes:
            return params, hashes
        root = '/var/lib/codex-studio/projects/mac-sync-' + operation[4:28]
        with tempfile.TemporaryDirectory(prefix='studio-mac-sync-') as scratch:
            archive = Path(scratch) / 'changes.tar.gz'
            with tarfile.open(archive, 'w:gz') as output:
                directory = tarfile.TarInfo('.')
                directory.type, directory.mode = tarfile.DIRTYPE, 0o755
                output.addfile(directory)
                for rel, entry, base in changes:
                    item = {'path': rel, 'kind': 'absent' if entry is None else entry['k'], 'base': base}
                    params['entries'].append(item)
                    if entry is None:
                        continue
                    path = _inside(self.root, rel)
                    info = tarfile.TarInfo(rel)
                    if entry['k'] == 'link':
                        target = os.readlink(path)
                        info.type, info.linkname = tarfile.SYMTYPE, target
                        output.addfile(info)
                        hashes[rel] = hashlib.sha256(target.encode()).hexdigest()
                        continue
                    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
                    with os.fdopen(descriptor, 'rb') as stream:
                        data = stream.read(MAX_FILE + 1)
                    info.size, info.mode = len(data), entry['x'] or 0o644
                    output.addfile(info, io.BytesIO(data))
                    # The hash of what was sent: a later Mac edit is never mistaken for it.
                    hashes[rel] = hashlib.sha256(data).hexdigest()
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            begin = {'uploadId': operation[4:36], 'root': root, 'totalBytes': archive.stat().st_size,
                     'sha256': digest, 'mode': 'full', 'deletePaths': []}
            self.send_archive(archive, begin, operation)
        params['upload'] = root
        return params, hashes

    def send_archive(self, archive: Path, begin: dict[str, Any], operation: str) -> None:
        from codex_linux_vm_share import upload_archive
        upload_archive(self.client, archive, begin, operation)

    def outbound(self, manifest: dict[str, dict[str, Any]], result: dict[str, Any], sent: dict[str, str],
                 conflicts: set[str]) -> tuple[set[str], set[str], set[str]]:
        """Bring main's changes to the Mac where the Mac file still has its agreed content,
        or the content this round sent, or nothing at all. Returns (agreed without a
        write, written, skipped because the Mac changed again)."""
        rules = Rules(self.root)
        agreed, written, skipped = set(), set(), set()
        state = result['mainStateId']
        for base, changes in result['outbound'].items():
            for change in changes:
                rel = change['path']
                known = manifest.get(rel)
                if rules.skipped(rel) or rel in conflicts or (known is not None and known.get('b') != base):
                    continue
                path = _inside(self.root, rel)
                try:
                    now = _describe(path, path.lstat(), digest=True)
                except FileNotFoundError:
                    now = None
                now_hash = now['h'] if now else None
                target = change.get('sha256') if change['kind'] != 'absent' else None
                if now_hash == target:
                    # The Mac already has main's content: agree without touching the file.
                    if now is None:
                        manifest.pop(rel, None)
                    else:
                        manifest[rel] = {**now, 'b': state}
                    agreed.add(rel)
                    continue
                allowed = {sent.get(rel)} | ({known['h']} if known else {None})
                if now_hash not in allowed:
                    skipped.add(rel)
                    continue
                data = b''
                if change['kind'] in {'file', 'link'}:
                    data = self.fetch(state, rel)
                    if hashlib.sha256(data).hexdigest() != target:
                        skipped.add(rel)
                        continue
                entry = write_mac(self.root, rel, change['kind'], data, change.get('mode', 0o644))
                if entry is None:
                    manifest.pop(rel, None)
                else:
                    manifest[rel] = {**entry, 'b': state}
                written.add(rel)
        return agreed, written, skipped

    def fetch(self, state: str, rel: str) -> bytes:
        data, offset = b'', 0
        while True:
            chunk = self.client.call('sync.mac.read', {'projectId': self.project_id, 'stateId': state,
                                                       'path': rel, 'offset': offset}, timeout=120)
            if chunk['kind'] == 'absent':
                return data
            piece = base64.b64decode(chunk['data'])
            data += piece
            offset += len(piece)
            if chunk['eof']:
                return data


INTERVAL = 10.0


def synced_projects(state_dir: Path) -> list[dict[str, Any]]:
    """VM projects with a Mac folder whose sync is not paused."""
    projects = []
    for record in sorted((state_dir / 'layr-projects').glob('*/project.json')):
        try:
            data = json.loads(record.read_text())
        except (OSError, ValueError):
            continue
        config = _load(record.with_name('sync') / 'config.json', {})
        if config.get('enabled', True) and data.get('source') and Path(data['source']).is_dir():
            projects.append(data)
    return projects


def tick(runtime: Any) -> None:
    """Start sync rounds at most every INTERVAL seconds, in the runtime's worker pool,
    never under the runtime lock and never twice at once. A stopped VM makes them wait."""
    import platform
    if platform.system() != 'Darwin' or runtime.__dict__.get('_mac_sync_busy'):
        return
    now = time.monotonic()
    if now - runtime.__dict__.get('_mac_sync_at', 0.0) < INTERVAL:
        return
    runtime._mac_sync_at = now
    from codex_linux_workspaces import client as vm_client
    client = vm_client(runtime)
    projects = synced_projects(client.state_dir)
    if not projects:
        return
    runtime._mac_sync_busy = True

    def run() -> None:
        try:
            if client.status().get('state') != 'running':
                return
            for data in projects:
                project = ProjectSync(client, data['projectId'], Path(data['source']),
                                      import_state=data.get('importStateId'))
                try:
                    project.round()
                except BlockingIOError:
                    continue
                except Exception as error:  # noqa: BLE001 - every failure stays visible in the status
                    project.status(state='error', reason=str(error)[:500])
        finally:
            runtime._mac_sync_busy = False
    runtime.pool.submit(run)
