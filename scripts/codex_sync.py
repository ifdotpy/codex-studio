"""SQLite-backed RxDB pull checkpoints. Action receipts remain in the runtime."""
import json
import hashlib
import sqlite3
import threading
import time
import uuid
import zlib

SCOPE_STRIPES = 64
# Windows pull state together after each RESYNC. Without a write in between,
# they share one snapshot. The age bound limits in-memory state staleness.
SNAPSHOT_REUSE_SECONDS = 2
TRANSCRIPT_MAX_TOMBSTONES = 512
TRANSCRIPT_ORDER_ID = '@order'
TRANSCRIPT_META_ID = '@meta'
TRANSCRIPT_FLOOR_ID = '@floor'


class SyncStore:
    def __init__(self, connect, snapshot, transcript, chat_snapshot=None):
        self.connect, self.snapshot, self.transcript = connect, snapshot, transcript
        self.chat_snapshot = chat_snapshot
        # One scope stays serial: its compare-and-replace keeps one checkpoint
        # per version. Other scopes proceed; a slow state snapshot must not
        # delay transcript pulls. A fixed stripe count bounds memory.
        self.locks = tuple(threading.RLock() for _ in range(SCOPE_STRIPES))
        self.snapshots = {}
        with self.connect() as db:
            from codex_sync_entities import ensure_tables
            ensure_tables(db)
            db.executescript('''
                CREATE TABLE IF NOT EXISTS sync_identity (id TEXT PRIMARY KEY);
                CREATE TABLE IF NOT EXISTS sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
                INSERT OR IGNORE INTO sync_generation VALUES (1, 0);
                CREATE TABLE IF NOT EXISTS sync_documents (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
                    id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(scope, id));
                CREATE INDEX IF NOT EXISTS sync_scope_seq ON sync_documents(scope, seq);
            ''')
            if not db.execute('SELECT id FROM sync_identity').fetchone():
                db.execute('INSERT INTO sync_identity VALUES (?)', (uuid.uuid4().hex,))

    def __del__(self):
        reader = getattr(self, '_version_reader', None)
        if reader is not None:
            reader.close()

    def identity(self):
        with self.connect() as db:
            return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                    **({'chatState': True} if self.chat_snapshot else {})}

    def generation(self):
        # A persistent reader sees one data_version change per committed writer
        # transaction. The old row triggers wrote the same page for every row.
        self._ensure_versions()
        with self._version_lock:
            return self._version_reader.execute('PRAGMA data_version').fetchone()[0]

    def entity_sequence(self):
        from codex_sync_entities import max_seq
        with self.connect() as db:
            return max_seq(db)

    def draft_sequence(self):
        with self.connect() as db:
            return db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_documents WHERE scope='drafts'").fetchone()[0]

    def _ensure_versions(self):
        # Existing make_server closures retain their SyncStore across a live
        # patch, so all new state must be initialized on first use.
        if getattr(self, '_versions_ready', False):
            return
        lock = self.__dict__.setdefault('_version_lock', threading.RLock())
        with lock:
            if getattr(self, '_versions_ready', False):
                return
            with self.connect() as db:
                db.execute('''CREATE TABLE IF NOT EXISTS sync_versions (
                    seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
                    hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL)''')
                # Remove the old row-trigger watches. Coarse old-client SSE
                # uses data_version; entity sync is driven by sync_entities.
                for (name,) in db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'sync_watch_%'"):
                    db.execute('DROP TRIGGER "' + name + '"')
                path = db.execute('PRAGMA database_list').fetchone()[2]
            self._version_reader = sqlite3.connect('file:' + path + '?mode=ro', uri=True,
                                                    check_same_thread=False, timeout=10)
            self._versions_ready = True

    def scope_lock(self, scope):
        return self.locks[zlib.crc32(str(scope).encode()) % SCOPE_STRIPES]

    @staticmethod
    def document(row):
        return {'id': row[1], 'payload': row[2], 'seq': row[0], '_deleted': bool(row[3])}

    def transcript_pull(self, scope, after, payload, deleted):
        """Project transcript deltas from bounded hash-only entity revisions."""
        from codex_sync_entities import next_sequence

        collection = scope
        items = payload.get('items', []) if isinstance(payload, dict) else []
        items_by_id = {str(item['id']): item for item in items
                       if isinstance(item, dict) and isinstance(item.get('id'), str)}
        order = [str(item['id']) for item in items
                 if isinstance(item, dict) and isinstance(item.get('id'), str)]
        metadata = {key: value for key, value in payload.items() if key != 'items'} if isinstance(payload, dict) else {}

        def digest(value):
            encoded = json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
            return hashlib.sha256(encoded.encode('utf-8')).hexdigest()

        with self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            rows = db.execute('SELECT id,seq,hash,deleted FROM sync_entities WHERE collection=?',
                              (collection,)).fetchall()
            current = {row[0]: row for row in rows}
            prior_meta = current.get(TRANSCRIPT_META_ID)
            base_missing = prior_meta is None
            was_deleted = bool(prior_meta[3]) if prior_meta else False
            sequence = next_sequence(db)

            def put(key, value_hash, is_deleted=False):
                nonlocal sequence
                old = current.get(key)
                if old and old[2] == value_hash and bool(old[3]) == is_deleted:
                    return False
                db.execute('''INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES (?,?,?,?,NULL,?) ON CONFLICT(collection,id) DO UPDATE SET
                              seq=excluded.seq,hash=excluded.hash,payload=NULL,deleted=excluded.deleted''',
                           (collection, key, sequence, value_hash, int(is_deleted)))
                current[key] = (key, sequence, value_hash, int(is_deleted))
                sequence += 1
                return True

            if deleted:
                for key, row in tuple(current.items()):
                    if key.startswith('item:') or key == TRANSCRIPT_ORDER_ID:
                        put(key, row[2], True)
                put(TRANSCRIPT_META_ID, digest({}), True)
            else:
                live_keys = {'item:' + key for key in items_by_id}
                for entity_id, item in items_by_id.items():
                    put('item:' + entity_id, digest(item), False)
                for key, row in tuple(current.items()):
                    if key.startswith('item:') and key not in live_keys and not row[3]:
                        put(key, row[2], True)
                put(TRANSCRIPT_ORDER_ID, digest(order), False)
                put(TRANSCRIPT_META_ID, digest(metadata), False)

            # Tombstones are retained for a bounded replay window. The floor
            # marks cursors that must receive a complete replacement.
            pruned = db.execute('''SELECT id,seq FROM sync_entities
                                   WHERE collection=? AND deleted=1 AND id LIKE 'item:%'
                                   ORDER BY seq DESC LIMIT -1 OFFSET ?''',
                                (collection, TRANSCRIPT_MAX_TOMBSTONES)).fetchall()
            floor = current.get(TRANSCRIPT_FLOOR_ID)
            floor_seq = floor[1] if floor else 0
            if pruned:
                db.executemany('DELETE FROM sync_entities WHERE collection=? AND id=?',
                               [(collection, row[0]) for row in pruned])
                floor_seq = max(floor_seq, max(row[1] for row in pruned))
                floor_hash = digest(floor_seq)
                db.execute('''INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES (?,?,?,?,NULL,0) ON CONFLICT(collection,id) DO UPDATE SET
                              seq=excluded.seq,hash=excluded.hash,payload=NULL,deleted=0''',
                           (collection, TRANSCRIPT_FLOOR_ID, floor_seq, floor_hash))
                current[TRANSCRIPT_FLOOR_ID] = (TRANSCRIPT_FLOOR_ID, floor_seq, floor_hash, 0)
            high = db.execute('SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection=?',
                              (collection,)).fetchone()[0]

            if base_missing and not deleted:
                floor_seq = high
                floor_hash = digest(floor_seq)
                db.execute('''INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES (?,?,?,?,NULL,0) ON CONFLICT(collection,id) DO UPDATE SET
                              seq=excluded.seq,hash=excluded.hash,payload=NULL,deleted=0''',
                           (collection, TRANSCRIPT_FLOOR_ID, floor_seq, floor_hash))
                high = max(high, floor_seq)

            full = after == 0 or base_missing or was_deleted or after < floor_seq
            if full:
                documents = [{'id': scope, 'payload': json.dumps(payload, ensure_ascii=False),
                              'seq': high, '_deleted': bool(deleted)}]
                checkpoint = high
            elif after < high and deleted:
                documents = [{'id': scope, 'payload': '{}', 'seq': high, '_deleted': True}]
                checkpoint = high
            elif after < high:
                changed = db.execute('''SELECT id,seq,hash,deleted FROM sync_entities
                                        WHERE collection=? AND seq>? AND id!=?
                                        ORDER BY seq''',
                                     (collection, after, TRANSCRIPT_FLOOR_ID)).fetchall()
                changed_ids = {row[0]: row for row in changed}
                item_ids = [key[5:] for key, row in changed_ids.items()
                            if key.startswith('item:') and not row[3] and key[5:] in items_by_id]
                removed = [key[5:] for key, row in changed_ids.items()
                           if key.startswith('item:') and row[3]]
                revisions = {key[5:]: row[2] for key, row in changed_ids.items()
                             if key.startswith('item:') and not row[3]}
                delta = {**metadata, 'delta': True,
                         'items': [items_by_id[key] for key in order if key in item_ids],
                         'itemRevisions': revisions, 'removed': removed}
                if TRANSCRIPT_ORDER_ID in changed_ids:
                    delta['order'] = order
                documents = [{'id': scope, 'payload': json.dumps(delta, ensure_ascii=False),
                              'seq': high, '_deleted': False}]
                checkpoint = high
            else:
                documents = []
                checkpoint = after

            return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                    'documents': documents, 'checkpoint': {'seq': checkpoint}}

    def _put(self, db, scope, key, payload, deleted=False):
        self._ensure_versions()
        encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
        old = db.execute('SELECT payload, deleted FROM sync_documents WHERE scope=? AND id=?', (scope, key)).fetchone()
        if old and old[0] == encoded and bool(old[1]) == deleted:
            return
        # All persisted sync collections share one sequence space.
        from codex_sync_entities import next_sequence
        db.execute('INSERT OR REPLACE INTO sync_documents(seq,scope,id,payload,deleted) VALUES (?,?,?,?,?)',
                   (next_sequence(db), scope, key, encoded, int(deleted)))

    def pull(self, scope, after=0, limit=100, fresh=False, initial_high=0):
        after = max(0, int(after))
        limit = min(500 if scope == 'state:entities:v1' else 100, max(1, int(limit)))
        with self.scope_lock(scope):
            self._ensure_versions()
            if scope == 'state:entities:v1':
                from codex_sync_entities import (max_seq, seed, sync_task_window,
                                                 sync_event_window, sync_monitor_window)
                with self.connect() as db:
                    db.execute('BEGIN IMMEDIATE')
                    seed(db, self.chat_snapshot() if self.chat_snapshot else self.snapshot())
                    # Retire old task DTOs gradually so an existing client checkpoint
                    # can consume the resulting tombstones through ordinary deltas.
                    sync_task_window(db)
                    sync_event_window(db)
                    sync_monitor_window(db)
                    high = max_seq(db)
                    initial_high = min(high, max(0, int(initial_high))) if fresh and after else high
                    # A new browser has no rows to remove. Existing checkpoints
                    # still receive tombstones through the ordinary delta path.
                    rows = db.execute('''SELECT collection,id,seq,payload,deleted FROM sync_entities
                                         WHERE collection NOT LIKE 'transcript:%' AND seq>?
                                           AND (?=0 OR deleted=0 OR seq>?)
                                         ORDER BY seq LIMIT ?''',
                                      (after, int(bool(fresh)), initial_high, limit)).fetchall()
                    documents = [{'id': 'entity:' + row[0] + ':' + row[1], 'payload': row[3],
                                  'seq': row[2], '_deleted': bool(row[4])} for row in rows]
                    checkpoint = (documents[-1]['seq'] if len(documents) == limit else high)
                    return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                            'documents': documents, 'checkpoint': {'seq': checkpoint},
                            'maxSeq': high, 'initialHigh': initial_high if fresh else 0}
            if scope == 'state' or (scope == 'state:chat' and self.chat_snapshot):
                payload = self.shared_snapshot(scope)
                deleted = False
            elif scope.startswith('transcript:') and len(scope) < 300:
                try:
                    payload, deleted = self.transcript(scope.split(':', 1)[1]), False
                except ValueError:
                    payload, deleted = {}, True
            elif scope != 'drafts':
                raise ValueError('Invalid sync scope')
            if scope.startswith('transcript:'):
                return self.transcript_pull(scope, after, payload, deleted)
            with self.connect() as db:
                if scope == 'drafts':
                    rows = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND seq>? ORDER BY seq LIMIT ?',
                                      (scope, after, limit)).fetchall()
                    documents = [self.document(row) for row in rows]
                else:
                    encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
                    digest = hashlib.sha256(encoded.encode('utf-8')).hexdigest()
                    current = db.execute('SELECT seq,hash,deleted FROM sync_versions WHERE scope=?', (scope,)).fetchone()
                    if current is None or current[1] != digest or bool(current[2]) != deleted:
                        # Different scope stripes can update simultaneously.
                        # Claim the next global sequence under SQLite's writer
                        # lock, then recheck a version written while waiting.
                        db.execute('BEGIN IMMEDIATE')
                        current = db.execute('SELECT seq,hash,deleted FROM sync_versions WHERE scope=?', (scope,)).fetchone()
                        if current is None or current[1] != digest or bool(current[2]) != deleted:
                            legacy = (db.execute('SELECT seq,payload,deleted FROM sync_documents WHERE scope=? AND id=?',
                                                 (scope, scope)).fetchone() if current is None else None)
                            if legacy and legacy[1] == encoded and bool(legacy[2]) == deleted:
                                seq = legacy[0]
                            else:
                                from codex_sync_entities import next_sequence
                                seq = next_sequence(db)
                            if current is None:
                                db.execute('INSERT INTO sync_versions VALUES (?,?,?,?,?)',
                                           (seq, scope, digest, int(deleted), time.time()))
                            else:
                                db.execute('UPDATE sync_versions SET seq=?,hash=?,deleted=?,updated=? WHERE scope=?',
                                           (seq, digest, int(deleted), time.time(), scope))
                        else:
                            seq = current[0]
                    else:
                        seq = current[0]
                    documents = ([{'id': scope, 'payload': encoded, 'seq': seq, '_deleted': deleted}]
                                 if seq > after else [])
                return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                        'documents': documents,
                        'checkpoint': {'seq': documents[-1]['seq'] if documents else after}}

    def shared_snapshot(self, scope):
        # The caller holds this scope's lock.
        generation = self.generation()
        cached = self.snapshots.get(scope)
        if cached and cached[0] == generation and time.monotonic() - cached[1] < SNAPSHOT_REUSE_SECONDS:
            return cached[2]
        payload = dict(self.chat_snapshot() if scope == 'state:chat' else self.snapshot())
        payload.pop('token', None)
        payload.pop('at', None)
        # A write during the snapshot changes the generation; the next pull rebuilds.
        self.snapshots[scope] = (generation, time.monotonic(), payload)
        return payload

    def push_drafts(self, rows):
        if not isinstance(rows, list) or len(rows) > 100:
            raise ValueError('Invalid draft batch')
        conflicts = []
        with self.scope_lock('drafts'), self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            for row in rows:
                if not isinstance(row, dict) or not isinstance(row.get('newDocumentState'), dict):
                    raise ValueError('Invalid draft row')
                new = row['newDocumentState']
                key, encoded = new.get('id'), new.get('payload')
                if not isinstance(key, str) or not key or len(key) > 300 or not isinstance(encoded, str):
                    raise ValueError('Invalid draft')
                value = json.loads(encoded)
                if not isinstance(value, dict) or not isinstance(new.get('_deleted', False), bool):
                    raise ValueError('Invalid draft')
                device, session = value.get('device'), value.get('session')
                if (not isinstance(device, str) or not device
                        or not isinstance(session, str) or not session
                        or f'{device}:{session}' != key
                        or ('id' in value and value.get('id') != key)):
                    raise ValueError('Draft identity does not match its key')
                assumed = row.get('assumedMasterState')
                assumed_payload = None
                if assumed is not None:
                    if (not isinstance(assumed, dict) or not isinstance(assumed.get('payload'), str)
                            or not isinstance(assumed.get('_deleted', False), bool)):
                        raise ValueError('Invalid assumed draft state')
                    assumed_payload = json.dumps(json.loads(assumed['payload']), sort_keys=True,
                                                 separators=(',', ':'), ensure_ascii=False)
                encoded = json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
                alternatives = value.get('alternatives', [])
                if not isinstance(alternatives, list) or not all(isinstance(text, str) for text in alternatives):
                    raise ValueError('Invalid draft alternatives')
                # Each browser owns its branch. Other branches remain visible, never overwritten.
                if not isinstance(value.get('text'), str) or len(value['text']) > 2_000_000:
                    raise ValueError('Invalid draft text')
                old = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND id=?', ('drafts', key)).fetchone()
                if old and (not assumed or assumed_payload != old[2]
                            or assumed.get('_deleted', False) != bool(old[3])):
                    if encoded != old[2] or new.get('_deleted', False) != bool(old[3]):
                        conflicts.append(self.document(old))
                        continue
                self._put(db, 'drafts', key, value, bool(new.get('_deleted')))
        return conflicts
