"""SQLite-backed RxDB pull checkpoints. Action receipts remain in the runtime."""
import json
import re
import threading
import uuid


class SyncStore:
    def __init__(self, connect, snapshot, transcript):
        self.connect, self.snapshot, self.transcript = connect, snapshot, transcript
        self.lock = threading.RLock()
        with self.connect() as db:
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
            tables = [r[1] for r in db.execute("PRAGMA table_list")
                      if r[0] == "main" and r[2] == "table"]
            for table in tables:
                if table.startswith(('sync_', 'sqlite_')) or not re.fullmatch(r'[a-zA-Z0-9_]+', table):
                    continue
                for op in ('INSERT', 'UPDATE', 'DELETE'):
                    db.execute(f'''CREATE TRIGGER IF NOT EXISTS sync_watch_{table}_{op}
                        AFTER {op} ON {table} BEGIN
                        UPDATE sync_generation SET value=value+1 WHERE id=1; END''')

    def identity(self):
        with self.connect() as db:
            return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0]}

    def generation(self):
        with self.connect() as db:
            return db.execute('SELECT value FROM sync_generation WHERE id=1').fetchone()[0]

    @staticmethod
    def document(row):
        return {'id': row[1], 'payload': row[2], 'seq': row[0], '_deleted': bool(row[3])}

    def _put(self, db, scope, key, payload, deleted=False):
        encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
        old = db.execute('SELECT payload, deleted FROM sync_documents WHERE scope=? AND id=?', (scope, key)).fetchone()
        if old and old[0] == encoded and bool(old[1]) == deleted:
            return
        # Replacing assigns a monotonically increasing checkpoint to the current version.
        db.execute('INSERT OR REPLACE INTO sync_documents(scope,id,payload,deleted) VALUES (?,?,?,?)',
                   (scope, key, encoded, int(deleted)))

    def pull(self, scope, after=0, limit=100):
        after, limit = max(0, int(after)), min(100, max(1, int(limit)))
        with self.lock:
            if scope == 'state':
                payload = dict(self.snapshot())
                payload.pop('token', None)
                payload.pop('at', None)
                deleted = False
            elif scope.startswith('transcript:') and len(scope) < 300:
                try:
                    payload, deleted = self.transcript(scope.split(':', 1)[1]), False
                except ValueError:
                    payload, deleted = {}, True
            elif scope != 'drafts':
                raise ValueError('Invalid sync scope')
            with self.connect() as db:
                if scope != 'drafts':
                    self._put(db, scope, scope, payload, deleted)
                rows = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND seq>? ORDER BY seq LIMIT ?',
                                  (scope, after, limit)).fetchall()
                return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                        'documents': [self.document(row) for row in rows],
                        'checkpoint': {'seq': rows[-1][0] if rows else after}}

    def push_drafts(self, rows):
        if not isinstance(rows, list) or len(rows) > 100:
            raise ValueError('Invalid draft batch')
        conflicts = []
        with self.lock, self.connect() as db:
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
                assumed = row.get('assumedMasterState')
                if assumed is not None and not isinstance(assumed, dict):
                    raise ValueError('Invalid assumed draft state')
                alternatives = value.get('alternatives', [])
                if not isinstance(alternatives, list) or not all(isinstance(text, str) for text in alternatives):
                    raise ValueError('Invalid draft alternatives')
                # Each browser owns its branch. Other branches remain visible, never overwritten.
                if not isinstance(value.get('text'), str) or len(value['text']) > 2_000_000:
                    raise ValueError('Invalid draft text')
                old = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND id=?', ('drafts', key)).fetchone()
                if old and (not assumed or assumed.get('payload') != old[2]
                            or assumed.get('_deleted', False) != bool(old[3])):
                    if new['payload'] != old[2] or new.get('_deleted', False) != bool(old[3]):
                        conflicts.append(self.document(old))
                        continue
                self._put(db, 'drafts', key, value, bool(new.get('_deleted')))
            db.execute('UPDATE sync_generation SET value=value+1 WHERE id=1')
        return conflicts
