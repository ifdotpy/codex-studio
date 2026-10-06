"""Typed renderer DTO projections and bounded per-entity sync versions."""
import hashlib
import json
import logging
import math
import sqlite3
from typing import TYPE_CHECKING, Annotated, Any, cast

from pydantic import AfterValidator, TypeAdapter

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
from codex_records import AgentRecord, JsonObject, RoomRecord
from studio_api.models import JsonValue, _validate_finite_json

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from codex_runtime import Runtime

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
    name: frozenset(model.model_fields)  # type: ignore[attr-defined]  # typed-narrowing: Pydantic v2 supplies model_fields dynamically
    for name, model in _DTO_MODELS.items()
    if name != "agent"
}
def _validate_finite_entity_payload(payload: SyncEntityPayload) -> SyncEntityPayload:
    _validate_finite_json(payload.value.model_dump(mode="python", exclude_unset=True))
    return payload


_SYNC_ENTITY_PAYLOAD_ADAPTER: TypeAdapter[SyncEntityPayload] = TypeAdapter(
    Annotated[SyncEntityPayload, AfterValidator(_validate_finite_entity_payload)]
)
_STRING_LIMITS = {
    "overview": 9000, "error": 2000, "tail": 2000, "description": 2000,
    "command": 2000, "query": 2000, "text": 4000, "lastAnswer": 4000,
}
_AGENT_FIELD_ALLOWLISTS = {
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
    "capacityRetry": ("id", "threadId", "epoch", "accountKey", "status",
                      "acceptedTurnId", "updatedAt", "dueAt", "claimedAt", "reason"),
    "usageResume": ("id", "status", "cause", "reason", "updatedAt", "plannedAt", "dueAt"),
    "contextRepairWait": ("scope", "error"),
}
_AGENT_REPROJECT_FIELDS = frozenset((*_AGENT_FIELD_ALLOWLISTS, "nativeRelease", "overview"))
_REPORTED_BAD_ENTITIES: set[tuple[str, str, str]] = set()
_REPORTED_BAD_FIELDS: set[tuple[str, str, str, str]] = set()


def _report_bad_entity(collection: str, key: str, error: BaseException) -> None:
    identity = (collection, key, type(error).__name__)
    if identity not in _REPORTED_BAD_ENTITIES:
        _REPORTED_BAD_ENTITIES.add(identity)
        logging.getLogger(__name__).warning(
            "Skipping malformed sync entity collection=%s id=%s error=%s",
            collection, key, identity[2])


def _report_bad_field(collection: str, key: str, field: str, error: BaseException) -> None:
    identity = (collection, key, field, type(error).__name__)
    if identity not in _REPORTED_BAD_FIELDS:
        _REPORTED_BAD_FIELDS.add(identity)
        logging.getLogger(__name__).warning(
            "Omitting invalid display field collection=%s id=%s field=%s error=%s",
            collection, key, field, identity[3])


def _bounded(value: JsonValue, key: str = "", list_limit: int = 200) -> JsonValue:
    if isinstance(value, str):
        maximum = _STRING_LIMITS.get(key, 12000)
        return value[:maximum]
    if isinstance(value, list):
        return [_bounded(item, list_limit=list_limit) for item in value[:list_limit]]
    if isinstance(value, dict):
        limit = 10000 if key == "sidebarOrder" else list_limit
        return {name: _bounded(item, name, limit) for name, item in value.items()}
    return value


def _contains_nonfinite(value: JsonValue) -> bool:
    if isinstance(value, float):
        return not math.isfinite(value)
    if isinstance(value, dict):
        return any(_contains_nonfinite(item) for item in value.values())
    if isinstance(value, list):
        return any(_contains_nonfinite(item) for item in value)
    return False


def project(collection: str, record: JsonValue) -> JsonValue | None:
    """Return only renderer-owned fields; never expose a raw runtime record."""
    if not isinstance(record, dict):
        _report_bad_entity(collection, "", TypeError("record is not an object"))
        return None
    model = _DTO_MODELS.get(collection)
    if model is None:
        _report_bad_entity(collection, str(record.get("id", "")), ValueError("unknown entity collection"))
        return None
    fields = AGENT_FIELDS if collection == "agent" else COLLECTION_FIELDS[collection]
    result = {
        key: _bounded(value, key)
        for key, value in record.items()
        if key in fields and not (
            collection == "agent"
            and key in _AGENT_REPROJECT_FIELDS
            and isinstance(value, dict)
        )
    }
    for field, value in tuple(result.items()):
        if _contains_nonfinite(value):
            result.pop(field, None)
            _report_bad_field(collection, str(record.get("id", "")), field,
                              ValueError("display field contains a non-finite number"))
    if collection == "agent":
        for field, allowed in _AGENT_FIELD_ALLOWLISTS.items():
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
                if key in {"task", "taskTruncated", "result", "resultTruncated", "resultTurnId", "resultFile"}
            }
    while True:
        try:
            validated = model.model_validate(result)  # type: ignore[attr-defined]  # typed-narrowing: Pydantic v2 supplies model_validate dynamically
            break
        except Exception as error:
            error_reader = getattr(error, "errors", None)
            failures = error_reader() if callable(error_reader) else []
            bad_fields = {
                str(item["loc"][0]) for item in failures
                if item.get("loc") and isinstance(item["loc"][0], str)
                and item["loc"][0] in result and item["loc"][0] != "id"
            }
            if not bad_fields:
                _report_bad_entity(collection, str(record.get("id", "")), error)
                return None
            for field in bad_fields:
                result.pop(field, None)
                _report_bad_field(collection, str(record.get("id", "")), field, error)
    return cast(JsonValue, validated.model_dump(mode="json", exclude_unset=True))


def validate_entity_payload(payload: str) -> SyncEntityPayload:
    """Validate a canonical sync entity JSON envelope without changing its bytes."""
    return _SYNC_ENTITY_PAYLOAD_ADAPTER.validate_json(payload)


def validate_stored_entity_payload(payload: str, collection: str, key: str, deleted: bool) -> None:
    """Validate persisted live DTOs and the deliberately empty tombstone envelope."""
    if not deleted:
        entity = validate_entity_payload(payload)
        if entity.collection != collection or entity.id != key:
            raise ValueError("entity identity does not match its row")
        return
    value = json.loads(payload)
    if (not isinstance(value, dict) or value.get("collection") != collection
            or value.get("id") != key or not isinstance(value.get("value"), dict)):
        raise ValueError("invalid entity tombstone")


def encoded(collection: str, key: str, value: JsonValue, deleted: bool = False) -> tuple[str, str, bool]:
    payload = json.dumps({"collection": collection, "id": key, "value": value},
                         sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return payload, hashlib.sha256(payload.encode("utf-8")).hexdigest(), bool(deleted)


def ensure_tables(db: sqlite3.Connection) -> None:
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


def register_functions(db: sqlite3.Connection) -> None:
    def payload(collection, key, raw, deleted):
        # type: (str, str, str, int) -> str
        try:
            value = json.loads(raw)
        except (TypeError, ValueError):
            value = {}
        dto = project(collection, value) if not deleted else {}
        return encoded(collection, key, dto, bool(deleted))[0]

    db.create_function("sync_entity_payload", 4, payload)
    db.create_function("sync_entity_hash", 1,
                       lambda value: hashlib.sha256(value.encode("utf-8")).hexdigest())


def install_bypass_triggers(db: sqlite3.Connection) -> None:
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


def put(db: sqlite3.Connection, collection: str, key: str, record: Any, deleted: bool = False) -> bool:
    dto = project(collection, record) if not deleted else {}
    if dto is None:
        return False
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


def patch(db: sqlite3.Connection, collection: str, key: str, changes: Any) -> bool:
    """Change fields of the stored renderer view; a raw record must not replace it."""
    try:
        row = db.execute("SELECT payload,deleted FROM sync_entities WHERE collection=? AND id=?",
                         (collection, key)).fetchone()
    except sqlite3.OperationalError:
        return False  # The entity store has not been created on this database yet.
    if not row or row[1] or not row[0]:
        return False
    try:
        entity = json.loads(row[0])
        value = entity.get("value") if isinstance(entity, dict) else None
        if not isinstance(value, dict):
            raise TypeError("entity value is not an object")
    except (TypeError, ValueError) as error:
        _report_bad_entity(collection, key, error)
        return False
    return put(db, collection, key, {**value, **changes})


def retire_closed_requests(db: sqlite3.Connection) -> int:
    """Remove answered or deleted requests that an older server kept as live entities."""
    rows = db.execute("SELECT id FROM sync_entities WHERE collection='request' AND deleted=0 "
                      "AND COALESCE(json_extract(payload,'$.value.status'),'')!='pending'").fetchall()
    for (key,) in rows:
        put(db, "request", key, {}, True)
    return len(rows)


def _rows(db: sqlite3.Connection, table: str) -> list[tuple[str, str]]:
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone():
        return []
    return [(str(key), str(raw)) for key, raw in
            db.execute(f"SELECT id,record FROM {table}").fetchall()]


def _stored_record(collection: str, key: str, raw: str) -> JsonObject | None:
    try:
        value = json.loads(raw)
    except (TypeError, ValueError) as error:
        _report_bad_entity(collection, key, error)
        return None
    if not isinstance(value, dict):
        _report_bad_entity(collection, key, TypeError("record is not an object"))
        return None
    return cast(JsonObject, value)


def _refresh_runtime_entities(db: sqlite3.Connection, runtime_owner: "Runtime | None",
                              canvas_owner: "Canvas | None") -> int:
    """Project stored records through the same views used by Runtime.put."""
    changed = 0
    agent_rows = _rows(db, "runtime_agents")
    managed_views: list[dict[str, object]] = []
    for key, raw in agent_rows:
        record = _stored_record("agent", key, raw)
        if record is None:
            continue
        try:
            agent_view = (runtime_owner.agent_entity_view(db, cast(AgentRecord, record))
                          if runtime_owner is not None else cast(dict[str, object], record))
        except Exception as error:
            _report_bad_entity("agent", key, error)
            continue
        if not record.get("deletedAt"):
            managed_views.append(agent_view)
        if record.get("deletedAt"):
            exists = db.execute("SELECT 1 FROM sync_entities WHERE collection='agent' AND id=? AND deleted=0",
                                (key,)).fetchone()
            if exists:
                changed += bool(put(db, "agent", key, {}, True))
        else:
            changed += bool(put(db, "agent", key, agent_view))

    # Wave and registered threads are stored by Canvas/file sources, not in
    # runtime_agents. Refresh their current views on seed and versioned upgrade.
    current_canvas_agents: set[str] = set()
    canvas_agents_ok = False
    if canvas_owner is not None:
        try:
            canvas_threads = canvas_owner.threads(runtime_agents=managed_views)
            canvas_agents_ok = True
        except Exception as error:
            _report_bad_entity("agent", "canvas", error)
            canvas_threads = []
        for value in canvas_threads:
            if value.get("source") == "managed":
                continue
            key = value.get("id")
            if isinstance(key, str) and key:
                current_canvas_agents.add(key)
                changed += bool(put(db, "agent", key, value))
    if canvas_owner is not None and canvas_agents_ok:
        for key, payload in db.execute(
            "SELECT id,payload FROM sync_entities WHERE collection='agent' AND deleted=0").fetchall():
            try:
                source = json.loads(payload).get("value", {}).get("source")
            except (TypeError, ValueError, AttributeError):
                source = None
            if source in {"app-server", "orchestrator-reference", "registered"} and key not in current_canvas_agents:
                changed += bool(put(db, "agent", str(key), {}, deleted=True))

    # Runtime.put resolves a stored room through this exact targeted room view.
    current_rooms: set[str] = set()
    for key, _raw in _rows(db, "runtime_rooms"):
        record = _stored_record("room", key, _raw)
        if record is None:
            current_rooms.add(key)
            continue
        view: dict[str, object] | None
        if runtime_owner is None:
            view = cast(dict[str, object], record)
        else:
            try:
                room = runtime_owner.room_entity_view(db, key)
            except Exception as error:
                _report_bad_entity("room", key, error)
                current_rooms.add(key)
                continue
            view = cast(dict[str, object], room) if room is not None else None
        if view is None:
            continue
        current_rooms.add(key)
        changed += bool(put(db, "room", key, view))
    for (key,) in db.execute(
            "SELECT id FROM sync_entities WHERE collection='room' AND deleted=0").fetchall():
        if key not in current_rooms:
            changed += bool(put(db, "room", str(key), {}, deleted=True))

    for key, raw in _rows(db, "runtime_complaints"):
        record = _stored_record("complaint", key, raw)
        if record is not None:
            try:
                complaint_view = (runtime_owner.complaint_entity_view(db, record)
                                  if runtime_owner is not None else cast(dict[str, object], record))
            except Exception as error:
                _report_bad_entity("complaint", key, error)
                continue
            changed += bool(put(db, "complaint", key, complaint_view))
    for key, raw in _rows(db, "runtime_projects"):
        record = _stored_record("project", key, raw)
        if record is not None:
            changed += bool(put(db, "project", key, cast(dict[str, object], record)))
    for key, raw in _rows(db, "runtime_requests"):
        record = _stored_record("request", key, raw)
        if record is not None:
            if record.get("status") != "pending":
                exists = db.execute("SELECT 1 FROM sync_entities WHERE collection='request' AND id=? AND deleted=0",
                                    (key,)).fetchone()
                if exists:
                    changed += bool(put(db, "request", key, {}, True))
            else:
                changed += bool(put(db, "request", key, record))
    for key, raw in _rows(db, "runtime_rules"):
        record = _stored_record("rule", key, raw)
        if record is not None:
            changed += bool(put(db, "rule", key, record))
    for key, raw in _rows(db, "runtime_work"):
        record = _stored_record("work", key, raw)
        if record is not None:
            changed += bool(put(db, "work", key, record))

    sync_task_window(db)
    sync_monitor_window(db)
    sync_event_window(db)

    if runtime_owner is not None:
        from codex_peer_teams import sync_entities as sync_peer_team_entities
        changed += sync_peer_team_entities(runtime_owner, db)
    if canvas_owner is not None:
        try:
            chats = canvas_owner.chats(db=db)
            chats_ok = True
        except Exception as error:
            _report_bad_entity("chat", "canvas", error)
            chats = []
            chats_ok = False
        current_chats = {str(value["id"]) for value in chats if value.get("id")}
        for value in chats:
            if value.get("id"):
                changed += bool(put(db, "chat", str(value["id"]), value))
        if chats_ok:
            for (key,) in db.execute(
                    "SELECT id FROM sync_entities WHERE collection='chat' AND deleted=0").fetchall():
                if key not in current_chats:
                    changed += bool(put(db, "chat", str(key), {}, deleted=True))
        try:
            agents = canvas_owner.threads(runtime_agents=managed_views)
            edges = canvas_owner.edges(agents, db=db)
            edges_ok = True
        except Exception as error:
            _report_bad_entity("edge", "canvas", error)
            edges = []
            edges_ok = False
        current_edges = {str(value["id"]) for value in edges if value.get("id")}
        for value in edges:
            if value.get("id"):
                changed += bool(put(db, "edge", str(value["id"]), value))
        if edges_ok:
            for (key,) in db.execute(
                    "SELECT id FROM sync_entities WHERE collection='edge' AND deleted=0").fetchall():
                if key not in current_edges:
                    changed += bool(put(db, "edge", str(key), {}, deleted=True))

    prior_workspace: dict[str, JsonValue] = {}
    prior_row = db.execute("SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()
    if prior_row and prior_row[0]:
        try:
            parsed_workspace = json.loads(prior_row[0]).get("value", {})
            if isinstance(parsed_workspace, dict):
                prior_workspace = parsed_workspace
        except (TypeError, ValueError, AttributeError):
            _report_bad_entity("workspace", "current", ValueError("invalid stored workspace payload"))
    workspace_value = dict(prior_workspace)
    if runtime_owner is not None:
        try:
            workspace_value = runtime_owner.workspace_entity_view(db, workspace_value)
        except Exception as error:
            _report_bad_entity("workspace", "current", error)
            workspace_value = prior_workspace
    elif canvas_owner is not None:
        workspace_value["stateDir"] = str(canvas_owner.root)
    workspace_value.setdefault("connected", False)
    workspace_value.setdefault("projectOrganizationVersion", 1)
    workspace_value.setdefault("peerTeamsVersion", 1)
    workspace_value.setdefault("tasksHistoryLimit", 100)
    changed += bool(put(db, "workspace", "current", workspace_value))
    return changed


def upgrade_agent_organization(db: sqlite3.Connection, runtime_owner: "Runtime | None" = None,
                               canvas_owner: "Canvas | None" = None) -> int:
    """Restore organization metadata and refresh renderer entities once, by marker."""
    marker = db.execute(
        "SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()
    original_marker = marker[0] if marker else None
    version = int(marker[0]) if marker else 0
    if version >= 3:
        return 0
    changed = 0
    if version < 1:
        for key, raw in _rows(db, "runtime_agents"):
            record = _stored_record("agent", key, raw)
            if record is None:
                continue
            values = {field: record.get(field, default) for field, default in (
                ('pinned', False), ('archived', False), ('projectFolder', None), ('projectFolderRevision', 0))}
            changed += bool(patch(db, 'agent', key, values))
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','1') "
                   "ON CONFLICT(key) DO UPDATE SET value='1'")
    if runtime_owner is None:
        if original_marker is None:
            db.execute("DELETE FROM sync_entity_meta WHERE key='agent_organization_fields'")
        else:
            db.execute("UPDATE sync_entity_meta SET value=? WHERE key='agent_organization_fields'",
                       (original_marker,))
        return changed
    changed += _refresh_runtime_entities(db, runtime_owner, canvas_owner)
    if runtime_owner is not None and canvas_owner is not None:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','3') "
                   "ON CONFLICT(key) DO UPDATE SET value='3'")
    elif runtime_owner is not None:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','2') "
                   "ON CONFLICT(key) DO UPDATE SET value='2'")
    return changed


def seed(db: sqlite3.Connection, runtime_owner: "Runtime | None" = None,
         canvas_owner: "Canvas | None" = None) -> None:
    """Seed or upgrade entity rows from stored runtime records under the write lock."""
    seeded = bool(db.execute("SELECT 1 FROM sync_entity_meta WHERE key='seeded'").fetchone())
    marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()
    if seeded and marker and int(marker[0]) >= 3:
        return
    if seeded and runtime_owner is None:
        return
    if seeded:
        upgrade_agent_organization(db, runtime_owner, canvas_owner)
        return
    _refresh_runtime_entities(db, runtime_owner, canvas_owner)
    db.execute("INSERT INTO sync_entity_meta VALUES ('seeded','1')")
    if runtime_owner is not None and canvas_owner is not None:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','3') "
                   "ON CONFLICT(key) DO UPDATE SET value='3'")
    elif runtime_owner is not None:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','2') "
                   "ON CONFLICT(key) DO UPDATE SET value='2'")


def sync_event_window(db: sqlite3.Connection) -> int:
    """Materialize the shared bounded event window."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_events'").fetchone():
        return 0
    changed = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'").fetchone()[0]
    marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='event_window_seq'").fetchone()
    if marker and int(marker[0]) == changed:
        return 0
    recent = event_records(db)
    selected: list[str] = []
    for record in recent:
        key = record.get("id")
        if not isinstance(key, str) or not key:
            _report_bad_entity("event", "", ValueError("event record has no valid id"))
            continue
        selected.append(key)
        put(db, "event", key, record)
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


def _recent_monitor_records(db: sqlite3.Connection) -> list[Any]:
    return monitor_records(db)


def sync_monitor_window(db: sqlite3.Connection) -> int:
    """Match active monitors and the recent terminal window in the chat snapshot."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_monitors'").fetchone():
        return 0
    recent = _recent_monitor_records(db)
    kept: set[str] = set()
    for row in recent:
        key = row.get("id")
        if not isinstance(key, str) or not key:
            _report_bad_entity("monitor", "", ValueError("monitor record has no valid id"))
            continue
        kept.add(key)
        put(db, "monitor", key, row)
    stale = db.execute("SELECT id FROM sync_entities WHERE collection='monitor' AND deleted=0").fetchall()
    retired = 0
    for (key,) in stale:
        if key not in kept:
            retired += bool(put(db, "monitor", key, {}, deleted=True))
    return retired


def sync_monitor_write(db: sqlite3.Connection, record: Any) -> None:
    """Project a monitor write and retire the displaced terminal record."""
    sync_monitor_window(db)


def sync_monitor_agent_change(db: sqlite3.Connection) -> None:
    """Refresh monitors after an agent's deletedAt state changes."""
    sync_monitor_window(db)


def sync_task_window(db: sqlite3.Connection, batch_size: int = 100, force: bool = False) -> int:
    """Match the shared bounded task window; retire legacy rows in batches."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tasks'").fetchone():
        return 0
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'").fetchone():
        return 0
    migrated = db.execute("SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone()
    if migrated and not force:
        return 0
    eligible = task_records(db)
    eligible_ids: set[str] = set()
    for record in eligible:
        key = record.get("id")
        if not isinstance(key, str) or not key:
            _report_bad_entity("task", "", ValueError("task record has no valid id"))
            continue
        eligible_ids.add(key)
        put(db, "task", key, record)
    candidates = db.execute("SELECT id FROM sync_entities WHERE collection='task' AND deleted=0").fetchall()
    stale = [(key,) for (key,) in candidates if key not in eligible_ids][:max(1, int(batch_size))]
    for (key,) in stale:
        put(db, "task", key, {}, deleted=True)
    if not stale:
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1') "
                   "ON CONFLICT(key) DO UPDATE SET value='1'")
    return len(stale)


def _trim_live_task_history(db: sqlite3.Connection) -> None:
    """Keep only the newest hundred archived task DTOs; running tasks are unbounded."""
    rows = db.execute(f"""SELECT id FROM sync_entities
        WHERE collection='task' AND deleted=0
          AND json_extract(payload,'$.value.status')!='running'
        ORDER BY CAST(json_extract(payload,'$.value.created') AS REAL) DESC""").fetchall()
    for (key,) in rows[TASK_ARCHIVE_WINDOW:]:
        put(db, "task", key, {}, deleted=True)


def _backfill_task_history(db: sqlite3.Connection) -> None:
    """Refill archived slots from the bounded, created-indexed runtime window."""
    for record in task_records(db):
        if record.get("status") == "running":
            continue
        if record.get("id"):
            put(db, "task", str(record["id"]), record)
    _trim_live_task_history(db)


def sync_task_write(db: sqlite3.Connection, record: Any) -> bool:
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


def sync_task_agent_change(db: sqlite3.Connection, agent_id: str, deleted: bool) -> None:
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


def next_sequence(db: sqlite3.Connection) -> int:
    row = db.execute("""SELECT max(seq)+1 FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions)""").fetchone()
    return row[0]  # type: ignore[no-any-return]  # typed-narrowing: sqlite3 aggregate results are dynamically typed


def max_seq(db: sqlite3.Connection) -> int:
    row = db.execute("SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'").fetchone()
    return max(row[0], entity_tombstone_floor(db))  # type: ignore[no-any-return]  # typed-narrowing: sqlite3 aggregate results are dynamically typed


def entity_tombstone_floor(db: sqlite3.Connection) -> int:
    row = db.execute("SELECT value FROM sync_entity_meta WHERE key=?",
                     (ENTITY_TOMBSTONE_FLOOR_KEY,)).fetchone()
    return int(row[0]) if row else 0


def prune_entity_tombstones(db: sqlite3.Connection, limit: int = ENTITY_TOMBSTONE_LIMIT,
                            batch_size: int = ENTITY_TOMBSTONE_PRUNE_BATCH) -> int:
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
