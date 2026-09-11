#!/usr/bin/env python3
"""Disconnected choices preserve native identities and local credentials."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('accounts_contract', Path(__file__).with_name('accounts-contract.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class DisconnectContract(unittest.TestCase):
    setUp = module.AccountsContract.setUp
    tearDown = module.AccountsContract.tearDown
    def test_disconnect_preserves_identity_across_restart_and_discovery(self):
        profile = self.home / 'Projects' / 'sample' / '.codex'
        module.auth(profile, 'account-two')
        key = self.store.register(str(profile))
        credential = (profile / 'auth.json').read_bytes()
        self.store.discover()
        self.store.disconnect(key)
        self.store.disconnect(key)
        restored = module.AccountStore(self.root / 'state')
        restored.discover()
        self.assertTrue(restored.get(key)['disconnected'])
        self.assertEqual(restored.home(key), profile.resolve())
        self.assertEqual(restored.get(key)['accountId'], 'account-two')
        self.assertEqual((profile / 'auth.json').read_bytes(), credential)
        with self.assertRaises(ValueError):
            restored.default(key)
        restored.reconnect(key)
        restored.default(key)
        self.assertFalse(restored.get(key).get('disconnected', False))
        self.assertEqual(restored.default(), key)

    def test_default_disconnect_requires_another_connected_account(self):
        with self.assertRaises(ValueError):
            self.store.disconnect('default')
        self.assertFalse(self.store.get('default').get('disconnected', False))
        profile = self.home / 'other'
        module.auth(profile, 'account-two')
        key = self.store.register(str(profile))
        self.store.disconnect('default')
        self.assertEqual(self.store.default(), key)
        self.assertEqual(self.store.home('default'), self.primary.resolve())
        with self.assertRaises(ValueError):
            self.store.disconnect(key)
        self.assertEqual(self.store.default(), key)

    def test_reconnect_rejects_changed_native_identity(self):
        profile = self.home / 'other'
        module.auth(profile, 'account-two')
        key = self.store.register(str(profile))
        self.store.disconnect(key)
        module.auth(profile, 'account-three')
        with self.assertRaises(ValueError):
            self.store.reconnect(key)
        self.assertTrue(self.store.get(key)['disconnected'])
        self.assertEqual(self.store.get(key)['accountId'], 'account-two')

project_spec = importlib.util.spec_from_file_location('project_links', Path(__file__).with_name('project-account-links-contract.py'))
project_fixture = importlib.util.module_from_spec(project_spec)
project_spec.loader.exec_module(project_fixture)


class ProjectDisconnectContract(unittest.TestCase):
    setUp = project_fixture.Links.setUp
    tearDown = project_fixture.Links.tearDown
    link = project_fixture.Links.link

    def test_accepted_project_default_replay_survives_disconnect(self):
        project = self.link(['default', self.other], self.other)
        self.store.accounts.disconnect(self.other)
        self.assertEqual(project, self.link(['default', self.other], self.other))
        self.assertEqual(project['accountKey'], self.other)

    def test_disconnected_membership_cannot_be_added_or_become_default(self):
        original = self.link(['default'])
        self.store.accounts.disconnect(self.other)
        with self.assertRaises(ValueError):
            self.link(['default', self.other], revision=1)
        self.assertIn(original, self.store.projects()['items'])
        self.store.accounts.reconnect(self.other)
        linked = self.link(['default', self.other], revision=1)
        self.store.accounts.disconnect(self.other)
        # Exact retry preserves existing project identity and membership.
        self.assertEqual(linked, self.link(['default', self.other], revision=1))
        with self.assertRaises(ValueError):
            self.link(['default', self.other], self.other, revision=2)
        self.assertIn(linked, self.store.projects()['items'])


transfer_spec = importlib.util.spec_from_file_location('transfer_fixture', Path(__file__).with_name('account-transfer-contract.py'))
transfer_fixture = importlib.util.module_from_spec(transfer_spec)
transfer_spec.loader.exec_module(transfer_fixture)


class TransferDisconnectContract(transfer_fixture.TransferContract):
    def test_new_transfer_rejects_disconnected_destination(self):
        self.runtime.accounts.disconnect(self.other_key)
        with self.assertRaisesRegex(ValueError, "Reconnect"):
            self.start_transfer()
        self.assertNotIn('accountTransferId', self.runtime.agent(self.lead_agent['id']))

    def test_transfer_replay_and_pending_operation_keep_original_identity(self):
        operation = self.start_transfer()
        self.runtime.accounts.disconnect(self.other_key)
        self.assertEqual(operation, self.store.request(self.lead_agent['id'], self.other_key, operation['id']))
        self.assertEqual(operation['id'], self.start_transfer()['id'])
        self.tick()
        self.until(lambda: len(self.pending) == 1)
        self.complete_fork()
        self.assertEqual(self.runtime.agent(self.lead_agent['id'])['accountKey'], self.other_key)


if __name__ == '__main__':
    suite = unittest.TestSuite()
    for case in (DisconnectContract, ProjectDisconnectContract):
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(case))
    for name in ('test_new_transfer_rejects_disconnected_destination', 'test_transfer_replay_and_pending_operation_keep_original_identity'):
        suite.addTest(TransferDisconnectContract(name))
    result = unittest.TextTestRunner().run(suite)
    raise SystemExit(not result.wasSuccessful())
