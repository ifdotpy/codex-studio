"""Isolated checkpoint, replay, draft branch and rollback contracts."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import contextlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
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
        db.execute('CREATE TABLE runtime_items(id TEXT PRIMARY KEY, record TEXT)')
        db.execute('CREATE VIRTUAL TABLE message_search USING fts5(text)')
    def transcript(key):
        if key != 'one':
            raise ValueError('Gone')
        return {'items': [{'id': 'message', 'text': 'hello'}]}
    store = SyncStore(connect, transcript)
    assert store.identity() == SyncStore(connect, transcript).identity()
    for scope in ('state', 'state:chat'):
        try:
            store.pull(scope)
            raise AssertionError(f'retired sync scope accepted: {scope}')
        except ValueError as error:
            assert str(error) == 'Invalid sync scope'
    assert store.pull('transcript:gone')['documents'][0]['_deleted']
    # A live upgrade accepts an old checkpoint without rewriting its payload.
    old_payload = json.dumps(transcript('one'), sort_keys=True, separators=(',', ':'))
    with connect() as db:
        db.execute('INSERT INTO sync_documents(scope,id,payload,deleted) VALUES (?,?,?,0)',
                   ('transcript:one', 'transcript:one', old_payload))
        old_seq = db.execute("SELECT seq FROM sync_documents WHERE scope='transcript:one'").fetchone()[0]
        db.execute('CREATE TRIGGER sync_watch_runtime_items_UPDATE AFTER UPDATE ON runtime_items '
                   'BEGIN UPDATE sync_generation SET value=value+1 WHERE id=1; END')
    upgraded = SyncStore(connect, transcript)
    replacement = upgraded.pull('transcript:one', old_seq)
    assert replacement['documents']
    assert replacement['documents'][0]['seq'] > old_seq
    assert 'delta' not in json.loads(replacement['documents'][0]['payload'])
    assert upgraded.pull('transcript:one')['documents'][0]['seq'] == replacement['documents'][0]['seq']
    with connect() as db:
        assert not db.execute("SELECT 1 FROM sqlite_master WHERE name='sync_watch_runtime_items_UPDATE'").fetchone()
        assert not db.execute("SELECT 1 FROM sqlite_master WHERE name LIKE 'sync_watch_runtime_agents_%'").fetchone()
        assert db.execute("SELECT payload FROM sync_documents WHERE scope='transcript:one'").fetchone()[0] == old_payload
    generation = store.generation()
    with connect() as db:
        db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('one', '{}'))
    assert store.generation() > generation
    from concurrent.futures import ThreadPoolExecutor
    concurrent = SyncStore(connect, lambda key: {'key': key})
    with ThreadPoolExecutor(max_workers=8) as pool:
        versions = list(pool.map(lambda key: concurrent.pull('transcript:' + str(key))['checkpoint']['seq'], range(24)))
    assert len(set(versions)) == len(versions)
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
    generation = store.generation()
    assert store.push_drafts([row('phone', 'first'), row('desktop', 'second')]) == []
    assert store.generation() > generation
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
    # An unchanged analytics agent row is not rewritten; windows are not told to resync.
    from codex_analytics import AnalyticsMixin
    with connect() as db:
        db.execute('CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    SyncStore(connect, transcript)
    agent = {'id': 'a1', 'name': 'Worker', 'rootId': 'a1'}
    with connect() as db:
        AnalyticsMixin.analytics_agent(None, db, agent)
    generation = store.generation()
    with connect() as db:
        AnalyticsMixin.analytics_agent(None, db, agent)
    assert store.generation() == generation
    with connect() as db:
        AnalyticsMixin.analytics_agent(None, db, {**agent, 'name': 'Renamed'})
        assert json.loads(db.execute("SELECT record FROM analytics_agents").fetchone()[0])['name'] == 'Renamed'
    assert store.generation() > generation
    # Each tab owns a draft branch; legacy device branches remain intact.
    legacy = next(d for d in store.pull('drafts')['documents'] if d['id'] == 'phone:one')
    def tab_row(writer, text):
        key = 'phone:' + writer + ':one'
        return {'newDocumentState': {'id': key, 'seq': 0, 'payload': json.dumps({
            'id': key, 'device': 'phone', 'session': 'one', 'text': text})}}
    assert store.push_drafts([tab_row('tab-a', 'First tab'), tab_row('tab-b', 'Second tab')]) == []
    tabs = {d['id']: d for d in store.pull('drafts')['documents']}
    assert tabs['phone:one'] == legacy
    assert json.loads(tabs['phone:tab-a:one']['payload'])['text'] == 'First tab'
    assert json.loads(tabs['phone:tab-b:one']['payload'])['text'] == 'Second tab'
    assert len(store.push_drafts([tab_row('tab-a', 'Concurrent edit')])) == 1
    for key in ['phone::one', 'phone:tab:a:one', 'other:tab-a:one', 'phone:tab-a:other']:
        invalid = tab_row('tab-a', 'Rejected edit')
        invalid['newDocumentState']['id'] = key
        value = json.loads(invalid['newDocumentState']['payload'])
        value['id'] = key
        invalid['newDocumentState']['payload'] = json.dumps(value)
        try:
            store.push_drafts([invalid])
        except ValueError:
            pass
        else:
            raise AssertionError('Invalid tab draft identity accepted: ' + key)
print('sync contract passed')
