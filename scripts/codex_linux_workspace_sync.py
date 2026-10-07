"""Build source archives with the image workspace Git change detector."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import stat
import shutil
import tarfile

from codex_workspace_images import _detected_paths, _repo_snapshots, _workspace_excludes


def archive_source(root, archive_path, baseline=None):
    root = Path(root).expanduser().resolve()
    if not root.is_dir():
        raise ValueError('The source folder does not exist')
    current = _repo_snapshots(root)
    full = baseline is None or not any(r['path'] == '.' for r in current)
    excludes = _workspace_excludes(root)
    def excluded(relative):
        return any(relative == item or relative.startswith(item + '/') for item in excludes)

    if full:
        selected = {'.'}
        deleted = set()
    else:
        selected, _ = _detected_paths(root, baseline, current)
        selected = {p for p in selected if not excluded(p)}
        deleted = {p for p in selected if not (root / p).exists() and not (root / p).is_symlink()}
        before = {r['path']: r for r in baseline.get('repositories', [])}
        for repo in current:
            prior = before.get(repo['path'], {})
            if prior.get('gitMetadataFingerprint') != repo.get('gitMetadataFingerprint'):
                metadata = '.git' if repo['path'] == '.' else repo['path'] + '/.git'
                selected.add(metadata)
                deleted.add(metadata)

    # A source linked to an external Git store needs a separate object import.
    # Refuse an incomplete copy; the runtime can choose the host workspace.
    for repo in current:
        directory = root if repo['path'] == '.' else root / repo['path']
        if not (directory / '.git').is_dir() or (directory / '.git/objects/info/alternates').exists():
            raise ValueError('The Linux VM source needs self-contained Git metadata; use a host workspace for this repository')

    added = set()
    identities = {}
    archive_path = Path(archive_path)
    floor = int(os.environ.get('CODEX_WORKSPACE_MIN_FREE_BYTES', str(20 * 1024 ** 3)))
    if floor < 0:
        raise ValueError('CODEX_WORKSPACE_MIN_FREE_BYTES must be non-negative')
    class GuardedOutput:
        def __init__(self, stream):
            self.stream = stream
        def write(self, data):
            if shutil.disk_usage(archive_path.parent).free < floor + len(data):
                raise RuntimeError('The host source archive has insufficient free disk space')
            return self.stream.write(data)
        def tell(self):
            return self.stream.tell()
        def flush(self):
            self.stream.flush()
    with archive_path.open('wb') as raw, tarfile.open(fileobj=GuardedOutput(raw), mode='w:gz', dereference=False) as archive:
        def add(relative):
            if relative in added or excluded(relative):
                return
            path = root if relative == '.' else root / relative
            try:
                value = path.lstat()
                mode = value.st_mode
            except FileNotFoundError:
                deleted.add(relative)
                return
            if stat.S_ISLNK(mode):
                if not path.resolve().is_relative_to(root):
                    raise ValueError('A source link points outside the project: ' + relative)
            elif not stat.S_ISDIR(mode) and not stat.S_ISREG(mode):
                raise ValueError('The source contains a special file: ' + relative)
            if stat.S_ISLNK(mode):
                info = archive.gettarinfo(str(path), arcname=relative)
                if os.path.isabs(info.linkname):
                    info.linkname = os.path.relpath(path.resolve(), path.parent)
                archive.addfile(info)
            else:
                archive.add(path, arcname=relative, recursive=False)
            identities[relative] = (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
            added.add(relative)
            if stat.S_ISDIR(mode):
                for child in sorted(path.iterdir()):
                    add(child.relative_to(root).as_posix())
        for relative in sorted(selected):
            add(relative)

    archive_path = Path(archive_path)
    for relative, before in identities.items():
        path = root if relative == '.' else root / relative
        value = path.lstat()
        after = (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        if after != before:
            raise ValueError('The source changed during the archive; retry when its files are stable')
    if _repo_snapshots(root) != current:
        raise ValueError('The source Git state changed during the archive; retry when its files are stable')
    archive_path.chmod(0o600)
    with archive_path.open('rb') as stream:
        os.fsync(stream.fileno())
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    return {'mode': 'full' if full else 'delta', 'deletePaths': sorted(deleted),
            'totalBytes': archive_path.stat().st_size, 'sha256': digest,
            'repositories': current}
