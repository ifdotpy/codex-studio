#!/usr/bin/env python3
"""Measure execution DDL on a synthetic database, never on user state."""
import argparse
import json
from pathlib import Path
import sqlite3
import statistics
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_execution import ensure_tables


def benchmark(shape, payload_bytes):
    with tempfile.TemporaryDirectory(prefix='studio-execution-migration-') as directory:
        path = Path(directory) / 'fixture.sqlite3'
        db = sqlite3.connect(path)
        db.execute('PRAGMA journal_mode=OFF')
        db.execute('PRAGMA synchronous=OFF')
        for name, details in shape['tables'].items():
            if not name.startswith('runtime_') or not name.replace('_', '').isalnum():
                raise ValueError('Invalid fixture table')
            # Preserve the large tables and their row counts. Payload contains
            # synthetic text only. Runtime never opens this fixture.
            db.execute('CREATE TABLE ' + name + '(id INTEGER PRIMARY KEY,record TEXT NOT NULL)')
            text = json.dumps({'text': 'x' * payload_bytes})
            count = details['maxRowid'] or 0
            db.executemany('INSERT INTO ' + name + ' VALUES (?,?)', ((index, text) for index in range(1, count + 1)))
            db.commit()
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('PRAGMA synchronous=FULL')
        size = path.stat().st_size
        reads = []
        db.set_authorizer(lambda action, table, column, *_: (reads.append(table) or sqlite3.SQLITE_OK)
                          if action == sqlite3.SQLITE_READ and table in shape['tables'] else sqlite3.SQLITE_OK)
        started = time.perf_counter()
        with db:
            db.execute('BEGIN')
            ensure_tables(db)
        first_ms = (time.perf_counter() - started) * 1000
        repeats = []
        for _ in range(10):
            started = time.perf_counter()
            with db:
                db.execute('BEGIN')
                ensure_tables(db)
            repeats.append((time.perf_counter() - started) * 1000)
        result = {'fixtureBytes': size, 'payloadBytesPerRow': payload_bytes, 'shape': shape,
                  'firstMigrationMs': round(first_ms, 3), 'existingSchemaMedianMs': round(statistics.median(repeats), 3),
                  'existingSchemaMaxMs': round(max(repeats), 3), 'oldTableReads': reads}
        print(json.dumps(result), flush=True)
        assert not reads, reads
        db.close()
        return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--shape-json', type=Path)
    parser.add_argument('--payload-bytes', type=int, default=64)
    args = parser.parse_args()
    shape = json.loads(args.shape_json.read_text()) if args.shape_json else {
        'tables': {'runtime_agents': {'maxRowid': 1766}, 'runtime_events': {'maxRowid': 80098},
                   'runtime_items': {'maxRowid': 855009}, 'runtime_tool_requests': {'maxRowid': 110909},
                   'runtime_tasks': {'maxRowid': 676814}, 'runtime_monitors': {'maxRowid': 36874}}}
    benchmark(shape, args.payload_bytes)
