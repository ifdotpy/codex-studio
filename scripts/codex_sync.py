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

    def _put(self, db, scope, key, payload, deleted=False):
        encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
        old = db.execute('SELECT payload, deleted FROM sync_documents WHERE scope=? AND id=?', (scope, key)).fetchone()
        if old and old[0] == encoded and bool(old[1]) == deleted:
            return
        # All persisted sync collections share one sequence space.
        from codex_sync_entities import next_sequence
        db.execute('INSERT OR REPLACE INTO sync_documents(seq,scope,id,payload,deleted) VALUES (?,?,?,?,?)',
                   (next_sequence(db), scope, key, encoded, int(deleted)))

    def pull(self, scope, after=0, limit=100):
        after, limit = max(0, int(after)), min(100, max(1, int(limit)))
        with self.scope_lock(scope):
            self._ensure_versions()
            if scope == 'state:entities:v1':
                from codex_sync_entities import max_seq, seed
                with self.connect() as db:
                    db.execute('BEGIN IMMEDIATE')
                    seed(db, self.chat_snapshot() if self.chat_snapshot else self.snapshot())
                    rows = db.execute('''SELECT collection,id,seq,payload,deleted FROM sync_entities
                                         WHERE collection NOT LIKE 'transcript:%' AND seq>?
                                         ORDER BY seq LIMIT ?''', (after, limit)).fetchall()
                    documents = [{'id': 'entity:' + row[0] + ':' + row[1], 'payload': row[3],
                                  'seq': row[2], '_deleted': bool(row[4])} for row in rows]
                    checkpoint = documents[-1]['seq'] if documents else after
                    return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                            'documents': documents, 'checkpoint': {'seq': checkpoint},
                            'maxSeq': max_seq(db)}
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
            with self.connect() as db:
                if scope == 'drafts':
                    rows = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND seq>? ORDER BY seq LIMIT ?',
                                      (scope, after, limit)).fetchall()
                    documents = [self.document(row) for row in rows]
                elif scope.startswith('transcript:'):
                    encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
                    old = db.execute('SELECT seq,payload,deleted FROM sync_documents WHERE scope=? AND id=?',
                                     (scope, scope)).fetchone()
                    old_payload = json.loads(old[1]) if old and not old[2] else None
                    changed = old is None or old_payload != payload or bool(old[2]) != deleted
                    if changed:
                        db.execute('BEGIN IMMEDIATE')
                        self._put(db, scope, scope, payload, deleted)
                        row = db.execute('SELECT seq,payload,deleted FROM sync_documents WHERE scope=? AND id=?',
                                         (scope, scope)).fetchone()
                    else:
                        row = old
                    seq, stored, stored_deleted = row[0], json.loads(row[1]), bool(row[2])
                    if after >= seq:
                        documents = []
                    elif stored_deleted or old_payload is None or after != old[0]:
                        documents = [{'id': scope, 'payload': json.dumps(stored, ensure_ascii=False),
                                      'seq': seq, '_deleted': stored_deleted}]
                    else:
                        previous_items = {item.get('id'): item for item in old_payload.get('items', [])}
                        current_items = {item.get('id'): item for item in stored.get('items', [])}
                        revisions = {key: hashlib.sha256(json.dumps(item, sort_keys=True, separators=(',', ':'),
                                                                      ensure_ascii=False).encode()).hexdigest()
                                     for key, item in current_items.items()
                                     if previous_items.get(key) != item}
                        delta = {key: value for key, value in stored.items() if key != 'items'}
                        delta.update({'delta': True, 'itemRevisions': revisions,
                                      'items': [item for key, item in current_items.items()
                                                if previous_items.get(key) != item],
                                      'removed': [key for key in previous_items if key not in current_items]})
                        if list(previous_items) != list(current_items):
                            delta['order'] = list(current_items)
                        documents = [{'id': scope, 'payload': json.dumps(delta, ensure_ascii=False),
                                      'seq': seq, '_deleted': False}]
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
