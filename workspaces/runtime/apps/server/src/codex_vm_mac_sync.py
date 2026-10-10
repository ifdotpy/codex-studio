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
import threading
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


def scan(root: Path, rules: Rules, manifest: dict[str, dict[str, Any]],
         start: Path | None = None) -> tuple[dict[str, dict[str, Any]], list[str]]:
    """The Mac's synced paths (below `start` when given). A file whose size and time
    match the manifest keeps its recorded hash; others are hashed. Returns (entries, notices)."""
    entries: dict[str, dict[str, Any]] = {}
    notices = []
    for directory, folders, files in os.walk(start or root, followlinks=False):
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


def scan_paths(root: Path, rules: Rules, manifest: dict[str, dict[str, Any]],
               paths: set[str]) -> tuple[dict[str, dict[str, Any]], list[str]]:
    """The Mac's state with only `paths` (and the folders among them) read again: every
    other path keeps its manifest entry. File events name what changed."""
    covered = sorted({rel for rel in paths if rel and not rel.startswith('../')})
    covered = [rel for index, rel in enumerate(covered)
               if not any(rel.startswith(other + '/') for other in covered[:index])]
    def under(rel: str) -> bool:
        return any(rel == item or rel.startswith(item + '/') for item in covered)
    entries = {rel: {key: value for key, value in known.items() if key != 'b'}
               for rel, known in manifest.items() if not under(rel)}
    notices: list[str] = []
    for rel in covered:
        if rules.skipped(rel):
            continue
        path = root / rel
        try:
            info = path.lstat()
        except FileNotFoundError:
            continue
        if any(parent.is_symlink() for parent in path.parents if parent != root and root in parent.parents):
            continue
        if stat.S_ISDIR(info.st_mode):
            found, more = scan(root, rules, manifest, start=path)
            entries.update(found)
            notices += more
        elif stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
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
        else:
            notices.append(rel + ': a special file does not sync')
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
    def lock(self, wait: float = 0.0) -> Iterator[None]:
        """One round or choice at a time; BlockingIOError when another holds it after `wait` seconds."""
        self.directory.mkdir(parents=True, mode=0o700, exist_ok=True)
        deadline = time.monotonic() + wait
        with (self.directory / 'round.lock').open('a') as lease:
            while True:
                try:
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.2)
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

    def round(self, paths: set[str] | None = None) -> dict[str, Any]:
        """One round; `paths` limits the Mac scan to what file events named, and an empty
        set reads nothing on the Mac (main changed). None scans the whole folder."""
        with self.lock():
            return self._round(paths)

    def _round(self, paths: set[str] | None = None) -> dict[str, Any]:
        rules = Rules(self.root)
        manifest_path = self.directory / 'manifest.json'
        manifest = _load(manifest_path, None)
        if manifest is None:
            manifest = self.bootstrap(rules)
            _save(manifest_path, manifest)
        previous = _load(self.directory / 'status.json', {})
        waiting = set(previous.get('choices', []))
        if paths is not None:
            # Paths a refused merge or a choice left unsent are read again.
            paths = set(paths) | set(previous.get('rescan', []))
        # Mac content in a conflict, or handed to the lead: it is not sent again until the
        # Mac file changes or the conflict is resolved.
        held = _load(self.directory / 'held.json', {})
        if paths is None or not (self.directory / 'manifest.json').exists():
            current, notices = scan(self.root, rules, manifest)
        else:
            current, notices = scan_paths(self.root, rules, manifest, paths)
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
        allowed = _load(self.directory / 'allow-deletes.json', {}).get('count', 0)
        if len(deleted) > 3 and len(deleted) > allowed \
                and (len(deleted) > DELETE_GUARD[1] or len(deleted) > len(manifest) * DELETE_GUARD[0]):
            return self.status(state='paused', notices=notices, deletes=len(deleted),
                               reason=f'{len(deleted)} files were deleted on the Mac; confirm before they sync')
        changes += deleted
        pending = _load(self.directory / 'pending.json', None)
        if pending is not None:
            # A lost reply repeats the same operation; the guest answers from its receipt.
            operation, params, sent = pending['operation'], pending['params'], pending['hashes']
        else:
            operation = 'mac-' + uuid.uuid4().hex
            params, sent = self.upload(operation, changes, manifest)
            # A path handed to the lead comes back when main changes it after the handoff.
            params['since'] = sorted(set(params['since']) | {value['base'] for value in held.values()
                                                              if value.get('why') == 'agent'})[:16]
            _save(self.directory / 'pending.json', {'operation': operation, 'params': params, 'hashes': sent})
        result = self.client.call('sync.mac.apply', params, request_id=operation, timeout=1800)
        merged = result['merge'] in {'merged', 'none'}
        conflicts = set(result['conflicts']) | set(result['mergeConflicts'])
        agreed, written, skipped = self.outbound(manifest, result, sent, conflicts, held)
        for rel in result['applied']:
            if rel in agreed or rel in written or rel in skipped or not merged:
                continue
            # main took the Mac content unchanged for this path.
            found = current.get(rel)
            if found is None:
                manifest.pop(rel, None)
            else:
                manifest[rel] = {**found, 'b': result['mainStateId']}
        for rel in result['conflicts']:
            if rel in sent:
                held[rel] = {'h': sent[rel], 'why': 'conflict'}
        if merged:
            # A path whose base the guest compared with main, and which main left alone or
            # the Mac took, now agrees with main.
            for rel, known in manifest.items():
                if rel not in skipped and rel not in conflicts and known.get('b') in result['outbound']:
                    known['b'] = result['mainStateId']
        def scanned(rel: str) -> bool:
            return paths is None or any(rel == item or rel.startswith(item + '/') for item in paths)
        # A held path ends when main settles it, or when the Mac file changes again.
        held = {rel: value for rel, value in held.items()
                if rel not in written and manifest.get(rel, {}).get('h') != value['h']
                and (not scanned(rel) or current.get(rel, {}).get('h', 'absent') == value['h']
                     or rel in result['conflicts'])}
        _save(manifest_path, manifest)
        _save(self.directory / 'held.json', held)
        (self.directory / 'pending.json').unlink(missing_ok=True)
        (self.directory / 'allow-deletes.json').unlink(missing_ok=True)
        in_conflict = sorted(rel for rel, value in held.items() if value['why'] == 'conflict')
        with_agent = sorted(rel for rel, value in held.items() if value['why'] == 'agent')
        state = ('conflict' if in_conflict or waiting else 'waiting' if result['merge'] in {'refused', 'conflict'}
                 or with_agent else 'synced')
        rescan = [] if merged else sorted(item['path'] for item in params['entries'])
        return self.status(state=state, mainStateId=result['mainStateId'], sent=len(params['entries']),
                           received=len(written), conflicts=in_conflict, withAgent=with_agent, rescan=rescan,
                           mergeError=result.get('mergeError'), notices=notices, reason=None, deletes=0)

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
                 conflicts: set[str], held: dict[str, dict[str, Any]] | None = None
                 ) -> tuple[set[str], set[str], set[str]]:
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
                handed = (held or {}).get(rel, {})
                from_agent = handed.get('why') == 'agent' and handed.get('base') == base
                if rules.skipped(rel) or (rel in conflicts and not from_agent) \
                        or (known is not None and known.get('b') != base and not from_agent):
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
                allowed = {sent.get(rel)} | ({known['h']} if known else {None}) | ({handed['h']} if from_agent else set())
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
        return self.fetch_entry(state, rel)[1]

    def fetch_entry(self, state: str, rel: str) -> tuple[str, bytes, int]:
        """(kind, bytes, mode) of `rel` in a main state; kind 'absent' when it has none."""
        data, offset, kind, mode = b'', 0, 'absent', 0
        while True:
            chunk = self.client.call('sync.mac.read', {'projectId': self.project_id, 'stateId': state,
                                                       'path': rel, 'offset': offset}, timeout=120)
            if chunk['kind'] == 'absent':
                return 'absent', b'', 0
            kind, mode = chunk['kind'], chunk.get('mode', 0)
            piece = base64.b64decode(chunk['data'])
            data += piece
            offset += len(piece)
            if chunk['eof']:
                return kind, data, mode

    def resolve(self, rel: str, choice: str) -> dict[str, Any]:
        """Settle a conflict or a first-round difference for one path: 'mac' keeps the Mac
        version (main takes it in the next round), 'vm' keeps main's version (written to the
        Mac now), 'agent' hands the Mac version to the lead in the mac-conflict line."""
        if choice not in {'mac', 'vm', 'agent'}:
            raise ValueError('Choose mac, vm or agent')
        with self.lock(wait=60):
            manifest = _load(self.directory / 'manifest.json', {})
            held = _load(self.directory / 'held.json', {})
            status = _load(self.directory / 'status.json', {})
            choices = set(status.get('choices', []))
            if rel not in held and rel not in choices:
                raise ValueError('This path has no conflict to settle: ' + rel)
            head = self.client.call('sync.mac.head', {'projectId': self.project_id}, timeout=30)['mainStateId']
            kind, data, mode = self.fetch_entry(head, rel)
            result: dict[str, Any] = {'path': rel, 'choice': choice, 'mainStateId': head}
            if choice == 'vm':
                entry = write_mac(self.root, rel, kind, data, mode)
                if entry is None:
                    manifest.pop(rel, None)
                else:
                    manifest[rel] = {**entry, 'b': head}
                held.pop(rel, None)
            elif choice == 'mac':
                # Agreed with main as it is: the next round sends the Mac file on top of it.
                if kind == 'absent':
                    manifest.pop(rel, None)
                else:
                    manifest[rel] = {'k': kind, 's': -1, 'm': 0, 'x': mode, 'h': hashlib.sha256(data).hexdigest(),
                                     'b': head}
                held.pop(rel, None)
            else:
                path = _inside(self.root, rel)
                try:
                    now = _describe(path, path.lstat(), digest=True)
                except FileNotFoundError:
                    now = None
                operation = 'mac-' + uuid.uuid4().hex
                params: dict[str, Any] = {'projectId': self.project_id, 'operationId': operation, 'path': rel,
                                          'kind': now['k'] if now else 'absent'}
                if now is not None:
                    upload, _ = self.upload(operation, [(rel, now, None)], manifest)
                    params['upload'] = upload['upload']
                handoff = self.client.call('sync.mac.handoff', params, request_id=operation, timeout=600)
                held[rel] = {'h': now['h'] if now else 'absent', 'why': 'agent', 'base': handoff['mainStateId']}
                result.update(line=handoff['line'], stateId=handoff['stateId'], kind=params['kind'])
            choices.discard(rel)
            _save(self.directory / 'manifest.json', manifest)
            _save(self.directory / 'held.json', held)
            remaining = sorted(path for path, value in held.items() if value['why'] == 'conflict')
            with_agent = sorted(path for path, value in held.items() if value['why'] == 'agent')
            rescan = set(status.get('rescan', [])) | ({rel} if choice == 'mac' else set())
            self.status(choices=sorted(choices), conflicts=remaining, withAgent=with_agent, rescan=sorted(rescan),
                        state='conflict' if remaining or choices else 'waiting' if with_agent else 'synced')
            if choice == 'mac':
                # The Mac file now differs from its agreed content: this round sends it.
                result['round'] = self._round({rel})
            return result

    def allow_deletes(self) -> dict[str, Any]:
        """The user confirmed the Mac deletions that paused the project: the next round sends them."""
        with self.lock(wait=60):
            count = int(_load(self.directory / 'status.json', {}).get('deletes') or 0)
            if count <= 0:
                raise ValueError('No Mac deletions wait for a confirmation')
            _save(self.directory / 'allow-deletes.json', {'count': count})
            return self._round(None)


def project_sync(client: Any, project_id: str) -> ProjectSync:
    """The sync of a layr project with a Mac folder; ValueError when it has none."""
    try:
        data = json.loads((client.state_dir / 'layr-projects' / project_id / 'project.json').read_text())
    except (OSError, ValueError) as error:
        raise ValueError('The project has no Mac folder to sync') from error
    if not data.get('source'):
        raise ValueError('The project has no Mac folder to sync')
    return ProjectSync(client, project_id, Path(data['source']), import_state=data.get('importStateId'))


def handoff_text(result: dict[str, Any], kind: str) -> str:
    rel, state, line = str(result['path']), str(result['stateId']), str(result['line'])
    mac = ('The Mac deleted this file.' if kind == 'absent'
           else 'Read the Mac version with `layr show ' + state + ':' + rel + '`.')
    return ('[Studio Mac sync] The user\'s Mac folder and main changed ' + rel + ' in different ways. '
            'The user asked you to combine the two versions. The Mac version is layr state ' + state
            + ' in the line ' + line + '. ' + mac + ' Put one version that keeps both changes '
            'in main and commit it. Studio then writes main\'s file to the Mac folder.')


def lead_for(runtime: Any, project_id: str) -> str | None:
    """The most recently updated lead chat that works in the project's main line."""
    with runtime.lock, runtime.db() as db:
        leads = [agent for agent in runtime.records(db, 'agents')
                 if agent.get('isLead') and agent.get('layrProjectId') == project_id
                 and agent.get('layrLine') == 'main' and not agent.get('deletedAt')]
    leads.sort(key=lambda agent: agent.get('updatedAt') or agent.get('createdAt') or 0, reverse=True)
    return str(leads[0]['id']) if leads else None


def resolve(runtime: Any, project_id: str, rel: str, choice: str) -> dict[str, Any]:
    """The user's choice for one conflicting path. 'agent' sends the lead a message once
    per handoff state, so a repeated request does not repeat it."""
    from codex_linux_workspaces import client as vm_client
    client = vm_client(runtime)
    project = project_sync(client, project_id)
    result = project.resolve(rel, choice)
    result.pop('round', None)
    if choice == 'agent':
        lead = lead_for(runtime, project_id)
        result['leadId'] = lead
        if lead is not None:
            runtime.send(lead, handoff_text(result, result['kind']),
                         'mac-conflict:' + result['stateId'] + ':' + hashlib.sha256(rel.encode()).hexdigest()[:16],
                         manual=False, delivery='after_turn')
    result['status'] = sync_status(client.state_dir, project_id)
    return result


def configure(runtime: Any, project_id: str, *, enabled: bool | None = None,
              allow_deletes: bool = False) -> dict[str, Any] | None:
    from codex_linux_workspaces import client as vm_client
    client = vm_client(runtime)
    if enabled is not None:
        set_enabled(client.state_dir, project_id, enabled)
    if allow_deletes:
        project_sync(client, project_id).allow_deletes()
    return sync_status(client.state_dir, project_id)


HEAD_INTERVAL = 10.0
FULL_INTERVAL = 600.0
QUIET = 2.0


def set_enabled(state_dir: Path, project_id: str, enabled: bool) -> dict[str, Any]:
    """Pause or resume one project's Mac sync. A paused project keeps its manifest: on
    resume, a full scan sends what changed on the Mac during the pause."""
    folder = state_dir / 'layr-projects' / project_id
    if not (folder / 'project.json').is_file():
        raise ValueError('The project has no Mac folder to sync')
    directory = folder / 'sync'
    _save(directory / 'config.json', {**_load(directory / 'config.json', {}), 'enabled': enabled})
    status = dict(_load(directory / 'status.json', {}))
    if enabled:
        status.update(state='syncing', reason=None)
    else:
        status.update(state='paused', reason='Paused by the user')
    status['at'] = time.time()
    _save(directory / 'status.json', status)
    return status


def sync_status(state_dir: Path, project_id: str) -> dict[str, Any] | None:
    """The sidebar's sync report for a project, None when it has no Mac folder."""
    folder = state_dir / 'layr-projects' / project_id
    try:
        data = json.loads((folder / 'project.json').read_text())
    except (OSError, ValueError):
        return None
    if not data.get('source'):
        return None
    status = _load(folder / 'sync' / 'status.json', {})
    enabled = _load(folder / 'sync' / 'config.json', {}).get('enabled', True)
    state = status.get('state') or 'syncing'
    if not enabled:
        state = 'paused'
    return {'state': state, 'folder': data['source'], 'enabled': enabled,
            'conflicts': list(status.get('conflicts') or []), 'choices': list(status.get('choices') or []),
            'withAgent': list(status.get('withAgent') or []), 'deletes': int(status.get('deletes') or 0),
            'reason': status.get('reason'), 'at': status.get('at')}


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


class Watcher:
    """One studio-fs-events process for every synced Mac folder. It collects the changed
    paths per folder; a dropped-events report asks for a full scan of that folder."""

    def __init__(self, helper: Path):
        self.helper = helper
        self.process: subprocess.Popen[bytes] | None = None
        self.roots: tuple[Path, ...] = ()
        self.pending: dict[Path, set[str] | None] = {}
        self.last: dict[Path, float] = {}
        self.guard = threading.Lock()

    def available(self) -> bool:
        return self.process is not None and self.process.poll() is None

    def watch(self, roots: list[Path]) -> None:
        wanted = tuple(sorted({root.resolve() for root in roots}))
        if wanted == self.roots and self.available():
            return
        self.stop()
        self.roots = wanted
        if not wanted or not self.helper.is_file():
            return
        self.process = subprocess.Popen([str(self.helper), *map(str, wanted)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, close_fds=True)
        with self.guard:
            # Whatever happened while no helper watched is unknown: scan in full.
            for root in wanted:
                self.pending[root] = None
                self.last[root] = 0.0
        threading.Thread(target=self.read, args=(self.process,), name='studio-fs-events', daemon=True).start()

    def read(self, process: subprocess.Popen[bytes]) -> None:
        assert process.stdout is not None
        with process.stdout:
            for line in process.stdout:
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                value = event.get('path') or event.get('rescan')
                if not isinstance(value, str):
                    continue
                changed = Path(value)
                for root in self.roots:
                    if changed == root or root in changed.parents:
                        with self.guard:
                            known = self.pending.get(root, set())
                            if 'rescan' in event or changed == root or known is None:
                                self.pending[root] = None
                            else:
                                known.add(changed.relative_to(root).as_posix())
                                self.pending[root] = known
                            self.last[root] = time.monotonic()
                        break

    def take(self, root: Path) -> tuple[bool, set[str] | None]:
        """(something changed and is quiet for QUIET seconds, its paths or None for all)."""
        with self.guard:
            if root not in self.pending or time.monotonic() - self.last.get(root, 0.0) < QUIET:
                return False, set()
            return True, self.pending.pop(root)

    def stop(self) -> None:
        if self.process is not None and self.process.poll() is None:
            assert self.process.stdin is not None
            self.process.stdin.close()
            try:
                self.process.wait(5)
            except subprocess.TimeoutExpired:
                self.process.kill()
        self.process = None


def watcher_helper() -> Path:
    from codex_layout import DESKTOP_ROOT, REPOSITORY_ROOT
    packaged = REPOSITORY_ROOT.parent / 'studio-fs-events'
    return packaged if packaged.is_file() else DESKTOP_ROOT / 'native' / 'studio-fs-events'


def _scope(client: Any, project: ProjectSync, watcher: Watcher, times: dict[str, float],
           now: float) -> tuple[bool, set[str] | None]:
    """(a round is due, what it reads on the Mac): None for all, or a set of paths that
    is empty when only main changed."""
    full_every = FULL_INTERVAL if watcher.available() else HEAD_INTERVAL
    changed, paths = watcher.take(project.root)
    status = _load(project.directory / 'status.json', {})
    # Mac deletions that wait for the user: only a full scan looks again.
    guarded = status.get('state') == 'paused' and bool(status.get('deletes'))
    if now - times['full'] >= full_every or (changed and guarded):
        return True, None
    if guarded:
        return False, None
    if changed:
        return True, paths
    if now - times['head'] < HEAD_INTERVAL:
        return False, None
    times['head'] = now
    head = client.call('sync.mac.head', {'projectId': project.project_id}, timeout=30)['mainStateId']
    # A conflict or a handoff waits for main or the Mac to change.
    if head == status.get('mainStateId') and not status.get('rescan') \
            and status.get('state') in {'synced', 'conflict', 'waiting'}:
        return False, None
    return True, set()


def tick(runtime: Any) -> None:
    """Start sync work in the runtime's worker pool, never under the runtime lock and never
    twice at once: a round after file events go quiet, a check of main every
    HEAD_INTERVAL seconds, and a full scan every FULL_INTERVAL seconds (every
    HEAD_INTERVAL seconds without the file event helper). A stopped VM makes it wait."""
    import platform
    if platform.system() != 'Darwin' or runtime.__dict__.get('_mac_sync_busy'):
        return
    from codex_linux_workspaces import client as vm_client
    client = vm_client(runtime)
    projects = synced_projects(client.state_dir)
    watcher = runtime.__dict__.get('_mac_sync_watcher')
    if watcher is None:
        watcher = runtime._mac_sync_watcher = Watcher(watcher_helper())
    watcher.watch([Path(data['source']) for data in projects])
    if not projects:
        return
    runtime._mac_sync_busy = True
    timing = runtime.__dict__.setdefault('_mac_sync_timing', {})

    def run() -> None:
        try:
            if client.status().get('state') != 'running':
                return
            now = time.monotonic()
            for data in projects:
                project = ProjectSync(client, data['projectId'], Path(data['source']),
                                      import_state=data.get('importStateId'))
                times = timing.setdefault(data['projectId'], {'full': 0.0, 'head': 0.0})
                try:
                    due, scope = _scope(client, project, watcher, times, now)
                    if not due:
                        continue
                    project.round(scope)
                    if scope is None:
                        times['full'] = now
                except BlockingIOError:
                    continue
                except Exception as error:  # noqa: BLE001 - every failure stays visible in the status
                    project.status(state='error', reason=str(error)[:500])
                    times['full'] = 0.0
        finally:
            runtime._mac_sync_busy = False
    runtime.pool.submit(run)
