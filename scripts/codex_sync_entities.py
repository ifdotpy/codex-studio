"""Typed renderer DTO projections and bounded per-entity sync versions."""
import hashlib
import json
import sqlite3
from typing import cast
from codex_entity_contracts import (TASK_ARCHIVE_WINDOW,
                                    event_records, monitor_records, task_records)
from studio_api.sync.models import (
    AgentEntityDto,
    ChatEntityDto,
    ComplaintEntityDto,
    EdgeEntityDto,
    EventEntityDto,
    MonitorEntityDto,
    PeerTeamEntityDto,
    ProjectEntityDto,
    RequestEntityDto,
    RoomEntityDto,
    RuleEntityDto,
    TaskEntityDto,
    WorkEntityDto,
    WorkspaceEntityDto,
    SyncEntityPayload,
)
from studio_api.models import JsonValue

ENTITY_TOMBSTONE_LIMIT = 10_000
ENTITY_TOMBSTONE_PRUNE_BATCH = 500
ENTITY_TOMBSTONE_COUNT_KEY = "entity_tombstone_count"
ENTITY_TOMBSTONE_FLOOR_KEY = "entity_tombstone_floor"


_DTO_MODELS = {
    "agent": AgentEntityDto,
    "room": RoomEntityDto,
    "task": TaskEntityDto,
    "monitor": MonitorEntityDto,
    "complaint": ComplaintEntityDto,
    "request": RequestEntityDto,
    "rule": RuleEntityDto,
    "project": ProjectEntityDto,
    "peerTeam": PeerTeamEntityDto,
    "chat": ChatEntityDto,
    "edge": EdgeEntityDto,
    "event": EventEntityDto,
    "work": WorkEntityDto,
    "workspace": WorkspaceEntityDto,
}
AGENT_FIELDS = frozenset(AgentEntityDto.model_fields)
COLLECTION_FIELDS = {
    name: frozenset(model.model_fields)
    for name, model in _DTO_MODELS.items()
    if name != "agent"
}


def _bounded(value: JsonValue, key: str = "", list_limit: int = 200) -> JsonValue:
    if isinstance(value, str):
        limits = {"overview": 9000, "error": 2000, "tail": 2000, "description": 2000,
                  "command": 2000, "query": 2000, "text": 4000, "lastAnswer": 4000}
        maximum = limits.get(key, 12000)
        return value[:maximum]
    if isinstance(value, list):
        return [_bounded(item, list_limit=list_limit) for item in value[:list_limit]]
    if isinstance(value, dict):
        limit = 10000 if key == "sidebarOrder" else list_limit
        return {name: _bounded(item, name, limit) for name, item in value.items()}
    return value


def project(collection: str, record: JsonValue) -> JsonValue | None:
    """Return only renderer-owned fields; never expose a raw runtime record."""
    if not isinstance(record, dict):
        return None
    model = _DTO_MODELS.get(collection)
    if model is None:
        return None
    fields = AGENT_FIELDS if collection == "agent" else COLLECTION_FIELDS[collection]
    result = {key: _bounded(value, key) for key, value in record.items() if key in fields}
    if collection == "agent":
        for field, allowed in {
            "activity": ("phase", "at", "tools"),
            "nativeStatus": ("phase", "error", "message", "turnId", "at"),
            "nativeSafetyBuffering": (
                "turnId", "threadId", "accountKey", "connectionId", "at",
                "dismissed", "responseStarted", "showBufferingUi", "fasterModel",
            ),
            "nativeSafetyRetry": (
                "id", "stage", "model", "turnId", "created", "updated",
                "epoch", "accountKey", "error", "newThreadId", "acceptedTurnId",
                "requestId", "rpcMethod",
            ),
            "nativeTurnError": ("turnId", "error"),
            "nativeThreadBlock": ("threadId", "error"),
            "connectionCheck": (
                "epoch", "accountKey", "threadId", "turnId", "at", "previousError",
                "nativeState", "restartTurnStatus", "readError",
            ),
            "readState": ("threadId", "turnId", "read", "revision"),
            "startAttempt": ("prepareError", "responseError", "retiredEvents"),
        }.items():
            value = record.get(field)
            if isinstance(value, dict):
                result[field] = {key: _bounded(value[key], "error") for key in allowed if key in value}
        release = record.get("nativeRelease")
        if isinstance(release, dict):
            # Only the values rendered by nativeReleaseLabel cross the wire.
            result["nativeRelease"] = {
                key: release[key] for key in ("phase", "resetPending") if key in release
            }
        overview = record.get("overview")
        if not isinstance(overview, dict) and ("prompt" in record or "lastAnswer" in record):
            task = str(record.get("prompt") or "")
            overview_result = str(record.get("lastAnswer") or "") if (
                record.get("lastCompletedTurn") and not record.get("turnId")
                and not record.get("inFlight") and record.get("status") == "completed"
            ) else ""
            overview = {"task": task[:4000], "taskTruncated": len(task) > 4000,
                        "result": overview_result[:4000], "resultTruncated": len(overview_result) > 4000,
                        "resultTurnId": record.get("lastCompletedTurn") if overview_result else None}
        if isinstance(overview, dict):
            result["overview"] = {
                key: _bounded(value, key)
                for key, value in overview.items()
                if key in {"task", "taskTruncated", "result", "resultTruncated", "resultTurnId"}
            }
    return cast(JsonValue, model.model_validate(result).model_dump(mode="json", exclude_unset=True))


def validate_entity_payload(payload: str) -> SyncEntityPayload:
    """Validate a canonical sync entity JSON envelope without changing its bytes."""
    envelope = SyncEntityPayload.model_validate_json(payload)
    model = _DTO_MODELS[envelope.collection.value]
    model.model_validate(envelope.value)
    return envelope


def encoded(collection: str, key: str, value: JsonValue, deleted: bool = False) -> tuple[str, str, bool]:
    payload = json.dumps({"collection": collection, "id": key, "value": value},
                         sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return payload, hashlib.sha256(payload.encode("utf-8")).hexdigest(), bool(deleted)


def ensure_tables(db):
    db.executescript("""
      CREATE TABLE IF NOT EXISTS sync_entities (
        collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(collection,id));
      CREATE INDEX IF NOT EXISTS sync_entities_seq ON sync_entities(seq);
      CREATE INDEX IF NOT EXISTS sync_entities_collection_seq ON sync_entities(collection,seq);
      CREATE INDEX IF NOT EXISTS sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
      CREATE INDEX IF NOT EXISTS sync_entities_tombstone_order
        ON sync_entities(seq,collection,id)
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%';
      CREATE TABLE IF NOT EXISTS sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS sync_documents (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
        id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
        UNIQUE(scope,id));
      CREATE INDEX IF NOT EXISTS sync_scope_seq ON sync_documents(scope,seq);
      CREATE TABLE IF NOT EXISTS sync_versions (
        seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
        hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
    """)
    db.executescript("""
      CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_insert
      AFTER INSERT ON sync_entities
      WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+1
        WHERE key='entity_tombstone_count';
      END;
      CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_update
      AFTER UPDATE OF deleted,collection ON sync_entities
      WHEN (OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%') !=
           (NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%') BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+
          CASE WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' THEN 1 ELSE -1 END
        WHERE key='entity_tombstone_count';
      END;
      CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_delete
      AFTER DELETE ON sync_entities
      WHEN OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)-1
        WHERE key='entity_tombstone_count';
      END;
    """)
    if not db.execute("SELECT 1 FROM sync_entity_meta WHERE key=?",
                      (ENTITY_TOMBSTONE_COUNT_KEY,)).fetchone():
        # executescript commits by itself. A plain execute() would leave this
        # connection holding an open write transaction that blocks other writers.
        db.executescript(f"""INSERT OR IGNORE INTO sync_entity_meta(key,value)
            SELECT '{ENTITY_TOMBSTONE_COUNT_KEY}',CAST(COUNT(*) AS TEXT) FROM sync_entities
            WHERE deleted=1 AND collection NOT LIKE 'transcript:%';""")
    # This index is built once by SQLite at startup and supports the bounded
    # newest-event pull. IF NOT EXISTS avoids rebuilding it on every start.
    if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_events'").fetchone():
        db.execute("CREATE INDEX IF NOT EXISTS runtime_event_created_id ON runtime_events(created DESC,id)")


def register_functions(db):
    def payload(collection, key, raw, deleted):
        try:
            value = json.loads(raw)
        except (TypeError, ValueError):
            value = {}
        dto = project(collection, value) if not deleted else {}
        return encoded(collection, key, dto, bool(deleted))[0]

    db.create_function("sync_entity_payload", 4, payload)
    db.create_function("sync_entity_hash", 1,
                       lambda value: hashlib.sha256(value.encode("utf-8")).hexdigest())


def install_bypass_triggers(db):
    """Track direct runtime_events writes; Runtime.put owns all other DTO rows."""
    for operation in ("INSERT", "UPDATE", "DELETE"):
        old = operation == "DELETE"
        record = "'{}'" if old else "json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error)"
        key = "OLD.id" if old else "NEW.id"
        deleted = 1 if old else 0
        payload = f"sync_entity_payload('event',{key},{record},{deleted})"
        db.execute(f"""CREATE TRIGGER IF NOT EXISTS sync_entity_event_{operation}
          AFTER {operation} ON runtime_events BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',{key},(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash({payload}),{payload},{deleted})
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
        END""")


def put(db, collection, key, record, deleted=False):
    dto = project(collection, record) if not deleted else {}
    payload, digest, deleted = encoded(collection, key, dto, deleted)
    old = db.execute("SELECT hash,deleted FROM sync_entities WHERE collection=? AND id=?",
                     (collection, key)).fetchone()
    if old and old[0] == digest and bool(old[1]) == deleted:
        return False
    seq = next_sequence(db)
    db.execute("""INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                  VALUES (?,?,?,?,?,?) ON CONFLICT(collection,id) DO UPDATE SET
                  seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted""",
               (collection, key, seq, digest, payload, int(deleted)))
    return True


def patch(db, collection, key, changes):
    """Change fields of the stored renderer view; a raw record must not replace it."""
    try:
        row = db.execute("SELECT payload,deleted FROM sync_entities WHERE collection=? AND id=?",
                         (collection, key)).fetchone()
    except sqlite3.OperationalError:
        return False  # The entity store has not been created on this database yet.
    if not row or row[1] or not row[0]:
        return False
    value = json.loads(row[0]).get("value") or {}
    return put(db, collection, key, {**value, **changes})


def retire_closed_requests(db):
    """Remove answered or deleted requests that an older server kept as live entities."""
    rows = db.execute("SELECT id FROM sync_entities WHERE collection='request' AND deleted=0 "
                      "AND COALESCE(json_extract(payload,'$.value.status'),'')!='pending'").fetchall()
    for (key,) in rows:
        put(db, "request", key, {}, True)
    return len(rows)


def upgrade_agent_organization(db):
    """Restore existing sidebar metadata without replacing derived agent fields."""
    if db.execute("SELECT 1 FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone():
        return 0
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'").fetchone():
        return 0
    changed = 0
    for key, raw in db.execute("SELECT id,record FROM runtime_agents").fetchall():
        record = json.loads(raw)
        values = {field: record.get(field, default) for field, default in (
            ('pinned', False), ('archived', False), ('projectFolder', None), ('projectFolderRevision', 0))}
        changed += bool(patch(db, 'agent', key, values))
    db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','1')")
    return changed


def seed(db, snapshot):
    """Seed once from the compatible view while the caller holds a write lock."""
    upgrade_agent_organization(db)
    if db.execute("SELECT 1 FROM sync_entity_meta WHERE key='seeded'").fetchone():
        return
    # Callers pass a builder: the full snapshot is costly and seeding runs once.
    if callable(snapshot):
        snapshot = snapshot()
    runtime = snapshot.get("runtime") or {}
    threads = {item.get("id"): item for item in snapshot.get("threads", []) if item.get("id")}
    for agent in runtime.get("agents", []):
        threads[agent["id"]] = {**threads.get(agent["id"], {}), **agent}
    for value in threads.values():
        put(db, "agent", value["id"], value, bool(value.get("deletedAt")))
    for name, collection in (("rooms", "room"), ("tasks", "task"), ("monitors", "monitor"),
                             ("complaints", "complaint"), ("requests", "request"),
                             ("rules", "rule"), ("projects", "project"),
                             ("peerTeams", "peerTeam"), ("events", "event"),
                             ("work", "work")):
        for value in runtime.get(name, []) or []:
            if value.get("id"):
                put(db, collection, str(value["id"]), value)
    for value in snapshot.get("chats", []):
        if value.get("id"):
            put(db, "chat", str(value["id"]), value)
    for value in snapshot.get("edges", []):
        if value.get("id"):
            put(db, "edge", str(value["id"]), value)
    for item in snapshot.get("nodes", []):
        if item.get("id") and item.get("id") not in threads:
            put(db, "agent", item["id"], item)
    # Mutable aggregate values are small and independently versioned.
    meta = {key: runtime.get(key) for key in ("connected", "rateLimits", "rateLimitsByAccount", "nativeNotices",
                                                  "projectOrganizationVersion", "sidebarOrder", "peerTeamsVersion",
                                                  "tasksHistoryLimit") if key in runtime}
    meta["stateDir"] = snapshot.get("stateDir", "")
    put(db, "workspace", "current", meta)
    db.execute("INSERT INTO sync_entity_meta VALUES ('seeded','1')")


def sync_event_window(db):
    """Materialize the shared bounded event window."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_events'").fetchone():
        return 0
    changed = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'").fetchone()[0]
    marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='event_window_seq'").fetchone()
    if marker and int(marker[0]) == changed:
        return 0
    recent = event_records(db)
    selected = [record["id"] for record in recent]
    for record in recent:
        put(db, "event", str(record["id"]), record)
    placeholders = ",".join("?" for _ in selected)
    exclusion = f" AND id NOT IN ({placeholders})" if selected else ""
    stale = db.execute("SELECT id FROM sync_entities WHERE collection='event' AND deleted=0" + exclusion,
                       selected).fetchall()
    for (key,) in stale:
        put(db, "event", key, {}, deleted=True)
    final_seq = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'").fetchone()[0]
    db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('event_window_seq',?) "
               "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (str(final_seq),))
    return len(stale)


def _recent_monitor_records(db):
    return monitor_records(db)


def sync_monitor_window(db):
    """Match active monitors and the recent terminal window in the chat snapshot."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_monitors'").fetchone():
        return 0
    recent = _recent_monitor_records(db)
    kept = {str(row["id"]) for row in recent}
    for row in recent:
        put(db, "monitor", str(row["id"]), row)
    stale = db.execute("SELECT id FROM sync_entities WHERE collection='monitor' AND deleted=0").fetchall()
    retired = 0
    for (key,) in stale:
        if key not in kept:
            retired += bool(put(db, "monitor", key, {}, deleted=True))
    return retired


def sync_monitor_write(db, record):
    """Project a monitor write and retire the displaced terminal record."""
    sync_monitor_window(db)


def sync_monitor_agent_change(db):
    """Refresh monitors after an agent's deletedAt state changes."""
    sync_monitor_window(db)


def sync_task_window(db, batch_size=100, force=False):
    """Match the shared bounded task window; retire legacy rows in batches."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tasks'").fetchone():
        return 0
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'").fetchone():
        return 0
    migrated = db.execute("SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone()
    if migrated and not force:
        return 0
    eligible = task_records(db)
    eligible_ids = {str(record["id"]) for record in eligible if record.get("id")}
    for record in eligible:
        if record.get("id"):
            put(db, "task", str(record["id"]), record)
    candidates = db.execute("SELECT id FROM sync_entities WHERE collection='task' AND deleted=0").fetchall()
    stale = [(key,) for (key,) in candidates if key not in eligible_ids][:max(1, int(batch_size))]
    for (key,) in stale:
        put(db, "task", key, {}, deleted=True)
    if not stale:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1') "
                   "ON CONFLICT(key) DO UPDATE SET value='1'")
    return len(stale)


def _trim_live_task_history(db):
    """Keep only the newest hundred archived task DTOs; running tasks are unbounded."""
    rows = db.execute(f"""SELECT id FROM sync_entities
        WHERE collection='task' AND deleted=0
          AND json_extract(payload,'$.value.status')!='running'
        ORDER BY CAST(json_extract(payload,'$.value.created') AS REAL) DESC""").fetchall()
    for (key,) in rows[TASK_ARCHIVE_WINDOW:]:
        put(db, "task", key, {}, deleted=True)


def _backfill_task_history(db):
    """Refill archived slots from the bounded, created-indexed runtime window."""
    for record in task_records(db):
        if record.get("status") == "running":
            continue
        if record.get("id"):
            put(db, "task", str(record["id"]), record)
    _trim_live_task_history(db)


def sync_task_write(db, record):
    """Incrementally project a task write without scanning runtime_tasks."""
    if not db.execute("SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone():
        return False
    key = str(record.get("id") or "")
    if not key:
        return False
    previous = db.execute("""SELECT deleted,json_extract(payload,'$.value.status')
        FROM sync_entities WHERE collection='task' AND id=?""", (key,)).fetchone()
    vacates_history = bool(previous and not previous[0] and previous[1] != "running")
    agent = db.execute("SELECT record FROM runtime_agents WHERE id=?",
                       (str(record.get("agent") or ""),)).fetchone()
    if not agent or json.loads(agent[0]).get("deletedAt"):
        return put(db, "task", key, {}, deleted=True)
    if record.get("status") == "running":
        changed = put(db, "task", key, record)
        if vacates_history:
            _backfill_task_history(db)
        return changed

    # A previously retained archived task remains in the window when it is
    # updated. Otherwise compare it with the 100 live archived rows.
    recent = db.execute(f"""SELECT id, CAST(json_extract(payload,'$.value.created') AS REAL) created
        FROM sync_entities WHERE collection='task' AND deleted=0
          AND json_extract(payload,'$.value.status')!='running'
        ORDER BY created DESC LIMIT {TASK_ARCHIVE_WINDOW}""").fetchall()
    retained = {row[0] for row in recent}
    oldest = recent[-1][1] if recent else None
    created = float(record.get("created") or 0)
    if key in retained:
        put(db, "task", key, record)
        return True
    if len(recent) < TASK_ARCHIVE_WINDOW or oldest is None:
        return put(db, "task", key, record)
    if created > oldest:
        changed = put(db, "task", key, record)
        put(db, "task", recent[-1][0], {}, deleted=True)
        return changed
    return put(db, "task", key, {}, deleted=True)


def sync_task_agent_change(db, agent_id, deleted):
    """Refresh one agent's task window after its rare deletedAt transition."""
    if not db.execute("SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone():
        return
    if deleted:
        rows = db.execute("""SELECT id FROM sync_entities
            WHERE collection='task' AND deleted=0
              AND json_extract(payload,'$.value.agent')=?""", (str(agent_id),)).fetchall()
        for (key,) in rows:
            put(db, "task", key, {}, deleted=True)
    # Agent membership changes are rare, so refill globally only here. This
    # restores tasks from other agents that become eligible when an agent's
    # entities are retired.
    for record in task_records(db):
        if record.get("id"):
            put(db, "task", str(record["id"]), record)
    _trim_live_task_history(db)


def next_sequence(db):
    row = db.execute("""SELECT max(seq)+1 FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions)""").fetchone()
    return row[0]


def max_seq(db):
    row = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'").fetchone()
    return max(row[0], entity_tombstone_floor(db))


def entity_tombstone_floor(db):
    row = db.execute("SELECT value FROM sync_entity_meta WHERE key=?",
                     (ENTITY_TOMBSTONE_FLOOR_KEY,)).fetchone()
    return int(row[0]) if row else 0


def prune_entity_tombstones(db, limit=ENTITY_TOMBSTONE_LIMIT,
                            batch_size=ENTITY_TOMBSTONE_PRUNE_BATCH):
    """Prune one small, committed batch of old entity tombstones.

    The caller runs this in a background worker with a pause between batches.
    Live rows and transcript history are outside this retention policy.
    """
    db.execute("BEGIN IMMEDIATE")
    if not db.execute("SELECT 1 FROM sync_entity_meta WHERE key=?",
                      (ENTITY_TOMBSTONE_COUNT_KEY,)).fetchone():
        db.execute("""INSERT INTO sync_entity_meta(key,value)
            SELECT ?,CAST(COUNT(*) AS TEXT) FROM sync_entities
            WHERE deleted=1 AND collection NOT LIKE 'transcript:%'""",
            (ENTITY_TOMBSTONE_COUNT_KEY,))
    count = int(db.execute("SELECT value FROM sync_entity_meta WHERE key=?",
                           (ENTITY_TOMBSTONE_COUNT_KEY,)).fetchone()[0])
    excess = count - limit
    if excess <= 0:
        db.commit()
        return 0
    rows = db.execute("""SELECT collection,id,seq FROM sync_entities
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%'
        ORDER BY seq,collection,id LIMIT ?""",
                      (min(max(1, int(batch_size)), excess),)).fetchall()
    if not rows:
        db.commit()
        return 0
    floor = max(entity_tombstone_floor(db), max(row[2] for row in rows))
    db.executemany("DELETE FROM sync_entities WHERE collection=? AND id=? AND deleted=1",
                   [(row[0], row[1]) for row in rows])
    db.execute("""INSERT INTO sync_entity_meta(key,value) VALUES(?,?)
        ON CONFLICT(key) DO UPDATE SET value=excluded.value""",
               (ENTITY_TOMBSTONE_FLOOR_KEY, str(floor)))
    db.commit()
    return len(rows)
