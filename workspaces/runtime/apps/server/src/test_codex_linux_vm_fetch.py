"""A fake guest exports a real Git bundle into a separate host repository."""
import base64
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid
from unittest.mock import patch

from codex_linux_vm_fetch import fetch


class BundleGuest:
    def __init__(self, repo):
        self.repo = repo
        self.calls = []
        self.exit_code = 0

    def stream(self, method, params, **options):
        self.calls.append((method, params, options))
        result = subprocess.run(params['argv'], cwd=self.repo, capture_output=True, timeout=10)
        data = result.stdout
        for offset in range(0, len(data), 57):
            yield {'event': 'output', 'data': {'stream': 'stdout',
                   'data': base64.b64encode(data[offset:offset + 57]).decode()}}
        yield {'result': {'exitCode': self.exit_code or result.returncode}}

    def request(self, method, params, **options):
        path = Path(params['path'])
        data = path.read_bytes()
        if method == 'file.stat':
            return {'path': str(path), 'bytes': len(data), 'token': 'fixture',
                    'sha256': hashlib.sha256(data).hexdigest()}
        if method == 'file.read':
            offset = params['offset']
            chunk = data[offset:offset + min(params['maxBytes'], 57)]
            return {'offset': offset, 'nextOffset': offset + len(chunk),
                    'data': base64.b64encode(chunk).decode(), 'sha256': hashlib.sha256(chunk).hexdigest()}
        raise AssertionError(method)


class FetchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='linux-vm-fetch-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.guest = self.root / 'guest'
        self.host = self.root / 'host'
        self.guest.mkdir()
        self.host.mkdir()
        for repo in (self.guest, self.host):
            self.git(repo, 'init', '-q', '-b', 'main')
            self.git(repo, 'config', 'user.name', 'Fixture')
            self.git(repo, 'config', 'user.email', 'fixture@example.test')
        (self.guest / 'source').write_text('guest commit')
        self.git(self.guest, 'add', 'source')
        self.git(self.guest, 'commit', '-qm', 'Fixture')
        self.client = BundleGuest(self.guest)
        self.agent_id = str(uuid.uuid4())

    def git(self, repo, *arguments):
        return subprocess.run(['git', '-C', str(repo), *arguments], check=True,
                              capture_output=True, text=True, timeout=10).stdout.strip()

    def test_real_fetch_reads_the_exact_guest_commit_and_keeps_host_files(self):
        (self.host / 'untracked').write_text('keep')
        with patch.dict(os.environ, {'CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES': '0'}):
            result = fetch(self.client, self.agent_id, '/guest/worker/repo', 'main', self.host,
                           request_id='fetch-fixture')
        self.assertEqual(self.git(self.host, 'rev-parse', 'FETCH_HEAD'), self.git(self.guest, 'rev-parse', 'HEAD'))
        self.assertEqual((self.host / 'untracked').read_text(), 'keep')
        self.assertFalse((self.host / 'source').exists())
        self.assertEqual(result['requestId'], 'fetch-fixture')
        self.assertEqual(self.client.calls[0][1]['agentId'], self.agent_id)
        self.assertEqual(self.client.calls[0][2]['request_id'], 'fetch-fixture')

    def test_guest_failure_does_not_write_fetch_head(self):
        self.client.exit_code = 1
        with patch.dict(os.environ, {'CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES': '0'}):
            with self.assertRaisesRegex(RuntimeError, 'could not export'):
                fetch(self.client, self.agent_id, '/guest/repo', 'main', self.host)
        self.assertFalse((self.host / '.git/FETCH_HEAD').exists())
        self.assertEqual(len(self.client.calls), 1)

    def test_invalid_branch_is_rejected_before_a_guest_request(self):
        with self.assertRaises(ValueError):
            fetch(self.client, self.agent_id, '/guest/repo', '--all', self.host)
        self.assertEqual(self.client.calls, [])


if __name__ == '__main__':
    unittest.main()
