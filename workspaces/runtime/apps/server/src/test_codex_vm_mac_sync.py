"""Mac side of the folder sync without a VM (docs/vm-mac-sync.md)."""
from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

import codex_vm_mac_sync as sync


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class FakeClient:
    """Answers sync.mac.apply from a script and records every call."""

    def __init__(self, state_dir: Path, replies: list[Any], files: dict[tuple[str, str], bytes | None] | None = None):
        self.state_dir, self.replies = state_dir, replies
        self.files = files or {}
        self.head = 'm9'
        self.calls: list[tuple[str, dict[str, Any], Any]] = []

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        self.calls.append((method, params, options.get('request_id')))
        if method == 'sync.mac.apply':
            reply = self.replies.pop(0)
            if isinstance(reply, Exception):
                raise reply
            if reply.pop('_exact', False):
                return dict(reply)
            # Like the guest: every requested base has a list of main's changes.
            return {**reply, 'outbound': {**{base: [] for base in params['since']}, **reply['outbound']}}
        if method == 'sync.mac.read':
            data = self.files.get((params['stateId'], params['path']), b'')
            if data is None:
                return {'kind': 'absent'}
            return {'kind': 'file', 'mode': 0o644, 'data': base64.b64encode(data).decode(), 'eof': True}
        if method == 'sync.mac.head':
            return {'mainStateId': self.head}
        if method == 'sync.mac.handoff':
            return {'line': 'mac-conflict', 'path': params['path'], 'mainStateId': self.head, 'stateId': 'h1'}
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


    def test_a_refused_merge_sends_its_paths_again_in_the_next_partial_round(self) -> None:
        (self.root / 'a.txt').write_text('agreed')
        manifest = {'a.txt': self.entry('a.txt')}
        (self.root / 'a.txt').write_text('mac')
        client, project = self.project([reply(main='s1', merge='refused', mergeError='main moved'),
                                        reply(applied=['a.txt'], merge='merged')], manifest)
        first = project.round()
        self.assertEqual((first['state'], first['rescan']), ('waiting', ['a.txt']))
        second = project.round(set())
        sent = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply']
        self.assertEqual([[item['path'] for item in entries] for entries in sent], [['a.txt'], ['a.txt']])
        self.assertEqual((second['state'], second['rescan']), ('synced', []))

    def test_a_replayed_round_keeps_the_paths_it_did_not_read(self) -> None:
        (self.root / 'a.txt').write_text('agreed')
        manifest = {'a.txt': self.entry('a.txt')}
        (self.root / 'a.txt').write_text('mac')
        client, project = self.project([TimeoutError('lost reply'), reply(applied=['a.txt'], merge='merged')], manifest)
        with self.assertRaises(TimeoutError):
            project.round()
        # A choice asks for b.txt to be read again while the lost round waits.
        project.status(rescan=['b.txt'])
        self.assertEqual(project.round(set())['rescan'], ['b.txt'])

    def test_a_base_the_guest_did_not_compare_stays_with_its_path(self) -> None:
        for rel in ('old.txt', 'new.txt'):
            (self.root / rel).write_text(rel)
        manifest = {'old.txt': self.entry('old.txt', 's0'), 'new.txt': self.entry('new.txt', 's1')}
        _, project = self.project([reply(merge='none', outbound={'s1': []}, _exact=True)], manifest)
        project.round()
        saved = json.loads((project.directory / 'manifest.json').read_text())
        self.assertEqual((saved['old.txt']['b'], saved['new.txt']['b']), ('s0', 'm2'))

    def test_confirmed_mac_deletions_go_to_the_vm(self) -> None:
        for index in range(10):
            (self.root / f'f{index}.txt').write_text(str(index))
        manifest = {f'f{index}.txt': self.entry(f'f{index}.txt') for index in range(10)}
        for index in range(5):
            (self.root / f'f{index}.txt').unlink()
        client, project = self.project([reply(applied=[f'f{index}.txt' for index in range(5)], merge='merged')],
                                       manifest)
        self.assertEqual(project.round()['deletes'], 5)
        self.assertEqual(project.allow_deletes()['state'], 'syncing')
        status = project.round()
        self.assertEqual((status['state'], status['deletes']), ('synced', 0))
        entries = next(params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply')
        self.assertEqual(sorted(item['path'] for item in entries if item['kind'] == 'absent'),
                         [f'f{index}.txt' for index in range(5)])
        self.assertFalse((project.directory / 'allow-deletes.json').exists())
        with self.assertRaises(ValueError):
            project.allow_deletes()



class ReviewRegressions(unittest.TestCase):
    """Rounds that must neither lose a Mac edit nor a main change (review of 2026-10-10)."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.root, self.state = base / 'project', base / 'state'
        self.root.mkdir()
        self.state.mkdir()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def project(self, replies: list[Any], manifest: dict[str, Any],
                files: dict[tuple[str, str], bytes | None] | None = None) -> tuple[FakeClient, Any]:
        client = FakeClient(self.state, replies, files)
        project = sync.ProjectSync(client, 'p1', self.root, import_state='s1')
        setattr(project, 'send_archive', lambda archive, begin, operation: None)
        project.directory.mkdir(parents=True)
        (project.directory / 'manifest.json').write_text(json.dumps(manifest))
        return client, project

    def agreed(self, rel: str, data: bytes, base: str = 's1', mode: int = 0o644) -> dict[str, Any]:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)
        info = path.lstat()
        return {'k': 'file', 's': info.st_size, 'm': info.st_mtime_ns, 'x': mode, 'h': sha(data), 'b': base}

    def saved(self, project: Any, name: str) -> Any:
        return json.loads((project.directory / name).read_text())

    @staticmethod
    def change(rel: str, data: bytes | None, mode: int = 0o644) -> dict[str, Any]:
        if data is None:
            return {'path': rel, 'kind': 'absent'}
        return {'path': rel, 'kind': 'file', 'mode': mode, 'size': len(data), 'sha256': sha(data)}

    def test_a_refused_merge_keeps_the_unmerged_mac_edit(self) -> None:
        manifest = {'a.txt': self.agreed('a.txt', b'agreed\n')}
        (self.root / 'a.txt').write_text('mac edit\n')
        outbound = {'s1': [self.change('a.txt', b'agent edit\n')]}
        _, project = self.project([reply(main='m2', merge='refused', outbound=outbound)], manifest,
                                  {('m2', 'a.txt'): b'agent edit\n'})
        status = project.round()
        self.assertEqual((self.root / 'a.txt').read_text(), 'mac edit\n')
        self.assertEqual(self.saved(project, 'manifest.json')['a.txt']['h'], sha(b'agreed\n'))
        self.assertEqual(status['rescan'], ['a.txt'])

    def test_a_newer_mac_edit_during_a_handoff_is_not_overwritten(self) -> None:
        manifest = {'app.txt': self.agreed('app.txt', b'agreed\n')}
        (self.root / 'app.txt').write_text('newer mac\n')
        combined = self.change('app.txt', b'combined\n')
        client, project = self.project([reply(main='m10', conflicts=['app.txt'],
                                              outbound={'s1': [combined], 'm9': [combined]})],
                                       manifest, {('m10', 'app.txt'): b'combined\n'})
        (project.directory / 'held.json').write_text(json.dumps(
            {'app.txt': {'h': sha(b'handed\n'), 'why': 'agent', 'base': 'm9', 'stateId': 'h1'}}))
        status = project.round()
        self.assertEqual((self.root / 'app.txt').read_text(), 'newer mac\n')
        self.assertEqual(status['conflicts'], ['app.txt'])
        self.assertEqual(self.saved(project, 'held.json')['app.txt']['h'], sha(b'newer mac\n'))

    def test_a_save_during_the_round_keeps_the_sent_content_as_the_next_base(self) -> None:
        manifest = {'a.txt': self.agreed('a.txt', b'x\n')}
        (self.root / 'a.txt').write_text('x abc\n')
        client, project = self.project([reply(main='m2', applied=['a.txt'], merge='merged',
                                              outbound={'s1': [self.change('a.txt', b'x abc\n')]})], manifest)
        original = client.call

        def save_during_apply(method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
            if method == 'sync.mac.apply':
                (self.root / 'a.txt').write_text('x abcd\n')
            return original(method, params, **options)
        setattr(client, 'call', save_during_apply)
        project.round()
        self.assertEqual((self.root / 'a.txt').read_text(), 'x abcd\n')
        entry = self.saved(project, 'manifest.json')['a.txt']
        self.assertEqual((entry['h'], entry['b']), (sha(b'x abc\n'), 'm2'))
        client.replies.append(reply(main='m3', applied=['a.txt'], merge='merged'))
        project.round()
        sent = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual(sent, [{'path': 'a.txt', 'kind': 'file', 'base': 'm2'}])

    def test_a_mac_deletion_against_a_main_edit_is_a_conflict_that_stays(self) -> None:
        manifest = {'a.txt': self.agreed('a.txt', b'agreed\n'), 'b.txt': self.agreed('b.txt', b'b\n')}
        (self.root / 'a.txt').unlink()
        outbound = {'s1': [self.change('a.txt', b'agent edit\n')]}
        client, project = self.project([reply(main='m2', conflicts=['a.txt'], outbound=outbound),
                                        reply(main='m2', outbound={'s1': outbound['s1']})], manifest,
                                       {('m2', 'a.txt'): b'agent edit\n'})
        first = project.round()
        self.assertEqual(first['conflicts'], ['a.txt'])
        self.assertEqual(self.saved(project, 'held.json')['a.txt'], {'h': 'absent', 'why': 'conflict'})
        second = project.round(set())
        self.assertFalse((self.root / 'a.txt').exists())
        self.assertEqual(second['conflicts'], ['a.txt'])
        entries = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual(entries, [])

    def test_many_main_deletions_wait_for_a_confirmation(self) -> None:
        manifest = {f'f{index}.txt': self.agreed(f'f{index}.txt', str(index).encode()) for index in range(10)}
        outbound = {'s1': [self.change(f'f{index}.txt', None) for index in range(5)]}
        _, project = self.project([reply(main='m2', outbound=outbound), reply(main='m2', outbound=outbound)],
                                  manifest)
        status = project.round()
        self.assertEqual((status['state'], status['deletes']), ('paused', 5))
        self.assertIn('deleted in the VM', status['reason'])
        self.assertEqual(len(list(self.root.iterdir())), 10)
        project.allow_deletes()
        done = project.round()
        self.assertEqual((done['state'], len(list(self.root.iterdir()))), ('synced', 5))

    def test_executable_bits_sync_both_ways(self) -> None:
        manifest = {'run.sh': self.agreed('run.sh', b'#!/bin/sh\n'), 'tool.sh': self.agreed('tool.sh', b'#!/bin/sh\n')}
        (self.root / 'tool.sh').chmod(0o755)
        outbound = {'s1': [self.change('run.sh', b'#!/bin/sh\n', 0o755)]}
        client, project = self.project([reply(main='m2', applied=['tool.sh'], merge='merged', outbound=outbound)],
                                       manifest)
        project.round()
        self.assertEqual((self.root / 'run.sh').stat().st_mode & 0o777, 0o755)
        entries = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual(entries, [{'path': 'tool.sh', 'kind': 'file', 'base': 's1', 'modeChanged': True}])

    def test_a_file_too_large_to_send_is_not_a_deletion(self) -> None:
        manifest = {'big.bin': self.agreed('big.bin', b'small')}
        (self.root / 'big.bin').write_bytes(b'now much larger')
        client, project = self.project([reply(main='m2')], manifest)
        with patch.object(sync, 'MAX_FILE', 8):
            status = project.round()
        entries = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual(entries, [])
        self.assertEqual(status['notices'], ['big.bin: files over 256 MiB do not sync'])

    def test_links_to_any_target_sync_as_links(self) -> None:
        (self.root / 'outside').symlink_to('/etc')
        (self.root / 'up').symlink_to('../elsewhere')
        client, project = self.project([reply(main='m2', applied=['outside', 'up'], merge='merged')], {})
        project.round()
        entries = [params['entries'] for method, params, _ in client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual(sorted((item['path'], item['kind']) for item in entries), [('outside', 'link'), ('up', 'link')])
        written = sync.write_mac(self.root, 'up', 'link', b'../other', 0)
        assert written is not None
        self.assertEqual(written['k'], 'link')
        self.assertEqual(os.readlink(self.root / 'up'), '../other')

    def test_a_refused_operation_is_not_repeated(self) -> None:
        manifest = {'a.txt': self.agreed('a.txt', b'agreed\n')}
        (self.root / 'a.txt').write_text('mac\n')
        refused = RuntimeError('Linux guest sync.mac.apply: invalid')
        setattr(refused, 'uncertain', False)
        client, project = self.project([refused, reply(main='m2', applied=['a.txt'], merge='merged')], manifest)
        with self.assertRaises(RuntimeError):
            project.round()
        self.assertFalse((project.directory / 'pending.json').exists())
        project.round()
        operations = [request for method, _params, request in client.calls if method == 'sync.mac.apply']
        self.assertNotEqual(operations[0], operations[1])


class Choices(unittest.TestCase):
    """A conflict waits until the user keeps the Mac version, keeps the VM version, or
    asks the lead to combine them."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.root, self.state = base / 'project', base / 'state'
        self.root.mkdir()
        self.state.mkdir()
        (self.root / 'app.txt').write_text('mac\n')
        self.client = FakeClient(self.state, [], {('m9', 'app.txt'): b'vm\n'})
        self.project = sync.ProjectSync(self.client, 'p1', self.root, import_state='s1')
        setattr(self.project, 'send_archive', lambda archive, begin, operation: None)
        self.project.directory.mkdir(parents=True)
        agreed = {'k': 'file', 's': 7, 'm': 0, 'x': 0o644, 'h': sha(b'agreed\n'), 'b': 's1'}
        (self.project.directory / 'manifest.json').write_text(json.dumps({'app.txt': agreed}))
        (self.project.directory / 'held.json').write_text(json.dumps({'app.txt': {'h': sha(b'mac\n'), 'why': 'conflict'}}))
        self.project.status(state='conflict', conflicts=['app.txt'], mainStateId='m9')

    def tearDown(self) -> None:
        self.temp.cleanup()

    def saved(self, name: str) -> Any:
        return json.loads((self.project.directory / name).read_text())

    def test_keep_vm_writes_main_version_to_the_mac(self) -> None:
        result = self.project.resolve('app.txt', 'vm')
        self.assertEqual((self.root / 'app.txt').read_text(), 'vm\n')
        self.assertEqual(self.saved('held.json'), {})
        self.assertEqual((self.saved('manifest.json')['app.txt']['h'], self.saved('manifest.json')['app.txt']['b']),
                         (sha(b'vm\n'), 'm9'))
        self.assertEqual((result['mainStateId'], self.saved('status.json')['state']), ('m9', 'synced'))

    def test_keep_mac_sends_the_mac_version_on_top_of_main(self) -> None:
        self.client.replies.append(reply(main='m10', applied=['app.txt'], merge='merged'))
        self.project.resolve('app.txt', 'mac')
        self.assertEqual(self.saved('status.json')['rescan'], ['app.txt'])
        # The scheduler's next round reads the path again without a full scan.
        self.project.round(set())
        entries = next(params['entries'] for method, params, _ in self.client.calls if method == 'sync.mac.apply')
        self.assertEqual(entries, [{'path': 'app.txt', 'kind': 'file', 'base': 'm9'}])
        self.assertEqual((self.root / 'app.txt').read_text(), 'mac\n')
        manifest = self.saved('manifest.json')['app.txt']
        self.assertEqual((manifest['h'], manifest['b']), (sha(b'mac\n'), 'm10'))
        self.assertEqual(self.saved('status.json')['state'], 'synced')

    def test_ask_the_lead_and_take_main_after_the_merge(self) -> None:
        result = self.project.resolve('app.txt', 'agent')
        handoff = next(params for method, params, _ in self.client.calls if method == 'sync.mac.handoff')
        self.assertEqual((handoff['path'], handoff['kind']), ('app.txt', 'file'))
        self.assertIn('upload', handoff)
        self.assertEqual((result['line'], result['stateId'], result['kind']), ('mac-conflict', 'h1', 'file'))
        self.assertEqual(self.saved('held.json')['app.txt'],
                         {'h': sha(b'mac\n'), 'why': 'agent', 'base': 'm9', 'stateId': 'h1'})
        # A repeated request after a lost reply makes no second handoff.
        again = self.project.resolve('app.txt', 'agent')
        self.assertEqual(again['stateId'], 'h1')
        self.assertEqual(len([call for call in self.client.calls if call[0] == 'sync.mac.handoff']), 1)
        status = self.saved('status.json')
        self.assertEqual((status['state'], status['withAgent'], status['conflicts']), ('waiting', ['app.txt'], []))
        # A quiet round leaves the Mac file alone while the lead works.
        self.client.replies.append(reply(main='m9', outbound={'s1': [], 'm9': []}))
        quiet = self.project.round(set())
        self.assertEqual((quiet['state'], quiet['sent']), ('waiting', 0))
        # The lead commits the combined file: the Mac takes main's version.
        self.client.files[('m10', 'app.txt')] = b'combined\n'
        change = {'path': 'app.txt', 'kind': 'file', 'mode': 0o644, 'size': 9, 'sha256': sha(b'combined\n')}
        self.client.replies.append(reply(main='m10', merge='none', outbound={'s1': [change], 'm9': [change]}))
        done = self.project.round(set())
        since = [params['since'] for method, params, _ in self.client.calls if method == 'sync.mac.apply'][-1]
        self.assertIn('m9', since)
        self.assertEqual((self.root / 'app.txt').read_text(), 'combined\n')
        self.assertEqual((done['state'], done['withAgent']), ('synced', []))
        self.assertEqual(self.saved('held.json'), {})

    def test_a_lost_handoff_reply_repeats_the_same_operation_and_archive(self) -> None:
        archives: list[bytes] = []
        setattr(self.project, 'send_archive', lambda archive, begin, operation: archives.append(archive.read_bytes()))
        original = self.client.call

        def lose_first_reply(method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
            reply = original(method, params, **options)
            if method == 'sync.mac.handoff' and len(archives) == 1:
                raise TimeoutError('lost reply')
            return reply
        setattr(self.client, 'call', lose_first_reply)
        with self.assertRaises(TimeoutError):
            self.project.resolve('app.txt', 'agent')
        self.project.resolve('app.txt', 'agent')
        operations = [request for method, _params, request in self.client.calls if method == 'sync.mac.handoff']
        self.assertEqual(len(operations), 2)
        self.assertEqual(operations[0], operations[1])
        self.assertEqual(archives[0], archives[1])

    def test_a_new_mac_edit_ends_the_handoff_and_syncs_as_usual(self) -> None:
        self.project.resolve('app.txt', 'agent')
        (self.root / 'app.txt').write_text('mac again\n')
        self.client.replies.append(reply(main='m9', conflicts=['app.txt'], outbound={'s1': []}))
        status = self.project.round()
        entries = [params['entries'] for method, params, _ in self.client.calls if method == 'sync.mac.apply'][-1]
        self.assertEqual([item['path'] for item in entries], ['app.txt'])
        self.assertEqual((status['conflicts'], status['withAgent']), (['app.txt'], []))

    def test_a_path_without_a_conflict_has_nothing_to_settle(self) -> None:
        for choice, error in (('vm', 'no conflict'), ('both', 'Choose')):
            with self.subTest(choice=choice), self.assertRaisesRegex(ValueError, error):
                self.project.resolve('other.txt' if choice == 'vm' else 'app.txt', choice)

    def test_the_lead_gets_one_message_with_the_mac_state(self) -> None:
        sent: list[tuple[str, str, str]] = []

        class Runtime:
            lock = __import__('threading').Lock()

            def db(self) -> Any:
                return __import__('contextlib').nullcontext()

            def records(self, _db: Any, table: str) -> list[dict[str, Any]]:
                return [{'id': 'old', 'isLead': True, 'layrProjectId': 'p1', 'layrLine': 'main', 'updatedAt': 1},
                        {'id': 'lead', 'isLead': True, 'layrProjectId': 'p1', 'layrLine': 'main', 'updatedAt': 5},
                        {'id': 'gone', 'isLead': True, 'layrProjectId': 'p1', 'layrLine': 'main', 'updatedAt': 9,
                         'deletedAt': 9},
                        {'id': 'other', 'isLead': True, 'layrProjectId': 'p2', 'layrLine': 'main', 'updatedAt': 9},
                        {'id': 'worker', 'layrProjectId': 'p1', 'layrLine': 'w', 'updatedAt': 9}]

            def send(self, key: str, text: str, message_id: str, **_: Any) -> None:
                sent.append((key, text, message_id))

        record = self.state / 'layr-projects/p1'
        (record / 'project.json').write_text(json.dumps({'projectId': 'p1', 'source': str(self.root),
                                                          'importStateId': 's1'}))
        with patch('codex_linux_workspaces.client', return_value=self.client), \
                patch.object(sync.ProjectSync, 'send_archive', lambda *args: None), \
                patch.object(sync, '_refresh') as refresh:
            result = sync.resolve(Runtime(), 'p1', 'app.txt', 'agent')
        refresh.assert_called_once()
        self.assertEqual(result['leadId'], 'lead')
        self.assertEqual(result['status']['withAgent'], ['app.txt'])
        self.assertEqual(len(sent), 1)
        self.assertIn('layr show h1:app.txt', sent[0][1])
        self.assertTrue(sent[0][2].startswith('mac-conflict:h1:'))

    def test_pause_and_resume(self) -> None:
        record = self.state / 'layr-projects/p1'
        (record / 'project.json').write_text(json.dumps({'projectId': 'p1', 'source': str(self.root)}))
        self.assertEqual(len(sync.synced_projects(self.state)), 1)
        sync.set_enabled(self.state, 'p1', False)
        self.assertEqual(sync.synced_projects(self.state), [])
        paused = sync.sync_status(self.state, 'p1')
        assert paused is not None
        self.assertEqual((paused['state'], paused['enabled']), ('paused', False))
        sync.set_enabled(self.state, 'p1', True)
        self.assertEqual(len(sync.synced_projects(self.state)), 1)
        self.assertIsNone(sync.sync_status(self.state, 'missing'))
        with self.assertRaises(ValueError):
            sync.set_enabled(self.state, 'missing', False)


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
            with patch('platform.system', return_value='Darwin'), patch.object(sync, '_refresh'), \
                    patch('codex_linux_workspaces.client', return_value=client):
                sync.tick(runtime)
                self.assertFalse((folder / 'sync/status.json').exists())
                client.running = True
                runtime._mac_sync_at = 0.0
                sync.tick(runtime)
            status = json.loads((folder / 'sync/status.json').read_text())
            self.assertEqual((status['state'], status['reason']), ('error', 'guest refused'))



class FileEvents(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_a_partial_scan_reads_only_the_named_paths(self) -> None:
        for rel in ('keep.txt', 'gone.txt', 'edit.txt'):
            (self.root / rel).write_text(rel)
        rules = sync.Rules(self.root)
        full, _ = sync.scan(self.root, rules, {})
        manifest = {rel: {**entry, 'b': 's1'} for rel, entry in full.items()}
        (self.root / 'gone.txt').unlink()
        (self.root / 'edit.txt').write_text('edited')
        (self.root / 'keep.txt').write_text('changed but not named')
        (self.root / 'new/deep').mkdir(parents=True)
        (self.root / 'new/deep/file.txt').write_text('new')
        current, _ = sync.scan_paths(self.root, rules, manifest, {'gone.txt', 'edit.txt', 'new'})
        self.assertEqual(set(current), {'keep.txt', 'edit.txt', 'new/deep/file.txt'})
        self.assertEqual(current['keep.txt']['h'], manifest['keep.txt']['h'])
        self.assertEqual(current['edit.txt']['h'], sha(b'edited'))
        self.assertNotIn('b', current['keep.txt'])
        nothing, _ = sync.scan_paths(self.root, rules, manifest, set())
        self.assertEqual(set(nothing), set(manifest))

    def test_the_watcher_collects_paths_per_folder_and_asks_for_rescans(self) -> None:
        other = Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(lambda: __import__('shutil').rmtree(other, True))
        helper = self.root / 'helper'
        helper.write_text('#!/bin/sh\n'
                          f'echo \'{{"ready": ["{self.root}"]}}\'\n'
                          'sleep 1\n'
                          f'echo \'{{"path": "{self.root}/a.txt"}}\'\n'
                          f'echo \'{{"path": "{self.root}/sub/b.txt"}}\'\n'
                          f'echo \'{{"rescan": "{other}"}}\'\n'
                          'cat > /dev/null\n')
        helper.chmod(0o755)
        watcher = sync.Watcher(helper)
        with patch.object(sync, 'QUIET', 0.0):
            watcher.watch([self.root, other])
            self.assertTrue(watcher.available())
            # The first take after a start scans in full: nothing was watched before.
            self.assertEqual(watcher.take(self.root), (True, None))
            import time as clock
            deadline = clock.monotonic() + 5
            while clock.monotonic() < deadline and not watcher.pending.get(self.root):
                clock.sleep(0.05)
            clock.sleep(0.2)
            self.assertEqual(watcher.take(self.root), (True, {'a.txt', 'sub/b.txt'}))
            self.assertEqual(watcher.take(other), (True, None))
            self.assertEqual(watcher.take(self.root), (False, set()))
        watcher.stop()
        self.assertFalse(watcher.available())

    @unittest.skipUnless(sys.platform == 'darwin' and shutil.which('xcrun'), 'needs macOS and Swift')
    def test_the_signed_helper_reports_file_events(self) -> None:
        from codex_layout import DESKTOP_ROOT
        helper = self.root / 'studio-fs-events'
        subprocess.run(['xcrun', 'swiftc', str(DESKTOP_ROOT / 'native/fs-events.swift'), '-O',
                        '-target', 'arm64-apple-macos13.0', '-o', str(helper)], check=True, timeout=300)
        folder = self.root / 'project'
        folder.mkdir()
        watcher = sync.Watcher(helper)
        with patch.object(sync, 'QUIET', 0.3):
            watcher.watch([folder])
            watcher.take(folder)
            import time as clock
            clock.sleep(1.0)
            (folder / 'sub').mkdir()
            (folder / 'sub/new.txt').write_text('x')
            deadline, found = clock.monotonic() + 10, set()
            while clock.monotonic() < deadline and 'sub/new.txt' not in found:
                clock.sleep(0.4)
                changed, paths = watcher.take(folder)
                if changed and paths is not None:
                    found |= paths
        watcher.stop()
        self.assertIn('sub/new.txt', found)


class Schedule(unittest.TestCase):
    def test_events_main_changes_and_quiet_projects(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            state, folder = Path(name) / 'state', (Path(name) / 'project').resolve()
            folder.mkdir()
            record = state / 'layr-projects/p'
            record.mkdir(parents=True)
            (record / 'project.json').write_text(json.dumps({'projectId': 'p', 'source': str(folder)}))
            rounds: list[Any] = []
            heads = ['s1']

            class Client:
                state_dir = state

                def status(self) -> dict[str, Any]:
                    return {'state': 'running'}

                def call(self, method: str, params: dict[str, Any], **_: Any) -> dict[str, Any]:
                    assert method == 'sync.mac.head'
                    return {'mainStateId': heads[0]}

            class Runtime:
                def __init__(self) -> None:
                    self.pool = self

                def submit(self, function: Any) -> None:
                    function()

            class Events:
                queued: tuple[bool, set[str] | None] = (False, set())

                def watch(self, roots: list[Path]) -> None:
                    pass

                def available(self) -> bool:
                    return True

                def take(self, root: Path) -> tuple[bool, set[str] | None]:
                    value, self.queued = self.queued, (False, set())
                    return value

            def fake_round(project: Any, paths: Any = None) -> dict[str, Any]:
                rounds.append(paths)
                project.status(state='synced', mainStateId=heads[0])
                return {}

            runtime, events = Runtime(), Events()
            setattr(runtime, '_mac_sync_watcher', events)
            with patch('platform.system', return_value='Darwin'), \
                    patch('codex_linux_workspaces.client', return_value=Client()), \
                    patch.object(sync, '_refresh') as refresh, \
                    patch.object(sync.ProjectSync, 'round', fake_round), \
                    patch('codex_vm_mac_sync.time.monotonic', side_effect=[1000.0 + n * 20 for n in range(20)]):
                sync.tick(runtime)                  # first: a full scan
                events.queued = (True, {'a.txt'})
                sync.tick(runtime)                  # file events: only their paths
                sync.tick(runtime)                  # main unchanged and synced: nothing
                heads[0] = 's2'
                sync.tick(runtime)                  # main moved: a round without a Mac scan
                (record / 'sync/allow-deletes.json').write_text('{"count": 9}')
                sync.tick(runtime)                  # confirmed Mac deletions: a full scan at once
            self.assertEqual(rounds, [None, {'a.txt'}, set(), None])
            # Only the first report changes what the sidebar shows; later rounds stay synced.
            self.assertEqual(refresh.call_count, 1)


if __name__ == '__main__':
    unittest.main()
