"""Mac side of the folder sync without a VM (docs/vm-mac-sync.md)."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

import codex_vm_mac_sync as sync


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class FakeClient:
    """Answers sync.mac.apply from a script and records every call."""

    def __init__(self, state_dir: Path, replies: list[Any]):
        self.state_dir, self.replies = state_dir, replies
        self.calls: list[tuple[str, dict[str, Any], Any]] = []

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        self.calls.append((method, params, options.get('request_id')))
        if method == 'sync.mac.apply':
            reply = self.replies.pop(0)
            if isinstance(reply, Exception):
                raise reply
            return dict(reply)
        if method == 'sync.mac.read':
            return {'kind': 'file', 'data': '', 'eof': True}
        raise AssertionError(method)


def reply(main: str = 'm2', **values: Any) -> dict[str, Any]:
    return {'mainStateId': main, 'macStateId': main, 'applied': [], 'conflicts': [], 'merge': 'none',
            'mergeConflicts': [], 'mergeError': None, 'outbound': {}, **values}


class RulesAndScan(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_ignored_folders_and_dependency_folders_stay_but_ignored_files_sync(self) -> None:
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        (self.root / '.gitignore').write_text('build/\n.env\n')
        for rel in ('src/a.py', '.env', 'build/out.o', 'node_modules/x/i.js', 'src/__pycache__/a.pyc'):
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text(rel)
        rules = sync.Rules(self.root)
        entries, notices = sync.scan(self.root, rules, {})
        self.assertEqual(set(entries), {'.gitignore', 'src/a.py', '.env'})
        self.assertEqual(notices, [])

    def test_unchanged_size_and_time_reuse_the_recorded_hash(self) -> None:
        (self.root / 'a.txt').write_text('one')
        rules = sync.Rules(self.root)
        first, _ = sync.scan(self.root, rules, {})
        manifest = {'a.txt': {**first['a.txt'], 'h': 'recorded'}}
        again, _ = sync.scan(self.root, rules, manifest)
        self.assertEqual(again['a.txt']['h'], 'recorded')
        os.utime(self.root / 'a.txt', ns=(1, 1))
        changed, _ = sync.scan(self.root, rules, manifest)
        self.assertEqual(changed['a.txt']['h'], sha(b'one'))

    def test_links_sync_as_links_and_special_files_are_reported(self) -> None:
        (self.root / 'target.txt').write_text('t')
        (self.root / 'link').symlink_to('target.txt')
        (self.root / 'folder-link').symlink_to('/etc')
        os.mkfifo(self.root / 'pipe')
        entries, notices = sync.scan(self.root, sync.Rules(self.root), {})
        self.assertEqual(entries['link']['k'], 'link')
        self.assertEqual(entries['folder-link']['k'], 'link')
        self.assertEqual(entries['link']['h'], sha(b'target.txt'))
        self.assertEqual(notices, ['pipe: a special file does not sync'])

    def test_writes_never_cross_a_symbolic_link_or_leave_the_folder(self) -> None:
        outside = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__('shutil').rmtree(outside, True))
        (self.root / 'escape').symlink_to(outside)
        for rel in ('escape/x.txt', '../x.txt'):
            with self.subTest(rel=rel), self.assertRaises(ValueError):
                sync.write_mac(self.root, rel, 'file', b'x', 0o644)
        self.assertEqual(list(outside.iterdir()), [])
        entry = sync.write_mac(self.root, 'new/deep/file.sh', 'file', b'#!/bin/sh\n', 0o755)
        assert entry is not None
        self.assertEqual((entry['h'], entry['x']), (sha(b'#!/bin/sh\n'), 0o755))
        self.assertEqual([p.name for p in (self.root / 'new/deep').iterdir()], ['file.sh'])


class Rounds(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.root, self.state = base / 'project', base / 'state'
        self.root.mkdir()
        self.state.mkdir()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def project(self, replies: list[Any], manifest: dict[str, Any] | None = None) -> tuple[FakeClient, Any]:
        client = FakeClient(self.state, replies)
        project = sync.ProjectSync(client, 'p1', self.root, import_state='s1')
        setattr(project, 'send_archive', lambda archive, begin, operation: None)
        if manifest is not None:
            project.directory.mkdir(parents=True)
            (project.directory / 'manifest.json').write_text(json.dumps(manifest))
        return client, project

    def entry(self, rel: str, base: str = 's1') -> dict[str, Any]:
        path = self.root / rel
        info = path.lstat()
        return {'k': 'file', 's': info.st_size, 'm': info.st_mtime_ns, 'x': 0o644,
                'h': sha(path.read_bytes()), 'b': base}

    def test_a_lost_reply_repeats_the_same_operation(self) -> None:
        (self.root / 'a.txt').write_text('agreed')
        manifest = {'a.txt': self.entry('a.txt')}
        (self.root / 'a.txt').write_text('changed on the Mac')
        client, project = self.project([TimeoutError('lost reply'), reply(applied=['a.txt'], merge='merged')], manifest)
        with self.assertRaises(TimeoutError):
            project.round()
        project.round()
        operations = [request for method, _params, request in client.calls if method == 'sync.mac.apply']
        self.assertEqual(len(operations), 2)
        self.assertEqual(operations[0], operations[1])
        saved = json.loads((project.directory / 'manifest.json').read_text())
        self.assertEqual((saved['a.txt']['h'], saved['a.txt']['b']), (sha(b'changed on the Mac'), 'm2'))

    def test_many_mac_deletions_pause_the_round(self) -> None:
        for index in range(10):
            (self.root / f'f{index}.txt').write_text(str(index))
        manifest = {f'f{index}.txt': self.entry(f'f{index}.txt') for index in range(10)}
        for index in range(5):
            (self.root / f'f{index}.txt').unlink()
        client, project = self.project([], manifest)
        status = project.round()
        self.assertEqual(status['state'], 'paused')
        self.assertIn('5 files were deleted', status['reason'])
        self.assertEqual(client.calls, [])

    def test_main_changes_skip_a_file_the_user_edited_again(self) -> None:
        (self.root / 'a.txt').write_text('agreed')
        manifest = {'a.txt': self.entry('a.txt')}
        (self.root / 'a.txt').write_text('edited during the round')
        outbound = {'s1': [{'path': 'a.txt', 'kind': 'file', 'mode': 0o644, 'size': 3, 'sha256': sha(b'vm!')}]}
        client, project = self.project([reply(outbound=outbound)], manifest)
        with patch.object(sync.ProjectSync, 'upload', return_value=({'projectId': 'p1', 'operationId': 'x',
                                                                      'since': ['s1'], 'entries': []}, {})):
            project.round()
        self.assertEqual((self.root / 'a.txt').read_text(), 'edited during the round')



class Scheduling(unittest.TestCase):
    def test_only_folders_with_sync_enabled_take_part(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            state = Path(name)
            for project, enabled in (('a', None), ('b', False), ('c', True)):
                folder = state / 'layr-projects' / project
                folder.mkdir(parents=True)
                (folder / 'project.json').write_text(json.dumps({'projectId': project, 'source': name}))
                if enabled is not None:
                    (folder / 'sync').mkdir()
                    (folder / 'sync/config.json').write_text(json.dumps({'enabled': enabled}))
            (state / 'layr-projects/d').mkdir()
            (state / 'layr-projects/d/project.json').write_text(json.dumps({'projectId': 'd', 'source': name + '/missing'}))
            self.assertEqual([row['projectId'] for row in sync.synced_projects(state)], ['a', 'c'])

    def test_a_stopped_vm_makes_the_round_wait_and_a_failure_stays_visible(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            state = Path(name)
            folder = state / 'layr-projects/p'
            folder.mkdir(parents=True)
            (folder / 'project.json').write_text(json.dumps({'projectId': 'p', 'source': name}))

            class Client:
                state_dir = state
                running = False

                def status(self) -> dict[str, Any]:
                    return {'state': 'running' if self.running else 'stopped'}

                def call(self, *args: Any, **kwargs: Any) -> Any:
                    raise RuntimeError('guest refused')

            class Runtime:
                _mac_sync_at = 0.0

                def __init__(self) -> None:
                    self.pool = self

                def submit(self, function: Any) -> None:
                    function()

            client, runtime = Client(), Runtime()
            with patch('platform.system', return_value='Darwin'), \
                    patch('codex_linux_workspaces.client', return_value=client):
                sync.tick(runtime)
                self.assertFalse((folder / 'sync/status.json').exists())
                client.running = True
                runtime._mac_sync_at = 0.0
                sync.tick(runtime)
            status = json.loads((folder / 'sync/status.json').read_text())
            self.assertEqual((status['state'], status['reason']), ('error', 'guest refused'))


if __name__ == '__main__':
    unittest.main()
