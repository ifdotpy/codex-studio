"""Verify Studio name synchronization against installed Codex, without inference."""
import importlib.util
from pathlib import Path
import unittest


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


native = load('native_fixture', 'native-primitives-integration.py')
contract = load('names_fixture', 'session-names-contract.py')


class NativeNames(unittest.TestCase):
    def test_names_reach_native_metadata_without_a_model_turn(self):
        fixture = contract.SessionNamesContract()
        fixture.setUp()
        try:
            with native.native_server() as (server, tid, provider, events, restart):
                rt = fixture.runtime
                a = rt.new_lead({'cwd': str(fixture.root)})
                with rt.lock, rt.db() as db:
                    a.update(threadId=tid, name='First native title')
                    rt.put(db, 'agents', a)
                rt.connect = lambda _='default': server
                for title in ['First native title', 'Название после изменения']:
                    rt.rename(a['id'], title)
                    fixture.tick()
                    contract.f.f.eventually(lambda: rt.agent(a['id']).get('nativeNameSynced', {}).get('name') == title)
                    self.assertEqual(server.call('thread/read', {'threadId': tid, 'includeTurns': False})['thread']['name'], title)
                self.assertEqual(len(provider.requests), 0)
                print('PASS native names in Studio and Codex; zero model requests')
        finally:
            fixture.tearDown()


if __name__ == '__main__': unittest.main(verbosity=2)
