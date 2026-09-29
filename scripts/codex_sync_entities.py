"""Small renderer DTOs and bounded, per-entity sync versions."""
import hashlib
import json
import sqlite3


AGENT_FIELDS = frozenset("""
    id name status source kind parentId rootId threadId orchestratorId orchestratorName
    isLead role sharedRoomId model provider effort fastMode accountKey cwd worktree created updated
    turnId turnStatus inFlight compactions tokensUsed contextUsage error tail canSend
    launcherAlive empty yoloMode agentMode agentModeRevision agentModeSupported
    workerDefaults pendingSettings pendingSettingsAccountKey queuedSettings quickCreate nativeThreadBlock
    daybreakEnabled accountTransfer
    overview nativeRelease activity nativeStatus startAttempt provider panelVersion panelDataVersion unreadCount lastReadAt deletedAt
    autoWake voiceState nativeError retryAt hasUnread hasQuestion hasApproval
    statusDetail lastAnswer lastCompletedTurn nextTurnSettingsSupported readStateSupported
""".split())

COLLECTION_FIELDS = {
    "room": frozenset("id name kind members rootId updated userHidden projectPath radio peerTeamId peerTeamName lastMessage".split()),
    # Match recent_tasks(): task details are fetched on demand from the workspace API.
    "task": frozenset("id turnId agent kind status created finished name command query cwd processId durationMs timeout_ms stdinClosed stdinCloseRequested stdinError cancelRequested exitCode bytes log outputTruncated".split()),
    "monitor": frozenset("id agent status created finished name command cwd processId durationMs timeout_ms stdinClosed stdinCloseRequested stdinError cancelRequested exitCode tail error bytes log outputTruncated".split()),
    "complaint": frozenset("id leadId author authorName leadName title status needsResponse created readAt leadStopped leadDeleted recipient version".split()),
    "request": frozenset("id method agent status created createdAt at updated updatedAt deferred error result params title".split()),
    "rule": frozenset("id agent name enabled description".split()),
    "project": frozenset("id path name created accountKey accountRevision accountKeys organizationRevision peerTeamsRevision folders".split()),
    "peerTeam": frozenset("id name projectPath members".split()),
    "chat": frozenset("id name members kind messageCount tail lastMessageAt".split()),
    "edge": frozenset("id source target kind".split()),
    "event": frozenset("id agent kind status created error".split()),
    "work": frozenset("id rootId agent status title".split()),
    "workspace": frozenset("connected rateLimits rateLimitsByAccount nativeNotices projectOrganizationVersion peerTeamsVersion tasksHistoryLimit stateDir".split()),
}


def _bounded(value, key=""):
    if isinstance(value, str):
        limits = {"overview": 9000, "error": 2000, "tail": 2000, "description": 2000,
                  "command": 2000, "query": 2000, "text": 4000, "lastAnswer": 4000}
        maximum = limits.get(key, 12000)
        return value[:maximum]
    if isinstance(value, list):
        return [_bounded(item) for item in value[:200]]
    if isinstance(value, dict):
        return {name: _bounded(item, name) for name, item in value.items()}
    return value


def project(collection, record):
    """Return only renderer-owned fields; never expose a raw runtime record."""
    if not isinstance(record, dict):
        return None
    fields = AGENT_FIELDS if collection == "agent" else COLLECTION_FIELDS.get(collection)
    if fields is None:
        return None
    result = {key: _bounded(value, key) for key, value in record.items() if key in fields}
    if collection == "agent":
        for field, allowed in {
            "activity": ("phase",),
            "nativeStatus": ("error",),
            "startAttempt": ("prepareError", "responseError"),
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
    return result


def encoded(collection, key, value, deleted=False):
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
      CREATE INDEX IF NOT EXISTS sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
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


def seed(db, snapshot):
    """Seed once from the compatible view while the caller holds a write lock."""
    if db.execute("SELECT 1 FROM sync_entity_meta WHERE key='seeded'").fetchone():
        return
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
                                                  "projectOrganizationVersion", "peerTeamsVersion",
                                                  "tasksHistoryLimit") if key in runtime}
    meta["stateDir"] = snapshot.get("stateDir", "")
    put(db, "workspace", "current", meta)
    db.execute("INSERT INTO sync_entity_meta VALUES ('seeded','1')")


def sync_task_window(db, batch_size=100, force=False):
    """Match recent_tasks(); retire legacy rows in bounded batches, then skip pulls."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tasks'").fetchone():
        return 0
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'").fetchone():
        return 0
    migrated = db.execute("SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone()
    if migrated and not force:
        return 0
    eligible = """SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
        ON json_extract(t.record,'$.agent')=a.id
        WHERE json_extract(a.record,'$.deletedAt') IS NULL
          AND json_extract(t.record,'$.status')='running'
        UNION ALL
        SELECT record FROM (SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
        ON json_extract(t.record,'$.agent')=a.id
        WHERE json_extract(a.record,'$.deletedAt') IS NULL
          AND json_extract(t.record,'$.status')!='running'
        ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)"""
    for (raw,) in db.execute(eligible):
        record = json.loads(raw)
        if record.get("id"):
            put(db, "task", str(record["id"]), record)
    stale = db.execute(f"""SELECT e.id FROM sync_entities e
        WHERE e.collection='task' AND e.deleted=0 AND NOT EXISTS (
          SELECT 1 FROM runtime_tasks t JOIN runtime_agents a
            ON json_extract(t.record,'$.agent')=a.id
          WHERE t.id=e.id AND json_extract(a.record,'$.deletedAt') IS NULL
            AND (json_extract(t.record,'$.status')='running' OR t.id IN (
              SELECT t2.id FROM runtime_tasks t2 JOIN runtime_agents a2
                ON json_extract(t2.record,'$.agent')=a2.id
              WHERE json_extract(a2.record,'$.deletedAt') IS NULL
                AND json_extract(t2.record,'$.status')!='running'
              ORDER BY json_extract(t2.record,'$.created') DESC LIMIT 100)))
        LIMIT ?""", (max(1, int(batch_size)),)).fetchall()
    for (key,) in stale:
        put(db, "task", key, {}, deleted=True)
    if not stale:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1') "
                   "ON CONFLICT(key) DO UPDATE SET value='1'")
    return len(stale)


def _trim_live_task_history(db):
    """Keep only the newest hundred archived task DTOs; running tasks are unbounded."""
    rows = db.execute("""SELECT id FROM sync_entities
        WHERE collection='task' AND deleted=0
          AND json_extract(payload,'$.value.status')!='running'
        ORDER BY CAST(json_extract(payload,'$.value.created') AS REAL) DESC""").fetchall()
    for (key,) in rows[100:]:
        put(db, "task", key, {}, deleted=True)


def _backfill_task_history(db):
    """Refill archived slots from the bounded, created-indexed runtime window."""
    rows = db.execute("""SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
        ON json_extract(t.record,'$.agent')=a.id
        WHERE json_extract(a.record,'$.deletedAt') IS NULL
          AND json_extract(t.record,'$.status')!='running'
        ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100""").fetchall()
    for (raw,) in rows:
        record = json.loads(raw)
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
    recent = db.execute("""SELECT id, CAST(json_extract(payload,'$.value.created') AS REAL) created
        FROM sync_entities WHERE collection='task' AND deleted=0
          AND json_extract(payload,'$.value.status')!='running'
        ORDER BY created DESC LIMIT 100""").fetchall()
    retained = {row[0] for row in recent}
    oldest = recent[-1][1] if recent else None
    created = float(record.get("created") or 0)
    if key in retained:
        put(db, "task", key, record)
        return True
    if len(recent) < 100 or oldest is None:
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
    rows = db.execute("""SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
        ON json_extract(t.record,'$.agent')=a.id
        WHERE json_extract(a.record,'$.deletedAt') IS NULL
          AND json_extract(t.record,'$.status')='running'
        UNION ALL
        SELECT record FROM (SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
          ON json_extract(t.record,'$.agent')=a.id
          WHERE json_extract(a.record,'$.deletedAt') IS NULL
            AND json_extract(t.record,'$.status')!='running'
          ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)""").fetchall()
    for (raw,) in rows:
        record = json.loads(raw)
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
    return row[0]
