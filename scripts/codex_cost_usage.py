"""Compact usage capture and a disposable, incremental session cost index.

The analytics database owns the capture revision. The private index never writes
to it. Existing history is copied in bounded read pages on the first request.
"""
import json
import os
import fcntl
import sqlite3
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Sequence, cast


PAGE_ROWS = 256
JOURNAL_REVISIONS = 8192
COLUMNS = ("seq", "agent", "thread", "turn", "at", "record_agent", "record_thread",
           "record_turn", "has_response", "model", "account_key", "input_tokens",
           "cached_tokens", "write_tokens", "output_tokens", "input_uncached",
           "log_account", "response_id")
GROUP_COLUMNS = ("model", "account_key", "input_tokens", "cached_tokens", "write_tokens",
                 "output_tokens", "input_uncached")


def projection(alias: str) -> str:
    """Use the same SQLite type rules as the legacy cost query."""
    original_record = alias + ".record"
    binary = sqlite3.sqlite_version_info >= (3, 45, 0)
    record = "cost_document" if binary else original_record
    def value(path: str) -> str:
        return f"json_extract({record},'$.{path}')"
    def kind(path: str) -> str:
        return f"json_type({record},'$.{path}')"
    def scalar(path: str) -> str:
        # json_extract returns objects as SQL text, without the JSON subtype.
        # json_array would otherwise embed that subtype as an object again.
        return (f"CASE WHEN {kind(path)} IN ('array','object') THEN json_extract({original_record},'$.{path}') || '' "
                f"ELSE {value(path)} END")
    delta = (f"{kind('delta.inputTokens')} IN ('integer','real','true','false') AND "
             f"{kind('delta.outputTokens')} IN ('integer','real','true','false')")
    tokens = []
    for field in ("inputTokens", "cachedInputTokens", "cacheWriteInputTokens", "outputTokens"):
        tokens.append(f"CASE WHEN {delta} THEN CASE WHEN {kind('delta.' + field)} IN "
                      f"('integer','real') THEN {value('delta.' + field)} END ELSE CASE WHEN "
                      f"{kind('last.' + field)} IN ('integer','real') THEN {value('last.' + field)} END END")
    fields = [f"{alias}.{name}" for name in ("seq", "agent", "thread", "turn", "at")]
    fields += [scalar(path) for path in ("agentId", "threadId", "turnId")]
    fields += [f"CASE WHEN {kind('responseId')} IS NULL OR {kind('responseId')} IN ('null','false') "
               f"OR ({kind('responseId')} IN ('integer','real') AND {value('responseId')}=0) "
               f"OR ({kind('responseId')}='text' AND {value('responseId')}='') "
               f"OR ({kind('responseId')}='array' AND json_array_length({record},'$.responseId')=0) "
               f"OR ({kind('responseId')}='object' AND NOT EXISTS "
               f"(SELECT 1 FROM json_each({record},'$.responseId'))) THEN 0 ELSE 1 END",
               f"CASE WHEN {kind('model')}='text' THEN {value('model')} END",
               f"COALESCE({scalar('accountKey')},'default')"]
    fields += tokens
    fields += [f"CASE WHEN {value('inputTokensAreUncached')}=1 THEN 1 ELSE 0 END",
               scalar("accountKey"), scalar("responseId")]
    result = "json_array(" + ",".join(fields) + ")"
    if binary:
        return (f"(WITH cost_raw AS MATERIALIZED (SELECT jsonb({original_record}) AS cost_document) "
                f"SELECT {result} FROM cost_raw)")
    return result


def install(db: sqlite3.Connection) -> None:
    """Install empty derived tables. Do not scan existing history at startup."""
    db.executescript("""
      CREATE INDEX IF NOT EXISTS analytics_agents_root ON analytics_agents(json_extract(record,'$.rootId'));
      CREATE TABLE IF NOT EXISTS analytics_cost_state_v1 (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), token TEXT NOT NULL);
      INSERT OR IGNORE INTO analytics_cost_state_v1 VALUES (1,lower(hex(randomblob(16))));
      CREATE TABLE IF NOT EXISTS analytics_cost_projection_v1 (id TEXT PRIMARY KEY,root TEXT,packed TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS analytics_cost_projection_seq_v1 ON analytics_cost_projection_v1(json_extract(packed,'$[0]'));
      CREATE TABLE IF NOT EXISTS analytics_cost_roots_v1 (
        root TEXT PRIMARY KEY,revision INTEGER NOT NULL,floor INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS analytics_cost_changes_v1 (
        root TEXT NOT NULL,id TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(root,id));
      CREATE INDEX IF NOT EXISTS analytics_cost_changes_revision_v1 ON analytics_cost_changes_v1(root,revision);
    """)
    for action in ("INSERT", "UPDATE", "DELETE"):
        roots = ["OLD"] if action == "DELETE" else ["NEW"]
        if action == "UPDATE":
            roots.append("OLD")
        body = []
        for alias in roots:
            condition = f"{alias}.root IS NOT NULL"
            if alias == "OLD" and action == "UPDATE":
                condition += " AND OLD.root IS NOT NEW.root"
            body += [f"INSERT INTO analytics_cost_roots_v1 SELECT {alias}.root,1,0 WHERE {condition} "
                     f"ON CONFLICT(root) DO UPDATE SET revision=revision+1,floor=MAX(0,revision+1-{JOURNAL_REVISIONS});",
                     f"INSERT INTO analytics_cost_changes_v1 SELECT root,{alias}.id,revision FROM analytics_cost_roots_v1 "
                     f"WHERE root IS {alias}.root AND {condition} ON CONFLICT(root,id) DO UPDATE SET revision=excluded.revision;",
                     f"DELETE FROM analytics_cost_changes_v1 WHERE root IS {alias}.root AND {condition} AND revision<="
                     f"(SELECT floor FROM analytics_cost_roots_v1 WHERE root IS {alias}.root);"]
        db.execute(f"CREATE TRIGGER IF NOT EXISTS analytics_cost_projection_{action.lower()}_v1 AFTER {action} "
                   "ON analytics_cost_projection_v1 BEGIN " + " ".join(body) + " END")
    for action in ("UPDATE", "DELETE"):
        db.execute(f"CREATE TRIGGER IF NOT EXISTS analytics_cost_usage_before_{action.lower()}_v1 BEFORE {action} "
                   f"ON analytics_usage BEGIN INSERT INTO analytics_cost_projection_v1 "
                   f"SELECT OLD.id,OLD.root,{projection('OLD')} WHERE NOT EXISTS "
                   "(SELECT 1 FROM analytics_cost_projection_v1 WHERE id=OLD.id); END")
    # SQLite REPLACE can remove a conflicting row without its DELETE triggers.
    # Save an unprojected legacy collision before SQLite removes that identity.
    db.execute("CREATE TRIGGER IF NOT EXISTS analytics_cost_usage_before_insert_v1 BEFORE INSERT ON analytics_usage "
               "BEGIN INSERT INTO analytics_cost_projection_v1 SELECT u.id,u.root," + projection("u") +
               " FROM analytics_usage u WHERE (u.seq=NEW.seq OR u.id=NEW.id) AND NOT EXISTS "
               "(SELECT 1 FROM analytics_cost_projection_v1 WHERE id=u.id); END")
    for action in ("INSERT", "UPDATE"):
        db.execute(f"CREATE TRIGGER IF NOT EXISTS analytics_cost_usage_{action.lower()}_v1 AFTER {action} "
                   f"ON analytics_usage BEGIN DELETE FROM analytics_cost_projection_v1 "
                   f"WHERE json_extract(packed,'$[0]')=+NEW.seq AND id IS NOT NEW.id; "
                   + ("DELETE FROM analytics_cost_projection_v1 WHERE id=OLD.id AND OLD.id IS NOT NEW.id; " if action == "UPDATE" else "") +
                   f"INSERT INTO analytics_cost_projection_v1 VALUES "
                   f"(NEW.id,NEW.root,{projection('NEW')}) ON CONFLICT(id) DO UPDATE SET root=excluded.root,packed=excluded.packed "
                   "WHERE root IS NOT excluded.root OR packed IS NOT excluded.packed; END")
    db.execute("CREATE TRIGGER IF NOT EXISTS analytics_cost_usage_delete_v1 AFTER DELETE ON analytics_usage "
               "BEGIN DELETE FROM analytics_cost_projection_v1 WHERE id=OLD.id; END")


def source_state(db: sqlite3.Connection, root: str) -> list[Any] | None:
    # During the online file migration, a legacy overlay remains authoritative.
    # Its writes do not pass through these triggers. Keep the legacy reader.
    if db.execute("SELECT 1 FROM sqlite_temp_master WHERE type='view' AND name='analytics_usage'").fetchone():
        return None
    try:
        row = db.execute("SELECT token,COALESCE((SELECT revision FROM analytics_cost_roots_v1 WHERE root=?),0),"
                         "COALESCE((SELECT floor FROM analytics_cost_roots_v1 WHERE root=?),0) "
                         "FROM analytics_cost_state_v1 WHERE singleton=1", (root, root)).fetchone()
        if not row:
            return None
        path = next(entry[2] for entry in db.execute("PRAGMA database_list") if entry[1] == "main")
        identity = os.stat(path) if path else None
        return [row[0], row[1], [identity.st_dev, identity.st_ino] if identity else None, row[2]]
    except sqlite3.OperationalError as error:
        if "no such table: analytics_cost_" not in str(error):
            raise
        return None


class UsageIndex:
    """Keep exact unpriced groups; catalog edits only price the current groups."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.cursor: sqlite3.Cursor | None = None
        self.capture: dict[str, Any] = {}
        self.guard = open(str(path) + ".lock", "a")
        os.chmod(self.guard.name, 0o600)
        try:
            fcntl.flock(self.guard, fcntl.LOCK_EX)
            descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
            os.close(descriptor)
            os.chmod(path, 0o600)
            self.db = sqlite3.connect(path, timeout=3)
            try:
                self._initialize()
            except sqlite3.DatabaseError:
                # This file is disposable. Keep the per-root guard while replacing
                # a corrupt cache, so another reader cannot open its old inode.
                self.db.close()
                path.unlink()
                descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
                os.close(descriptor)
                self.db = sqlite3.connect(path, timeout=3)
                self._initialize()
        except BaseException:
            if hasattr(self, "db"):
                self.db.close()
            self.guard.close()
            raise

    def _initialize(self) -> None:
        self.db.execute("PRAGMA cache_size=-8192")
        self.db.execute("PRAGMA temp_store=FILE")
        self.db.execute("PRAGMA temp.cache_size=-8192")
        self.db.executescript("""
          CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT);
          CREATE TABLE IF NOT EXISTS rows(id TEXT PRIMARY KEY,seq,agent,thread,turn,at,
            record_agent,record_thread,record_turn,has_response,model,account_key,
            input_tokens,cached_tokens,write_tokens,output_tokens,input_uncached,
            log_account TEXT,response_id TEXT,excluded INTEGER NOT NULL DEFAULT 0,group_key TEXT);
          CREATE INDEX IF NOT EXISTS rows_turn ON rows(agent,thread,turn);
          CREATE INDEX IF NOT EXISTS rows_record_turn ON rows(record_agent,record_turn,at DESC,seq DESC);
          CREATE INDEX IF NOT EXISTS rows_record_thread ON rows(record_agent,record_thread,at DESC,seq DESC);
          CREATE INDEX IF NOT EXISTS rows_model_turn ON rows(record_agent,record_turn,at DESC,seq DESC) WHERE model IS NOT NULL AND NOT excluded;
          CREATE INDEX IF NOT EXISTS rows_model_thread ON rows(record_agent,record_thread,at DESC,seq DESC) WHERE model IS NOT NULL AND NOT excluded;
          CREATE INDEX IF NOT EXISTS rows_receipt ON rows(log_account,thread,response_id);
          CREATE TABLE IF NOT EXISTS groups(key TEXT PRIMARY KEY,model,account_key,input_tokens,
            cached_tokens,write_tokens,output_tokens,input_uncached,count INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS receipts(account_key TEXT,thread_id TEXT,response_id TEXT,
            PRIMARY KEY(account_key,thread_id,response_id)) WITHOUT ROWID;
        """)
        self.applied_rows = 0

    def close(self) -> None:
        self.db.close()
        self.guard.close()

    def __iter__(self) -> Iterator[tuple[Any, ...]]:
        try:
            assert self.cursor is not None
            yield from self.cursor
        finally:
            self.close()

    @staticmethod
    def prune(directory: Path, limit: int) -> None:
        try:
            paths = sorted(directory.glob("usage-v1-*.sqlite3"), key=lambda item: item.stat().st_mtime)
            for path in (paths[:-limit] if limit else paths):
                with open(str(path) + ".lock", "a") as guard:
                    try:
                        fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        continue
                    path.unlink(missing_ok=True)
        except OSError:
            pass

    def _meta(self, key: str) -> list[Any] | None:
        row = self.db.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        try:
            value = json.loads(row[0]) if row else None
            if (isinstance(value, list) and len(value) == 4 and isinstance(value[0], str)
                    and type(value[1]) is int and type(value[3]) is int and 0 <= value[3] <= value[1]
                    and (value[2] is None or isinstance(value[2], list) and len(value[2]) == 2
                         and all(type(part) is int for part in value[2]))):
                return value
            return None
        except (ValueError, TypeError):
            return None

    def _save(self, key: str, value: Any) -> None:
        self.db.execute("INSERT INTO meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        (key, json.dumps(value, separators=(",", ":"))))

    def _source_model(self, columns: str, key: tuple[Any, ...]) -> str | None:
        names = columns.split(",")
        where = " AND ".join(f"u.{name} IS ?" for name in names)
        row = self.db.execute(f"SELECT model FROM rows u WHERE {where} AND model IS NOT NULL AND NOT excluded "
                              "AND (has_response OR NOT EXISTS (SELECT 1 FROM rows r WHERE r.agent=u.agent "
                              "AND r.thread IS u.thread AND r.turn IS u.turn AND r.has_response AND NOT r.excluded)) "
                              "ORDER BY at DESC,seq DESC LIMIT 1", key).fetchone()
        return cast(str, row[0]) if row else None

    def _turn_rows(self, turns: set[tuple[Any, ...]]) -> set[str]:
        result: set[str] = set()
        for turn in turns:
            result.update(row[0] for row in self.db.execute(
                "SELECT id FROM rows WHERE agent IS ? AND thread IS ? AND turn IS ? AND NOT has_response", turn))
        return result

    def _keys(self, ids: set[str]) -> tuple[set[tuple[Any, ...]], set[tuple[Any, ...]]]:
        turn_keys: set[tuple[Any, ...]] = set()
        thread_keys: set[tuple[Any, ...]] = set()
        for identity in ids:
            row = self.db.execute("SELECT record_agent,record_turn,record_thread FROM rows WHERE id=?", (identity,)).fetchone()
            if row:
                turn_keys.add(tuple(row[:2]))
                thread_keys.add((row[0], row[2]))
        return turn_keys, thread_keys

    def _remove_group(self, identity: str) -> None:
        self.db.execute("UPDATE groups SET count=count-1 WHERE key=(SELECT group_key FROM rows WHERE id=?)", (identity,))
        self.db.execute("UPDATE rows SET group_key=NULL WHERE id=?", (identity,))

    def _add_group(self, identity: str) -> None:
        row = self.db.execute("SELECT " + ",".join(COLUMNS) + ",excluded FROM rows WHERE id=?", (identity,)).fetchone()
        if not row or row[18]:
            return
        if not row[8] and self.db.execute("SELECT 1 FROM rows WHERE agent IS ? AND thread IS ? AND turn IS ? "
                                         "AND has_response AND NOT excluded LIMIT 1", row[1:4]).fetchone():
            return
        model = row[9]
        if model is None:
            turn_model = self._source_model("record_agent,record_turn", (row[5], row[7]))
            model = turn_model if turn_model else self._source_model("record_agent,record_thread", (row[5], row[6]))
        group = [model, *row[10:16]]
        key = json.dumps(group, separators=(",", ":"), ensure_ascii=False)
        self.db.execute("INSERT INTO groups VALUES (?,?,?,?,?,?,?,?,1) ON CONFLICT(key) DO UPDATE SET count=count+1",
                        (key, *group))
        self.db.execute("UPDATE rows SET group_key=? WHERE id=?", (key, identity))

    def apply(self, changes: Sequence[tuple[str, str | None]],
              receipt_changes: Iterable[tuple[Any, ...]] = ()) -> None:
        """Recompute affected turns, then missing models whose source changes."""
        ids = {identity for identity, _packed in changes}
        turns = set()
        for identity in ids:
            row = self.db.execute("SELECT agent,thread,turn FROM rows WHERE id=?", (identity,)).fetchone()
            if row:
                turns.add(tuple(row))
        decoded = [(identity, json.loads(packed) if packed is not None else None) for identity, packed in changes]
        turns.update(tuple(row[1:4]) for _identity, row in decoded if row is not None)
        for receipt in receipt_changes:
            for row in self.db.execute("SELECT id,agent,thread,turn FROM rows WHERE log_account IS ? AND thread IS ? AND response_id IS ?", receipt):
                ids.add(row[0])
                turns.add(tuple(row[1:]))
        affected = ids | self._turn_rows(turns)
        turn_keys, thread_keys = self._keys(affected)
        turn_keys.update((row[5], row[7]) for _identity, row in decoded if row is not None)
        thread_keys.update((row[5], row[6]) for _identity, row in decoded if row is not None)
        before_turn = {key: self._source_model("record_agent,record_turn", key) for key in turn_keys}
        before_thread = {key: self._source_model("record_agent,record_thread", key) for key in thread_keys}
        for identity in affected:
            self._remove_group(identity)
        for identity, row in decoded:
            self.db.execute("DELETE FROM rows WHERE id=?", (identity,))
            if row is not None:
                self.db.execute("INSERT INTO rows(id," + ",".join(COLUMNS) + ") VALUES (" + ",".join("?" for _ in range(19)) + ")", (identity, *row))
        affected |= self._turn_rows(turns)
        for identity in affected:
            self.db.execute("UPDATE rows SET excluded=EXISTS (SELECT 1 FROM receipts r WHERE "
                            "r.account_key IS rows.log_account AND r.thread_id IS rows.thread AND r.response_id IS rows.response_id) WHERE id=?", (identity,))
        for columns, keys, before in (("record_agent,record_turn", turn_keys, before_turn),
                                      ("record_agent,record_thread", thread_keys, before_thread)):
            where = " AND ".join(name + " IS ?" for name in columns.split(","))
            for key in keys:
                if before[key] != self._source_model(columns, key):
                    for identity, in self.db.execute(f"SELECT id FROM rows WHERE model IS NULL AND {where}", key):
                        if identity not in affected:
                            self._remove_group(identity)
                            affected.add(identity)
        for identity in affected:
            self._add_group(identity)
        self.db.execute("DELETE FROM groups WHERE count=0")
        self.applied_rows += len(changes)

    def _rebuild_groups(self) -> None:
        # The initial full pass uses only the private compact copy. It holds no
        # analytics snapshot and needs no per-row Python objects.
        self.db.executescript("""
          CREATE TEMP TABLE eligible AS SELECT * FROM rows u WHERE NOT excluded AND
            (has_response OR NOT EXISTS (SELECT 1 FROM rows r WHERE r.agent=u.agent
              AND r.thread IS u.thread AND r.turn IS u.turn AND r.has_response AND NOT r.excluded));
          CREATE TEMP TABLE turn_models AS SELECT record_agent,record_turn,model FROM
            (SELECT record_agent,record_turn,model,ROW_NUMBER() OVER
              (PARTITION BY record_agent,record_turn ORDER BY at DESC,seq DESC) AS rank
             FROM eligible WHERE model IS NOT NULL) WHERE rank=1;
          CREATE INDEX turn_models_key ON turn_models(record_agent,record_turn);
          CREATE TEMP TABLE thread_models AS SELECT record_agent,record_thread,model FROM
            (SELECT record_agent,record_thread,model,ROW_NUMBER() OVER
              (PARTITION BY record_agent,record_thread ORDER BY at DESC,seq DESC) AS rank
             FROM eligible WHERE model IS NOT NULL) WHERE rank=1;
          CREATE INDEX thread_models_key ON thread_models(record_agent,record_thread);
          CREATE TEMP TABLE effective AS SELECT u.id,
            COALESCE(u.model,NULLIF(tm.model,''),hm.model) AS model,u.account_key,
            u.input_tokens,u.cached_tokens,u.write_tokens,u.output_tokens,u.input_uncached
            FROM eligible u LEFT JOIN turn_models tm ON tm.record_agent IS u.record_agent
              AND tm.record_turn IS u.record_turn
            LEFT JOIN thread_models hm ON hm.record_agent IS u.record_agent AND hm.record_thread IS u.record_thread;
          CREATE INDEX effective_key ON effective(id);
        """)
        # executescript commits implicitly. All subsequent changes still need
        # one transaction, so an interrupted build cannot become a valid cache.
        fields = ",".join(GROUP_COLUMNS)
        key = "json_array(" + fields + ")"
        self.db.execute(f"UPDATE rows SET group_key=(SELECT {key} FROM effective e WHERE e.id=rows.id)")
        self.db.execute(f"INSERT INTO groups SELECT {key},{fields},COUNT(*) FROM effective GROUP BY {key}")
        for table in ("effective", "eligible", "turn_models", "thread_models"):
            self.db.execute(f"DROP TABLE {table}")

    def _copy_page(self, page: Sequence[Any]) -> None:
        values = [(row[0], *json.loads(row[2])) for row in page]
        self.db.executemany("INSERT OR REPLACE INTO rows(id," + ",".join(COLUMNS) + ") VALUES (" + ",".join("?" for _ in range(19)) + ")", values)
        self.applied_rows += len(page)

    def refresh(self, source: sqlite3.Connection, root: str, state: list[Any],
                receipts: Iterable[tuple[Any, ...]],
                capture: Callable[[], dict[str, Any]] | None = None) -> sqlite3.Cursor:
        """Release each shared read snapshot before private cache work."""
        previous = self._meta("source") or self._meta("pendingSource")
        high_water = source.execute("SELECT COALESCE(MAX(seq),0) FROM analytics_usage WHERE root=?", (root,)).fetchone()[0]
        source.commit()
        try:
            if (previous is None or previous[0] != state[0] or previous[2] != state[2]
                    or previous[1] > state[1] or previous[1] < state[3]):
                self.db.execute("DELETE FROM meta WHERE key='source'")
                self.db.execute("DELETE FROM meta WHERE key='pendingSource'")
                self.db.commit()
                self.db.execute("DELETE FROM rows")
                self.db.execute("DELETE FROM groups")
                self.db.execute("DELETE FROM receipts")
                self.db.executemany("INSERT OR IGNORE INTO receipts VALUES (?,?,?)", receipts)
                after = -1
                while True:
                    source.execute("BEGIN")
                    page = source.execute("SELECT u.id,u.seq,COALESCE(p.packed," + projection("u") + ") "
                        "FROM analytics_usage u LEFT JOIN analytics_cost_projection_v1 p ON p.id=u.id "
                        "WHERE u.root=? AND u.seq>? AND u.seq<=? ORDER BY u.seq LIMIT ?", (root, after, high_water, PAGE_ROWS)).fetchall()
                    source.commit()
                    if not page:
                        break
                    self._copy_page(page)
                    after = page[-1][1]
                self.db.execute("UPDATE rows SET excluded=EXISTS (SELECT 1 FROM receipts r WHERE "
                                "r.account_key IS rows.log_account AND r.thread_id IS rows.thread AND r.response_id IS rows.response_id)")
                self._rebuild_groups()
                # The private base can already contain concurrent source edits.
                # Its journal watermark remains the start revision until catch-up
                # reconciles every identity that changed during the page copy.
                self._save("pendingSource", state)
                self.db.commit()
                previous = state
            else:
                old_receipts = set(self.db.execute("SELECT * FROM receipts"))
                new_receipts = set(receipts)
                changed_receipts = old_receipts ^ new_receipts
                # apply() needs the old model sources before receipt exclusion changes.
                # Update the receipt table first, leaving row.excluded unchanged until apply().
                self.db.executemany("DELETE FROM receipts WHERE account_key IS ? AND thread_id IS ? AND response_id IS ?", old_receipts - new_receipts)
                self.db.executemany("INSERT OR IGNORE INTO receipts VALUES (?,?,?)", new_receipts - old_receipts)
                if changed_receipts:
                    self.apply([], changed_receipts)
            after = previous[1]
            for attempt in range(8):
                pass_start = after
                source.execute("BEGIN")
                target = source_state(source, root)
                source.commit()
                if (target is None or target[0] != state[0] or target[2] != state[2]
                        or after < target[3] or after > target[1]):
                    self.db.execute("DELETE FROM meta WHERE key='source'")
                    self.db.execute("DELETE FROM meta WHERE key='pendingSource'")
                    self.db.commit()
                    raise RuntimeError("The session usage journal changed during cost capture; retry later")
                while after < target[1]:
                    source.execute("BEGIN")
                    page = source.execute("SELECT c.id,c.revision,CASE WHEN p.root=? THEN p.packed END "
                        "FROM analytics_cost_changes_v1 c LEFT JOIN analytics_cost_projection_v1 p ON p.id=c.id "
                        "WHERE c.root=? AND c.revision>? AND c.revision<=? ORDER BY c.revision,c.id LIMIT ?",
                        (root, root, after, target[1], PAGE_ROWS)).fetchall()
                    source.commit()
                    if not page:
                        break
                    # One revision updates one usage identity, even after a root move.
                    self.apply([(row[0], row[2]) for row in page])
                    after = page[-1][1]
                # Changed identities can move past the selected watermark while
                # paging. The next journal pass includes their later revision.
                after = target[1]
                source.execute("BEGIN")
                current = source_state(source, root)
                if (current is None or current[0] != state[0] or current[2] != state[2]
                        or current[3] > pass_start):
                    source.rollback()
                    self.db.execute("DELETE FROM meta WHERE key='source'")
                    self.db.execute("DELETE FROM meta WHERE key='pendingSource'")
                    self.db.commit()
                    raise RuntimeError("The session usage journal changed during cost capture; retry later")
                self.capture = {"costProjection": current, **(capture() if capture else {})}
                source.commit()
                if current == target:
                    break
            else:
                raise RuntimeError("The session source changed during cost capture; retry later")
            self._save("source", current)
            self.db.execute("DELETE FROM meta WHERE key='pendingSource'")
            self.db.commit()
        except BaseException:
            self.db.rollback()
            source.rollback()
            raise
        fields = ",".join(GROUP_COLUMNS)
        return self.db.execute(f"SELECT {fields},SUM(count) FROM groups GROUP BY {fields} ORDER BY {fields}")
