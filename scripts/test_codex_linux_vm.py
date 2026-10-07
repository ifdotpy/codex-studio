"""Linux VM client contracts without a VM or account credentials."""
import fcntl
import gzip
import io
import json
from pathlib import Path
import socket
import tarfile
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import codex_linux_vm as vm


class Guest:
    def __init__(self, directory, handler):
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(directory / 'guest.sock'))
        self.listener.listen(2)
        self.listener.settimeout(2)
        self.handler = handler
        self.error = None
        self.thread = threading.Thread(target=self.run)
        self.thread.start()

    def run(self):
        try:
            with self.listener.accept()[0] as channel:
                line = channel.makefile('rb').readline()
                self.handler(channel, json.loads(line))
        except Exception as exc:
            self.error = exc
        finally:
            self.listener.close()

    def finish(self):
        self.thread.join(3)
        if self.thread.is_alive():
            raise AssertionError('Guest fixture did not stop')
        if self.error:
            raise self.error


class ClientTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='vm-client-')
        self.directory = Path(self.temporary.name)
        self.client = vm.Client(self.directory)

    def tearDown(self):
        self.temporary.cleanup()

    def test_stream_and_result_preserve_identity(self):
        def handler(channel, request):
            self.assertEqual(request['id'], 'same-operation')
            frames = [{'id': request['id'], 'event': 'stdout', 'data': 'hello'},
                      {'id': request['id'], 'result': {'exitCode': 0}}]
            channel.sendall(b''.join(json.dumps(frame).encode() + b'\n' for frame in frames))
        guest = Guest(self.directory, handler)
        frames = list(self.client.stream('exec', {'argv': ['true']}, request_id='same-operation'))
        guest.finish()
        self.assertEqual(frames[0]['data'], 'hello')
        self.assertEqual(frames[-1]['result']['exitCode'], 0)

    def test_lost_mutation_response_is_uncertain_without_retry(self):
        applied = []
        guest = Guest(self.directory, lambda channel, request: applied.append(request['id']))
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('workspace.remove', {}, request_id='remove-once')
        guest.finish()
        self.assertTrue(error.exception.uncertain)
        self.assertEqual(applied, ['remove-once'])

    def test_guest_unknown_outcome_remains_uncertain(self):
        def handler(channel, request):
            channel.sendall(json.dumps({'id': request['id'], 'error': {'code': 'outcome_unknown', 'message': 'No receipt'}}).encode() + b'\n')
        guest = Guest(self.directory, handler)
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('provider.start')
        guest.finish()
        self.assertTrue(error.exception.uncertain)

    def test_wrong_identity_is_uncertain(self):
        guest = Guest(self.directory, lambda channel, request: channel.sendall(b'{"id":"wrong","result":true}\n'))
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('health')
        guest.finish()
        self.assertTrue(error.exception.uncertain)

    def test_response_deadline_is_bounded(self):
        guest = Guest(self.directory, lambda channel, request: time.sleep(0.3))
        started = time.monotonic()
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('health', timeout=0.05)
        self.assertLess(time.monotonic() - started, 0.2)
        self.assertTrue(error.exception.uncertain)
        guest.finish()

    def test_missing_socket_is_certain(self):
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('provider.start')
        self.assertFalse(error.exception.uncertain)

    def test_stale_socket_cannot_replace_owned_helper(self):
        (self.directory / 'control.sock').touch()
        with (self.directory / 'host.lock').open('a') as lease:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(vm.LinuxVMError, 'owns its lease'):
                self.client.status()

    def test_invalid_json_response_preserves_uncertainty(self):
        guest = Guest(self.directory, lambda channel, request: channel.sendall(b'not-json\n'))
        with self.assertRaises(vm.LinuxVMError) as error:
            self.client.request('workspace.archive', {'agentId': 'one'})
        guest.finish()
        self.assertTrue(error.exception.uncertain)

    def test_default_path_preserves_xdg_state_identity(self):
        with patch.dict('os.environ', {'XDG_STATE_HOME': '/tmp/studio-xdg-test'}, clear=True):
            self.assertEqual(vm.Client().state_dir, Path('/tmp/studio-xdg-test/codex-agents/linux-vm').resolve())

    def test_settings_reject_boolean_negative_and_oversized_disk(self):
        for settings in [vm.Settings(cpus=True), vm.Settings(memoryBytes=-1), vm.Settings(dataDiskBytes=2048 * vm._GIB)]:
            with self.assertRaises(vm.LinuxVMError):
                settings.validate()

    def test_sparse_archive_rejects_unsafe_format_and_bound(self):
        archive = self.directory / 'disk.tar.gz'
        with tarfile.open(archive, 'w:gz') as output:
            member = tarfile.TarInfo('../../escape.img')
            content = bytearray(4 * 1024 * 1024)
            content[1080:1082] = b'\x53\xef'
            member.size = len(content)
            output.addfile(member, io.BytesIO(content))
        target = self.directory / 'system.raw'
        vm._extract_raw(archive, target, 128 * 1024 * 1024)
        self.assertEqual(target.stat().st_size, 128 * 1024 * 1024)
        self.assertLess(target.stat().st_blocks * 512, target.stat().st_size // 2)
        self.assertFalse((self.directory.parent / 'escape.img').exists())
        with self.assertRaises(vm.LinuxVMError):
            vm._extract_raw(archive, self.directory / 'too-small.raw', 1024)

    def test_disk_floor_rejects_low_space(self):
        with patch.dict('os.environ', {'CODEX_WORKSPACE_MIN_FREE_BYTES': str(1024 ** 6)}):
            with self.assertRaisesRegex(vm.LinuxVMError, 'Not enough free disk'):
                vm._require_space(self.directory, create=True)

    def test_kernel_rejects_invalid_and_accepts_gzip_image(self):
        image = bytearray(4096)
        image[56:60] = b'ARM\x64'
        self.assertEqual(vm._kernel_image(gzip.compress(image)), image)
        for invalid in [b'garbage', gzip.compress(b'not a kernel')]:
            with self.assertRaises(vm.LinuxVMError):
                vm._kernel_image(invalid)

    def test_checksum_mismatch_rejects_download(self):
        target = self.directory / 'download'
        with patch.object(vm, '_run', side_effect=lambda *args, **kwargs: target.write_bytes(b'bad')):
            with self.assertRaisesRegex(vm.LinuxVMError, 'checksum'):
                vm._download('https://example.invalid/image', '0' * 64, target, 1024, 5)

    def test_settings_save_does_not_download_or_start_vm(self):
        limits = {'cpus': 1, 'memoryBytes': vm._GIB, 'systemDiskBytes': 8 * vm._GIB, 'dataDiskBytes': 16 * vm._GIB}
        with patch.object(self.client, 'create', side_effect=AssertionError('must not create')):
            self.assertEqual(self.client.set_settings(limits), limits)
        self.assertEqual(self.client.get_settings(), limits)
        self.assertFalse((self.directory / 'config.json').exists())

    def test_invalid_id_and_timeout_do_not_send(self):
        for arguments in [{'request_id': ''}, {'request_id': 'x' * 129}, {'timeout': 0}]:
            with self.assertRaises(vm.LinuxVMError) as error:
                self.client.request('provider.start', {}, **arguments)
            self.assertFalse(error.exception.uncertain)

    def test_start_does_not_launch_after_create_consumes_deadline(self):
        def slow_create(*args, **kwargs):
            time.sleep(0.03)
        with patch.object(vm.platform, 'system', return_value='Darwin'), \
             patch.object(vm.platform, 'machine', return_value='arm64'), \
             patch.object(self.client, 'create', side_effect=slow_create), \
             patch.object(vm.subprocess, 'Popen') as launch:
            with self.assertRaisesRegex(vm.LinuxVMError, 'exceeded its timeout'):
                self.client.ensure_running(timeout=0.01)
            launch.assert_not_called()

    def test_guest_clock_failure_is_explicit_and_does_not_retry(self):
        applied = []
        def handler(channel, request):
            applied.append(request['id'])
            channel.sendall(json.dumps({'id': request['id'], 'result': {'exitCode': 124}}).encode() + b'\n')
        guest = Guest(self.directory, handler)
        with self.assertRaisesRegex(vm.LinuxVMError, 'clock did not synchronize'):
            self.client._wait_guest_clock(time.monotonic() + 10)
        guest.finish()
        self.assertEqual(len(applied), 1)

    def test_payload_excludes_cache_and_credentials(self):
        guest = self.directory / 'vm/guest'
        guest.mkdir(parents=True)
        (guest / 'install.sh').write_text('true')
        (guest / 'credentials.json').write_text('not a runtime file')
        cache = guest / '__pycache__'
        cache.mkdir()
        (cache / 'bad.pyc').write_text('cache')
        scripts = self.directory / 'scripts'
        scripts.mkdir()
        for name in ['codex_workspace_images.py', 'codex_workspace_linux.py', 'codex_process_supervisor.py', 'codex_open_file_limit.py']:
            (scripts / name).write_text('# runtime')
        bridge = scripts / 'claude_bridge'
        bridge.mkdir()
        (bridge / 'package-lock.json').write_text('{}')
        config = vm._cloud_config(guest, '1.2.3', '4.5.6')
        paths = [entry['path'] for entry in config['write_files']]
        self.assertEqual(paths[0], '/opt/codex-studio/vm/guest/install.sh')
        self.assertEqual(len(paths), 8)
        self.assertNotIn('credentials', json.dumps(config))


if __name__ == '__main__':
    unittest.main()
