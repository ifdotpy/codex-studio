#!/usr/bin/env python3
"""Isolated WAL page fixture; operation rates may be scaled to a live minute."""
import contextlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_sync import SyncStore
from codex_sync_entities import put as put_entity
from codex_sqlite import connect as instrumented_connect, diagnostics as sqlite_diagnostics

PAGE = 4096
COUNT = 60
BODY = 'x' * 500_000


def measure(name, action, setup=lambda db: None):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / 'volume.sqlite3'
        anchor = sqlite3.connect(path)
        try:
            anchor.execute('PRAGMA journal_mode=WAL')
            anchor.execute('PRAGMA wal_autocheckpoint=0')
            anchor.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
            setup(anchor)
            anchor.commit()
            @contextlib.contextmanager
            def connect():
                db = instrumented_connect(path, site=name)
                db.execute('PRAGMA wal_autocheckpoint=0')
                try:
                    with db:
                        yield db
                finally:
                    db.close()
            store = SyncStore(connect, lambda _: {'body': BODY, 'tick': tick[0]})
            tick = [0]
            anchor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            start = os.stat(str(path) + '-wal').st_size
            for index in range(COUNT):
                tick[0] = index + 1
                action(connect, store, tick[0])
            wal = os.stat(str(path) + '-wal').st_size - start
            checkpoint = anchor.execute('PRAGMA wal_checkpoint(PASSIVE)').fetchone()
            stat_site = ('SyncStore.transcript_pull' if name == 'transcript_entity_refresh' else name)
            transaction_site = sqlite_diagnostics().get('transaction', {}).get('sites', {}).get(stat_site, {})
            return {'source': name, 'operations': COUNT, 'walBytesPerMinuteAt60Ops': wal,
                    'checkpointBytesAtEnd': checkpoint[1] * PAGE,
                    'transactionMetrics': transaction_site}
        finally:
            anchor.close()


def legacy(connect, store, index):
    encoded = json.dumps({'body': BODY, 'tick': index}, sort_keys=True, separators=(',', ':'))
    with connect() as db:
        db.execute('INSERT OR REPLACE INTO sync_documents(scope,id,payload,deleted) VALUES (?,?,?,0)',
                   ('state', 'state', encoded))


def version(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   ('a', str(index)))
        put_entity(db, 'agent', 'a', {'id': 'a', 'kind': 'agent', 'tokensUsed': index})


def history(connect, store, index):
    with connect() as db:
        db.execute('INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)', ('a', 'a', json.dumps({'updated': index, 'body': 'x' * 1300})))


def items(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO analytics_items VALUES (?,?)', (str(index), 'x' * 1400))


def runtime(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_items VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   ('a', str(index) + 'x' * 1400))


def agent(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   ('a', str(index) + 'x' * 1400))


def search(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)',
                   (str(index), 'a', 'tool', str(index) + ' x' * 700))


def growing_item(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_items VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   ('a', 'x' * (index * 8192)))


def growing_search(connect, store, index):
    with connect() as db:
        db.execute('DELETE FROM runtime_search WHERE rowid=1')
        db.execute('INSERT INTO runtime_search(rowid,id,agent,kind,body) VALUES (1,?,?,?,?)',
                   ('a', 'a', 'assistant', 'x' * (index * 8192)))


def trigger(connect, store, index):
    with connect() as db:
        db.execute('INSERT INTO runtime_items VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   ('a', str(index)))


def transcript(connect, store, index):
    store.transcript_pull('transcript', 0,
                          {'items': [{'id': str(item), 'text': 'x' * 2048}
                                     for item in range(250)], 'tick': index}, False)


def schema(sql):
    return lambda db: db.executescript(sql)


if __name__ == '__main__':
    results = [
        measure('sync_payload_replace', legacy),
        measure('sync_version_plus_agent_change', version),
        measure('history_replace', history, schema('CREATE TABLE analytics_history(id TEXT PRIMARY KEY,agent TEXT,record TEXT)')),
        measure('analytics_item_insert', items, schema('CREATE TABLE analytics_items(id TEXT PRIMARY KEY,record TEXT)')),
        measure('runtime_item_stream_update', runtime, schema('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,record TEXT)')),
        measure('runtime_item_growing_stream', growing_item, schema('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,record TEXT)')),
        measure('agent_record_update', agent),
        measure('search_fts_insert', search, schema('CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body)')),
        measure('search_fts_growing_stream', growing_search, schema('CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body)')),
        measure('item_without_trigger', trigger, schema('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,record TEXT)')),
        measure('item_with_trigger', trigger, schema('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,record TEXT); CREATE TRIGGER watch AFTER UPDATE ON runtime_items BEGIN UPDATE sync_generation SET value=value+1 WHERE id=1; END; CREATE TRIGGER watch_insert AFTER INSERT ON runtime_items BEGIN UPDATE sync_generation SET value=value+1 WHERE id=1; END;')),
        measure('transcript_entity_refresh', transcript),
    ]
    by = {r['source']: r for r in results}
    assert by['sync_version_plus_agent_change']['walBytesPerMinuteAt60Ops'] < by['sync_payload_replace']['walBytesPerMinuteAt60Ops'] / 20
    assert by['item_with_trigger']['walBytesPerMinuteAt60Ops'] > by['item_without_trigger']['walBytesPerMinuteAt60Ops']
    print(json.dumps(results, indent=2))
