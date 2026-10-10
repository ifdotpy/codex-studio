"""Durable source upload contracts through the Studio lifecycle caller."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
import os
from unittest.mock import patch

from codex_linux_workspaces import build_base, guest_root


class Guest:
    def __init__(self):
        self.calls = []
        self.receipts = {}
        self.uploads = {}
        self.fail_commit_once = False
        self.commits = 0

    def ensure_running(self):
        pass

    def request(self, method, params, *, request_id, timeout):
        self.calls.append((method, params, request_id))
        if request_id in self.receipts:
            return self.receipts[request_id]
        if method == 'upload.begin':
            self.uploads[params['uploadId']] = {'begin': params, 'chunks': {}}
            result = {'state': 'receiving'}
        elif method == 'upload.chunk':
            self.uploads[params['uploadId']]['chunks'][params['seq']] = base64.b64decode(params['data'])
            result = {'nextSeq': params['seq'] + 1}
        elif method == 'upload.commit':
            upload = self.uploads[params['uploadId']]
            data = b''.join(upload['chunks'][n] for n in sorted(upload['chunks']))
            assert len(data) == upload['begin']['totalBytes']
            assert hashlib.sha256(data).hexdigest() == upload['begin']['sha256']
            self.commits += 1
            result = {'state': 'applied'}
        elif method == 'workspace.startBase':
            result = {'state': 'ready', 'version': 'fixture'}
        else:
            raise AssertionError(method)
        self.receipts[request_id] = result
        if method == 'upload.commit' and self.fail_commit_once:
            self.fail_commit_once = False
            raise ConnectionError('The commit receipt was lost')
        return result


class WorkspaceLifecycle(unittest.TestCase):
    def setUp(self):
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"CODEX_WORKSPACE_MIN_FREE_BYTES": "0"}).start()
        self.temp = tempfile.TemporaryDirectory(prefix='linux-workspace-caller-')
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.repo)], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'config', 'user.name', 'Fixture'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'config', 'user.email', 'fixture@example.test'], check=True)
        (self.repo / 'one').write_text('one')
        subprocess.run(['git', '-C', str(self.repo), 'add', '.'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'commit', '-qm', 'base'], check=True)
        self.guest = Guest()
        self.runtime = SimpleNamespace(root=self.root / 'state', _linux_vm_client=self.guest)

    def tearDown(self):
        self.temp.cleanup()

    def test_response_loss_preserves_upload_and_archive_identity(self):
        self.guest.fail_commit_once = True
        with self.assertRaises(ConnectionError):
            build_base(self.runtime, self.repo)
        original = next(params for method, params, _ in self.guest.calls if method == 'upload.begin')
        # A resumed operation uses its saved tree, even when the host changes.
        (self.repo / 'one').write_text('changed after uncertain commit')
        self.assertEqual(build_base(self.runtime, self.repo)['state'], 'ready')
        begins = [params for method, params, _ in self.guest.calls if method == 'upload.begin']
        self.assertEqual(begins, [original, original])
        self.assertEqual(self.guest.commits, 1)
        self.assertEqual(original['root'], guest_root(self.repo))
        self.assertFalse(list(self.runtime.root.rglob('source.tar.gz')))
        build_base(self.runtime, self.repo)
        latest = [params for method, params, _ in self.guest.calls if method == 'upload.begin'][-1]
        self.assertEqual(latest['mode'], 'delta')
        self.assertNotEqual(latest['uploadId'], original['uploadId'])

    def test_delta_uses_same_guest_root_and_explicit_deletions(self):
        build_base(self.runtime, self.repo)
        (self.repo / 'one').unlink()
        (self.repo / 'new').write_text('new')
        build_base(self.runtime, self.repo)
        begins = [params for method, params, _ in self.guest.calls if method == 'upload.begin']
        self.assertEqual([row['mode'] for row in begins], ['full', 'delta'])
        self.assertEqual(begins[0]['root'], begins[1]['root'])
        self.assertIn('one', begins[1]['deletePaths'])


if __name__ == '__main__':
    unittest.main()
