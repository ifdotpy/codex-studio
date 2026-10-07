"""Build source archives with the image workspace Git change detector."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import stat
import shutil
import tarfile
import subprocess
import tempfile
import time
import select

from codex_workspace_images import _detected_paths, _repo_snapshots, _workspace_excludes


def _git(root, *args, input=None):
    return subprocess.run(['git', '-C', str(root), *args], input=input,
        capture_output=True, check=True, timeout=180,
        env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'}).stdout


def _git_roots(root):
    refs = _git(root, 'for-each-ref', '--format=%(objectname)').splitlines()
    staged = _git(root, 'ls-files', '--stage', '-z').split(b'\0')
    roots = {line.decode() for line in refs if line}
    for row in staged:
        if row:
            sha = row.split(b'\t', 1)[0].split()[1].decode()
            if set(sha) != {'0'}:
                roots.add(sha)
    head = subprocess.run(['git', '-C', str(root), 'rev-parse', '--verify', 'HEAD'],
        capture_output=True, timeout=10).stdout.strip().decode()
    if head:
        roots.add(head)
    return sorted(roots)


def _git_metadata_paths(root):
    result = []
    for current, directories, files in os.walk(root / '.git', followlinks=False):
        directory = Path(current)
        directories[:] = [name for name in directories if name != 'objects']
        result.extend((directory / name).relative_to(root).as_posix() for name in files)
    return sorted(result)


def _validate_object_store(root):
    # Git follows links in its object store. Check metadata before Git can read
    # an object into a pack, including loose objects and existing pack files.
    objects = root / '.git/objects'
    for directory, children, files in os.walk(objects, followlinks=False):
        for name in [*children, *files]:
            if (Path(directory) / name).is_symlink():
                raise ValueError('The Git object store contains a symlink')


def _object_pack(root, prior, current, directory, floor):
    # Exclude roots proved present in the preceding upload. A self-contained
    # pack holds only newly needed objects, including staged blobs outside HEAD.
    known = prior.get('objectRoots') or ([prior['head']] if prior.get('head') else [])
    if known:
        checked = _git(root, 'cat-file', '--batch-check=%(objectname) %(objecttype)',
                       input=('\n'.join(known)+'\n').encode()).decode().splitlines()
        known = [line.split()[0] for line in checked if not line.endswith(' missing')]
    revisions = current['objectRoots'] + ['^' + sha for sha in known]
    pack = directory / 'objects.pack'
    process = subprocess.Popen(['git', '-C', str(root), 'pack-objects', '--revs', '--stdout'],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'})
    deadline = time.monotonic()+180
    try:
        process.stdin.write(('\n'.join(revisions)+'\n').encode())
        process.stdin.close()
        with pack.open('wb') as output:
            while True:
                if time.monotonic() >= deadline:
                    raise ValueError('The Git object delta exceeded its timeout')
                readable, _, _ = select.select([process.stdout], [], [], min(1, max(0,deadline-time.monotonic())))
                if not readable:
                    continue
                data = os.read(process.stdout.fileno(), 512*1024)
                if not data:
                    break
                if shutil.disk_usage(directory).free < floor+len(data):
                    raise ValueError('The Git object delta has insufficient free disk space')
                output.write(data)
        if process.wait(timeout=max(1,deadline-time.monotonic())):
            raise ValueError('The Git object delta could not be created')
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        process.stdout.close()
    _git(root, 'index-pack', str(pack))
    with pack.open('rb') as stream:
        name = 'studio-' + hashlib.file_digest(stream, 'sha256').hexdigest()
    metadata = '.git' if current['path'] == '.' else current['path']+'/.git'
    return [(pack, metadata+'/objects/pack/'+name+'.pack'),
            (pack.with_suffix('.idx'), metadata+'/objects/pack/'+name+'.idx')]


def archive_source(root, archive_path, baseline=None):
    root = Path(root).expanduser().resolve()
    if not root.is_dir():
        raise ValueError('The source folder does not exist')
    detected = _repo_snapshots(root)
    current = [dict(repo) for repo in detected]
    full = baseline is None or not any(r['path'] == '.' for r in current)
    excludes = _workspace_excludes(root)
    object_excludes = {('.git' if repo['path'] == '.' else repo['path']+'/.git')+'/objects' for repo in current}
    def excluded(relative):
        skip = excludes if full else [*excludes, *object_excludes]
        return any(relative == item or relative.startswith(item + '/') for item in skip)

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
            if any(prior.get(field) != repo.get(field)
                   for field in ('gitMetadataFingerprint', 'indexFingerprint')):
                # Refresh metadata and import only newly needed Git objects.
                metadata = '.git' if repo['path'] == '.' else repo['path'] + '/.git'
                selected.add(metadata)
                deleted.update(set(prior.get('gitMetadataPaths', [])) - set(_git_metadata_paths(root if repo['path'] == '.' else root/repo['path'])))

    # A source linked to an external Git store needs a separate object import.
    # Refuse an incomplete copy; the runtime can choose the host workspace.
    for repo in current:
        directory = root if repo['path'] == '.' else root / repo['path']
        if (not (directory / '.git').is_dir() or (directory / '.git').is_symlink()
                or (directory / '.git/objects').is_symlink()
                or (directory / '.git/objects/info/alternates').exists()):
            raise ValueError('The Linux VM source needs self-contained Git metadata; use a host workspace for this repository')

    for repo in current:
        directory = root if repo['path'] == '.' else root/repo['path']
        _validate_object_store(directory)
        repo['objectRoots'] = _git_roots(directory)
        repo['gitMetadataPaths'] = _git_metadata_paths(directory)

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
    packs = []
    staging = tempfile.TemporaryDirectory(prefix='linux-git-delta-', dir=archive_path.parent)
    try:
        if not full:
            for n, repo in enumerate(current):
                prior = before.get(repo['path'], {})
                if any(prior.get(field) != repo.get(field) for field in ('gitMetadataFingerprint','indexFingerprint')):
                    directory = Path(staging.name)/str(n)
                    directory.mkdir()
                    packs.extend(_object_pack(root if repo['path'] == '.' else root/repo['path'], prior, repo, directory, floor))
    except BaseException:
        staging.cleanup()
        raise
    with staging, archive_path.open('wb') as raw, tarfile.open(fileobj=GuardedOutput(raw), mode='w:gz', dereference=False) as archive:
        def add(relative):
            if relative in added or excluded(relative):
                return
            path = root if relative == '.' else root / relative
            # Check every parent before lstat, iteration, or file reads. A
            # selected tracked path can traverse a newly ignored symlink.
            parent = root
            for component in Path(relative).parts[:-1]:
                parent = parent / component
                value = parent.lstat()
                if stat.S_ISLNK(value.st_mode):
                    raise ValueError('A source path passes through a symlink: ' + relative)
                parent_relative = parent.relative_to(root).as_posix()
                identities.setdefault(parent_relative, (value.st_dev, value.st_ino, value.st_size,
                    value.st_mtime_ns, value.st_ctime_ns))
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
        for source, relative in packs:
            archive.add(source, arcname=relative, recursive=False)

    archive_path = Path(archive_path)
    for relative, before in identities.items():
        path = root if relative == '.' else root / relative
        value = path.lstat()
        after = (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        if after != before:
            raise ValueError('The source changed during the archive; retry when its files are stable')
    if _repo_snapshots(root) != detected:
        raise ValueError('The source Git state changed during the archive; retry when its files are stable')
    archive_path.chmod(0o600)
    with archive_path.open('rb') as stream:
        os.fsync(stream.fileno())
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    return {'mode': 'full' if full else 'delta', 'deletePaths': sorted(deleted),
            'totalBytes': archive_path.stat().st_size, 'sha256': digest,
            'repositories': current}
