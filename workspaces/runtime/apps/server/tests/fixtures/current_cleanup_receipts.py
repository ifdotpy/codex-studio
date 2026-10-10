"""One-time receipt conversion for the current Studio tool protocol.

Run migrate_receipts(db) inside the deployment transaction and Runtime lock.
The caller must prevent callbacks from reserving receipts during conversion.
This module does not start work or contact a native server.
"""

import json
import time

from codex_tool_requests import _prefix, _spawned_ids


def request_prefixes(actor):
    """Scopes recorded by the server, including completed account transfers."""
    scopes = [actor, *actor.get("accountHistory", [])]
    return list(dict.fromkeys(_prefix(scope.get("accountKey", "default"), scope["threadId"])
                             for scope in scopes if isinstance(scope, dict) and scope.get("threadId")))


def migrate_receipts(db):
    """Preserve evidence and ownership without reconstructing runtime commands."""
    tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    required = {'runtime_agents', 'runtime_tool_requests', 'runtime_tool_request_aliases', 'runtime_tool_results'}
    if not required <= tables:
        raise ValueError('Current receipt tables must exist before receipt conversion')
    owners = {}
    for row in db.execute('SELECT record FROM runtime_agents'):
        actor = json.loads(row[0])
        for prefix in request_prefixes(actor):
            owners.setdefault(prefix, set()).add(actor['id'])
    counts = {'converted': 0, 'normalized': 0, 'unowned': 0, 'searchRows': 0}
    now = time.time()
    # Normalize only the explicit identity saved when the original call ran.
    # Do not guess a tool from output text or the name of an old wrapper.
    for key, encoded in db.execute('SELECT id,record FROM runtime_tool_requests').fetchall():
        record = json.loads(encoded)
        identity = record.pop('identityTool', None)
        if identity is not None:
            record['tool'] = identity
            db.execute('UPDATE runtime_tool_requests SET record=? WHERE id=?', (json.dumps(record), key))
            counts['normalized'] += 1
        if record.get('agent'):
            for alias in {key, record.get('callId'), record.get('request_id')} - {None, ''}:
                db.execute('INSERT OR IGNORE INTO runtime_tool_request_aliases VALUES (?,?,?)',
                           (record['agent'], alias, key))
    sources = ['SELECT id FROM runtime_tool_results']
    if 'runtime_operation_receipts' in tables:
        sources.append('SELECT id FROM runtime_operation_receipts')
    keys = db.execute(' UNION '.join(sources)).fetchall()
    for (key,) in keys:
        if db.execute('SELECT 1 FROM runtime_tool_requests WHERE id=?', (key,)).fetchone():
            continue
        matches = [(prefix, actor) for prefix, actors in owners.items()
                   if key.startswith(prefix) for actor in actors]
        actors = {actor for _, actor in matches}
        actor = next(iter(actors)) if len(actors) == 1 else None
        # Overlapping account/thread prefixes cannot establish a unique call ID.
        prefixes = {prefix for prefix, owner in matches if owner == actor}
        prefix = next(iter(prefixes)) if len(prefixes) == 1 else None
        if prefix is None:
            actor = None
        row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()
        result = json.loads(row[0]) if row else None
        record = {'id': key, 'agent': actor, 'callId': key[len(prefix):] if prefix else None,
                  'tool': None, 'signature': None, 'created': 0, 'updated': 0, 'migratedAt': now,
                  'stage': 'unknown', 'outcome': 'unknown', 'cancelRequested': False,
                  'migrationSource': 'saved_receipt', 'readOnly': False}
        if result is not None:
            record['result'] = result
            if actor is not None:
                record['stage'] = 'completed' if result.get('success') is True else 'failed'
                record['outcome'] = 'applied' if result.get('success') is True else 'unknown'
                record['agentIds'] = _spawned_ids(result)
        db.execute('INSERT INTO runtime_tool_requests VALUES (?,?)', (key, json.dumps(record)))
        if actor is not None:
            for alias in {key, record['callId']}:
                db.execute('INSERT OR IGNORE INTO runtime_tool_request_aliases VALUES (?,?,?)', (actor, alias, key))
        else:
            counts['unowned'] += 1
        counts['converted'] += 1
    # Native output reads use the current direct document index. Keep every body.
    if 'runtime_search' in tables and 'runtime_search_rows' not in tables:
        db.execute('CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE)')
        db.execute('INSERT INTO runtime_search_rows SELECT id,rowid FROM runtime_search')
        counts['searchRows'] = db.execute('SELECT COUNT(*) FROM runtime_search_rows').fetchone()[0]
    return counts
