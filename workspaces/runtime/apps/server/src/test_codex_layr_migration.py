"""Moving projects and their worktrees into layr, without a VM (codex_layr_migration.py)."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

import codex_layr_migration as migration


def git(directory: Path, *arguments: str) -> None:
    subprocess.run(['git', '-C', str(directory), *arguments], check=True, capture_output=True, timeout=30)


def repository(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    git(path, 'init', '-q', '-b', 'main')
    git(path, 'config', 'user.name', 'Fixture')
    git(path, 'config', 'user.email', 'fixture@example.test')
    (path / 'code.txt').write_text('code')
    git(path, 'add', '.')
    git(path, 'commit', '-qm', 'First')
    return path


class Client:
    def __init__(self, state_dir: Path):
        self.state_dir = state_dir
        self.calls: list[tuple[str, dict[str, Any], Any]] = []
        self.imports: list[tuple[str, str]] = []

    def ensure_layr_project(self, project: str, source: str, owner: Any = None, request_id: str = '') -> dict[str, Any]:
        self.imports.append((project, source))
        (self.state_dir / 'layr-projects' / project / 'project.json').write_text(json.dumps({'source': source}))
        return {'projectId': project}

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        self.calls.append((method, params, options.get('request_id')))
        return {'line': params['line'], 'stateId': 's-' + params['line'], 'files': 1}


class Migration(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix='layr-migration-')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = repository(self.base / 'project')
        (self.root / '.gitignore').write_text('data/\n.env\nnested/\n')
        git(self.root, 'add', '.gitignore')
        git(self.root, 'commit', '-qm', 'Ignore')
        git(self.root, 'worktree', 'add', '-q', '-b', 'topic', str(self.root / '.worktrees' / 'topic'))
        git(self.root, 'worktree', 'add', '-q', '-b', 'elsewhere', str(self.base / 'elsewhere'))
        repository(self.root / 'nested')
        git(self.root / 'nested', 'worktree', 'add', '-q', '-b', 'fix', str(self.root / 'nested' / '.worktrees' / 'fix'))
        repository(self.root / 'data' / 'clone')
        git(self.root / 'data' / 'clone', 'worktree', 'add', '-q', '-b', 'x', str(self.base / 'clone-worktree'))

    def test_worktrees_of_the_project_and_its_nested_repositories(self) -> None:
        found = {(item['repo'], Path(item['path']).name, item.get('branch')) for item in migration.worktrees(self.root)}
        self.assertEqual(found, {('.', 'topic', 'topic'), ('.', 'elsewhere', 'elsewhere'), ('nested', 'fix', 'fix')})

    def test_a_worktree_line_takes_code_and_ignored_files_not_folders(self) -> None:
        worktree = self.root / '.worktrees' / 'topic'
        (worktree / 'code.txt').write_text('changed, not committed')
        (worktree / 'new.txt').write_text('untracked')
        (worktree / '.env').write_text('SECRET=1')
        for folder in ('data/big', 'node_modules/x', 'crate/target/debug'):
            (worktree / folder).mkdir(parents=True)
            (worktree / folder / 'file').write_text('skip')
        (worktree / 'crate' / 'Cargo.toml').write_text('[package]')
        (worktree / 'link').symlink_to('code.txt')
        files = migration.worktree_files(worktree)
        self.assertEqual(files, ['.env', '.gitignore', 'code.txt', 'crate/Cargo.toml', 'link', 'new.txt'])
        archive = self.base / 'one.tar.gz'
        self.assertEqual(migration.worktree_archive(worktree, archive), 6)
        again = self.base / 'two.tar.gz'
        migration.worktree_archive(worktree, again)
        self.assertEqual(archive.read_bytes(), again.read_bytes())
        with tarfile.open(archive) as output:
            self.assertTrue(output.getmember('link').issym())
            self.assertEqual(output.extractfile('code.txt').read(), b'changed, not committed')  # type: ignore[union-attr]

    def test_line_names_are_valid_and_unique(self) -> None:
        used: set[str] = set()
        first = migration.line_name({'repo': '.', 'path': '/x/.worktrees/a b..c'}, used)
        second = migration.line_name({'repo': '.', 'path': '/y/a b..c'}, used)
        nested = migration.line_name({'repo': 'apps/web', 'path': '/z/fix.lock'}, used)
        self.assertEqual((first, second, nested), ('wt-a-b.c', 'wt-a-b.c-2', 'wt-apps-web-fix'))

    def test_a_run_imports_paused_moves_each_worktree_once_and_reports_failures(self) -> None:
        state = self.base / 'state'
        client = Client(state)
        with patch('codex_linux_vm_share.upload_archive'):
            dry = migration.migrate_project(client, self.root, dry_run=True, log=lambda _: None)
            self.assertEqual((dry['worktrees'], client.imports), (3, []))
            result = migration.migrate_project(client, self.root, log=lambda _: None)
            project = migration.project_id(self.root)
            folder = state / 'layr-projects' / project
            self.assertEqual(json.loads((folder / 'sync' / 'config.json').read_text()), {'enabled': False})
            self.assertEqual(client.imports, [(project, str(self.root))])
            self.assertEqual((result['moved'], result['failed']), (3, 0))
            calls = [params for method, params, _ in client.calls if method == 'sync.mac.worktree']
            self.assertEqual(sorted((params['line'], params['subpath']) for params in calls),
                             [('wt-elsewhere', '.'), ('wt-nested-fix', 'nested'), ('wt-topic', '.')])
            journal = json.loads((folder / 'worktrees.json').read_text())
            self.assertEqual(len(journal), 3)
            again = migration.migrate_project(client, self.root, log=lambda _: None)
            self.assertEqual((again['moved'], len(client.imports)), (0, 1))

            def refuse(method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
                raise RuntimeError('guest refused')
            (folder / 'worktrees.json').unlink()
            setattr(client, 'call', refuse)
            failed = migration.migrate_project(client, self.root, log=lambda _: None)
            self.assertEqual((failed['moved'], failed['failed']), (0, 3))


    def test_an_inner_worktree_is_a_line_and_a_prunable_one_is_skipped(self) -> None:
        git(self.root, 'worktree', 'add', '-q', '-b', 'rehearsal', str(self.root / 'rehearsal'))
        git(self.root, 'worktree', 'add', '-q', '-b', 'gone', str(self.base / 'gone'))
        (self.base / 'gone' / '.git').unlink()
        self.assertNotIn('rehearsal', migration.repositories(self.root))
        found = {Path(item['path']).name: item for item in migration.worktrees(self.root)}
        self.assertEqual(sorted(found), ['elsewhere', 'fix', 'gone', 'rehearsal', 'topic'])
        self.assertEqual((found['rehearsal']['repo'], 'prunable' in found['gone']), ('.', True))
        client = Client(self.base / 'state')
        with patch('codex_linux_vm_share.upload_archive'):
            result = migration.migrate_project(client, self.root, log=lambda _: None)
        self.assertEqual((result['moved'], result['failed'], result['skipped']), (4, 0, 1))

if __name__ == '__main__':
    unittest.main()
