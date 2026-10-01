"""SQLite-backed RxDB pull checkpoints. Action receipts remain in the runtime."""
import json
import re
import threading
import time
import uuid
import zlib

SCOPE_STRIPES = 64
PROTOCOL_VERSION = 2
STATE_TABLES = frozenset({
    "groups", "messages", "graph_agents", "graph_edges", "runtime_agents",
    "runtime_events", "runtime_tasks", "runtime_monitors", "runtime_requests",
    "runtime_complaints", "runtime_rooms", "runtime_chat_messages", "runtime_rules", "runtime_work",
    "runtime_projects", "runtime_profiles", "runtime_assets", "runtime_checkpoints",
    "runtime_native_notices", "analytics_limits",
})
TRANSCRIPT_TABLES = frozenset({
    "runtime_agents", "runtime_items", "runtime_item_bodies", "runtime_events", "runtime_event_meta",
    "runtime_completed_turns", "runtime_assets", "runtime_tool_results",
    "runtime_tool_request_aliases",
})


class SyncStore:
    def __init__(self, connect, snapshot, transcript, chat_snapshot=None, state_signature=None):
        self.connect, self.snapshot, self.transcript = connect, snapshot, transcript
        self.chat_snapshot = chat_snapshot
        self.state_signature = state_signature
        self.locks = tuple(threading.RLock() for _ in range(SCOPE_STRIPES))
        self.snapshots = {}
        self.snapshot_builds = 0
        self.invalidation_count = 0
        self._migration_lock = threading.Lock()
        self._signature_lock = threading.Lock()
        self._volatile_epoch = 0
        with self.connect() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS sync_identity (id TEXT PRIMARY KEY);
                CREATE TABLE IF NOT EXISTS sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
                INSERT OR IGNORE INTO sync_generation VALUES (1, 0);
                CREATE TABLE IF NOT EXISTS sync_scope_generation (scope TEXT PRIMARY KEY, value INTEGER NOT NULL);
                INSERT OR IGNORE INTO sync_scope_generation VALUES ('state',0),('transcripts',0),('drafts',0);
                CREATE TABLE IF NOT EXISTS sync_documents (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
                    id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(scope, id));
                CREATE INDEX IF NOT EXISTS sync_scope_seq ON sync_documents(scope, seq);
            ''')
            if not db.execute('SELECT id FROM sync_identity').fetchone():
                db.execute('INSERT INTO sync_identity VALUES (?)', (uuid.uuid4().hex,))
            tables = [r[1] for r in db.execute("PRAGMA table_list")
                      if r[0] == "main" and r[2] == "table"]
            self._reconcile_triggers(db, tables)
            self._schema_version = db.execute('PRAGMA schema_version').fetchone()[0]
            self._known_tables = set(tables)
        self._observed_state_signature = state_signature() if state_signature else None

    def _ensure_current_triggers(self):
        with self.connect() as db:
            version = db.execute('PRAGMA schema_version').fetchone()[0]
            if version == self._schema_version:
                return
            with self._migration_lock:
                version = db.execute('PRAGMA schema_version').fetchone()[0]
                if version == self._schema_version:
                    return
                tables = [r[1] for r in db.execute("PRAGMA table_list")
                          if r[0] == "main" and r[2] == "table"]
                new_tables = set(tables) - self._known_tables
                self._reconcile_triggers(db, tables)
                # A table can be created and populated before this process next
                # notices the schema change. Triggers protect future writes, so
                # explicitly invalidate the corresponding cached view for rows
                # that already existed when the table appeared.
                changed_scopes = set()
                for table in new_tables:
                    if table not in STATE_TABLES and table not in TRANSCRIPT_TABLES:
                        continue
                    if db.execute(f'SELECT 1 FROM "{table}" LIMIT 1').fetchone():
                        if table in STATE_TABLES or table == "runtime_items":
                            changed_scopes.add("state")
                        if table in TRANSCRIPT_TABLES:
                            changed_scopes.add("transcripts")
                for scope in changed_scopes:
                    db.execute('UPDATE sync_scope_generation SET value=value+1 WHERE scope=?', (scope,))
                self._schema_version = db.execute('PRAGMA schema_version').fetchone()[0]
                self._known_tables = set(tables)

    @staticmethod
    def _normalized_trigger_sql(sql):
        return ' '.join(sql.split()).rstrip(';')

    @classmethod
    def _reconcile_triggers(cls, db, tables):
        # Keep the historical broad clock for scheduler write observation while
        # changing only missing or obsolete trigger definitions. Schema drift
        # is common during startup; rebuilding every scoped trigger for an
        # unrelated table would needlessly invalidate active SQLite statements.
        desired = {}
        for table in tables:
            if table.startswith(('sync_', 'sqlite_')) or not re.fullmatch(r'[a-zA-Z0-9_]+', table):
                continue
            for op in ('INSERT', 'UPDATE', 'DELETE'):
                name = f'sync_watch_{table}_{op}'
                desired[name] = f'''CREATE TRIGGER "{name}" AFTER {op} ON "{table}" BEGIN
                    UPDATE sync_generation SET value=value+1 WHERE id=1; END'''
        for scope, owned in (("state", STATE_TABLES), ("transcripts", TRANSCRIPT_TABLES)):
            for table in sorted(owned.intersection(tables)):
                for op in ('INSERT', 'UPDATE', 'DELETE'):
                    name = f'sync_scope_{scope}_{table}_{op}'
                    desired[name] = f'''CREATE TRIGGER "{name}" AFTER {op} ON "{table}" BEGIN
                        UPDATE sync_scope_generation SET value=value+1 WHERE scope='{scope}'; END'''
        if "runtime_items" in tables:
            # The chat-state snapshot only reads item existence for empty-lead
            # eligibility. Stream deltas update the transcript, not that flag.
            desired['sync_scope_state_runtime_items_first'] = '''CREATE TRIGGER sync_scope_state_runtime_items_first
                AFTER INSERT ON runtime_items
                WHEN NOT EXISTS (
                    SELECT 1 FROM runtime_items
                    WHERE agent=NEW.agent AND id<>NEW.id LIMIT 1
                )
                BEGIN UPDATE sync_scope_generation SET value=value+1 WHERE scope='state'; END'''
            desired['sync_scope_state_runtime_items_last'] = '''CREATE TRIGGER sync_scope_state_runtime_items_last
                AFTER DELETE ON runtime_items
                WHEN NOT EXISTS (SELECT 1 FROM runtime_items WHERE agent=OLD.agent)
                BEGIN UPDATE sync_scope_generation SET value=value+1 WHERE scope='state'; END'''

        existing = dict(db.execute(
            "SELECT name,sql FROM sqlite_master WHERE type='trigger' AND "
            "(name LIKE 'sync_watch_%' OR name LIKE 'sync_scope_%')"
        ).fetchall())
        owned_prefixes = ('sync_watch_', 'sync_scope_')
        for name in sorted(existing.keys() - desired.keys()):
            if name.startswith(owned_prefixes):
                quoted_name = name.replace('"', '""')
                db.execute(f'DROP TRIGGER "{quoted_name}"')
        for name, sql in desired.items():
            old_sql = existing.get(name)
            if (old_sql is not None and
                    cls._normalized_trigger_sql(old_sql) == cls._normalized_trigger_sql(sql)):
                continue
            if old_sql is not None:
                db.execute(f'DROP TRIGGER "{name}"')
            db.execute(sql)

    def identity(self):
        with self.connect() as db:
            return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                    'syncProtocol': PROTOCOL_VERSION, 'chatState': True}

    def generations(self):
        return self.generation_state()['generations']

    def _observe_state_signature(self):
        if self.state_signature is None:
            return
        current = self.state_signature()
        if current is None:
            # Volatile snapshot inputs are sampled with nonblocking locks. Skip
            # this tick when their owner is busy and retry at the next poll.
            return
        with self._signature_lock:
            if current == self._observed_state_signature:
                return
            self._ensure_current_triggers()
            with self.connect() as db:
                db.execute("UPDATE sync_scope_generation SET value=value+1 WHERE scope='state'")
            self._observed_state_signature = current
            self._volatile_epoch += 1

    def generation_state(self):
        self._observe_state_signature()
        self._ensure_current_triggers()
        with self.connect() as db:
            return {
                'protocol': PROTOCOL_VERSION,
                'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                'generations': {r[0]: r[1] for r in db.execute('SELECT scope,value FROM sync_scope_generation')},
            }

    def generation(self):
        self._ensure_current_triggers()
        with self.connect() as db:
            return db.execute('SELECT value FROM sync_generation WHERE id=1').fetchone()[0]

    def legacy_generation(self):
        # Preserve broad table-write notifications plus volatile Runtime.snapshot
        # state without changing the scheduler's persistent write_generation.
        self._observe_state_signature()
        return self.generation() + self._volatile_epoch

    def scope_lock(self, scope):
        return self.locks[zlib.crc32(str(scope).encode()) % SCOPE_STRIPES]

    @staticmethod
    def document(row):
        return {'id': row[1], 'payload': row[2], 'seq': row[0], '_deleted': bool(row[3])}

    def _put(self, db, scope, key, payload, deleted=False):
        encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
        old = db.execute('SELECT payload, deleted FROM sync_documents WHERE scope=? AND id=?', (scope, key)).fetchone()
        if old and old[0] == encoded and bool(old[1]) == deleted:
            return False
        db.execute('INSERT OR REPLACE INTO sync_documents(scope,id,payload,deleted) VALUES (?,?,?,?)',
                   (scope, key, encoded, int(deleted)))
        if scope == 'drafts':
            db.execute("UPDATE sync_scope_generation SET value=value+1 WHERE scope='drafts'")
            db.execute('UPDATE sync_generation SET value=value+1 WHERE id=1')
        return True

    def pull(self, scope, after=0, limit=100):
        after, limit = max(0, int(after)), min(100, max(1, int(limit)))
        with self.scope_lock(scope):
            if scope in {'state', 'state:chat'}:
                gen, payload = self.shared_snapshot(scope)
                deleted = False
                generation_scope = 'state'
            elif scope.startswith('transcript:') and len(scope) < 300:
                generation_scope = 'transcripts'
                gen = self.generations()[generation_scope]
                try:
                    payload, deleted = self.transcript(scope.split(':', 1)[1]), False
                except ValueError:
                    payload, deleted = {}, True
            elif scope == 'drafts':
                generation_scope = 'drafts'
                gen = self.generations()[generation_scope]
            else:
                raise ValueError('Invalid sync scope')
            with self.connect() as db:
                if scope != 'drafts':
                    self._put(db, scope, scope, payload, deleted)
                rows = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND seq>? ORDER BY seq LIMIT ?',
                                  (scope, after, limit)).fetchall()
                return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                        'generation': gen, 'documents': [self.document(row) for row in rows],
                        'checkpoint': {'seq': rows[-1][0] if rows else after}}

    def shared_snapshot(self, scope):
        generations = self.generations()
        generation = generations['state']
        cached = self.snapshots.get(scope)
        if cached and cached[0] == generation:
            return cached
        payload = dict(self.chat_snapshot() if scope == 'state:chat' and self.chat_snapshot else self.snapshot())
        payload.pop('token', None)
        payload.pop('at', None)
        self.snapshot_builds += 1
        self.invalidation_count += 1
        self.snapshots[scope] = (generation, payload)
        return generation, payload

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
                if (not isinstance(device, str) or not device or not isinstance(session, str) or not session
                        or f'{device}:{session}' != key or ('id' in value and value.get('id') != key)):
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
                if not isinstance(value.get('text'), str) or len(value['text']) > 2_000_000:
                    raise ValueError('Invalid draft text')
                old = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND id=?', ('drafts', key)).fetchone()
                if old and (not assumed or assumed_payload != old[2] or assumed.get('_deleted', False) != bool(old[3])):
                    if encoded != old[2] or new.get('_deleted', False) != bool(old[3]):
                        conflicts.append(self.document(old))
                        continue
                self._put(db, 'drafts', key, value, bool(new.get('_deleted')))
        return conflicts
