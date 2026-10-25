"""Local delivery boundaries, using disposable bundles and a scripted tool runner."""
import hashlib
import http.server
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import urllib.error
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import desktop_updates as updates
import deliver_server as delivery


class DeliveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.state = root / 'state'
        self.state.mkdir(mode=0o700)
        self.installed = root / 'applications' / updates.APP_NAME
        self.source = root / 'package' / updates.APP_NAME
        self.receipts = root / 'receipts'
        self.commit = 'b' * 40
        self.build = {'version': 1, 'builtAt': 2000.5, 'commit': self.commit,
                      'commitTime': 2000, 'branch': 'fix/example'}
        self.make_app(self.installed, {**self.build, 'builtAt': 1000.5, 'commit': 'a' * 40}, b'old')
        self.make_app(self.source, self.build, b'new')
        self.config = {'version': 1, 'port': 4620, 'python': '/fixture/python3',
                       'codex': '/fixture/codex', 'processSupervisor': '/fixture/supervisor',
                       'stateDir': str(self.state), 'backend': 'rust', 'supervisorEnabled': True,
                       'runtimeServer': str(self.server(self.installed)),
                       'resources': str(self.installed / 'Contents/Resources')}
        updates.write_json(self.state / 'background-recovery.json', self.config)
        with sqlite3.connect(self.state / 'supervisor.sqlite3') as database:
            database.execute('CREATE TABLE handles(id TEXT, pid INTEGER, generation INTEGER, closed_at REAL)')
            database.execute('INSERT INTO handles VALUES(?, ?, ?, NULL)', ('account:example', 31, 4))
        self.calls = []
        self.identity = {'application': 'codex-agents', 'protocol': 1,
                         'pid': 11, 'stateDir': str(self.state), 'backendKind': 'rust',
                         'supervisorMode': True,
                         'backendBuild': hashlib.sha256(b'old').hexdigest()}
        proof = patch.object(delivery.recovery, 'verify_backend_process')
        self.proof = proof.start()
        self.addCleanup(proof.stop)

    @staticmethod
    def server(app):
        return app / 'Contents/Resources/workspace/bin/studio-runtime-server'

    def make_app(self, app, build, binary):
        self.server(app).parent.mkdir(parents=True)
        self.server(app).write_bytes(binary)
        updates.write_json(app / updates.BUILD_STAMP, build)

    def run_tool(self, argv, timeout):
        self.calls.append(argv)
        if argv[0] == updates.DITTO:
            shutil.copytree(argv[1], argv[2], symlinks=True)
        text = 'designated => identifier "local.codex.agents" and anchor trusted\n'
        return subprocess.CompletedProcess(argv, 0, text, '')

    def stage(self, **kwargs):
        return delivery.stage(self.source, self.commit, self.installed, self.receipts,
                              observe=lambda: dict(self.identity), run=self.run_tool, **kwargs)

    def copy_count(self):
        return sum(call[0] == updates.DITTO for call in self.calls)

    def test_dry_run_has_no_state_or_install_effect(self):
        before = {str(p.relative_to(self.state)): p.read_bytes()
                  for p in self.state.rglob('*') if p.is_file()}
        result = self.stage(dry_run=True)
        self.assertEqual(result['phase'], 'dry_run')
        self.assertEqual(self.copy_count(), 0)
        self.assertEqual(before, {str(p.relative_to(self.state)): p.read_bytes()
                                  for p in self.state.rglob('*') if p.is_file()})
        self.assertEqual(self.server(self.installed).read_bytes(), b'old')

    def test_workspace_recovery_layout_passes_preflight_without_changes(self):
        self.config['resources'] = str(self.installed / 'Contents/Resources/workspace')
        updates.write_json(self.state / 'background-recovery.json', self.config)
        before = {str(p.relative_to(self.state)): p.read_bytes()
                  for p in self.state.rglob('*') if p.is_file()}
        result = self.stage(dry_run=True)
        self.assertEqual(result['phase'], 'dry_run')
        self.proof.assert_called_once_with(self.identity, self.config)
        self.assertEqual(self.copy_count(), 0)
        self.assertEqual(before, {str(p.relative_to(self.state)): p.read_bytes()
                                  for p in self.state.rglob('*') if p.is_file()})
        self.assertEqual(self.server(self.installed).read_bytes(), b'old')

    def test_recovery_resources_accept_no_other_bundle_location(self):
        resources = self.installed / 'Contents/Resources'
        for folder in (self.installed, resources.parent, resources / 'workspace-other',
                       resources / 'workspace/bin', resources / 'workspace/nested',
                       self.source / 'Contents/Resources/workspace'):
            with self.subTest(folder=folder):
                self.config['resources'] = str(folder)
                updates.write_json(self.state / 'background-recovery.json', self.config)
                with self.assertRaisesRegex(delivery.DeliveryError, 'recovery configuration'):
                    self.stage(dry_run=True)
                self.proof.assert_not_called()
                self.assertEqual(self.copy_count(), 0)
                self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_workspace_layout_keeps_binary_hash_state_and_process_proofs(self):
        self.config['resources'] = str(self.installed / 'Contents/Resources/workspace')
        updates.write_json(self.state / 'background-recovery.json', self.config)
        valid_identity = dict(self.identity)
        valid_config = dict(self.config)
        cases = (
            ({'backendBuild': '0' * 64}, {}, None, 'installed binary'),
            ({'stateDir': str(self.state / 'another')}, {}, None, 'state|recovery'),
            ({}, {'runtimeServer': str(self.server(self.source))}, None, 'recovery configuration'),
            ({}, {}, RuntimeError('The reported PID does not own the listener'), 'listener'),
        )
        for changed, config_changed, failure, message in cases:
            with self.subTest(changed=changed, failure=failure):
                self.identity = {**valid_identity, **changed}
                self.config = {**valid_config, **config_changed}
                updates.write_json(self.state / 'background-recovery.json', self.config)
                self.proof.side_effect = failure
                with self.assertRaisesRegex(delivery.DeliveryError, message):
                    self.stage(dry_run=True)
                self.assertEqual(self.copy_count(), 0)
                self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_lost_submission_response_reuses_exact_request_and_copy(self):
        first = self.stage()
        second = self.stage()
        self.assertEqual(first, second)
        self.assertEqual(self.copy_count(), 1)
        pending = updates.read_pending(self.state)
        self.assertEqual(pending['id'], first['id'])
        self.assertEqual(pending['build'], self.build)
        self.assertEqual(self.server(self.installed).read_bytes(), b'old')
        self.assertFalse(any(call[0] in ('kill', 'launchctl', '/bin/kill') for call in self.calls))

    def test_existing_invalid_pending_file_is_not_overwritten(self):
        pending = self.state / updates.PENDING_FILE
        pending.write_text('{interrupted other writer')
        with self.assertRaisesRegex(delivery.DeliveryError, 'pending'):
            self.stage()
        self.assertEqual(pending.read_text(), '{interrupted other writer')
        self.assertEqual(self.copy_count(), 0)

    def test_same_request_refuses_changed_package_bytes(self):
        self.stage()
        self.server(self.source).write_bytes(b'changed')
        with self.assertRaisesRegex(delivery.DeliveryError, 'different'):
            self.stage()
        self.assertEqual(self.copy_count(), 1)

    def test_missing_pending_and_receipt_keep_uncertain_submission(self):
        self.stage()
        (self.state / updates.PENDING_FILE).unlink()
        with self.assertRaisesRegex(delivery.DeliveryError, 'unknown'):
            self.stage()
        self.assertEqual(self.copy_count(), 1)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_lost_cache_and_removed_stage_cannot_repeat_an_uncertain_submission(self):
        first = self.stage()
        (self.state / updates.PENDING_FILE).unlink()
        shutil.rmtree(Path(first['app']).parent)
        shutil.rmtree(self.receipts)
        self.receipts = self.receipts.with_name('other-receipts')
        with self.assertRaisesRegex(delivery.DeliveryError, 'unknown'):
            self.stage()
        self.assertEqual(self.copy_count(), 1)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_unresolved_provider_reservations_refuse_before_copy_or_publication(self):
        with sqlite3.connect(self.state / 'supervisor.sqlite3') as database:
            database.execute('INSERT INTO handles VALUES(?, ?, ?, NULL)', ('reserved', 0, 0))
        with self.assertRaisesRegex(delivery.DeliveryError, 'reservation|provider'):
            self.stage()
        self.assertEqual(self.copy_count(), 0)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_state_journal_is_durable_before_pending_publication(self):
        synchronized = []
        original_sync = delivery.sync_directory
        original_publish = delivery.publish_pending
        def sync(path):
            synchronized.append(path)
            original_sync(path)
        def publish(state, pending):
            journal = state / 'local-deliveries' / (pending['id'] + '.json')
            self.assertTrue(journal.exists())
            self.assertEqual(delivery.read_delivery_record(journal)['phase'], 'publishing')
            self.assertIn(state, synchronized)
            self.assertIn(journal.parent, synchronized)
            original_publish(state, pending)
        with patch.object(delivery, 'sync_directory', side_effect=sync), \
                patch.object(delivery, 'publish_pending', side_effect=publish):
            self.stage()

    def test_invalid_receipts_never_publish_another_request(self):
        first = self.stage()
        (self.state / updates.PENDING_FILE).unlink()
        for invalid in ({}, {**first, 'phase': 'unknown'},
                        {key: value for key, value in first.items() if key != 'handles'}):
            with self.subTest(invalid=invalid):
                updates.write_json(self.receipts / delivery.CONTRACT['receiptFile'], invalid)
                with self.assertRaisesRegex(delivery.DeliveryError, 'receipt|record|phase'):
                    self.stage()
                self.assertFalse((self.state / updates.PENDING_FILE).exists())
                self.assertEqual(self.copy_count(), 1)

    def test_terminal_owner_receipt_survives_a_different_local_receipt_directory(self):
        first = self.stage()
        (self.state / updates.PENDING_FILE).unlink()
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': first['id'], 'outcome': 'failed'})
        self.receipts = self.receipts.with_name('other-receipts')
        with self.assertRaisesRegex(delivery.DeliveryError, 'failed'):
            self.stage()
        self.assertFalse((self.state / updates.PENDING_FILE).exists())
        self.assertEqual(self.copy_count(), 1)

    def test_another_application_cannot_authorize_a_delivery(self):
        self.identity['application'] = 'another-application'
        with self.assertRaises(delivery.DeliveryError):
            self.stage()
        self.assertEqual(self.copy_count(), 0)

    def test_an_unverified_process_or_listener_cannot_authorize_a_delivery(self):
        self.proof.side_effect = RuntimeError('The reported PID does not own the listener')
        with self.assertRaisesRegex(delivery.DeliveryError, 'listener'):
            self.stage()
        self.proof.assert_called_once()
        self.assertEqual(self.copy_count(), 0)

    def test_another_protocol_or_observed_port_is_refused(self):
        self.identity['protocol'] = 2
        with self.assertRaises(delivery.DeliveryError):
            self.stage()
        self.identity['protocol'] = 1
        self.identity['_deliveryPort'] = 4621
        with self.assertRaisesRegex(delivery.DeliveryError, 'port'):
            self.stage()
        self.assertEqual(self.copy_count(), 0)

    def test_desktop_observation_never_follows_a_redirect(self):
        hits = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                hits.append(self.path)
                if self.path == '/api/desktop':
                    self.send_response(302)
                    self.send_header('Location', '/other')
                    self.end_headers()
                else:
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(b'{}')
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            with self.assertRaises(urllib.error.HTTPError):
                delivery.observe_desktop('http://127.0.0.1:' + str(server.server_port))
            self.assertEqual(hits, ['/api/desktop'])
        finally:
            server.shutdown()
            thread.join()
            server.server_close()

    def test_backend_of_another_state_is_refused_before_copy(self):
        self.identity['stateDir'] = str(self.state / 'another')
        with self.assertRaises(delivery.DeliveryError):
            self.stage()
        self.assertEqual(self.copy_count(), 0)

    def test_receipt_directory_cannot_change_a_sealed_bundle_or_runtime_state(self):
        for folder in (self.source, self.installed, self.state):
            with self.subTest(folder=folder):
                self.receipts = folder / 'delivery'
                with self.assertRaisesRegex(delivery.DeliveryError, 'receipt directory'):
                    self.stage()
                self.assertFalse(self.receipts.exists())
                self.assertEqual(self.copy_count(), 0)

    def test_receipt_directory_sync_failure_prevents_a_pending_request(self):
        with patch.object(delivery, 'sync_directory', side_effect=OSError('directory sync failed')):
            with self.assertRaisesRegex(OSError, 'directory sync'):
                self.stage()
        self.assertFalse((self.state / updates.PENDING_FILE).exists())
        self.assertEqual(self.copy_count(), 0)

    def test_pending_publish_does_not_replace_another_writer(self):
        path = self.state / updates.PENDING_FILE
        updates.write_json(path, {'id': 'other'})
        with self.assertRaises(FileExistsError):
            delivery.publish_pending(self.state, {'id': 'mine'})
        self.assertEqual(json.loads(path.read_text()), {'id': 'other'})

    def test_install_receipt_without_matching_backend_is_not_success(self):
        staged = self.stage()
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': staged['id'], 'outcome': 'installed'})
        with self.assertRaisesRegex(delivery.DeliveryError, 'unknown|match'):
            delivery.verify_completion(staged, self.installed, lambda: dict(self.identity))

    def test_changed_provider_generation_is_not_success(self):
        staged = self.stage()
        shutil.copytree(self.source, self.installed, dirs_exist_ok=True)
        self.identity.update(pid=12, backendBuild=hashlib.sha256(b'new').hexdigest())
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': staged['id'], 'outcome': 'installed'})
        with sqlite3.connect(self.state / 'supervisor.sqlite3') as database:
            database.execute('UPDATE handles SET generation = 5')
        with self.assertRaisesRegex(delivery.DeliveryError, 'provider'):
            delivery.verify_completion(staged, self.installed, lambda: dict(self.identity))

    def test_success_requires_receipt_bundle_backend_and_retained_providers(self):
        staged = self.stage()
        shutil.copytree(self.source, self.installed, dirs_exist_ok=True)
        self.identity.update(pid=12, backendBuild=hashlib.sha256(b'new').hexdigest())
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': staged['id'], 'outcome': 'installed'})
        result = delivery.verify_completion(staged, self.installed, lambda: dict(self.identity))
        self.assertEqual(result['phase'], 'installed')
        self.assertEqual(result['id'], staged['id'])

    def test_definite_provider_loss_does_not_wait_or_repeat_the_install(self):
        staged = self.stage()
        shutil.copytree(self.source, self.installed, dirs_exist_ok=True)
        self.identity.update(pid=12, backendBuild=hashlib.sha256(b'new').hexdigest())
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': staged['id'], 'outcome': 'installed'})
        with sqlite3.connect(self.state / 'supervisor.sqlite3') as database:
            database.execute('UPDATE handles SET generation = 5')
        with patch.object(delivery.time, 'sleep', side_effect=AssertionError('must not wait')):
            with self.assertRaisesRegex(delivery.DeliveryError, 'provider'):
                delivery.wait_for_completion(staged, self.installed, self.receipts,
                                             lambda: dict(self.identity))
        self.assertEqual(self.copy_count(), 1)

    def test_matching_live_bundle_needs_no_new_request_after_local_receipts_are_lost(self):
        shutil.copytree(self.source, self.installed, dirs_exist_ok=True)
        self.identity.update(pid=12, backendBuild=hashlib.sha256(b'new').hexdigest())
        result = self.stage()
        self.assertEqual(result['phase'], 'already_installed')
        self.assertEqual(self.copy_count(), 0)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_retry_during_backend_start_only_observes_the_same_install(self):
        staged = self.stage()
        shutil.copytree(self.source, self.installed, dirs_exist_ok=True)
        (self.state / updates.PENDING_FILE).unlink()
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'id': staged['id'], 'outcome': 'installed'})
        def backend_starts(_seconds):
            self.identity.update(pid=12, backendBuild=hashlib.sha256(b'new').hexdigest())
        with patch.object(delivery.time, 'sleep', side_effect=backend_starts):
            result = delivery.resume_saved(self.source, self.commit, self.installed,
                                           self.receipts, lambda: dict(self.identity))
        self.assertEqual(result['phase'], 'installed')
        self.assertEqual(self.copy_count(), 1)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())


if __name__ == '__main__':
    unittest.main()
