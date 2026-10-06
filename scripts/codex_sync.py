"""SQLite-backed RxDB pull checkpoints. Action receipts remain in the runtime."""
import json
import hashlib
import sqlite3
import threading
import time
import uuid
import zlib
from contextlib import contextmanager
from codex_startup_memory import mark as startup_memory_mark
from codex_sqlite import scope as sqlite_scope

SCOPE_STRIPES = 64
# Windows pull state together after each RESYNC. Without a write in between,
# they share one snapshot. The age bound limits in-memory state staleness.
SNAPSHOT_REUSE_SECONDS = 2
TRANSCRIPT_MAX_TOMBSTONES = 512
TRANSCRIPT_ORDER_ID = '@order'
TRANSCRIPT_META_ID = '@meta'
TRANSCRIPT_FLOOR_ID = '@floor'
TRANSCRIPT_REVISION_ID = '@revision'


class SyncStore:
    def __init__(self, connect, snapshot, transcript, chat_snapshot=None, state_signature=None):
        self.connect, self.snapshot, self.transcript = connect, snapshot, transcript
        self.chat_snapshot = chat_snapshot
        self.state_signature = state_signature
        self._observed_state_signature = None
        self._signature_lock = threading.Lock()
        self._entity_prune_lock = threading.Lock()
        self.entity_prune_status = {"status": "idle", "updated": time.time(), "deleted": 0}
        # Transcript and legacy snapshot comparisons stay serial within their
        # stripe. The fixed stripe count bounds memory. Entity reads use SQLite
        # snapshots without these stripes.
        self.locks = tuple(threading.RLock() for _ in range(SCOPE_STRIPES))
        self.snapshots = {}
        with self.connection("SyncStore.initialize") as db:
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

    @contextmanager
    def connection(self, site="SyncStore"):
        """Guard the provider connection at every SyncStore handoff."""
        with self.connect() as db:
            with sqlite_scope(db, site):
                yield db

    def __del__(self):
        reader = getattr(self, '_version_reader', None)
        if reader is not None:
            reader.close()

    def identity(self) -> dict[str, object]:
        reader = getattr(self, '_version_reader', None)
        if reader is None:
            with self.connection("SyncStore.identity") as db:
                workspace_id = db.execute('SELECT id FROM sync_identity').fetchone()[0]
        else:
            with self._version_lock:
                workspace_id = reader.execute('SELECT id FROM sync_identity').fetchone()[0]
        return {'workspaceId': workspace_id, 'syncProtocol': 2,
                **({'chatState': True} if self.chat_snapshot else {})}

    def generation(self):
        # A persistent reader sees one data_version change per committed writer
        # transaction. The old row triggers wrote the same page for every row.
        self._ensure_versions()
        with self._version_lock:
            return self._version_reader.execute('PRAGMA data_version').fetchone()[0]

    def generation_state(self):
        # The legacy generation also covers file-backed/state:chat clients.
        # Transcript pulls still use their per-agent revision to skip unchanged
        # projections; the shared stream is only an invalidation hint.
        if self.state_signature:
            current = self.state_signature()
            with self._signature_lock:
                if current is not None and current != self._observed_state_signature:
                    with self.connection("SyncStore.external_state") as db:
                        db.execute('UPDATE sync_generation SET value=value+1 WHERE id=1')
                    self._observed_state_signature = current
        generation = self.generation()
        revisions = self.transcript_revisions(generation)
        return {"protocol": 2, **self.identity(), "generations": {
            "state": generation, "transcripts": generation,
            "drafts": self.draft_sequence()},
            **({"transcriptRevisions": revisions} if revisions is not None else {})}

    def transcript_revisions(self, generation):
        """Read compact chat revisions once per committed database change."""
        with self._version_lock:
            cached = getattr(self, '_transcript_revisions_cache', None)
            if cached is not None and cached[0] == generation:
                return cached[1]
            try:
                rows = self._version_reader.execute(
                    "SELECT a.id,COALESCE(r.revision,0) FROM runtime_agents a "
                    "LEFT JOIN runtime_transcript_revisions r ON r.agent=a.id",
                ).fetchall()
            except sqlite3.OperationalError:
                return None
            revisions = {agent: int(revision) for agent, revision in rows}
            self._transcript_revisions_cache = (generation, revisions)
            return revisions

    def entity_sequence(self):
        from codex_sync_entities import max_seq
        with self.connection("SyncStore.entity_sequence") as db:
            return max_seq(db)

    def _schedule_entity_pruning(self):
        """Continue tombstone cleanup outside task writes and HTTP pulls."""
        if not self._entity_prune_lock.acquire(blocking=False):
            return
        self.entity_prune_status = {"status": "running", "updated": time.time(), "deleted": 0}

        def prune():
            total_deleted = 0
            try:
                from codex_sync_entities import (ENTITY_TOMBSTONE_LIMIT,
                                                 prune_entity_tombstones)
                while True:
                    try:
                        with self.connection("SyncStore.prune_entities") as db:
                            deleted = prune_entity_tombstones(db)
                            remaining = int(db.execute(
                                "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'"
                            ).fetchone()[0])
                    except sqlite3.OperationalError as error:
                        if 'locked' not in str(error).lower() and 'busy' not in str(error).lower():
                            raise
                        self.entity_prune_status = {"status": "waitingForLock", "updated": time.time(),
                                                    "deleted": total_deleted}
                        time.sleep(0.15)
                        continue
                    total_deleted += deleted
                    self.entity_prune_status = {"status": "running", "updated": time.time(),
                                                "deleted": total_deleted, "remaining": remaining}
                    if deleted == 0 or remaining <= ENTITY_TOMBSTONE_LIMIT:
                        break
                    time.sleep(0.15)
                self.entity_prune_status = {"status": "complete", "updated": time.time(),
                                            "deleted": total_deleted, "remaining": remaining}
            except Exception as error:
                # Keep the failure visible; a later entity pull can retry.
                self.entity_prune_status = {"status": "error", "updated": time.time(),
                                            "deleted": total_deleted,
                                            "error": f"{type(error).__name__}: {error}"[:500]}
            finally:
                self._entity_prune_lock.release()

        threading.Thread(target=prune, name='entity-tombstone-pruner', daemon=True).start()

    def draft_sequence(self):
        with self.connection("SyncStore.draft_sequence") as db:
            return db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_documents WHERE scope='drafts'").fetchone()[0]

    def stream_batch(self, scope, after, limit=100):
        """Read one bounded durable stream batch; never retain per-client events."""
        after = int(after)
        limit = min(100, max(1, int(limit)))
        if scope == 'state:entities:v1':
            from codex_sync_entities import entity_tombstone_floor, max_seq
            with self.connection("SyncStore.stream_entities") as db:
                high, floor = max_seq(db), entity_tombstone_floor(db)
                if after > high:
                    return {'kind': 'cursor-ahead', 'cursor': after, 'maxSeq': high}
                if after and after < floor:
                    return {'kind': 'reset', 'floor': floor, 'maxSeq': high}
                rows = db.execute('''SELECT collection,id,seq,payload,deleted FROM sync_entities
                    WHERE collection NOT LIKE 'transcript:%' AND seq>? AND seq<=?
                    ORDER BY seq LIMIT ?''', (after, high, limit)).fetchall()
                documents = [{'id': 'entity:' + row[0] + ':' + row[1], 'payload': row[3],
                              'seq': row[2], '_deleted': bool(row[4])} for row in rows]
                cursor = documents[-1]['seq'] if len(documents) == limit else high
                return {'kind': 'changes' if documents else 'idle', 'documents': documents,
                        'cursor': cursor, 'maxSeq': high, 'floor': floor}
        if scope == 'drafts':
            with self.connection("SyncStore.stream_drafts") as db:
                high = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_documents WHERE scope='drafts'").fetchone()[0]
                if after > high:
                    return {'kind': 'cursor-ahead', 'cursor': after, 'maxSeq': high}
                rows = db.execute('''SELECT seq,id,payload,deleted FROM sync_documents
                    WHERE scope='drafts' AND seq>? ORDER BY seq LIMIT ?''', (after, limit)).fetchall()
                documents = [self.document(row) for row in rows]
                cursor = documents[-1]['seq'] if len(documents) == limit else high
                return {'kind': 'changes' if documents else 'idle', 'documents': documents,
                        'cursor': cursor, 'maxSeq': high}
        if scope.startswith('transcript:') and len(scope) < 300:
            result = self.pull(scope, after, limit)
            documents = result['documents']
            cursor = result['checkpoint']['seq']
            if after > cursor:
                return {'kind': 'cursor-ahead', 'cursor': after, 'maxSeq': cursor}
            return {'kind': 'changes' if documents else 'idle', 'documents': documents,
                    'cursor': cursor, 'maxSeq': cursor}
        raise ValueError('Invalid sync stream scope')

    def _ensure_versions(self):
        # Existing make_server closures retain their SyncStore across a live
        # patch, so all new state must be initialized on first use.
        if getattr(self, '_versions_ready', False):
            return
        lock = self.__dict__.setdefault('_version_lock', threading.RLock())
        with lock:
            if getattr(self, '_versions_ready', False):
                return
            with self.connection("SyncStore.ensure_versions") as db:
                db.execute('''CREATE TABLE IF NOT EXISTS sync_versions (
                    seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
                    hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL)''')
                # Remove the old row-trigger watches. Coarse old-client SSE
                # uses data_version; entity sync is driven by sync_entities.
                for (name,) in db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'sync_watch_%'"):
                    db.execute('DROP TRIGGER "' + name + '"')
                if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'").fetchone():
                    db.execute('''CREATE TABLE IF NOT EXISTS runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0)''')
                    for table, key in (("runtime_agents", "id"), ("runtime_items", "agent")):
                        if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone():
                            continue
                        for operation in ("INSERT", "UPDATE", "DELETE"):
                            trigger = f"sync_transcript_revision_{table}_{operation.lower()}"
                            if operation == "INSERT":
                                agent = f"NEW.{key}"
                                body = f'''INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES ({agent},1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;'''
                            elif operation == "DELETE":
                                agent = f"OLD.{key}"
                                body = f'''INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES ({agent},1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;'''
                            else:
                                old_agent, new_agent = f"OLD.{key}", f"NEW.{key}"
                                body = f'''INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES ({new_agent},1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT {old_agent},1 WHERE {old_agent}!={new_agent}
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1;'''
                            db.execute(f"CREATE TRIGGER IF NOT EXISTS {trigger} AFTER {operation} ON {table} BEGIN {body} END")
                path = db.execute('PRAGMA database_list').fetchone()[2]
            self._version_reader = sqlite3.connect('file:' + path + '?mode=ro', uri=True,
                                                    check_same_thread=False, timeout=10)
            self._versions_ready = True

    def scope_lock(self, scope):
        return self.locks[zlib.crc32(str(scope).encode()) % SCOPE_STRIPES]

    def transcript_revision(self, agent):
        """Return the per-agent durable revision, or None on older stores."""
        try:
            with self.connection("SyncStore.transcript_revision") as db:
                row = db.execute(
                    "SELECT revision FROM runtime_transcript_revisions WHERE agent=?",
                    (agent,),
                ).fetchone()
        except sqlite3.OperationalError:
            return None
        return int(row[0]) if row else 0

    def transcript_revision_matches(self, scope, revision, after):
        with self.connection("SyncStore.transcript_revision_matches") as db:
            row = db.execute(
                "SELECT hash,deleted FROM sync_entities WHERE collection=? AND id=?",
                (scope, TRANSCRIPT_REVISION_ID),
            ).fetchone()
            high = db.execute(
                "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection=?",
                (scope,),
            ).fetchone()[0]
        if not row or row[1] or after < high:
            return None
        digest = hashlib.sha256(str(revision).encode()).hexdigest()
        return high if row[0] == digest else None

    @staticmethod
    def document(row):
        return {'id': row[1], 'payload': row[2], 'seq': row[0], '_deleted': bool(row[3])}

    def transcript_pull(self, scope, after, payload, deleted, revision=None):
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

        # Hashing arbitrarily many transcript items is CPU work. Complete it
        # before BEGIN IMMEDIATE so other writers do not wait for serialization.
        item_hashes = {key: digest(item) for key, item in items_by_id.items()} if not deleted else {}
        order_hash = digest(order) if not deleted else None
        metadata_hash = digest(metadata) if not deleted else digest({})
        revision_hash = digest(revision) if revision is not None else None

        with self.connection("SyncStore.transcript_pull") as db:
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
                put(TRANSCRIPT_META_ID, metadata_hash, True)
                if revision_hash is not None:
                    put(TRANSCRIPT_REVISION_ID, revision_hash, False)
            else:
                live_keys = {'item:' + key for key in items_by_id}
                for entity_id, value_hash in item_hashes.items():
                    put('item:' + entity_id, value_hash, False)
                for key, row in tuple(current.items()):
                    if key.startswith('item:') and key not in live_keys and not row[3]:
                        put(key, row[2], True)
                put(TRANSCRIPT_ORDER_ID, order_hash, False)
                put(TRANSCRIPT_META_ID, metadata_hash, False)
                if revision_hash is not None:
                    put(TRANSCRIPT_REVISION_ID, revision_hash, False)

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

    def entity_maintenance_needed(self, db):
        """Check the existing window rules within the pull's read snapshot."""
        from codex_entity_contracts import monitor_records
        from codex_sync_entities import encoded, project
        tables = {row[0] for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN "
            "('runtime_agents','runtime_events','runtime_tasks','runtime_monitors')")}
        markers = dict(db.execute("SELECT key,value FROM sync_entity_meta WHERE key IN "
            "('seeded','agent_organization_fields','task_window_migrated','event_window_seq')"))
        if ('seeded' not in markers
                or ('runtime_agents' in tables and
                    int(markers.get('agent_organization_fields', '0')) < 2)
                or ({'runtime_tasks', 'runtime_agents'} <= tables and 'task_window_migrated' not in markers)):
            return True
        if 'runtime_events' in tables:
            sequence = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'").fetchone()[0]
            if 'event_window_seq' not in markers or int(markers['event_window_seq']) != sequence:
                return True
        if 'runtime_monitors' in tables:
            recent = monitor_records(db)
            stored = dict(db.execute("SELECT id,hash FROM sync_entities WHERE collection='monitor' "
                                     "AND deleted=0 LIMIT ?", (len(recent) + 1,)))
            if set(stored) != {str(record['id']) for record in recent}:
                return True
            for record in recent:
                key = str(record['id'])
                _, digest, _ = encoded('monitor', key, project('monitor', record))
                if stored[key] != digest:
                    return True
        return False

    def entity_pull(self, after, limit, fresh, initial_high, reset_support, priority_id):
        """Use SQLite snapshots and its writer lock without a shared scope stripe."""
        from codex_sync_entities import (entity_tombstone_floor, max_seq,
                                         seed, sync_task_window,
                                         sync_event_window, sync_monitor_window,
                                         ENTITY_TOMBSTONE_COUNT_KEY, ENTITY_TOMBSTONE_LIMIT)
        with self.connection("SyncStore.pull") as db:
            db.execute('BEGIN')
            startup_memory_mark("first-renderer-sync-pull")
            if self.entity_maintenance_needed(db):
                # Do not upgrade a read snapshot after another writer commits.
                # The existing maintenance functions read again under the writer.
                db.rollback()
                db.execute('BEGIN IMMEDIATE')
                seed(db, self.chat_snapshot or self.snapshot)
                startup_memory_mark("entity-seed")
                # Retire old task DTOs gradually so existing checkpoints
                # consume the resulting tombstones through ordinary deltas.
                sync_task_window(db)
                sync_event_window(db)
                sync_monitor_window(db)
                db.commit()
                db.execute('BEGIN')
            count = db.execute("SELECT value FROM sync_entity_meta WHERE key=?",
                               (ENTITY_TOMBSTONE_COUNT_KEY,)).fetchone()
            if count is None or int(count[0]) > ENTITY_TOMBSTONE_LIMIT:
                self._schedule_entity_pruning()
            high = max_seq(db)
            floor = entity_tombstone_floor(db)
            initial_high = (min(high, max(0, int(initial_high)))
                            if fresh and int(initial_high) > 0 else high)
            if reset_support and not fresh and after > 0 and after < floor:
                return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                        'reset': True, 'floor': floor, 'maxSeq': high}
            # A new browser has no rows to remove. Existing checkpoints
            # still receive tombstones through the ordinary delta path.
            if after == 0 and fresh and isinstance(priority_id, str):
                rows = db.execute('''SELECT collection,id,seq,payload,deleted FROM sync_entities
                                 WHERE collection NOT LIKE 'transcript:%' AND seq>?
                                   AND (deleted=0 OR seq>?)
                                   AND (?=0 OR deleted=0 OR seq>?)
                                 ORDER BY CASE WHEN collection='agent' AND id=? THEN 0 ELSE 1 END,
                                          seq LIMIT ?''',
                              (after, floor, int(bool(fresh)), initial_high, priority_id, limit)).fetchall()
            else:
                rows = db.execute('''SELECT collection,id,seq,payload,deleted FROM sync_entities
                                 WHERE collection NOT LIKE 'transcript:%' AND seq>?
                                   AND (deleted=0 OR seq>?)
                                   AND (?=0 OR deleted=0 OR seq>?)
                                 ORDER BY seq LIMIT ?''',
                              (after, floor, int(bool(fresh)), initial_high, limit)).fetchall()
            documents = [{'id': 'entity:' + row[0] + ':' + row[1], 'payload': row[3],
                          'seq': row[2], '_deleted': bool(row[4])} for row in rows]
            checkpoint = (documents[-1]['seq'] if len(documents) == limit else high)
            return {'workspaceId': db.execute('SELECT id FROM sync_identity').fetchone()[0],
                    'documents': documents, 'checkpoint': {'seq': checkpoint},
                    'maxSeq': high, 'initialHigh': initial_high if fresh else 0}

    def pull(self, scope, after=0, limit=100, fresh=False, initial_high=0,
             reset_support=False, priority_id=None):
        after = max(0, int(after))
        limit = min(500 if scope == 'state:entities:v1' else 100, max(1, int(limit)))
        self._ensure_versions()
        if scope == 'state:entities:v1':
            # This path has no cached compare-and-replace state. SQLite keeps
            # its read snapshot and maintenance writes consistent. A scope
            # stripe would queue readers behind unrelated transcript work.
            return self.entity_pull(after, limit, fresh, initial_high, reset_support, priority_id)
        with self.scope_lock(scope):
            if scope == 'state' or (scope == 'state:chat' and self.chat_snapshot):
                encoded, digest = self.shared_snapshot(scope)
                deleted = False
            elif scope.startswith('transcript:') and len(scope) < 300:
                revision = self.transcript_revision(scope.split(':', 1)[1])
                if revision is not None:
                    checkpoint = self.transcript_revision_matches(scope, revision, after)
                    if checkpoint is not None:
                        return {'workspaceId': self.identity()['workspaceId'],
                                'documents': [], 'checkpoint': {'seq': checkpoint}}
                try:
                    payload, deleted = self.transcript(scope.split(':', 1)[1]), False
                except ValueError:
                    payload, deleted = {}, True
                if scope.startswith('transcript:'):
                    return self.transcript_pull(scope, after, payload, deleted, revision)
            elif scope != 'drafts':
                raise ValueError('Invalid sync scope')
            if scope.startswith('transcript:'):
                return self.transcript_pull(scope, after, payload, deleted)
            with self.connection("SyncStore.pull") as db:
                if scope == 'drafts':
                    rows = db.execute('SELECT seq,id,payload,deleted FROM sync_documents WHERE scope=? AND seq>? ORDER BY seq LIMIT ?',
                                      (scope, after, limit)).fetchall()
                    documents = [self.document(row) for row in rows]
                else:
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
            return cached[2], cached[3]
        payload = dict(self.chat_snapshot() if scope == 'state:chat' else self.snapshot())
        payload.pop('token', None)
        payload.pop('at', None)
        encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
        digest = hashlib.sha256(encoded.encode('utf-8')).hexdigest()
        # A write during the snapshot changes the generation; the next pull rebuilds.
        # Retain the wire representation in the existing cache. The raw object
        # has no caller after this point and would otherwise be kept as well.
        self.snapshots[scope] = (generation, time.monotonic(), encoded, digest)
        return encoded, digest

    def push_drafts(self, rows):
        if not isinstance(rows, list) or len(rows) > 100:
            raise ValueError('Invalid draft batch')
        conflicts = []
        # Lazy DDL must finish before the draft write transaction. The
        # initializer opens its own connection and can otherwise deadlock with
        # the writer that reaches _put below.
        if rows:
            self._ensure_versions()
        with self.scope_lock('drafts'), self.connection("SyncStore.draft_write") as db:
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
                        or not isinstance(session, str) or not session):
                    raise ValueError('Draft identity does not match its key')
                prefix, suffix = f'{device}:', f':{session}'
                writer = key[len(prefix):-len(suffix)] if key.startswith(prefix) and key.endswith(suffix) else ''
                tab_branch = bool(writer and ':' not in writer)
                if (key != f'{device}:{session}' and not tab_branch
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
