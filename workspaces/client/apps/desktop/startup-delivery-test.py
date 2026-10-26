"""Startup delivery uses disposable state and fake kernel observations, never a live PID."""
import fcntl
from contextlib import closing
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import sys
import tempfile
from types import SimpleNamespace
import unittest
import uuid
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import desktop_updates as updates
import deliver_server as delivery
import recover_backend as recovery
import desktop_delivery
import desktop_process
import install_update as installer


class StartupDeliveryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='studio-startup-delivery-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.state = self.root / 'state'
        self.state.mkdir(mode=0o700)
        self.installed = self.root / 'Applications' / updates.APP_NAME
        self.previous = self.installed.with_name('Codex Studio.previous.app')
        self.old = {'version': 1, 'builtAt': 1000.5, 'commit': 'a' * 40,
                    'commitTime': 1000, 'branch': 'old'}
        self.new = {**self.old, 'builtAt': 2000.5, 'commit': 'b' * 40,
                    'commitTime': 2000, 'branch': 'fix/startup'}
        for app, build, content in [(self.previous, self.old, b'old'),
                                    (self.installed, self.new, b'new')]:
            binary = delivery.server_path(app)
            binary.parent.mkdir(parents=True)
            binary.write_bytes(content)
            binary.chmod(0o700)
            updates.write_json(app / updates.BUILD_STAMP, build)
            installer_file = app / 'Contents/Resources/install_update.py'
            installer_file.write_bytes(Path(installer.__file__).read_bytes())
            if build == self.new:
                resources = app / 'Contents/Resources'
                for name in desktop_delivery.CONTRACT['startupResources']:
                    (resources / name).write_text('fixture recovery resource\n')
                updates.write_json(resources / 'delivery-contract.json', desktop_delivery.CONTRACT)
        self.config = {'version': 1, 'enabled': True, 'stateDir': str(self.state),
                       'port': 4620, 'python': sys.executable, 'codex': '/fixture/codex',
                       'resources': str(self.installed / 'Contents/Resources'),
                       'runtimeServer': str(delivery.server_path(self.installed)),
                       'backend': 'rust', 'supervisorEnabled': True,
                       'processSupervisor': '/fixture/supervisor'}
        updates.write_json(self.state / 'background-recovery.json', self.config)
        self.files = {}
        for name in ['runtime.lock', 'canvas.sqlite3', 'analytics.sqlite3']:
            file = self.state / name
            file.touch(mode=0o600)
            self.files[name] = self.file_identity(file)
        self.lease = (self.state / 'runtime.lock').open('r')
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.addCleanup(self.lease.close)
        updates.write_json(self.state / 'rust-state-owner.json', {
            'runtime': 'rust', 'formatVersion': 1,
            'backendBuild': hashlib.sha256(b'old').hexdigest(),
        })
        with closing(sqlite3.connect(self.state / 'supervisor.sqlite3')) as db:
            db.execute('CREATE TABLE handles(id TEXT,pid INTEGER,generation INTEGER,closed_at REAL)')
            db.execute('INSERT INTO handles VALUES(?,?,?,NULL)', ('account:fixture', 31, 4))
            db.commit()
        self.handles = {'account:fixture': [31, 4]}
        self.health = {'protocol': 2, 'stateDir': str(self.state), 'recovery': {},
                       'handles': [{'id': 'account:fixture', 'pid': 31,
                                    'signature': 'c' * 64, 'startTime': 'fixture-child-start'}]}
        self.facts = {'pid': 11, 'uid': os.getuid(), 'birth': '100:123456',
                      'executable': str(delivery.server_path(self.previous)),
                      'argv': [str(delivery.server_path(self.installed)), '--port', '4620'],
                      'environment': {'CODEX_AGENTS_STATE_DIR': str(self.state),
                                      'CODEX_AGENTS_SUPERVISOR_MODE': '1'},
                      'openFiles': self.files,
                      'textFiles': [self.file_identity(delivery.server_path(self.previous))]}
        proof = {'version': 1, 'kind': 'startup-backend', 'pid': 11, 'birth': self.facts['birth'],
                 'argv': self.facts['argv'], 'environment': self.facts['environment'],
                 'executable': {**self.file_identity(delivery.server_path(self.previous)),
                                'sha256': hashlib.sha256(b'old').hexdigest()},
                 'files': self.files, 'previousBuild': self.old,
                 'previousBundleHash': delivery.bundle_hash(self.previous),
                 'supervisorHandles': self.health['handles']}
        request = {'commit': self.new['commit'], 'build': self.new,
                   'bundleHash': delivery.bundle_hash(self.installed),
                   'backendHash': hashlib.sha256(b'new').hexdigest(),
                   'stateDir': str(self.state), 'installed': str(self.installed),
                   'startupBackend': proof}
        self.operation = str(uuid.uuid5(uuid.NAMESPACE_URL,
                                       'studio-delivery:' + str(self.state) + ':' + self.new['commit']))
        self.record = {'id': self.operation, 'request': request, 'beforePid': 11,
                       'handles': self.handles, 'phase': 'submitted'}
        self.journal = self.state / 'local-deliveries' / (self.operation + '.json')
        self.intent = self.state / 'startup-delivery.json'
        self.save_intent()
        updates.write_json(self.state / updates.RECEIPT_FILE, {
            'id': self.operation, 'build': self.new, 'from': 'local-delivery', 'outcome': 'installed',
        })
        self.alive = True
        self.calls = []

    @staticmethod
    def file_identity(path):
        info = path.stat()
        return {'device': info.st_dev, 'inode': info.st_ino}

    def save_intent(self):
        updates.write_json(self.journal, self.record)
        digest = hashlib.sha256(json.dumps(self.record['request'], sort_keys=True,
                                           separators=(',', ':')).encode()).hexdigest()
        updates.write_json(self.intent, {'version': 1, 'id': self.operation, 'requestHash': digest})

    def kill(self, pid, number):
        if number == 0:
            return  # The real HTTP identity reader checks only a fixture PID's existence.
        self.calls.append(('signal', pid, number))
        self.assertEqual((pid, number), (11, signal.SIGTERM))
        self.alive = False
        fcntl.flock(self.lease, fcntl.LOCK_UN)

    def run_once(self, *, launch=False, install=None, replacement=None, maps=None, legacy=False):
        handlers = {}
        def install_handler(number, callback):
            handlers[number] = callback
        def sleep(_seconds):
            handlers[signal.SIGTERM]()
        def tick(*_args, **_kwargs):
            self.calls.append(('http-tick',))
            raise TimeoutError('startup does not answer HTTP')
        normal_tick = recovery.tick
        normal_identity = recovery.identity
        normal_delivery_tick = recovery.startup_delivery_tick
        def spawn(command, **options):
            self.calls.append(('spawn', command, options['env']['CODEX_AGENTS_STATE_DIR']))
            return SimpleNamespace(pid=77, poll=lambda: None)
        with patch.object(recovery, 'startup_backend_facts', return_value=self.facts, create=True), \
                patch.object(recovery, 'startup_backend_birth', side_effect=lambda _pid: self.facts['birth'] if self.alive else None, create=True), \
                patch.object(recovery, 'listener_pids', side_effect=lambda _port: [11] if self.alive else [77] if replacement else [], create=True), \
                patch.object(recovery, 'mapped_backend_pids', side_effect=lambda binary, _state, **_options: (maps or {}).get('previous' if binary == delivery.server_path(self.previous) else 'target', [])), \
                patch.object(recovery, 'verify_delivery_recovery_owner', return_value=None), \
                patch.object(recovery, 'startup_delivery_tick', side_effect=None if legacy else normal_delivery_tick, return_value=(False, None)), \
                patch.object(recovery, 'supervisor_request', return_value=self.health), \
                patch.object(recovery, 'supervisor_tick', return_value=(None, 'supervisor ready')), \
                patch.object(recovery, 'verify_listener'), \
                patch.object(recovery, 'tick', side_effect=normal_tick if launch else tick), \
                patch.object(recovery, 'identity', side_effect=normal_identity if replacement else None, return_value=None), \
                patch.object(recovery.urllib.request, 'build_opener') as opener, \
                patch.object(recovery, 'backend_process_arguments', return_value=[self.config['runtimeServer'], '--port', '4620']), \
                patch.object(recovery.subprocess, 'Popen', side_effect=spawn), \
                patch.object(recovery, 'desktop_tick', return_value=(None, 'desktop closed')), \
                patch.object(recovery, 'update_tick', side_effect=install, return_value=None), \
                patch.object(recovery.signal, 'signal', side_effect=install_handler), \
                patch.object(recovery.os, 'kill', side_effect=self.kill), \
                patch.object(recovery.time, 'sleep', side_effect=sleep):
            opener.return_value.open.side_effect = lambda *_args, **_kwargs: io.BytesIO(json.dumps(replacement).encode())
            recovery.run(self.state / 'background-recovery.json', interval=0.001)

    def test_installed_startup_intent_runs_before_http_and_stops_only_the_pinned_backend(self):
        self.run_once()
        self.assertEqual(self.calls, [('signal', 11, signal.SIGTERM)])
        self.assertEqual(updates.read_json(self.journal)['startupRecovery']['phase'], 'stop-sent')
        self.run_once(launch=True)
        self.assertEqual(self.calls.count(('signal', 11, signal.SIGTERM)), 1)
        self.assertEqual(updates.read_json(self.journal)['startupRecovery']['phase'], 'stopped')
        self.assertEqual(self.calls[-1], ('spawn', [self.config['runtimeServer'], '--port', '4620'], str(self.state)))

    def test_every_process_and_supervisor_mismatch_holds_without_signal_or_http(self):
        original_facts = copy.deepcopy(self.facts)
        original_health = copy.deepcopy(self.health)
        cases = [
            lambda: self.facts.update(birth='100:999999'),
            lambda: self.facts.update(uid=os.getuid() + 1),
            lambda: self.facts.update(argv=['/foreign/server', '--port', '4620']),
            lambda: self.facts.update(executable='/foreign/server'),
            lambda: self.facts.update(textFiles=[]),
            lambda: self.facts['openFiles'].pop('canvas.sqlite3'),
            lambda: self.facts['environment'].update(CODEX_AGENTS_STATE_DIR='/foreign/state'),
            lambda: self.facts['environment'].update(CODEX_AGENTS_SUPERVISOR_MODE='0'),
            lambda: self.facts['environment'].update(CODEX_AGENTS_SUPERVISOR_FALLBACK='1'),
            lambda: self.health.update(protocol=1),
            lambda: self.health.update(stateDir='/foreign/state'),
            lambda: self.health.update(recovery={'blocked': 'unknown owner'}),
            lambda: self.health.update(recovery={'degraded': True}),
            lambda: self.health.update(handles=[]),
            lambda: self.health['handles'][0].update(pid=999),
            lambda: self.health['handles'][0].update(signature='d' * 64),
            lambda: self.health['handles'][0].update(startTime='another-start'),
        ]
        for index, mutate in enumerate(cases):
            with self.subTest(index=index):
                self.facts = copy.deepcopy(original_facts)
                self.health = copy.deepcopy(original_health)
                mutate()
                before = self.journal.read_bytes()
                self.run_once()
                self.assertEqual(self.calls, [])
                self.assertEqual(self.journal.read_bytes(), before)

    def test_pinned_pid_reuse_during_ended_backend_verification_does_not_authorize_start(self):
        fcntl.flock(self.lease, fcntl.LOCK_UN)
        before = self.journal.read_bytes()
        with patch.object(recovery, 'startup_backend_birth', side_effect=[None, '200:123456']), \
                patch.object(recovery, 'listener_pids', return_value=[]), \
                patch.object(recovery, 'supervisor_request', return_value=self.health), \
                patch.object(recovery.os, 'kill') as send:
            with self.assertRaisesRegex(RuntimeError, 'PID was reused'):
                recovery.startup_delivery_tick(self.config, self.state)
            send.assert_not_called()
        self.assertEqual(self.journal.read_bytes(), before)

    def test_artifact_state_receipt_generation_and_lease_mismatches_hold(self):
        targets = [
            (delivery.server_path(self.installed), b'changed-new'),
            (delivery.server_path(self.previous), b'changed-old'),
            (self.previous / updates.BUILD_STAMP, b'{}'),
            (self.state / 'rust-state-owner.json', json.dumps({'runtime':'rust','formatVersion':True,'backendBuild':'0'*64}).encode()),
            (self.state / updates.RECEIPT_FILE, json.dumps({'id':str(uuid.uuid4()),'outcome':'installed'}).encode()),
            (self.intent, b'{broken'),
            (self.journal, b'{}'),
        ]
        for path, content in targets:
            with self.subTest(path=path):
                original = path.read_bytes()
                path.write_bytes(content)
                try:
                    self.run_once()
                    self.assertEqual(self.calls, [])
                    self.assertEqual(path.read_bytes(), content)
                finally:
                    path.write_bytes(original)
        with closing(sqlite3.connect(self.state / 'supervisor.sqlite3')) as db:
            db.execute('UPDATE handles SET generation=5')
            db.commit()
            self.run_once()
            self.assertEqual(self.calls, [])
            db.execute('UPDATE handles SET generation=4')
            db.commit()
        fcntl.flock(self.lease, fcntl.LOCK_UN)
        self.run_once()
        self.assertEqual(self.calls, [])

    def test_symlinks_at_owner_boundaries_never_authorize_a_signal(self):
        for path in [self.intent, self.journal, self.state / 'rust-state-owner.json',
                     self.state / 'background-recovery.json', self.state / 'runtime.lock',
                     self.state / updates.RECEIPT_FILE]:
            with self.subTest(path=path):
                original = path.with_name(path.name + '.original')
                path.rename(original)
                path.symlink_to(original)
                try:
                    self.run_once()
                    self.assertEqual(self.calls, [])
                finally:
                    path.unlink()
                    original.rename(path)

    def test_an_uncertain_stop_is_never_sent_again(self):
        self.record['startupRecovery'] = {'phase': 'stop-requested'}
        self.save_intent()
        self.run_once()
        self.assertEqual(self.calls, [])
        self.assertEqual(updates.read_json(self.journal)['startupRecovery'], {'phase':'stop-requested'})

    def test_stopped_intent_still_fences_changed_configuration_receipt_and_target(self):
        self.record['startupRecovery'] = {'phase': 'stopped'}
        self.save_intent()
        self.alive = False
        fcntl.flock(self.lease, fcntl.LOCK_UN)
        cases = [
            (self.state / 'background-recovery.json', json.dumps({**self.config, 'runtimeServer': str(delivery.server_path(self.previous))}).encode()),
            (self.state / 'background-recovery.json', json.dumps({**self.config, 'port': 4621}).encode()),
            (self.state / updates.RECEIPT_FILE, b'{"id":"another","outcome":"installed"}'),
            (delivery.server_path(self.installed), b'changed-new-backend'),
        ]
        for path, changed in cases:
            with self.subTest(path=path):
                original = path.read_bytes()
                path.write_bytes(changed)
                try:
                    self.run_once(launch=True)
                    self.assertEqual(self.calls, [], 'a stopped proof cannot authorize another target')
                finally:
                    path.write_bytes(original)
                    self.calls.clear()

    def test_pinned_backend_can_end_before_the_existing_install_owner_swaps(self):
        source = self.prepare_uninstalled_delivery()
        self.stage(source)
        self.alive = False
        fcntl.flock(self.lease, fcntl.LOCK_UN)
        installed = []
        def install(*_args):
            installed.append(True)
            return 'update installed'
        self.run_once(install=install)
        self.assertEqual(installed, [True], 'a proven ended backend must not block the existing installer')
        self.assertEqual(self.calls, [], 'only the install owner runs; there is no signal or early spawn')

    def test_existing_startup_owner_can_supply_the_exact_target_before_recovery_relaunch(self):
        self.alive = False
        # Another existing startup owner already holds the same lease for the requested replacement.
        updates.write_json(self.state / 'rust-state-owner.json', {
            'runtime':'rust', 'formatVersion':1, 'backendBuild':self.record['request']['backendHash'],
        })
        target = {'application':'codex-agents','protocol':1,'pid':77,'stateDir':str(self.state),
                  'backendKind':'rust','backendBuild':self.record['request']['backendHash'],
                  'supervisorMode':True,'supervisorFallback':False}
        original_request = copy.deepcopy(self.record['request'])
        self.run_once(launch=True, replacement=target)
        self.assertEqual(self.calls, [], 'the exact existing target is observed, never signaled or spawned')
        self.assertEqual(updates.read_json(self.journal).get('startupRecovery'), {'phase':'stopped'})
        self.assertEqual(updates.read_json(self.journal)['request'], original_request)

    def test_an_existing_listener_needs_exact_target_http_and_retained_proof(self):
        self.alive = False
        updates.write_json(self.state / 'rust-state-owner.json', {
            'runtime':'rust', 'formatVersion':1, 'backendBuild':self.record['request']['backendHash'],
        })
        target = {'application':'codex-agents','protocol':1,'pid':77,'stateDir':str(self.state),
                  'backendKind':'rust','backendBuild':self.record['request']['backendHash'],
                  'supervisorMode':True,'supervisorFallback':False}
        for patch_data in [
            {'application':'another'}, {'protocol':2}, {'pid':11}, {'pid':88},
            {'stateDir':'/foreign/state'}, {'backendKind':'python'}, {'backendBuild':'f'*64},
            {'supervisorMode':False}, {'supervisorFallback':True},
        ]:
            with self.subTest(patch_data=patch_data):
                before = self.journal.read_bytes()
                self.run_once(launch=True, replacement={**target, **patch_data})
                self.assertEqual(self.calls, [])
                self.assertEqual(self.journal.read_bytes(), before)
        self.health['handles'][0]['signature'] = 'd' * 64
        self.run_once(launch=True, replacement=target)
        self.assertEqual(self.calls, [])
        self.assertNotIn('startupRecovery', updates.read_json(self.journal))

    def test_completion_preserves_the_original_proof_and_clears_only_its_pointer_after_http(self):
        self.record['startupRecovery'] = {'phase':'stopped'}
        self.save_intent()
        target = {'application':'codex-agents','protocol':1,'pid':77,'stateDir':str(self.state),
                  'backendKind':'rust','backendBuild':self.record['request']['backendHash'],
                  'supervisorMode':True}
        original_request = copy.deepcopy(self.record['request'])
        with patch.object(delivery.urllib.request, 'build_opener') as opener, \
                patch.object(recovery, 'backend_process_arguments', return_value=[self.config['runtimeServer']]), \
                patch.object(recovery, 'verify_listener'):
            opener.return_value.open.side_effect = lambda *_args, **_kwargs: io.BytesIO(json.dumps(target).encode())
            result = delivery.wait_for_completion(
                self.record, self.installed, self.root / 'receipts',
                lambda: delivery.observe_desktop('http://127.0.0.1:4620'))
        self.assertEqual(result['phase'], 'installed')
        self.assertEqual(result['afterPid'], 77)
        self.assertEqual(result['request'], original_request)
        self.assertEqual(updates.read_json(self.journal)['startupRecovery'], {'phase':'stopped'})
        self.assertFalse(self.intent.exists())

    def test_startup_stage_requires_the_target_recovery_capability_before_publication(self):
        source = self.prepare_uninstalled_delivery()
        contract = source / 'Contents/Resources/delivery-contract.json'
        helper = source / 'Contents/Resources/desktop_delivery.py'
        original_contract, original_helper = contract.read_bytes(), helper.read_bytes()
        for dry_run in [True, False]:
            for kind in ['missing-contract', 'version', 'names', 'helper']:
                with self.subTest(dry_run=dry_run, kind=kind):
                    if kind == 'missing-contract':
                        contract.unlink()
                    elif kind == 'helper':
                        helper.unlink()
                    else:
                        data = dict(desktop_delivery.CONTRACT)
                        data['startupIntentVersion' if kind == 'version' else 'startupIntentFile'] = 99 if kind == 'version' else 'foreign-intent.json'
                        updates.write_json(contract, data)
                    try:
                        with self.assertRaisesRegex(delivery.DeliveryError, 'startup|Startup'):
                            self.stage(source, dry_run=dry_run)
                        self.assertFalse(self.intent.exists())
                        self.assertFalse(self.journal.exists())
                        self.assertFalse((self.state / updates.PENDING_FILE).exists())
                        self.assertFalse((self.state / updates.UPDATES_DIR).exists())
                    finally:
                        contract.write_bytes(original_contract)
                        helper.write_bytes(original_helper)

    def test_existing_install_and_maintenance_locks_fence_startup_recovery(self):
        maintenance = self.state / 'linux-vm' / 'maintenance.lock'
        maintenance.parent.mkdir()
        for path in [self.state / 'update-install.lock', maintenance]:
            with self.subTest(path=path), path.open('a') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.run_once()
                self.assertEqual(self.calls, [])

    def test_old_completion_cannot_unlink_a_new_startup_publication(self):
        following = copy.deepcopy(self.record)
        following['request']['commit'] = 'd' * 40
        following['request']['build']['commit'] = 'd' * 40
        following['id'] = str(uuid.uuid5(uuid.NAMESPACE_URL,
            'studio-delivery:' + str(self.state) + ':' + following['request']['commit']))
        original_read = desktop_delivery.read_private_json
        trace = []
        def complete_then_publish():
            with (self.state / 'delivery.lock').open('a+') as lease:
                try:
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    trace.append('blocked')
                    return
                self.intent.unlink(missing_ok=True)
                desktop_delivery.publish_startup_intent(self.state, following)
                trace.append('published')
        def competing_read(path):
            value = original_read(path)
            if path == self.intent:
                complete_then_publish()
            return value
        with patch.object(desktop_delivery, 'read_private_json', side_effect=competing_read):
            desktop_delivery.clear_startup_intent(self.state, self.record)
        if trace == ['blocked']:
            complete_then_publish()
        self.assertTrue(self.intent.exists(), 'an old completion must not remove the newer pointer')
        self.assertEqual(original_read(self.intent)['id'], following['id'])

    def prepare_uninstalled_delivery(self):
        source = self.root / 'package' / updates.APP_NAME
        source.parent.mkdir()
        self.installed.rename(source)
        self.previous.rename(self.installed)
        self.intent.unlink()
        self.journal.unlink()
        (self.state / updates.RECEIPT_FILE).unlink()
        self.facts['executable'] = str(delivery.server_path(self.installed))
        return source

    def fake_tool(self, command, _timeout):
        import subprocess
        if command[0] == updates.DITTO:
            shutil.copytree(command[1], command[2], symlinks=True)
        output = 'false\n' if command[0] == installer.OSASCRIPT else 'designated => same signing identity\n'
        return subprocess.CompletedProcess(command, 0, output, '')

    def stage(self, source, **options):
        with patch.object(recovery, 'startup_backend_facts', return_value=self.facts), \
                patch.object(recovery, 'startup_backend_birth', return_value=self.facts['birth']), \
                patch.object(recovery, 'listener_pids', return_value=[11]), \
                patch.object(recovery, 'supervisor_request', return_value=self.health):
            return delivery.stage(source, self.new['commit'], self.installed, self.root / 'receipts',
                                  observe=lambda: self.fail('startup staging must not fabricate or request HTTP identity'),
                                  run=self.fake_tool, starting_pid=11, state=self.state, **options)

    def test_explicit_stage_is_readonly_until_submission_and_keeps_the_legacy_installer_owner(self):
        source = self.prepare_uninstalled_delivery()
        before = {str(path): path.read_bytes() for path in self.state.rglob('*') if path.is_file()}
        result = self.stage(source, dry_run=True)
        self.assertEqual(result['phase'], 'dry_run')
        self.assertEqual(before, {str(path): path.read_bytes() for path in self.state.rglob('*') if path.is_file()})
        self.assertFalse(self.intent.exists())
        result = self.stage(source)
        self.assertEqual(result['phase'], 'submitted')
        self.assertEqual(self.stage(source), result, 'a retry reuses the exact pending request')
        self.assertEqual(desktop_delivery.read_private_json(self.intent)['id'], result['id'])
        def legacy_install(*_args):
            def no_http(*_):
                raise TimeoutError('startup HTTP is unavailable')
            with patch.object(installer, 'stop_backend', side_effect=lambda *_: self.fail('legacy installer cannot stop without HTTP')):
                code, _ = installer.install(self.state, self.config, applications=self.installed.parent,
                                             run=self.fake_tool, verify=no_http,
                                             stop=lambda *_: self.fail('unverified stop'),
                                             sleep=lambda _: None)
            self.assertEqual(code, installer.INSTALLED)
            return 'update installed'
        self.run_once(install=legacy_install)
        self.assertEqual(self.calls, [], 'the existing install owner swaps; it sends no unverified stop')
        self.assertEqual(updates.read_build(self.installed), self.new)
        self.assertIn('not restarted', updates.read_json(self.state / updates.RECEIPT_FILE)['backend'])
        self.facts['executable'] = str(delivery.server_path(self.previous))
        self.run_once()
        self.assertEqual(self.calls, [('signal', 11, signal.SIGTERM)])
        self.run_once(launch=True)
        self.assertEqual(self.calls[-1][0], 'spawn')
        with self.assertRaises(TimeoutError):
            delivery.verify_completion(result, self.installed,
                                       lambda: (_ for _ in ()).throw(TimeoutError('real HTTP still required')))

    def prepare_installed_recovery(self):
        self.record['request'].pop('startupBackend')
        updates.write_json(self.journal, self.record)
        self.intent.unlink()
        updates.write_json(self.state / 'rust-state-owner.json', {
            'runtime': 'rust', 'formatVersion': 1,
            'backendBuild': hashlib.sha256(b'new').hexdigest(),
        })
        fcntl.flock(self.lease, fcntl.LOCK_UN)
        self.alive = False
        self.following = {**self.new, 'builtAt': 3000.5, 'commit': 'd' * 40, 'commitTime': 3000}
        source = self.root / 'fixed' / updates.APP_NAME
        source.parent.mkdir()
        shutil.copytree(self.installed, source)
        delivery.server_path(source).write_bytes(b'fixed')
        updates.write_json(source / updates.BUILD_STAMP, self.following)
        return source

    def installed_stage(self, source, **options):
        receipts = options.pop('receipts', self.root / 'recovery-receipts')
        with patch.object(recovery, 'supervisor_request', return_value=self.health), \
                patch.object(recovery, 'listener_pids', return_value=[]), \
                patch.object(recovery, 'verify_delivery_recovery_owner', return_value=None, create=True):
            return delivery.stage(source, self.following['commit'], self.installed,
                                  receipts,
                                  observe=lambda: self.fail('an installed-delivery recovery must not fabricate HTTP identity'),
                                  run=self.fake_tool, recover_installed=self.operation,
                                  state=self.state, port=4620, **options)

    def test_exact_installed_delivery_can_stage_a_newer_fix_without_http_or_a_backend_pid(self):
        source = self.prepare_installed_recovery()
        original = self.journal.read_bytes()
        before = {str(path):path.read_bytes() for path in self.state.rglob('*') if path.is_file()}
        result = self.installed_stage(source, dry_run=True)
        self.assertEqual(result['phase'], 'dry_run')
        self.assertIn('installedDelivery', result['request'])
        self.assertNotIn('startupBackend', result['request'])
        self.assertEqual(result['handles'], self.handles)
        self.assertEqual(before, {str(path):path.read_bytes() for path in self.state.rglob('*') if path.is_file()})
        result = self.installed_stage(source)
        self.assertEqual(result['phase'], 'submitted')
        self.assertEqual(self.installed_stage(source), result)
        self.assertEqual(updates.read_pending(self.state)['id'], result['id'])
        self.assertEqual(self.journal.read_bytes(), original)

    def test_measured_image_inspection_completes_without_treating_timeout_as_absence(self):
        import subprocess
        def measured_read(command, **options):
            # The real exact-image scan completed in 3.148 seconds, beyond the former 2-second budget.
            if options['timeout'] < 3.148:
                raise subprocess.TimeoutExpired(command, options['timeout'])
            return subprocess.CompletedProcess(command,1,'','')
        with patch.object(recovery.subprocess, 'run', side_effect=measured_read):
            self.assertEqual(recovery.mapped_backend_pids(delivery.server_path(self.installed), self.state), [])
        with patch.object(recovery.subprocess, 'run', side_effect=subprocess.TimeoutExpired('lsof', 10)):
            with self.assertRaises(subprocess.TimeoutExpired):
                recovery.mapped_backend_pids(delivery.server_path(self.installed), self.state)

    def prepare_installed_chain(self):
        source = self.prepare_installed_recovery()
        record = self.install_recovered_delivery(source)
        proof = record['request']['installedDelivery']
        proof['version'] = 1
        proof.pop('stateOwner', None)
        updates.write_json(delivery.journal_path(self.state, record['id']), record)
        updates.write_json(self.intent, {'version':desktop_delivery.STARTUP_INTENT_VERSION,
                           'id':record['id'],'requestHash':desktop_delivery.request_hash(record['request'])})
        self.operation = record['id']
        self.record = record
        self.journal = delivery.journal_path(self.state, record['id'])
        self.following = {**self.following, 'builtAt':4000.5,'commit':'e'*40,'commitTime':4000}
        following = self.root / 'chain' / updates.APP_NAME
        following.parent.mkdir()
        shutil.copytree(self.installed, following)
        delivery.server_path(following).write_bytes(b'chain-fix')
        updates.write_json(following / updates.BUILD_STAMP, self.following)
        updates.write_json(following / 'Contents/Resources/delivery-contract.json', desktop_delivery.CONTRACT)
        return following

    def test_exact_installed_predecessor_can_be_superseded_without_changing_its_state_marker_or_journal(self):
        source = self.prepare_installed_chain()
        prior = self.journal.read_bytes()
        marker = (self.state / 'rust-state-owner.json').read_bytes()
        record = self.installed_stage(source, receipts=self.root/'chain-receipts')
        self.assertEqual(record['phase'], 'submitted')
        self.assertEqual(desktop_delivery.read_private_json(self.intent)['id'], record['id'])
        self.assertEqual(self.journal.read_bytes(), prior)
        self.assertEqual((self.state / 'rust-state-owner.json').read_bytes(), marker)
        self.assertEqual(record['request']['installedDelivery']['stateOwner']['backendBuild'], json.loads(marker)['backendBuild'])

    def test_old_v1_consumer_holds_but_official_dispatch_uses_the_exact_existing_installer_once(self):
        import importlib.util
        import subprocess
        source = self.prepare_installed_chain()
        record = self.installed_stage(source, receipts=self.root/'chain-receipts')
        fixture = Path(__file__).parent/'fixtures/installed_delivery_c004.py'
        namespace = dict(recovery.__dict__)
        exec(compile(fixture.read_text(), str(fixture), 'exec'), namespace)
        namespace['verify_delivery_recovery_owner'] = lambda *_a,**_k:None
        with patch.object(recovery, 'verify_delivery_recovery_owner', return_value=None), \
                patch.object(recovery, 'supervisor_request', return_value=self.health):
            with self.assertRaisesRegex(RuntimeError, 'state owner marker'):
                namespace['installed_delivery_tick'](self.config, self.state, record)
        calls = []
        def existing_installer(command, **options):
            self.assertEqual(command, [self.config['python'],'-B',str(self.installed/'Contents/Resources/install_update.py'),
                                       '--config',str(self.state/'background-recovery.json')])
            self.assertFalse(recovery.startup_lease_held(self.state), 'publication must release the runtime lease before dispatch')
            calls.append(command)
            code, _ = installer.install(self.state, self.config, applications=self.installed.parent,
                                         run=self.fake_tool,
                                         verify=lambda *_: (_ for _ in ()).throw(TimeoutError('no HTTP')),
                                         stop=lambda *_: self.fail('no unverified signal'),sleep=lambda _:None)
            with patch.object(recovery,'mapped_backend_pids',return_value=[]), \
                    self.assertRaisesRegex(delivery.DeliveryError,'Another local delivery is active'):
                recovery.installed_delivery_tick(self.config,self.state,record)
            return subprocess.CompletedProcess(command,code)
        with patch.object(recovery, 'verify_delivery_recovery_owner', return_value=None), \
                patch.object(recovery, 'supervisor_request', return_value=self.health), \
                patch.object(recovery, 'listener_pids', return_value=[]):
            done = delivery.dispatch_recovery_installer(record, self.installed, run=existing_installer)
            delivery.dispatch_recovery_installer(record, self.installed, run=lambda *_a,**_k:self.fail('no second installer'))
        self.assertEqual(len(calls),1)
        self.assertEqual(done['installerDispatch']['phase'],'installed')
        self.assertNotIn('startupRecovery',done)
        self.assertEqual(updates.read_build(self.installed),self.following)
        self.calls.clear()
        self.run_once(launch=True)
        self.assertEqual(self.calls[-1][0],'spawn')

    def test_chained_publication_crash_resumes_same_request_and_late_completion_cannot_clear_it(self):
        source = self.prepare_installed_chain()
        predecessor = copy.deepcopy(self.record)
        original_pointer = desktop_delivery.read_private_json(self.intent)
        with patch.object(desktop_delivery, 'publish_startup_intent', side_effect=OSError('lost before CAS')):
            with self.assertRaises(OSError):
                self.installed_stage(source, receipts=self.root/'chain-receipts')
        self.assertEqual(desktop_delivery.read_private_json(self.intent),original_pointer)
        self.assertIsNone(delivery.resume_saved(source,self.following['commit'],self.installed,self.root/'chain-receipts',
            lambda:self.fail('a durable publication must resume before HTTP'),recover_installed=self.operation,state=self.state))
        with patch.object(delivery, 'publish_pending', side_effect=OSError('lost after CAS')):
            with self.assertRaises(OSError):
                self.installed_stage(source, receipts=self.root/'chain-receipts')
        pointer = desktop_delivery.read_private_json(self.intent)
        with self.assertRaises(delivery.DeliveryError):
            desktop_delivery.clear_startup_intent(self.state, predecessor)
        self.assertEqual(desktop_delivery.read_private_json(self.intent),pointer)
        result = self.installed_stage(source, receipts=self.root/'chain-receipts')
        self.assertEqual(result['id'],pointer['id'])
        self.assertEqual(updates.read_pending(self.state)['id'],pointer['id'])

    def test_valid_foreign_historical_marker_is_not_an_inherited_state_owner(self):
        source = self.prepare_installed_chain()
        record = self.installed_stage(source, receipts=self.root/'chain-receipts')
        proof = record['request']['installedDelivery']
        original = delivery.read_delivery_record(delivery.journal_path(self.state,proof['stateOwner']['id']))
        foreign = copy.deepcopy(original)
        foreign['request']['commit'] = 'f'*40
        foreign['request']['build']['commit'] = 'f'*40
        foreign['id'] = str(uuid.uuid5(uuid.NAMESPACE_URL,'studio-delivery:'+str(self.state)+':'+'f'*40))
        updates.write_json(delivery.journal_path(self.state,foreign['id']),foreign)
        proof['stateOwner'] = {'id':foreign['id'],'requestHash':desktop_delivery.request_hash(foreign['request']),
                               'backendBuild':foreign['request']['backendHash']}
        with self.assertRaisesRegex(RuntimeError,'exact predecessor chain'):
            recovery.installed_state_owner(self.state,self.installed,proof,record['handles'])

    def test_predecessor_cas_refuses_a_replaced_pointer(self):
        source = self.prepare_installed_chain()
        record = self.installed_stage(source,receipts=self.root/'chain-receipts')
        expected = desktop_delivery.read_private_json(self.intent)
        successor = {**record,'id':str(uuid.uuid4()),'request':{**record['request'],'extra':'different'}}
        foreign = {'version':1,'id':str(uuid.uuid4()),'requestHash':'f'*64}
        actual = desktop_delivery.read_private_json
        reads = []
        def competing(path):
            if path == self.intent:
                reads.append(True)
                if len(reads) == 2:
                    updates.write_json(path,foreign)
            return actual(path)
        with desktop_delivery.delivery_lease(self.state), patch.object(desktop_delivery,'read_private_json',side_effect=competing):
            with self.assertRaisesRegex(delivery.DeliveryError,'predecessor changed'):
                desktop_delivery.publish_startup_intent(self.state,successor,predecessor=expected)
        self.assertEqual(actual(self.intent),foreign)

    def test_unknown_installer_dispatch_is_durable_and_cannot_be_repeated(self):
        import subprocess
        source = self.prepare_installed_chain()
        record = self.installed_stage(source, receipts=self.root/'chain-receipts')
        with patch.object(recovery, 'verify_delivery_recovery_owner', return_value=None), \
                patch.object(recovery, 'supervisor_request', return_value=self.health), \
                patch.object(recovery, 'listener_pids', return_value=[]):
            with self.assertRaises(subprocess.TimeoutExpired):
                delivery.dispatch_recovery_installer(record,self.installed,
                    run=lambda *_a,**_k:(_ for _ in ()).throw(subprocess.TimeoutExpired('installed installer',1800)))
            with self.assertRaises(delivery.DeliveryPending):
                delivery.dispatch_recovery_installer(record,self.installed,run=lambda *_a,**_k:self.fail('unknown must hold'))
        self.run_once(install=lambda *_a:self.fail('the recovery owner must not repeat an unknown dispatch'))
        self.assertEqual(self.calls,[])

    def install_recovered_delivery(self, source):
        record = self.installed_stage(source)
        def legacy_install(*_args):
            code, _ = installer.install(self.state, self.config, applications=self.installed.parent,
                                         run=self.fake_tool,
                                         verify=lambda *_: (_ for _ in ()).throw(TimeoutError('no backend HTTP')),
                                         stop=lambda *_: self.fail('a PID-free recovery must never signal'),
                                         sleep=lambda _: None)
            self.assertEqual(code, installer.INSTALLED)
            return 'update installed'
        self.run_once(launch=True, install=legacy_install, legacy=True)
        self.assertFalse(any(row[0] == 'signal' for row in self.calls))
        self.calls.clear()
        return record

    def test_legacy_owner_swaps_and_new_owner_waits_for_both_images_before_normal_tick(self):
        source = self.prepare_installed_recovery()
        record = self.install_recovered_delivery(source)
        self.assertEqual(updates.read_build(self.installed), self.following)
        for maps in ({'previous':[19]}, {'target':[77]}):
            with self.subTest(maps=maps):
                self.run_once(launch=True, maps=maps)
                self.assertEqual(self.calls, [], 'a mapped prior or target runtime must prevent another launch')
        self.run_once(launch=True)
        self.assertEqual(self.calls, [('spawn', [self.config['runtimeServer'], '--port', '4620'], str(self.state))])
        durable = delivery.read_delivery_record(delivery.journal_path(self.state, record['id']))
        self.assertEqual(durable['startupRecovery'], {'phase':'stopped'})
        with self.assertRaises(TimeoutError):
            delivery.verify_completion(durable, self.installed,
                                       lambda: (_ for _ in ()).throw(TimeoutError('real HTTP required')))

    def test_existing_target_requires_real_http_and_completion_accepts_reused_prior_pid(self):
        source = self.prepare_installed_recovery()
        record = self.install_recovered_delivery(source)
        target = {'application':'codex-agents','protocol':1,'backendKind':'rust',
                  'stateDir':str(self.state),'pid':77,'backendBuild':hashlib.sha256(b'fixed').hexdigest(),
                  'supervisorMode':True}
        updates.write_json(self.state / 'rust-state-owner.json', {
            'runtime':'rust','formatVersion':1,'backendBuild':target['backendBuild']})
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.run_once(launch=True, maps={'target':[77]}, replacement=target)
        self.assertEqual(self.calls, [], 'the existing target must attach without another launch')
        self.assertEqual(delivery.read_delivery_record(delivery.journal_path(self.state, record['id']))['startupRecovery'], {'phase':'stopped'})
        # The copied old beforePid is provenance. Numeric PID reuse has no signal or success authority.
        target['pid'] = record['beforePid']
        with patch.object(delivery, 'check_backend', return_value=self.state), \
                patch.object(recovery, 'supervisor_request', return_value=self.health):
            done = delivery.verify_completion(record, self.installed, lambda: target)
        self.assertEqual(done['afterPid'], record['beforePid'])
        original = self.state / 'canvas.sqlite3'
        replacement = self.state / 'replacement.sqlite3'
        replacement.write_bytes(original.read_bytes())
        os.replace(replacement, original)
        with patch.object(delivery, 'check_backend', return_value=self.state), \
                patch.object(recovery, 'supervisor_request', return_value=self.health):
            with self.assertRaisesRegex(RuntimeError, 'state files changed'):
                delivery.verify_completion(record, self.installed, lambda: target)

    def test_installed_recovery_mismatches_cannot_publish_or_start(self):
        source = self.prepare_installed_recovery()
        cases = [
            (self.state / updates.RECEIPT_FILE, {'id':str(uuid.uuid4()),'outcome':'installed'}),
            (self.state / 'rust-state-owner.json', {'runtime':'rust','formatVersion':1,'backendBuild':'0'*64}),
            (self.state / 'background-recovery.json', {**self.config,'port':4621}),
            (source / 'Contents/Resources/delivery-contract.json', {**desktop_delivery.CONTRACT,'installedRecoveryVersion':0}),
        ]
        for path, value in cases:
            original = path.read_bytes()
            with self.subTest(path=path):
                updates.write_json(path, value)
                try:
                    with self.assertRaises((RuntimeError, delivery.DeliveryError)):
                        self.installed_stage(source)
                    self.assertIsNone(updates.read_pending(self.state))
                    self.assertFalse(self.intent.exists())
                finally:
                    path.write_bytes(original)

    def test_mapping_reader_preserves_other_states_and_readonly_commands_but_holds_unknown(self):
        binary = delivery.server_path(self.installed)
        facts = {**self.facts, 'textFiles':[self.file_identity(binary)]}
        facts['environment'] = {**self.facts['environment'], 'CODEX_AGENTS_STATE_DIR':str(self.state)}
        import subprocess
        with patch.object(recovery.subprocess, 'run', return_value=subprocess.CompletedProcess([],0,'11\n','')), \
                patch.object(recovery, 'startup_backend_facts', return_value=facts):
            self.assertEqual(recovery.mapped_backend_pids(binary, self.state), [11])
            with self.assertRaisesRegex(RuntimeError, 'mapping changed'):
                recovery.mapped_backend_pids(binary, self.state, expected={'device':0,'inode':1,'sha256':'a'*64})
            other = self.root / 'preview-state'
            other.mkdir()
            facts['environment']['CODEX_AGENTS_STATE_DIR'] = str(other)
            self.assertEqual(recovery.mapped_backend_pids(binary, self.state), [])
            facts['argv'] = [str(binary), 'state-check', str(self.state)]
            self.assertEqual(recovery.mapped_backend_pids(binary, self.state), [])
            facts['argv'] = [str(binary), '--unknown']
            with self.assertRaisesRegex(RuntimeError, 'unknown launch scope'):
                recovery.mapped_backend_pids(binary, self.state)

    def test_empty_successful_process_scan_is_unknown_instead_of_absence(self):
        import subprocess
        with patch.object(recovery.subprocess, 'run', return_value=subprocess.CompletedProcess([],0,'','')):
            with self.assertRaises(RuntimeError):
                recovery.mapped_backend_pids(delivery.server_path(self.installed), self.state)
            with self.assertRaises(RuntimeError):
                recovery.listener_pids(4620)

    def test_registered_recovery_owner_requires_exact_job_argv_birth_and_held_lease(self):
        import subprocess
        import plistlib
        lock = self.state / 'background-recovery.lock'
        lock.touch(mode=0o600)
        command = [self.config['python'], '-B', str(self.installed / 'Contents/Resources/recover_backend.py'),
                   '--config', str(self.state / 'background-recovery.json')]
        label = recovery.DELIVERY_RECOVERY_LABEL_PREFIX + hashlib.sha256(str(self.state).encode()).hexdigest()[:recovery.DELIVERY_RECOVERY_LABEL_HEX_LENGTH]
        home = self.root / 'home'
        plist = home / 'Library/LaunchAgents' / (label + '.plist')
        plist.parent.mkdir(parents=True)
        plist.write_bytes(plistlib.dumps({'Label':label,'ProgramArguments':command}))
        output = f'\tpath = {plist}\n\targuments = {{\n' + ''.join(f'\t\t{arg}\n' for arg in command) + '\t}\n\tpid = 99\n'
        image = {'path':str(Path(self.config['python']).resolve()),'device':1,'inode':2,'sha256':'a'*64}
        with lock.open('r') as lease, \
                patch.object(recovery.Path, 'home', return_value=home), \
                patch.object(recovery, 'configured_interpreter_image', return_value=image), \
                patch.object(recovery, 'observed_interpreter_image', return_value={'birth':'100:123456','image':image}):
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            outputs = [subprocess.CompletedProcess([],0,output,''),
                       subprocess.CompletedProcess([],0,'99\n','')]
            with patch.object(recovery.subprocess, 'run', side_effect=outputs), \
                    patch.object(recovery, 'startup_backend_birth', return_value='100:123456'), \
                    patch.object(recovery, 'backend_process_arguments', return_value=command):
                recovery.verify_delivery_recovery_owner(self.config, self.state, self.installed)
            with patch.object(recovery.subprocess, 'run', return_value=outputs[0]), \
                    patch.object(recovery.os, 'getpid', return_value=100):
                with self.assertRaisesRegex(RuntimeError, 'Only the existing registered recovery owner'):
                    recovery.verify_delivery_recovery_owner(self.config, self.state, self.installed, current=True)
            for changed in ([*command[:-1], '/foreign/background-recovery.json'],
                            [command[0], '-B', '/foreign/recover_backend.py', *command[3:]]):
                with patch.object(recovery.subprocess, 'run', return_value=outputs[0]), \
                        patch.object(recovery, 'startup_backend_birth', return_value='100:123456'), \
                        patch.object(recovery, 'backend_process_arguments', return_value=changed):
                    with self.assertRaises(RuntimeError):
                        recovery.verify_delivery_recovery_owner(self.config, self.state, self.installed)
            with patch.object(recovery.subprocess, 'run', side_effect=outputs), \
                    patch.object(recovery, 'startup_backend_birth', side_effect=['100:123456','101:123456']), \
                    patch.object(recovery, 'backend_process_arguments', return_value=command):
                with self.assertRaises(RuntimeError):
                    recovery.verify_delivery_recovery_owner(self.config, self.state, self.installed)

    def test_configured_launcher_can_prove_its_exact_framework_interpreter_companion(self):
        import plistlib
        import subprocess
        launcher = self.root / 'bin/python3'
        launcher.parent.mkdir()
        launcher.write_bytes(b'configured launcher')
        launcher.chmod(0o700)
        companion = self.root / 'Framework/Python.app/Contents/MacOS/Python'
        companion.parent.mkdir(parents=True)
        companion.write_bytes(b'exact interpreter image')
        companion.chmod(0o700)
        config = {**self.config, 'python':str(launcher)}
        label = recovery.DELIVERY_RECOVERY_LABEL_PREFIX + hashlib.sha256(str(self.state).encode()).hexdigest()[:recovery.DELIVERY_RECOVERY_LABEL_HEX_LENGTH]
        home = self.root / 'home'
        plist = home / 'Library/LaunchAgents' / (label + '.plist')
        plist.parent.mkdir(parents=True)
        registered = [str(launcher), '-B', str(self.installed / 'Contents/Resources/recover_backend.py'),
                      '--config', str(self.state / 'background-recovery.json')]
        plist.write_bytes(plistlib.dumps({'Label':label,'ProgramArguments':registered}))
        command = [str(companion), *registered[1:]]
        output = f'\tpath = {plist}\n\targuments = {{\n' + ''.join(f'\t\t{arg}\n' for arg in registered) + '\t}\n\tpid = 99\n'
        image = {'path':str(companion), **self.file_identity(companion), 'sha256':hashlib.sha256(companion.read_bytes()).hexdigest()}
        lock = self.state / 'background-recovery.lock'
        lock.touch(mode=0o600)
        with lock.open('r') as lease:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(recovery.Path, 'home', return_value=home), \
                    patch.object(recovery.subprocess, 'run') as process, \
                    patch.object(recovery, 'startup_backend_birth', return_value='100:123456'), \
                    patch.object(recovery, 'backend_process_arguments', return_value=command), \
                    patch.object(recovery, 'configured_interpreter_image', return_value=image) as probe, \
                    patch.object(recovery, 'observed_interpreter_image', return_value={'birth':'100:123456','image':image}) as observed:
                def reset():
                    process.side_effect = [subprocess.CompletedProcess([],0,output,''),subprocess.CompletedProcess([],0,'99\n','')]
                    probe.return_value = image
                    observed.side_effect = None
                    observed.return_value = {'birth':'100:123456','image':image}
                reset()
                recovery.verify_delivery_recovery_owner(config, self.state, self.installed)
                reset()
                probe.return_value = {**image,'path':str(companion.with_name('OtherPython'))}
                with self.assertRaisesRegex(RuntimeError, 'launcher proof'):
                    recovery.verify_delivery_recovery_owner(config, self.state, self.installed)
                reset()
                observed.side_effect = [{'birth':'100:123456','image':image}, {'birth':'101:123456','image':image}]
                with self.assertRaisesRegex(RuntimeError, 'launcher proof'):
                    recovery.verify_delivery_recovery_owner(config, self.state, self.installed)
                for value in ({'Label':label,'ProgramArguments':[str(companion),*registered[1:]]},
                              {'Label':label + '.foreign','ProgramArguments':registered}):
                    reset()
                    plist.write_bytes(plistlib.dumps(value))
                    with self.assertRaisesRegex(RuntimeError, 'protected recovery registration'):
                        recovery.verify_delivery_recovery_owner(config, self.state, self.installed)
                plist.write_bytes(plistlib.dumps({'Label':label,'ProgramArguments':registered}))
                reset()
                process.side_effect = [subprocess.CompletedProcess([],0,output.replace(str(launcher),str(companion)), '')]
                with self.assertRaisesRegex(RuntimeError, 'protected recovery registration'):
                    recovery.verify_delivery_recovery_owner(config, self.state, self.installed)
                reset()
                plist.chmod(0o666)
                with self.assertRaisesRegex(RuntimeError, 'protected regular file'):
                    recovery.verify_delivery_recovery_owner(config, self.state, self.installed)

    def test_configured_probe_uses_only_isolated_flags_and_fixed_environment(self):
        import subprocess
        image = recovery.protected_interpreter_image(sys.executable)
        children, launches = [], []
        actual = subprocess.Popen
        def spawn(command, **options):
            launches.append((command, options))
            child = actual(command, **options)
            children.append(child)
            return child
        with patch.object(recovery.subprocess, 'Popen', side_effect=spawn), \
                patch.object(recovery, 'observed_interpreter_image', side_effect=lambda pid: {'birth':'probe-birth','image':image}):
            self.assertEqual(recovery.configured_interpreter_image(sys.executable), image)
        self.assertEqual(launches[0][0][:5], [sys.executable,'-I','-S','-B','-c'])
        self.assertEqual(launches[0][1]['env'], {'PATH':'/usr/bin:/bin','LANG':'C','LC_ALL':'C'})
        self.assertEqual(launches[0][1]['cwd'], '/')
        self.assertTrue(all(child.poll() == 0 for child in children))

    def test_owned_probe_timeout_oversize_wrong_pid_and_launcher_change_clean_up(self):
        import subprocess
        actual = subprocess.Popen
        programs = [
            ('import sys; sys.stdin.buffer.read(1)', 0.05, 'did not become ready'),
            ('import sys; print("x"*100,flush=True); sys.stdin.buffer.read(1)', 5, 'invalid PID'),
            ('import os,sys; print(os.getpid()+1,flush=True); sys.stdin.buffer.read(1)', 5, 'owned child'),
        ]
        for program, timeout, reason in programs:
            children = []
            def spawn(command, **options):
                child = actual([*command[:-1], program], **options)
                children.append(child)
                return child
            with self.subTest(program=program), \
                    patch.object(recovery.subprocess, 'Popen', side_effect=spawn), \
                    patch.object(recovery, 'INTERPRETER_PROBE_SECONDS', timeout), \
                    patch.object(recovery, 'observed_interpreter_image') as observed:
                with self.assertRaisesRegex(RuntimeError, reason):
                    recovery.configured_interpreter_image(sys.executable)
                observed.assert_not_called()
                self.assertTrue(all(child.poll() is not None for child in children))
        image = recovery.protected_interpreter_image(sys.executable)
        with patch.object(recovery, 'protected_interpreter_image', side_effect=[image,{**image,'inode':image['inode']+1}]), \
                patch.object(recovery, 'observed_interpreter_image', return_value={'birth':'probe-birth','image':image}):
            with self.assertRaisesRegex(RuntimeError, 'launcher changed'):
                recovery.configured_interpreter_image(sys.executable)

    def test_interpreter_kernel_reader_returns_only_image_identity_and_exact_birth(self):
        image = self.root / 'Python'
        image.write_bytes(b'framework image')
        identity = self.file_identity(image)
        output = f'p99\nftxt\nD{hex(identity["device"])}\ni{identity["inode"]}\nn{image}\n'
        with patch.object(desktop_process, 'startup_backend_birth', return_value='100:123456'), \
                patch.object(desktop_process, '_darwin_executable_path', return_value=str(image)), \
                patch.object(desktop_process.subprocess, 'check_output', return_value=output):
            facts = desktop_process.recovery_interpreter_facts(99)
        self.assertEqual(set(facts), {'pid','uid','birth','executable','textFiles'})
        self.assertEqual(facts['textFiles'], [identity])
        self.assertEqual(facts['birth'], '100:123456')
        with patch.object(recovery, 'recovery_interpreter_facts', return_value=facts), \
                patch.object(recovery, 'startup_backend_birth', return_value=facts['birth']):
            self.assertEqual(recovery.observed_interpreter_image(99)['image']['inode'], identity['inode'])
            facts['textFiles'] = []
            with self.assertRaisesRegex(RuntimeError, 'kernel mapping'):
                recovery.observed_interpreter_image(99)
        with patch.object(desktop_process, 'startup_backend_birth', side_effect=['100:123456','101:123456']), \
                patch.object(desktop_process, '_darwin_executable_path', return_value=str(image)), \
                patch.object(desktop_process.subprocess, 'check_output', return_value=output):
            with self.assertRaisesRegex(RuntimeError, 'changed during observation'):
                desktop_process.recovery_interpreter_facts(99)

    @unittest.skipUnless(sys.platform == 'darwin', 'The production kernel image reader uses the native process API.')
    def test_owned_launcher_probe_observes_its_real_kernel_companion_without_runtime_state(self):
        import shlex
        launcher = self.root / 'owned-python-launcher'
        launcher.write_text('#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' "$@"\n')
        launcher.chmod(0o700)
        before = {str(path):path.read_bytes() for path in self.state.rglob('*') if path.is_file()}
        image = recovery.configured_interpreter_image(launcher)
        self.assertEqual(image, recovery.protected_interpreter_image(image['path']))
        self.assertNotEqual(image['path'], str(launcher))
        self.assertEqual(before, {str(path):path.read_bytes() for path in self.state.rglob('*') if path.is_file()})

    def test_installed_publication_lease_fences_the_actual_legacy_tick(self):
        source = self.prepare_installed_recovery()
        actual_tool = self.fake_tool
        observed = []
        def stage_tool(command, timeout):
            if command[0] == updates.DITTO:
                with patch.object(recovery, 'identity', return_value=None), \
                        patch.object(recovery.subprocess, 'Popen') as spawn:
                    child, status = recovery.tick(self.config, self.state)
                    observed.append(status)
                    self.assertIsNone(child)
                    self.assertEqual(status, 'waiting for runtime owner')
                    spawn.assert_not_called()
            return actual_tool(command, timeout)
        with patch.object(self, 'fake_tool', side_effect=stage_tool):
            self.installed_stage(source)
        self.assertEqual(observed, ['waiting for runtime owner'])

    def test_legacy_probe_race_cannot_acquire_a_second_state_lease_or_write_database_state(self):
        import subprocess
        self.prepare_installed_recovery()
        before = {name:(self.state/name).read_bytes() for name in ('canvas.sqlite3','analytics.sqlite3','supervisor.sqlite3')}
        attempts = []
        owned_spawn = subprocess.Popen
        def raced_spawn(_command, **options):
            script = '''import fcntl, os, pathlib, sys
state = pathlib.Path(os.environ['CODEX_AGENTS_STATE_DIR'])
with (state / 'runtime.lock').open('r') as lease:
    try:
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        sys.exit(75)
    (state / 'canvas.sqlite3').write_bytes(b'forbidden second owner write')
'''
            with owned_spawn([sys.executable, '-B', '-c', script], env=options['env'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
                child.communicate(timeout=5)
                attempts.append(child.returncode)
            return SimpleNamespace(pid=99, poll=lambda: attempts[-1])
        with recovery.installed_delivery_admission(self.state), \
                patch.object(recovery, 'identity', return_value=None), \
                patch.object(recovery, 'lease_available', return_value=True), \
                patch.object(recovery.subprocess, 'Popen', side_effect=raced_spawn):
            recovery.tick(self.config, self.state)
        self.assertEqual(attempts, [75], 'a legacy launch attempt can occur, but its authoritative lease must fail')
        self.assertEqual(before, {name:(self.state/name).read_bytes() for name in before})

    def test_pid_free_postswap_mismatches_hold_without_signal_or_start(self):
        source = self.prepare_installed_recovery()
        record = self.install_recovered_delivery(source)
        request_journal = delivery.journal_path(self.state, record['id'])
        cases = [
            (delivery.server_path(self.previous), b'changed-old'),
            (delivery.server_path(self.installed), b'changed-target'),
            (self.journal, b'{}'),
            (self.state / updates.RECEIPT_FILE, b'{}'),
            (self.state / 'background-recovery.json', json.dumps({**self.config,'port':4621}).encode()),
        ]
        for path, value in cases:
            original = path.read_bytes()
            before = request_journal.read_bytes()
            with self.subTest(path=path):
                path.write_bytes(value)
                try:
                    self.run_once(launch=True)
                    self.assertEqual(self.calls, [])
                    self.assertEqual(request_journal.read_bytes(), before)
                finally:
                    path.write_bytes(original)

    def test_pid_free_recovery_refuses_a_same_byte_executable_with_another_inode(self):
        source = self.prepare_installed_recovery()
        record = self.install_recovered_delivery(source)
        binary = delivery.server_path(self.previous)
        replacement = binary.with_name('replacement-runtime')
        replacement.write_bytes(binary.read_bytes())
        replacement.chmod(binary.stat().st_mode & 0o777)
        os.replace(replacement, binary)
        self.run_once(launch=True)
        self.assertEqual(self.calls, [], 'the original executable inode must remain part of the no-signal proof')
        self.assertNotIn('startupRecovery', delivery.read_delivery_record(delivery.journal_path(self.state, record['id'])))

    def test_installed_proof_does_not_copy_environment_or_provider_launch_records(self):
        source = self.prepare_installed_recovery()
        config = {**self.config, 'environment':{'OPENAI_API_KEY':'never-copy-credential', 'CODEX_HOME':'/private/account'}}
        updates.write_json(self.state / 'background-recovery.json', config)
        record = self.installed_stage(source, dry_run=True)
        encoded = json.dumps(record['request']['installedDelivery'])
        self.assertNotIn('never-copy-credential', encoded)
        self.assertNotIn('/private/account', encoded)
        self.assertNotIn('environment', encoded)

    def test_delivery_recovery_label_and_readonly_command_match_their_named_owners(self):
        import re
        here = Path(__file__).resolve().parent
        paths = (here / 'recovery.cjs').read_text().split('function recoveryPaths(', 1)[1].split('\nfunction ', 1)[0]
        compact = re.sub(r'\s+', '', paths)
        formula = 'createHash("sha256").update(canonical).digest("hex").slice(0,' + str(recovery.DELIVERY_RECOVERY_LABEL_HEX_LENGTH) + ')'
        self.assertIn(formula, compact)
        self.assertIn('label:`' + recovery.DELIVERY_RECOVERY_LABEL_PREFIX + '${suffix}`', compact)
        repository = here.parents[3]
        main = (repository / 'workspaces/runtime/apps/runtime-server/src/main.rs').read_text()
        command = main.split('if args.first().map(String::as_str) == Some("state-check")', 1)[1].split('let port = parse_port(', 1)[0]
        self.assertIn('if args.len() != 2', command)
        self.assertIn('studio_runtime_store::check_state_readonly(PathBuf::from(state_dir).as_path())', command)
        self.assertIn('return Ok(())', command)

    def test_native_reader_exposes_only_fixed_noncredential_launch_facts(self):
        import struct
        raw = struct.pack('i', 3) + b'/app/server\0\0/app/server\0--port\0' + b'4620\0'
        raw += b'CODEX_AGENTS_STATE_DIR=/state\0CODEX_AGENTS_SUPERVISOR_MODE=1\0OPENAI_API_KEY=never-return\0\0apple=ignored\0'
        command, environment = desktop_process._split_darwin_launch(raw)
        self.assertEqual(command, ['/app/server', '--port', '4620'])
        self.assertEqual(environment, {'CODEX_AGENTS_STATE_DIR':'/state','CODEX_AGENTS_SUPERVISOR_MODE':'1'})
        self.assertNotIn('never-return', json.dumps(environment))
        duplicate = raw.replace(b'OPENAI_API_KEY=never-return', b'CODEX_AGENTS_SUPERVISOR_MODE=0')
        with self.assertRaises(ValueError):
            desktop_process._split_darwin_launch(duplicate)

    def test_native_kernel_reader_binds_birth_executable_and_open_state_files(self):
        import ctypes
        import struct
        raw = struct.pack('i', 3) + b'/app/studio-runtime-server\0\0/app/studio-runtime-server\0--port\0' + b'4620\0'
        raw += b'CODEX_AGENTS_STATE_DIR=/state\0CODEX_AGENTS_SUPERVISOR_MODE=1\0SECRET=not-returned\0\0'
        output = 'p11\nftxt\nD0x1\ni2\nn/app/studio-runtime-server\n'
        for index, name in enumerate(desktop_process.STARTUP_STATE_FILES):
            output += f'f{index + 3}\nD0x1\ni{index + 10}\nn/state/{name}\n'
        def bsd(pid, _flavor, _arg, pointer, size):
            info = pointer._obj
            info.pid, info.uid, info.status = pid, os.geteuid(), 2
            info.start_seconds, info.start_microseconds = 100, 123456
            self.assertEqual(size, 136, 'Darwin proc_bsdinfo layout from sys/proc_info.h')
            return size
        def executable(_pid, buffer, _size):
            buffer.value = b'/app/studio-runtime-server'
            return len(buffer.value)
        library = SimpleNamespace(proc_pidinfo=unittest.mock.Mock(side_effect=bsd),
                                  proc_pidpath=unittest.mock.Mock(side_effect=executable))
        with patch.object(desktop_process.sys, 'platform', 'darwin'), \
                patch.object(ctypes, 'CDLL', return_value=library), \
                patch.object(desktop_process, '_darwin_launch_data', return_value=raw), \
                patch.object(desktop_process.subprocess, 'check_output', return_value=output):
            facts = desktop_process.startup_backend_facts(11, Path('/state'))
        self.assertEqual(facts['birth'], '100:123456')
        self.assertEqual(facts['textFiles'], [{'device':1,'inode':2}])
        self.assertEqual(facts['openFiles']['runtime.lock'], {'device':1,'inode':10})
        self.assertEqual(facts['argv'], ['/app/studio-runtime-server','--port','4620'])
        self.assertNotIn('not-returned', json.dumps(facts))

    def test_legacy_argument_reader_does_not_parse_unrelated_environment(self):
        import struct
        raw = struct.pack('i', 1) + b'/app/server\0\0/app/server\0invalid-environment\0'
        with patch.object(desktop_process.sys, 'platform', 'darwin'), \
                patch.object(desktop_process, '_darwin_launch_data', return_value=raw):
            self.assertEqual(desktop_process.process_launch_command(11), ['/app/server'])
        with self.assertRaises(ValueError):
            desktop_process._split_darwin_launch(raw)

    def test_rust_state_owner_contract_has_one_named_source(self):
        import re
        owner = Path(__file__).resolve().parents[4] / 'workspaces/runtime/packages/runtime-store/src/state_owner.rs'
        source = owner.read_text()
        self.assertEqual(recovery.STARTUP_STATE_OWNER_FILE,
                         re.search(r'pub const STATE_OWNER_FILE: &str = "([^"]+)";', source)[1])
        self.assertEqual(recovery.STARTUP_STATE_FORMAT_VERSION,
                         int(re.search(r'pub const STATE_FORMAT_VERSION: i64 = (\d+);', source)[1]))
        store = owner.with_name('lib.rs').read_text()
        names = [re.search(pattern, store)[1] for pattern in [
            r'let lock_path = state_dir.join\("([^"]+)"\);',
            r'let canvas_path = root.join\("([^"]+)"\);',
            r'let analytics_path = root.join\("([^"]+)"\);',
        ]]
        self.assertEqual(tuple(names), desktop_process.STARTUP_STATE_FILES)


if __name__ == '__main__':
    unittest.main()
