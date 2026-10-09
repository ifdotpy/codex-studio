"""Linux VM client contracts without a VM or account credentials."""
import base64
import fcntl
import gzip
import io
import json
import os
import subprocess
from pathlib import Path
import socket
import tarfile
import tempfile
import threading
import time
import unittest
from unittest.mock import patch, Mock

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

    def test_host_exec_uses_its_outer_budget_without_expanding_other_methods(self):
        from contextlib import nullcontext
        channel = Mock()
        for timeout in (3960, 7260):
            with patch.object(self.client, '_channel', return_value=nullcontext(channel)), \
                 patch.object(self.client, '_read', return_value={'id':'host-budget','result':{}}):
                self.assertEqual(list(self.client.stream('host.exec', {}, request_id='host-budget', timeout=timeout)),
                                 [{'id':'host-budget','result':{}}])
        for method, timeout, maximum in [('host.exec',9000.01,9000),('exec',3600.01,3600)]:
            with patch.object(self.client, '_channel') as open_channel:
                with self.assertRaisesRegex(vm.LinuxVMError, 'between 0 and ' + str(maximum)):
                    list(self.client.stream(method, {}, timeout=timeout))
                open_channel.assert_not_called()

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

    def test_create_removes_abandoned_staging_without_touching_vm_disks(self):
        limits = vm.Settings(cpus=1, memoryBytes=vm._GIB, systemDiskBytes=8*vm._GIB, dataDiskBytes=8*vm._GIB)
        (self.directory / 'config.json').write_text(json.dumps(vm.asdict(limits)))
        for name in ['system.raw', 'data.raw']:
            with (self.directory / name).open('wb') as disk:
                disk.write(b'keep')
                disk.truncate(8*vm._GIB)
        abandoned = self.directory / 'create-interrupted'
        abandoned.mkdir()
        (abandoned / 'ubuntu.tar.gz').write_bytes(b'partial download')
        unrelated = self.directory / 'other-state'
        unrelated.mkdir()
        self.client.create(limits)
        self.assertFalse(abandoned.exists())
        self.assertTrue(unrelated.exists())
        with (self.directory / 'system.raw').open('rb') as disk:
            self.assertEqual(disk.read(4), b'keep')

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

    def test_later_spawn_reboots_failed_provision_and_preserves_disks(self):
        (self.directory / 'config.json').write_text('{}')
        (self.directory / 'image.json').write_text('{"codex":"0.160.1","claude":"2.1.291"}')
        (self.directory / 'console.log').write_text('STUDIO_PROVISION_CODEX\nSTUDIO_PROVISION_ERROR: line 29 failed\n')
        for name in ('system.raw', 'data.raw'):
            (self.directory / name).write_bytes(b'preserved disk')
        self.client.helper = self.directory / 'helper'
        self.client.helper.touch(mode=0o700)
        process = Mock()
        process.poll.return_value = None
        def launch(*args, **kwargs):
            (self.directory / 'console.log').write_text('STUDIO_PROVISION_READY\n')
            return process
        with patch.object(vm.platform, 'system', return_value='Darwin'), \
             patch.object(vm.platform, 'machine', return_value='arm64'), \
             patch.object(self.client, 'status', side_effect=[{'state':'running'}, {'state':'stopped'},
                                                            {'state':'stopped'}, {'state':'running'}]), \
             patch.object(self.client, 'call', side_effect=[vm.LinuxVMError('No guest'), {'protocol':1}]), \
             patch.object(self.client, '_host', return_value={}) as host, \
             patch.object(self.client, '_refresh_provision_seed') as seed, \
             patch.object(self.client, '_wait_guest_clock'), \
             patch.object(vm, '_require_space'), \
             patch.object(vm.subprocess, 'Popen', side_effect=launch), \
             patch.object(self.client, 'create') as create:
            result = self.client.ensure_running(timeout=10)
        self.assertEqual(result['health'], {'protocol':1})
        host.assert_any_call('host.stop', timeout=unittest.mock.ANY)
        seed.assert_called_once()
        create.assert_not_called()
        for name in ('system.raw', 'data.raw'):
            self.assertEqual((self.directory / name).read_bytes(), b'preserved disk')

    def test_recovery_seed_keeps_provider_pins_and_changes_boot_identity(self):
        (self.directory / 'image.json').write_text('{"codex":"0.160.1","claude":"2.1.291"}')
        (self.directory / 'console.log').write_text('previous failure')
        def make_iso(argv, **options):
            seed = Path(argv[-1])
            self.assertIn('instance-id: studio-linux-',(seed / 'meta-data').read_text())
            config = json.loads((seed / 'user-data').read_text().split('\n',1)[1])
            script = next(row['content'] for row in config['write_files'] if row['path'].endswith('/provision.sh'))
            self.assertIn('codex-cli 0.160.1',script)
            self.assertIn('@anthropic-ai/claude-code@2.1.291',script)
            Path(argv[-2]).write_bytes(b'new seed')
            return ''
        with patch.object(vm,'_run',side_effect=make_iso):
            self.client._refresh_provision_seed(time.monotonic()+10)
        self.assertEqual((self.directory / 'seed.iso').read_bytes(),b'new seed')
        identity = json.loads((self.directory / 'provision-seed.json').read_text())['instanceId']
        self.assertRegex(identity,r'^studio-linux-[a-f0-9]{64}$')
        self.assertEqual((self.directory / 'console.previous.log').read_text(),'previous failure')

    def test_healthy_guest_is_not_rebooted_for_an_old_console_error(self):
        (self.directory / 'config.json').write_text('{}')
        (self.directory / 'console.log').write_text('STUDIO_PROVISION_ERROR: codex: failed\n')
        with patch.object(vm.platform, 'system', return_value='Darwin'), \
             patch.object(vm.platform, 'machine', return_value='arm64'), \
             patch.object(self.client, 'status', return_value={'state':'running'}), \
             patch.object(self.client, 'call', return_value={'protocol':1}), \
             patch.object(self.client, '_wait_guest_clock'), \
             patch.object(self.client, '_host') as host, \
             patch.object(self.client, '_refresh_provision_seed') as seed:
            self.client.ensure_running(timeout=10)
        host.assert_not_called()
        seed.assert_not_called()

    def test_failure_is_one_line_with_stage_cause_action_and_log_path(self):
        (self.directory / 'config.json').write_text('{}')
        def health(*args, **kwargs):
            (self.directory / 'console.log').write_text('raw console dump\nSTUDIO_PROVISION_STAGE: codex\n'
                'STUDIO_PROVISION_ERROR: codex: deadline exceeded\nraw console tail\n')
            raise vm.LinuxVMError('No guest')
        with patch.object(vm.platform, 'system', return_value='Darwin'), \
             patch.object(vm.platform, 'machine', return_value='arm64'), \
             patch.object(self.client, 'status', return_value={'state':'running'}), \
             patch.object(self.client, 'call', side_effect=health):
            with self.assertRaises(vm.LinuxVMError) as failure:
                self.client.ensure_running(timeout=10)
        message = str(failure.exception)
        self.assertIn('codex failed: deadline exceeded', message)
        self.assertIn('Retry the Linux spawn', message)
        self.assertIn(str(self.directory / 'console.log'), message)
        self.assertNotIn('raw console', message)
        self.assertNotIn('\n', message)

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
        for name in ['codex_workspace_images.py', 'codex_workspace_linux.py', 'codex_process_supervisor.py', 'codex_open_file_limit.py', 'codex_records.py', 'codex_file_lock.py', 'codex_private_paths.py']:
            (scripts / name).write_text('# runtime')
        bridge = scripts / 'claude_bridge'
        bridge.mkdir()
        (bridge / 'package-lock.json').write_text('{}')
        layr = self.directory / 'vm/layr'
        layr.mkdir()
        (layr / 'manifest.json').write_text(json.dumps({'files': {}}))
        config = vm._cloud_config(guest, '1.2.3', '4.5.6')
        paths = [entry['path'] for entry in config['write_files']]
        self.assertEqual(paths[0], '/opt/codex-studio/vm/guest/install.sh')
        self.assertEqual(len(paths), 10)
        self.assertNotIn('credentials', json.dumps(config))
        unit = next(row['content'] for row in config['write_files'] if row['path'].endswith('/codex-studio-provision.service'))
        ready = next(line.split('=!',1)[1] for line in unit.splitlines() if line.startswith('ConditionPathExists='))
        self.assertRegex(ready, r'^/var/lib/codex-studio/provision-ready-[a-f0-9]{64}$')
        script = next(row['content'] for row in config['write_files'] if row['path'].endswith('/provision.sh'))
        self.assertIn('touch ' + ready + '\n', script)
        self.assertEqual(config, vm._cloud_config(guest, '1.2.3', '4.5.6'))
        (guest / 'install.sh').write_text('# new installation payload')
        self.assertNotEqual(config, vm._cloud_config(guest, '1.2.3', '4.5.6'))



class ProvisionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='vm-provision-script-')
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.script = vm._provision_script('0.160.1', '2.1.291')

    def helper(self, name, content):
        target = self.root / name
        target.write_text('#!/bin/bash\n' + content)
        target.chmod(0o700)

    def run_helpers(self, command):
        # Execute the actual generated retry helpers without touching guest paths.
        helpers = self.script[:self.script.index('mkdir -p /var/lib/codex-studio')]
        helpers = helpers.replace('/var/lib/codex-studio', str(self.root))
        return subprocess.run(['bash'], input=helpers + command, text=True,
            capture_output=True, timeout=5,
            env={**os.environ, 'PATH':str(self.root) + ':' + os.environ['PATH']})

    def test_retry_uses_longer_bounded_npm_deadline_and_backoff(self):
        self.helper('sleep', 'echo "$*" >> "'+str(self.root / 'backoff')+'"\n')
        self.helper('timeout', 'echo "$*" >> "'+str(self.root / 'limits')+'"\n'
                    'if [ ! -f "'+str(self.root / 'attempt')+'" ]; then touch "'+str(self.root / 'attempt')+'"; exit 124; fi\n'
                    'test "$2" -gt 180\n')
        self.helper('codex', 'exit 1\n')
        section = self.script[self.script.index('stage codex\n'):self.script.index('stage claude\n')]
        result = self.run_helpers('download() { return 44; }\n' + section)
        self.assertEqual(result.returncode, 0, result.stderr)
        limits = (self.root / 'limits').read_text().splitlines()
        self.assertEqual(len(limits), 2)
        self.assertTrue(all(line.startswith('--kill-after=5 600 npm install') for line in limits))
        self.assertEqual((self.root / 'backoff').read_text(), '10\n')
        self.assertIn('attempt=2', result.stdout)

    def test_download_retry_records_size_and_elapsed_time(self):
        self.helper('sleep', 'true\n')
        self.helper('curl', 'if [ ! -f "'+str(self.root / 'attempt')+'" ]; then touch "'+str(self.root / 'attempt')+'"; '
                    'echo "STUDIO_DOWNLOAD: codex attempt=1 bytes=10 seconds=600.0 http=200"; exit 28; fi\n'
                    'echo complete > "'+str(self.root / 'asset.part')+'"\n'
                    'echo "STUDIO_DOWNLOAD: codex attempt=2 bytes=100 seconds=2.5 http=200"\n')
        result = self.run_helpers('stage codex\ndownload https://example.test/asset "'+str(self.root / 'asset')+'" 600\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'asset').read_text(), 'complete\n')
        (self.root / 'console.log').write_text(result.stdout)
        metrics = vm._provision_info(self.root / 'console.log')['downloads']
        self.assertEqual([(row['bytes'],row['seconds']) for row in metrics], [(10,600.0),(100,2.5)])

    def test_missing_asset_and_forbidden_response_have_distinct_results(self):
        for http, expected in ((404,44),(403,22)):
            self.helper('curl', f'echo "STUDIO_DOWNLOAD: codex attempt=1 bytes=0 seconds=0.1 http={http}"\nexit 22\n')
            self.helper('sleep', 'true\n')
            result = self.run_helpers('stage codex\nif download https://example.test/asset "'+str(self.root / 'asset')+'" 600; then exit 0; else exit $?; fi\n')
            self.assertEqual(result.returncode, expected)

    def test_checksum_failure_cannot_install_or_use_npm_fallback(self):
        section = self.script[self.script.index('stage codex\n'):self.script.index('stage claude\n')]
        section = section.replace('/tmp/', str(self.root) + '/').replace('/opt/codex-studio',str(self.root / 'install'))
        self.helper('codex', 'exit 1\n')
        self.helper('sha256sum', 'exit 1\n')
        # Fake successful downloads with a matching asset name and a corrupt archive.
        command = ('download() { echo "' + '0'*64 + '  codex-package-aarch64-unknown-linux-musl.tar.gz" > "$2"; }\n'
                   'retry_npm() { touch "'+str(self.root / 'npm-used')+'"; }\n' + section)
        result = self.run_helpers(command)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'npm-used').exists())
        self.assertFalse((self.root / 'install').exists())
        self.assertIn('STUDIO_PROVISION_ERROR: codex:',result.stderr)

    def test_legacy_timeout_log_has_a_specific_cause(self):
        console = self.root / 'console.log'
        console.write_text('STUDIO_PROVISION_CODEX\nline 29: Killed timeout --kill-after=5 180 npm install\n'
                           'STUDIO_PROVISION_ERROR: line 29 failed\n')
        info = vm._provision_info(console)
        self.assertEqual((info['stage'],info['cause']),('codex','deadline exceeded'))
        console.write_text(console.read_text()+'STUDIO_PROVISION_READY\n')
        self.assertEqual(vm._provision_info(console)['state'],'ready')

    def test_runtime_payload_imports_without_host_source_modules(self):
        config = vm._cloud_config(Path(__file__).resolve().parents[1] / 'vm/guest', '0.160.1', '2.1.291')
        for name in ('codex-orchestrator', 'codex-subagent', 'codex-workspace'):
            entry = next(row for row in config['write_files']
                         if row['path'] == '/opt/codex-studio/.agents/skills/' + name + '/SKILL.md')
            source = Path(__file__).resolve().parents[1] / '.agents/skills' / name / 'SKILL.md'
            self.assertEqual(gzip.decompress(base64.b64decode(entry['content'])), source.read_bytes())
        payload = self.root / 'scripts'
        payload.mkdir()
        for entry in config['write_files']:
            if entry['path'].startswith('/opt/codex-studio/scripts/'):
                (payload / Path(entry['path']).name).write_bytes(gzip.decompress(base64.b64decode(entry['content'])))
        code = ('import sys;sys.path.insert(0,' + repr(str(payload)) + ');'
                'import codex_process_supervisor,codex_file_lock,codex_private_paths')
        result = subprocess.run([__import__('sys').executable,'-I','-c',code],capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse((payload / 'codex_workspace_linux.py').exists())
        self.assertFalse((payload / 'codex_workspace_images.py').exists())

    def test_saved_versions_cannot_inject_shell_commands(self):
        with self.assertRaises(vm.LinuxVMError):
            vm._provision_script('1.2.3; false', '2.1.291')

if __name__ == '__main__':
    unittest.main()
