"""Compact chat hints follow commits and do not read transcript content."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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
        db.execute('CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT)')
        db.executemany('INSERT INTO runtime_agents VALUES (?,?)',
                       [(f'chat-{n}', '{}') for n in range(500)])
    def forbidden(*_):
        raise AssertionError('Revision hints must not project history or workspace state')
    store = SyncStore(connect, forbidden, forbidden)
    first = store.generation_state()
    assert len(first['transcriptRevisions']) == 500
    assert set(first['transcriptRevisions'].values()) == {0}
    assert len(json.dumps(first)) < 10000
    queries = []
    store._version_reader.set_trace_callback(queries.append)
    assert store.generation_state() == first
    assert not any('SELECT a.id' in query for query in queries), 'Idle hints reuse the revision map'
    with connect() as db:
        db.execute('INSERT INTO runtime_items VALUES (?,?,?)', ('message', 'chat-41', '{"text":"new"}'))
    changed = store.generation_state()
    assert changed['transcriptRevisions']['chat-41'] == 1
    assert changed['transcriptRevisions']['chat-40'] == 0
    assert sum('SELECT a.id' in query for query in queries) == 1
    try:
        with connect() as db:
            db.execute('UPDATE runtime_items SET record=? WHERE id=?', ('{}', 'message'))
            raise ValueError('roll back')
    except ValueError:
        pass
    assert store.generation_state() == changed
    with connect() as db:
        db.execute('DELETE FROM runtime_agents WHERE id=?', ('chat-41',))
    assert 'chat-41' not in store.generation_state()['transcriptRevisions']
print('PASS: 500 chat hints, scoped commits, cached reads, rollback')
