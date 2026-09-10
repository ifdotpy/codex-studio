"""Native title synchronization and destination account selection, no model calls."""
import concurrent.futures
import importlib.util
from pathlib import Path
import unittest
import uuid

spec = importlib.util.spec_from_file_location('account_fixture', Path(__file__).with_name('runtime-accounts-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_session_names import session_names, identity


class SessionNamesContract(f.AccountContracts):
    # Reuse isolated account setup, not the inherited test suite.
    def server(self, lead):
        server = self.runtime.connect(lead.get('accountKey', 'default'))
        original = server.submit
        pending = []
        def submit(method, params):
            if method != 'thread/name/set':
                return original(method, params)
            future = concurrent.futures.Future()
            pending.append((params, future))
            return future
        server.submit = submit
        return server, pending

    def tick(self):
        store = session_names(self.runtime)
        store.next_scan = 0
        store.tick()
        return store

    def test_name_receipt_race_and_quiet_after_sync(self):
        a = self.lead()
        server, pending = self.server(a)
        store = self.tick()
        f.f.eventually(lambda: len(pending) == 1)
        self.assertEqual(pending[0][0], {'threadId': a['threadId'], 'name': 'Lead'})
        self.runtime.rename(a['id'], 'Renamed while pending')
        self.tick()
        self.assertEqual(len(pending), 1)
        pending[0][1].set_result({})
        self.assertNotIn('nativeNameSynced', self.runtime.agent(a['id']))
        self.tick()
        f.f.eventually(lambda: len(pending) == 2)
        pending[1][1].set_result({})
        self.assertEqual(self.runtime.agent(a['id'])['nativeNameSynced']['name'], 'Renamed while pending')
        for _ in range(5): self.tick()
        self.assertEqual(len(pending), 2)

    def test_failure_keeps_studio_name_and_retries_with_backoff(self):
        a = self.lead(); server, pending = self.server(a)
        self.runtime.rename(a['id'], 'Keep this title')
        self.tick(); f.f.eventually(lambda: len(pending) == 1)
        pending[0][1].set_exception(RuntimeError('Native connection failed'))
        agent = self.runtime.agent(a['id'])
        self.assertEqual(agent['name'], 'Keep this title')
        self.assertIn('Native connection failed', agent['nativeNameFailure']['error'])
        self.tick(); self.assertEqual(len(pending), 1)
        with self.runtime.lock, self.runtime.db() as db:
            agent['nativeNameFailure']['retryAt'] = 0
            self.runtime.put(db, 'agents', agent)
        self.tick(); f.f.eventually(lambda: len(pending) == 2)
        pending[1][1].set_result({})
        self.assertNotIn('nativeNameFailure', self.runtime.agent(a['id']))

    def test_draft_waits_for_thread_and_auto_title_is_copied(self):
        a = self.runtime.new_lead({'cwd': str(self.root)})
        self.tick(); self.assertFalse(session_names(self.runtime).pending)
        a = self.runtime.prepare(a); server, pending = self.server(a)
        with self.runtime.lock, self.runtime.db() as db:
            a.update(name='Model generated title', needsTitle=False)
            self.runtime.put(db, 'agents', a)
        self.tick(); f.f.eventually(lambda: len(pending) == 1)
        self.assertEqual(pending[0][0]['name'], 'Model generated title')
        pending[0][1].set_result({})

    def test_account_scoped_sync_and_native_thread_replacement(self):
        a = self.lead(); b = self.lead(self.other_key)
        _, one = self.server(a); _, two = self.server(b)
        self.assertEqual(a['threadId'], b['threadId'])
        self.tick(); f.f.eventually(lambda: len(one) == len(two) == 1)
        one[0][1].set_result({}); two[0][1].set_result({})
        self.assertEqual(self.runtime.agent(b['id'])['nativeNameSynced']['accountKey'], self.other_key)
        with self.runtime.lock, self.runtime.db() as db:
            b = self.runtime.agent(b['id'], db); b['threadId'] = 'replacement-thread'
            self.runtime.put(db, 'agents', b)
        self.tick(); f.f.eventually(lambda: len(two) == 2)
        self.assertEqual(two[1][0]['threadId'], 'replacement-thread')
        two[1][1].set_result({})

    def test_new_project_does_not_inherit_previous_chat_account(self):
        source = self.root / 'lumina'; source.mkdir()
        previous = self.runtime.create({'name': 'Lumina', 'prompt': '', 'cwd': str(source), 'account_key': self.other_key}, draft=True)
        previous = self.runtime.prepare(previous)
        target = self.root / 'assistant'; target.mkdir()
        request = {'id': str(uuid.uuid4()), 'previous': previous['id'], 'cwd': str(target)}
        a = self.runtime.new_lead(request)
        self.assertEqual(a['accountKey'], 'default')
        self.assertEqual(Path(a['cwd']), target.resolve())
        self.runtime.accounts.default(self.other_key)
        self.assertEqual(self.runtime.new_lead(request)['id'], a['id'])
        self.assertEqual(self.runtime.new_lead(request)['accountKey'], 'default')

    def test_explicit_new_chat_keeps_requested_identity_after_an_empty_chat(self):
        target = self.root / 'assistant'; target.mkdir()
        first = self.runtime.new_lead({'cwd': str(target)})
        request = {'id': str(uuid.uuid4()), 'previous': first['id'], 'cwd': str(target), 'reuse_empty': False}
        second = self.runtime.new_lead(request)
        self.assertEqual(second['id'], request['id'])
        self.assertNotEqual(second['id'], first['id'])
        self.assertEqual(self.runtime.new_lead(request)['id'], second['id'])

    def test_project_preference_beats_global_default_and_explicit_wins(self):
        target = self.root / 'assistant'; target.mkdir()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'projects', {'id': str(target), 'path': str(target), 'accountKey': self.other_key})
        a = self.runtime.new_lead({'cwd': str(target)})
        self.assertEqual(a['accountKey'], self.other_key)
        b = self.runtime.new_lead({'cwd': str(target), 'account_key': 'default'})
        self.assertEqual(b['accountKey'], 'default')


if __name__ == '__main__':
    suite = unittest.TestSuite(SessionNamesContract(name) for name in SessionNamesContract.__dict__ if name.startswith('test_'))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
