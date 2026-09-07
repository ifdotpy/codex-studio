"""Isolated checkpoint, replay, draft branch and rollback contracts."""
import contextlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_sync import SyncStore

with tempfile.TemporaryDirectory() as directory:
    @contextlib.contextmanager
    def connect():
        db = sqlite3.connect(Path(directory) / 'state.sqlite3')
        try:
            with db:
                yield db
        finally:
            db.close()
    with connect() as db:
        db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
        db.execute('CREATE VIRTUAL TABLE message_search USING fts5(text)')
    state = {'nodes': [], 'token': 'secret', 'at': 12}
    def transcript(key):
        if key != 'one':
            raise ValueError('Gone')
        return {'items': [{'id': 'message', 'text': 'hello'}]}
    store = SyncStore(connect, lambda: state, transcript)
    assert store.identity() == SyncStore(connect, lambda: state, transcript).identity()
    first = store.pull('state')
    assert 'token' not in json.loads(first['documents'][0]['payload'])
    assert not store.pull('state', first['checkpoint']['seq'])['documents']
    state['nodes'] = [{'id': 'one'}]
    second = store.pull('state', first['checkpoint']['seq'])
    assert second['checkpoint']['seq'] > first['checkpoint']['seq']
    assert second == store.pull('state', first['checkpoint']['seq'])
    assert store.pull('transcript:gone')['documents'][0]['_deleted']
    generation = store.generation()
    with connect() as db:
        db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('one', '{}'))
    assert store.generation() > generation
    generation = store.generation()
    try:
        with connect() as db:
            db.execute('DELETE FROM runtime_agents')
            raise RuntimeError()
    except RuntimeError:
        pass
    assert store.generation() == generation
    def row(device, text):
        return {'newDocumentState': {'id': device + ':one', 'seq': 0, 'payload': json.dumps({'device': device, 'session': 'one', 'text': text})}}
    assert store.push_drafts([row('phone', 'first'), row('desktop', 'second')]) == []
    drafts = store.pull('drafts')['documents']
    assert len(drafts) == 2
    assert len(store.push_drafts([row('phone', 'conflict')])) == 1
    assert len(store.pull('drafts')['documents']) == 2
    try:
        store.push_drafts([row('third', 'valid'), {'newDocumentState': {'id':'bad','payload':'{}'}}])
        raise AssertionError('Invalid batch accepted')
    except ValueError:
        pass
    assert len(store.pull('drafts')['documents']) == 2
    # JavaScript key order and JSON whitespace do not create a concurrent edit.
    original = row('phone', 'first')['newDocumentState']
    assert original['payload'] != next(d for d in drafts if d['id'] == 'phone:one')['payload']
    assert store.push_drafts([{'newDocumentState': original}]) == []
    cleared = row('phone', '')['newDocumentState']
    assert store.push_drafts([{'newDocumentState': cleared, 'assumedMasterState': original}]) == []
    current_phone = next(d for d in store.pull('drafts')['documents'] if d['id'] == 'phone:one')
    assert json.loads(current_phone['payload'])['text'] == ''
    assert store.push_drafts([{'newDocumentState': cleared, 'assumedMasterState': original}]) == []
    # A real intervening edit remains a conflict, even when its keys are reordered.
    stale = store.push_drafts([{'newDocumentState': row('phone', 'stale')['newDocumentState'],
                               'assumedMasterState': original}])
    assert len(stale) == 1 and json.loads(stale[0]['payload'])['text'] == ''
    phone = next(d for d in store.pull('drafts')['documents'] if d['id'] == 'phone:one')
    deleted = {**phone, '_deleted': True}
    assert store.push_drafts([{'newDocumentState': deleted, 'assumedMasterState': phone}]) == []
    conflicts = store.push_drafts([{'newDocumentState': phone, 'assumedMasterState': phone}])
    assert len(conflicts) == 1 and conflicts[0]['_deleted']
    for invalid in [None, {}, {'newDocumentState': None}, {'newDocumentState': {'id':'x','payload':[]}}]:
        try:
            store.push_drafts([invalid])
            raise AssertionError('Invalid row accepted')
        except ValueError:
            pass
print('sync contract passed')
