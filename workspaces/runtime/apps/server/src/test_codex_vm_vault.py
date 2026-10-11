"""The project vault in the sidebar without a VM (docs/vault.md): the host records of the VM
writer's report, and pulls through a stand-in layr-desk command."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import json
import os
from pathlib import Path
import stat
import tempfile
import time
import unittest
from typing import Any
from unittest.mock import patch

import codex_vm_vault as vault
from codex_linux_vm import LinuxVMError


class FakeClient:
    """Answers vault.status from a table: a dict, or an error to raise."""

    def __init__(self, state_dir: Path, replies: dict[str, Any]):
        self.state_dir, self.replies = state_dir, replies
        self.calls: list[str] = []

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        assert method == 'vault.status'
        self.calls.append(params['projectId'])
        reply = self.replies[params['projectId']]
        if isinstance(reply, Exception):
            raise reply
        return dict(reply)


def project(state: Path, name: str) -> Path:
    folder = state / 'layr-projects' / name
    folder.mkdir(parents=True)
    (folder / 'project.json').write_text(json.dumps({'projectId': name}))
    return folder


class Reports(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_a_check_keeps_each_writer_report_and_says_when_a_view_changed(self) -> None:
        for name in ('ok', 'failed', 'plain', 'down'):
            project(self.state, name)
        client = FakeClient(self.state, {
            'ok': {'prefix': 'ok', 'lastPush': 1791720000, 'lastAttempt': 1791720600, 'lastError': None},
            'failed': {'prefix': 'failed', 'lastPush': None, 'lastError': 'S3 said 403'},
            'plain': LinuxVMError('The project has no vault', code='not_found'),
            'down': LinuxVMError('the VM does not answer', code='unavailable'),
        })
        self.assertIsNone(vault.vault_view(self.state, 'ok'))
        self.assertTrue(vault.check(client))
        self.assertEqual(vault.vault_view(self.state, 'ok'),
                         {'projectId': 'ok', 'state': 'ok', 'prefix': 'ok', 'lastPush': 1791720000})
        self.assertEqual(vault.vault_view(self.state, 'failed'),
                         {'projectId': 'failed', 'state': 'error', 'prefix': 'failed', 'reason': 'S3 said 403'})
        self.assertEqual(vault.vault_view(self.state, 'plain'), {'projectId': 'plain', 'state': 'none'})
        # A VM that cannot answer now leaves no report, not a false "none".
        self.assertIsNone(vault.vault_view(self.state, 'down'))
        # Only the time of an attempt changed: the view is the same, so no refresh.
        client.replies['ok'] = {**client.replies['ok'], 'lastAttempt': 1791721200}
        self.assertFalse(vault.check(client))
        client.replies['ok'] = {**client.replies['ok'], 'lastPush': 1791721200}
        self.assertTrue(vault.check(client))

    def test_the_desk_link_needs_an_https_site(self) -> None:
        folder = project(self.state, 'p')
        (folder / 'vault.json').write_text(json.dumps({'configured': True, 'prefix': 'p'}))
        vault.store_site(self.state, 'p', 'https://desk.example.com')
        view = vault.vault_view(self.state, 'p')
        assert view is not None
        self.assertEqual(view['site'], 'https://desk.example.com/')
        for bad in ('http://desk.example.com/', 'https://desk example/', 'file:///etc'):
            with self.assertRaises(ValueError):
                vault.store_site(self.state, 'p', bad)


class Runtime:
    def __init__(self) -> None:
        self.pool = ThreadPoolExecutor(max_workers=2)


class Pulls(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name) / 'state'
        self.downloads = Path(self.temp.name) / 'Downloads'
        self.downloads.mkdir()
        folder = project(self.state, 'p')
        (folder / 'vault.json').write_text(json.dumps({'configured': True, 'prefix': 'proj'}))
        vault.store_site(self.state, 'p', 'https://desk.example.com/')
        # A stand-in layr-desk: it counts its runs and makes the folder that --to names.
        self.runs = Path(self.temp.name) / 'runs'
        self.tool = Path(self.temp.name) / 'layr-desk'
        self.tool.write_text(
            '#!/bin/sh\n'
            f'echo "$@" >> {self.runs}\n'
            'while [ "$1" != "--to" ]; do shift; done\n'
            'mkdir "$2" && echo \'{"seq": 7, "state": "s7", "files": 3, "bytes": 42}\'\n')
        self.tool.chmod(self.tool.stat().st_mode | stat.S_IXUSR)
        self.runtime = Runtime()
        client = type('Client', (), {'state_dir': self.state})()
        self.patches = ExitStack()
        self.patches.enter_context(patch.dict(os.environ, {'STUDIO_LAYR_DESK_BIN': str(self.tool)}))
        self.patches.enter_context(patch('codex_linux_workspaces.client', lambda runtime: client))

    def tearDown(self) -> None:
        self.patches.close()
        self.runtime.pool.shutdown(wait=True)
        self.temp.cleanup()

    def wait(self, request_id: str) -> dict[str, Any]:
        for _ in range(200):
            job = vault.start_pull(self.runtime, 'p', request_id, str(self.downloads))
            if job['state'] != 'running':
                return job
            time.sleep(0.02)
        raise AssertionError('the pull did not end')

    def test_a_repeated_request_reports_the_same_pull(self) -> None:
        first = vault.start_pull(self.runtime, 'p', 'pull-1', str(self.downloads))
        self.assertTrue(first['folder'].startswith(str(self.downloads / 'proj ')))
        done = self.wait('pull-1')
        self.assertEqual((done['state'], done['seq'], done['files'], done['folder']), ('done', 7, 3, first['folder']))
        self.assertEqual(len(self.runs.read_text().splitlines()), 1)
        args = self.runs.read_text().split()
        self.assertEqual(args[:5], ['pull', '--site', 'https://desk.example.com/', '--project', 'proj'])
        # Another request pulls again, into another new folder.
        other = self.wait('pull-2')
        self.assertNotEqual(other['folder'], first['folder'])
        self.assertEqual(len(self.runs.read_text().splitlines()), 2)

    def test_a_failed_pull_says_why(self) -> None:
        self.tool.write_text('#!/bin/sh\necho "Error: the vault proj is locked on this Mac" >&2\nexit 1\n')
        failed = self.wait('pull-3')
        self.assertEqual((failed['state'], failed['error']), ('failed', 'the vault proj is locked on this Mac'))

    def test_a_pull_needs_a_vault_a_site_and_a_folder(self) -> None:
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-4', str(self.downloads / 'missing'))
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-5', 'relative/folder')
        (self.state / 'layr-projects/p/vault-site.json').unlink()
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-6', str(self.downloads))
        (self.state / 'layr-projects/p/vault.json').write_text(json.dumps({'configured': False}))
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-7', str(self.downloads))
        self.assertFalse(self.runs.exists())


if __name__ == '__main__':
    unittest.main()
