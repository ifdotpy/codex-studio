"""Transfer complete native rollout bytes. No summaries or credential files."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile
from typing import Any
import uuid
import zipfile

MAX_HISTORY_BYTES = 256 * 1024 * 1024
MAX_HISTORY_FILES = 1024


def private_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix='.move-')
    temporary = Path(name)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _metadata(data: bytes) -> dict[str, Any]:
    if not data.endswith(b'\n'):
        raise ValueError('The native history has an incomplete final record')
    first = json.loads(data.split(b'\n', 1)[0])
    if not isinstance(first, dict) or first.get('type') != 'session_meta':
        raise ValueError('The native history has no session metadata')
    payload = first.get('payload')
    if not isinstance(payload, dict):
        raise ValueError('The native history has invalid session metadata')
    uuid.UUID(payload['id'])
    return payload


def export_codex(home: Path, rollout: Path, destination: Path) -> dict[str, Any]:
    """Export the complete rollout and exact inherited byte boundaries."""
    from codex_account_transfer import AccountTransfers
    home = home.resolve()
    files: dict[str, bytes] = {}
    active: set[str] = set()
    total = 0

    def collect(path: Path, boundary: int | None = None) -> str:
        nonlocal total
        path = path.resolve()
        relative = path.relative_to(home)
        if relative.parts[0] not in {'sessions', 'archived_sessions'} or path.suffix != '.jsonl':
            raise ValueError('Only native session files can move')
        if path.name in active:
            raise ValueError('The native history ancestry contains a cycle')
        active.add(path.name)
        size = path.stat().st_size if boundary is None else boundary
        if isinstance(size, bool) or size < 1 or size > path.stat().st_size:
            raise ValueError('The native history boundary is invalid')
        if total + size > MAX_HISTORY_BYTES or len(files) >= MAX_HISTORY_FILES:
            raise ValueError('The complete native history exceeds the move limit (256 MiB or 1024 files). No history was truncated')
        with path.open('rb') as stream:
            data = stream.read(size)
        if len(data) != size:
            raise ValueError('The native history changed during export')
        metadata = _metadata(data)
        parent = metadata.get('history_base')
        if parent is not None:
            if not isinstance(parent, dict) or not isinstance(parent.get('end_byte_offset'), int):
                raise ValueError('The native history ancestor boundary is invalid')
            collect(AccountTransfers.find_rollout(home, parent['thread_id']), parent['end_byte_offset'])
        name = 'sessions/' + path.name
        if name in files and files[name] != data:
            raise ValueError('The native history has conflicting file names')
        if name not in files:
            if total + len(data) > MAX_HISTORY_BYTES or len(files) >= MAX_HISTORY_FILES:
                raise ValueError('The complete native history exceeds the move limit. No history was truncated')
            files[name] = data
            total += len(data)
        active.remove(path.name)
        return name

    primary = collect(rollout)
    descriptor = {'version': 1, 'provider': 'codex', 'primary': primary, 'bytes': total,
                  'files': [{'name': name, 'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
                            for name, data in files.items()]}
    destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=destination.parent, prefix='.history-')
    temporary = Path(name)
    try:
        with os.fdopen(fd, 'wb') as stream:
            with zipfile.ZipFile(stream, 'w', compression=zipfile.ZIP_STORED) as archive:
                archive.writestr('manifest.json', json.dumps(descriptor).encode())
                for entry, data in files.items():
                    archive.writestr(entry, data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    return descriptor


def unpack_codex(archive_path: Path, staging_home: Path) -> Path:
    """Validate before publication. Importers use the existing account copy path."""
    with zipfile.ZipFile(archive_path) as archive:
        if len(archive.infolist()) > MAX_HISTORY_FILES + 1:
            raise ValueError('The native history contains too many files')
        if len(archive.namelist()) != len(set(archive.namelist())):
            raise ValueError('The native history contains duplicate entries')
        if archive.getinfo('manifest.json').file_size > 1024 * 1024:
            raise ValueError('The native history manifest exceeds its limit')
        manifest = json.loads(archive.read('manifest.json'))
        if manifest.get('version') != 1 or manifest.get('provider') != 'codex':
            raise ValueError('The native history archive format is unsupported')
        entries = manifest.get('files')
        if not isinstance(entries, list) or not entries or len(entries) > MAX_HISTORY_FILES:
            raise ValueError('The native history file list is invalid')
        names = [entry['name'] for entry in entries]
        if len(names) != len(set(names)):
            raise ValueError('The native history manifest contains duplicate entries')
        if set(archive.namelist()) != {'manifest.json', *names}:
            raise ValueError('The native history contains unlisted files')
        total = 0
        validated: list[tuple[str, bytes]] = []
        for entry in entries:
            name = entry['name']
            path = Path(name)
            if (path.is_absolute() or len(path.parts) != 2 or path.parts[0] != 'sessions'
                    or path.suffix != '.jsonl' or '\\' in name or path.name in {'.', '..'}):
                raise ValueError('The native history contains an invalid path')
            info = archive.getinfo(name)
            total += info.file_size
            if total > MAX_HISTORY_BYTES or info.file_size != entry['size']:
                raise ValueError('The native history exceeds its complete size limit')
            data = archive.read(name)
            if hashlib.sha256(data).hexdigest() != entry['sha256']:
                raise ValueError('The native history checksum does not match')
            _metadata(data)
            validated.append((name, data))
        if total != manifest.get('bytes') or manifest.get('primary') not in names:
            raise ValueError('The native history manifest is invalid')
    for name, data in validated:
        destination = staging_home / name
        if destination.exists() and destination.read_bytes() != data:
            raise ValueError('The saved native history has different bytes')
        private_write(destination, data)
    return staging_home / str(manifest['primary'])


def frozen_codex_parameters(rollout: Path) -> dict[str, Any]:
    """Read persisted instructions and settings without changing old records."""
    metadata: dict[str, Any] | None = None
    context: dict[str, Any] | None = None
    with rollout.open('rb') as stream:
        for line in stream:
            record = json.loads(line)
            if record.get('type') == 'session_meta':
                metadata = record['payload']
            elif record.get('type') == 'turn_context':
                context = record['payload']
    if metadata is None or context is None:
        raise ValueError('Exact move settings are unavailable before the first native turn')
    base = metadata.get('base_instructions')
    if not isinstance(base, dict) or not isinstance(base.get('text'), str):
        raise ValueError('The saved native base instructions are unavailable')
    developer = context.get('developer_instructions')
    if developer is not None and not isinstance(developer, str):
        raise ValueError('The saved native developer instructions are invalid')
    model = context.get('model')
    if not isinstance(model, str) or not model:
        raise ValueError('The saved native model is unavailable')
    return {'baseInstructions': base['text'], 'developerInstructions': developer,
            'model': model, 'dynamicTools': metadata.get('dynamic_tools', []),
            'effort': context.get('effort'), 'summary': context.get('summary')}


def extend_codex_history(native: Any, home: Path, thread: str, imported: Path, backup: Path) -> None:
    """Release one idle session and extend its indexed path with exact bytes."""
    home = home.resolve()
    data = imported.read_bytes()
    if _metadata(data)['id'] != thread:
        raise ValueError('The imported native history has a different identity')

    def current() -> Path:
        snapshot = native.call('thread/read', {'threadId': thread, 'includeTurns': False}, timeout=20)['thread']
        if snapshot.get('status', {}).get('type') == 'active':
            raise ValueError('The destination native session is active')
        path = Path(snapshot['path']).resolve()
        relative = path.relative_to(home)
        if relative.parts[0] not in {'sessions', 'archived_sessions'} or path.suffix != '.jsonl':
            raise ValueError('The destination native history is outside the account')
        old = path.read_bytes()
        if _metadata(old)['id'] != thread or not data.startswith(old):
            raise ValueError('The imported history does not extend the exact destination prefix')
        return path

    path = current()
    private_write(backup, path.read_bytes())
    # Unsubscribe retains an idle loaded session. Archive closes only this
    # session; native paginated metadata still requires its indexed file path.
    native.call('thread/archive', {'threadId': thread}, timeout=20)
    path = current()
    private_write(path, data)
    native.call('thread/unarchive', {'threadId': thread}, timeout=20)


def export_claude(path: Path, destination: Path) -> None:
    size = path.stat().st_size
    if not 0 < size <= MAX_HISTORY_BYTES:
        raise ValueError('The complete Claude history exceeds 256 MiB. No history was truncated')
    data = path.read_bytes()
    if len(data) != size or not data.endswith(b'\n'):
        raise ValueError('The Claude native history changed or has an incomplete record')
    private_write(destination, data)
