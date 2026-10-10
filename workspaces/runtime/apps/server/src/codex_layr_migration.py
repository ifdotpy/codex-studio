"""Move Studio projects and their Git worktrees into layr (docs/vm-layr.md, "Migration").

Each project folder is imported as a layr project with Mac folder sync paused. Each Git
worktree of the project's repositories becomes a line that starts at main, with the
repository folder taken from the worktree: committed and uncommitted work, and files that
Git ignores such as .env. Folders that Git ignores and regenerable folders stay on the Mac.
The Mac worktrees stay as they are. A repeated run skips the worktrees it already moved.

Run it with the Studio Python from the installed app scripts while the VM runs:

    python3 codex_layr_migration.py [--dry-run] PROJECT_FOLDER...
"""
from __future__ import annotations

import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
from typing import Any

from codex_linux_workspace_sync import _code_only_skips, _regenerable
from codex_workspace_images import _git_repositories, _workspace_excludes


def _save(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    temporary = path.with_name('.' + path.name + '.tmp')
    temporary.write_text(json.dumps(value, indent=1, sort_keys=True))
    temporary.chmod(0o600)
    os.replace(temporary, path)


def project_id(folder: Path) -> str:
    """The layr project ID that a new chat of this folder project uses (codex_vm_agents.project_id):
    from the registered path, without following links."""
    return hashlib.sha256(str(folder).encode()).hexdigest()


def repositories(root: Path) -> list[str]:
    """The repositories that a code import of `root` takes, as paths below it ('.' first)."""
    found = [relative for relative, _ in _git_repositories(root)]
    excludes = _workspace_excludes(root)
    skipped = _code_only_skips(root, found, excludes) | set(excludes)

    def inside(relative: str) -> bool:
        parts = relative.split('/')
        return '.worktrees' in parts or any('/'.join(parts[:index]) in skipped for index in range(1, len(parts) + 1))
    return sorted((path for path in found if path == '.' or not inside(path)),
                  key=lambda path: (path != '.', path))


def worktrees(root: Path) -> list[dict[str, str]]:
    """Every Git worktree of the project's repositories, other than the repository folder itself."""
    result = []
    for repo in repositories(root):
        directory = root if repo == '.' else root / repo
        listing = subprocess.run(['git', '-C', str(directory), 'worktree', 'list', '--porcelain'],
                                 capture_output=True, text=True, timeout=120)
        if listing.returncode:
            continue
        entry: dict[str, str] = {}
        for line in listing.stdout.splitlines() + ['']:
            if line.startswith('worktree '):
                entry = {'path': line[9:]}
            elif line.startswith('HEAD '):
                entry['head'] = line[5:]
            elif line.startswith('branch '):
                entry['branch'] = line[7:].removeprefix('refs/heads/')
            elif not line and entry:
                path = Path(entry['path'])
                if path.resolve() != directory.resolve() and path.is_dir():
                    result.append({**entry, 'repo': repo})
                entry = {}
    return result


def line_name(worktree: dict[str, str], used: set[str]) -> str:
    """A layr line name for a worktree: wt-<repository>-<worktree folder>, unique."""
    repo = '' if worktree['repo'] == '.' else worktree['repo'].replace('/', '-') + '-'
    base = re.sub(r'[^A-Za-z0-9._-]+', '-', 'wt-' + repo + Path(worktree['path']).name).strip('-.')
    base = re.sub(r'\.{2,}', '.', base)[:120].removesuffix('.lock')
    name, number = base, 2
    while name in used:
        name, number = f'{base}-{number}', number + 1
    used.add(name)
    return name


def worktree_files(worktree: Path) -> list[str]:
    """The files of a worktree that a line takes: what Git tracks or would track, and files
    that Git ignores outside ignored folders (such as .env). No .git, no regenerable folders."""
    skipped = _code_only_skips(worktree, ['.'], ())
    files = []
    for directory, folders, names in os.walk(worktree, followlinks=False):
        base = Path(directory)
        prefix = '' if base == worktree else base.relative_to(worktree).as_posix() + '/'
        keep = []
        for folder in folders:
            rel = prefix + folder
            if folder == '.git' or (base / folder).is_symlink() or rel in skipped or _regenerable(worktree, rel):
                if (base / folder).is_symlink() and rel not in skipped:
                    files.append(rel)
                continue
            keep.append(folder)
        folders[:] = keep
        for name in names:
            rel = prefix + name
            if name == '.git' or rel in skipped:
                continue
            files.append(rel)
    return sorted(files)


def worktree_archive(worktree: Path, archive: Path) -> int:
    """A gzip tar of the worktree files with no time in it: the same files give the same bytes."""
    files = worktree_files(worktree)
    folders = sorted({parent.as_posix() for rel in files for parent in Path(rel).parents if parent != Path('.')})
    with archive.open('wb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as packed, \
            tarfile.open(fileobj=packed, mode='w') as output:
        root = tarfile.TarInfo('.')
        root.type, root.mode = tarfile.DIRTYPE, 0o755
        output.addfile(root)
        for folder in folders:
            info = tarfile.TarInfo(folder)
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            output.addfile(info)
        for rel in files:
            info = output.gettarinfo(str(worktree / rel), arcname=rel)
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            info.mtime = 0
            if info.isfile():
                with (worktree / rel).open('rb') as stream:
                    output.addfile(info, stream)
            else:
                output.addfile(info)
    return len(files)


def migrate_project(client: Any, root: Path, *, dry_run: bool = False, log: Any = print) -> dict[str, Any]:
    registered = Path(os.path.abspath(root.expanduser()))
    project = project_id(registered)
    root = registered.resolve()
    folder = client.state_dir / 'layr-projects' / project
    found = worktrees(root)
    log(f'{root}: project {project[:12]}, {len(found)} worktrees')
    if dry_run:
        for item in found:
            log(f'  would move {item["path"]} ({item.get("branch", "detached")})')
        return {'projectId': project, 'worktrees': len(found)}
    if not (folder / 'project.json').exists():
        # Sync starts paused: the user turns it on per project (docs/vm-mac-sync.md).
        _save(folder / 'sync' / 'config.json', {'enabled': False})
        client.ensure_layr_project(project, str(registered), owner=None, request_id='layr-project:' + project)
        log('  imported')
    journal_path = folder / 'worktrees.json'
    journal: dict[str, Any] = json.loads(journal_path.read_text()) if journal_path.exists() else {}
    used = {value['line'] for value in journal.values()}
    from codex_linux_vm_share import upload_archive
    moved = failed = 0
    for item in found:
        if item['path'] in journal:
            continue
        name = line_name(item, used)
        try:
            with tempfile.TemporaryDirectory(prefix='studio-worktree-') as scratch:
                archive = Path(scratch) / 'worktree.tar.gz'
                count = worktree_archive(Path(item['path']), archive)
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                # The same project, worktree and content give the same operation: a retry finds
                # the guest's receipt.
                operation = 'mac-' + hashlib.sha256('\0'.join((project, item['path'], digest)).encode()).hexdigest()[:32]
                begin = {'uploadId': operation[4:36], 'root': '/var/lib/codex-studio/projects/mac-wt-' + operation[4:28],
                         'totalBytes': archive.stat().st_size, 'sha256': digest, 'mode': 'full', 'deletePaths': []}
                upload_archive(client, archive, begin, operation)
            message = ('Worktree ' + item['path'] + ' (' + (item.get('branch') or 'detached') + ' at '
                       + item.get('head', '?')[:12] + '), moved from the Mac')
            result = client.call('sync.mac.worktree', {
                'projectId': project, 'operationId': operation, 'line': name, 'subpath': item['repo'],
                'upload': begin['root'], 'message': message}, request_id=operation, timeout=1800)
        except Exception as error:  # noqa: BLE001 - one worktree must not stop the others
            failed += 1
            used.discard(name)
            log(f'  FAILED {item["path"]}: {str(error)[:300]}')
            continue
        journal[item['path']] = {'line': result['line'], 'stateId': result['stateId'], 'files': count,
                                 'branch': item.get('branch'), 'head': item.get('head'), 'repo': item['repo']}
        _save(journal_path, journal)
        moved += 1
        log(f'  {item["path"]} -> line {result["line"]} ({count} files)')
    return {'projectId': project, 'worktrees': len(found), 'moved': moved, 'failed': failed,
            'total': len(journal)}


def main(arguments: list[str]) -> int:
    dry_run = '--dry-run' in arguments
    folders = [Path(value) for value in arguments if value != '--dry-run']
    if not folders:
        print(__doc__)
        return 2
    from codex_linux_vm import Client
    client = Client()
    failed = 0
    for folder in folders:
        try:
            result = migrate_project(client, folder, dry_run=dry_run)
        except Exception as error:  # noqa: BLE001 - report each project and go on
            print(f'{folder}: FAILED {str(error)[:500]}')
            failed += 1
            continue
        print(json.dumps(result))
        failed += int(result.get('failed', 0) > 0)
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
