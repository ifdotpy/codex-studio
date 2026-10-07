"""Bounded, resumable metrics import from each managed thread's native rollout.

The runtime is the only writer. File reads happen outside its transaction lock.
Checkpoints contain identities and counters, never prompts or tool output.
"""

import hashlib
import json
import os
from pathlib import Path
import stat
import threading
import time
import uuid
from contextlib import nullcontext

from analytics.rollout_parser import rollout_actions
from codex_analytics import event_payload_measurements, model_payload_measurements
from codex_budget import budget_capture
from codex_startup_memory import mark as startup_memory_mark


MAX_LINE_BYTES = 64 * 1024 * 1024


def inherited_usage_threads(agent):
    """Accept inherited source IDs only through the saved completed repair chain."""
    receipts = [agent.get('contextRepair') or {}, *(agent.get('contextRepairHistory') or [])]
    thread, allowed, seen = agent.get('threadId'), set(), set()
    while thread and thread not in seen:
        seen.add(thread)
        receipt = next((r for r in receipts if r.get('phase') == 'completed' and r.get('newThreadId') == thread
                        and r.get('agent') == agent['id'] and (r.get('source') or {}).get('id') == agent['id']
                        and (r.get('source') or {}).get('accountKey') == agent.get('accountKey', 'default')), None)
        if not receipt:
            break
        source = receipt['source'].get('threadId')
        if source:
            allowed.add(source)
        snapshot = receipt.get('snapshot') or {}
        copied = snapshot.get('importThreadId')
        if copied:
            allowed.add(copied)
        ancestry = snapshot.get('ancestry')
        if isinstance(ancestry, list) and 1 < len(ancestry) <= 32:
            try:
                valid = (snapshot.get('sourcePath') == ancestry[-1]['path']
                         and snapshot.get('sourceSha256') == ancestry[-1]['sha256']
                         and snapshot.get('sourceBytes') == ancestry[-1]['endByteOffset']
                         and ancestry[-1]['threadId'] == source
                         and bool(snapshot.get('copyPath')) and bool(snapshot.get('terminalTurnId'))
                         and len(snapshot['copySha256']) == 64
                         and all(c in '0123456789abcdef' for c in snapshot['copySha256']))
                ids = set()
                for index, segment in enumerate(ancestry):
                    tid = segment['threadId']
                    valid = valid and str(uuid.UUID(tid)) == tid and tid not in ids
                    ids.add(tid)
                    valid = (valid and isinstance(segment['path'], str) and bool(segment['path'])
                             and type(segment['endByteOffset']) is int and segment['endByteOffset'] > 0
                             and len(segment['sha256']) == 64
                             and all(c in '0123456789abcdef' for c in segment['sha256']))
                    boundary = segment['endOrdinalExclusive']
                    valid = valid and (boundary is None if index == len(ancestry) - 1
                                       else type(boundary) is int and boundary > 0)
                if valid:
                    allowed.update(ids)
            except (KeyError, TypeError, ValueError, AttributeError):
                # Incomplete proof never expands the accepted source identities.
                pass
        thread = source
    return sorted(allowed)


def repair_terminal_errors(db, limit=64):
    """Repair old projections from exact saved native errors, one bounded page."""
    required = {'analytics_meta', 'analytics_turns', 'runtime_items'}
    present = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    if not required <= present:
        return
    key = 'terminalErrorRepairV1'
    row = db.execute('SELECT value FROM analytics_meta WHERE key=?', (key,)).fetchone()
    state = json.loads(row[0]) if row else {'cursor': 0, 'end': db.execute('SELECT COALESCE(MAX(rowid),0) FROM analytics_turns').fetchone()[0],
                                           'repaired': 0, 'ambiguous': 0}
    if state['cursor'] >= state['end']:
        return
    rows = db.execute('SELECT rowid,id,agent,record FROM analytics_turns WHERE rowid>? AND rowid<=? ORDER BY rowid LIMIT ?',
                      (state['cursor'], state['end'], limit)).fetchall()
    for rowid, identity, agent, raw in rows:
        state['cursor'] = rowid
        record = json.loads(raw)
        turn = record.get('turnId')
        if not turn or record.get('status') == 'failed' and record.get('error'):
            continue
        notice = db.execute('SELECT record FROM runtime_items WHERE id=? AND agent=?',
                            (agent + ':native-notice:error:' + turn, agent)).fetchone()
        if not notice:
            continue
        notice = json.loads(notice[0])
        if notice.get('turnId') != turn or not notice.get('nativeError'):
            continue
        if notice.get('threadId'):
            matches = notice['threadId'] == record.get('threadId')
        else:
            # Older notices lack a thread ID. A duplicate turn across forked
            # histories does not supply an exact native source identity.
            matches = db.execute("SELECT COUNT(*) FROM analytics_turns WHERE agent=? AND json_extract(record,'$.turnId')=?",
                                 (agent, turn)).fetchone()[0] == 1
        if not matches:
            state['ambiguous'] += 1
            continue
        record.update(status='failed', error=notice['nativeError'], terminalSource='liveNoticeRepair')
        if isinstance(notice.get('at'), (int, float)):
            record['finishedAt'] = notice['at']
            if record.get('startedAt') is not None:
                record['durationMs'] = max(0, (record['finishedAt'] - record['startedAt']) * 1000)
        db.execute('UPDATE analytics_turns SET record=? WHERE id=?', (json.dumps(record), identity))
        state['repaired'] += 1
    if len(rows) < limit:
        state['cursor'] = state['end']
    db.execute('INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value',
               (key, json.dumps(state)))


def prepare_budget_migration(runtime):
    """Create the bounded history-scan index before the worker takes the writer lock."""
    if getattr(runtime, "_budget_index_ready", False):
        return
    opener = getattr(runtime, "analytics_connection", None) if hasattr(runtime, "analytics_db") else None
    with (opener() if opener else runtime.db()) as db:
        if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_usage'").fetchone():
            db.execute("CREATE INDEX IF NOT EXISTS analytics_usage_migration ON analytics_usage(agent,seq)")
    runtime._budget_index_ready = True


def migrate_budget_usage(db, agent, limit=64, budget_db=None):
    """Import one idempotent page; open a supplied budget context only for rows."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_usage'").fetchone():
        return False
    key = "budgetUsageMigrationV1:" + agent["id"]
    row = db.execute("SELECT value FROM analytics_meta WHERE key=?", (key,)).fetchone()
    if row:
        state = json.loads(row[0])
    else:
        state = {"cursor": 0, "end": db.execute("SELECT COALESCE(MAX(seq),0) FROM analytics_usage").fetchone()[0]}
    if state["cursor"] >= state["end"]:
        return False
    rows = db.execute("SELECT seq,record FROM analytics_usage WHERE agent=? AND seq>? AND seq<=? ORDER BY seq LIMIT ?",
                      (agent["id"], state["cursor"], state["end"], limit)).fetchall()
    if rows:
        from contextlib import AbstractContextManager
        # An entered SQL connection retains the original caller-owned scope.
        budget_scope = (budget_db if isinstance(budget_db, AbstractContextManager) and not hasattr(budget_db, "execute")
                        else nullcontext(budget_db or db))
        with budget_scope as writer:
            for seq, raw in rows:
                record = json.loads(raw)
                payload = {"threadId": record.get("threadId"), "turnId": record.get("turnId"),
                           "responseId": record.get("responseId"), "rawTokenUsageRecord": record.get("rawTokenUsageRecord"),
                           "requestUsage": record.get("requestUsage"),
                           "tokenUsage": {"total": record.get("total"), "last": record.get("last")},
                           "_analyticsTimestampSource": record.get("timestampSource")}
                budget_capture(writer, agent, payload, at=record.get("at", 0), source="rollout")
                state["cursor"] = seq
    if len(rows) < limit:
        state["cursor"] = state["end"]
    db.execute("INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
               (key, json.dumps(state)))
    return bool(rows)


def _history_worker_error(error, prefix):
    detail = ''.join(character if character.isprintable() else ' ' for character in str(error)[:1000])
    return f'{prefix} ({type(error).__name__}): {detail}'


def _history_worker_state(runtime):
    """Supply missing worker state without replacing an active import guard."""
    if getattr(runtime, '_analytics_history_guard', None) is None:
        runtime._analytics_history_guard = threading.Lock()
    if not hasattr(runtime, '_analytics_history_cursor'):
        runtime._analytics_history_cursor = 0
    if not hasattr(runtime, '_analytics_history_paths'):
        runtime._analytics_history_paths = {}


def _history_directory_signature(directory, *, follow_symlinks=False):
    try:
        info = directory.stat(follow_symlinks=follow_symlinks)
        if stat.S_ISDIR(info.st_mode):
            return (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_ctime_ns)
    except OSError:
        pass
    return None


def _history_rollout_index(home, cache):
    """Check directories each discovery round; enumerate only changed entries."""
    directories = dict(cache[2]) if cache is not None and len(cache) > 2 else {}
    paths = dict(cache[1]) if directories else {}
    visited = set()
    pending = [(home / folder, True) for folder in ("sessions", "archived_sessions")]

    def replace_paths(previous, current):
        for suffix, path in previous:
            matches = [value for value in paths.get(suffix, ()) if value != path]
            if matches:
                paths[suffix] = matches
            else:
                paths.pop(suffix, None)
        for suffix, path in current:
            paths[suffix] = [*paths.get(suffix, ()), path]

    while pending:
        directory, root = pending.pop()
        key = str(directory)
        visited.add(key)
        signature = _history_directory_signature(directory, follow_symlinks=root)
        previous = directories.get(key)
        if signature is None:
            if previous is not None:
                replace_paths(previous[1], ())
                directories.pop(key)
            continue
        if previous is not None and previous[0] == signature:
            pending.extend((path, False) for path in previous[2])
            continue
        files, children = [], []
        try:
            with os.scandir(directory) as scanner:
                for entry in scanner:
                    path = None
                    if entry.name.endswith(".jsonl"):
                        path = Path(entry.path)
                        files.append((path.stem[-36:], path))
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            children.append(path if path is not None else Path(entry.path))
                    except OSError:
                        continue
        except OSError:
            # Do not retain a partial scan or suppress its next discovery check.
            files, children, signature = [], [], None
        if signature != _history_directory_signature(directory, follow_symlinks=root):
            signature = None
        replace_paths(previous[1] if previous is not None else (), files)
        directories[key] = (signature, tuple(files), tuple(children))
        pending.extend((path, False) for path in children)
    for key in directories.keys() - visited:
        replace_paths(directories.pop(key)[1], ())
    return paths, directories


def _history_step_connections(runtime):
    """Keep one owner connection while each import phase ends its transaction."""
    from contextlib import contextmanager, ExitStack

    @contextmanager
    def step():
        local = runtime.__dict__.setdefault('_analytics_history_connections', threading.local())
        if getattr(local, 'step', None) is not None:
            raise RuntimeError('History import already owns a database step')

        def scope():
            return (getattr(runtime, 'root', None), getattr(runtime, 'db_path', None),
                    getattr(runtime, 'analytics_db_path', None),
                    getattr(getattr(runtime, 'analytics_db', None), '__func__', None),
                    getattr(getattr(runtime, 'analytics_connection', None), '__func__', None))

        with ExitStack() as stack:
            local.step = {'owner': threading.current_thread(), 'scope': scope,
                          'expected': scope(), 'stack': stack, 'db': None, 'depth': 0}
            try:
                yield
            finally:
                del local.step

    return step()


class AnalyticsHistoryMixin:
    def analytics_history_db(self, fallback=None):
        opener = getattr(self, "analytics_connection", None)
        if opener and hasattr(self, "analytics_db"):
            local = getattr(self, '_analytics_history_connections', None)
            state = getattr(local, 'step', None) if local is not None else None
            if state is None:
                return opener(fallback) if fallback is not None else opener()
            from contextlib import contextmanager
            from codex_sqlite import scope as sqlite_scope

            @contextmanager
            def connection():
                if (getattr(local, 'step', None) is not state
                        or state['owner'] is not threading.current_thread()
                        or state['scope']() != state['expected']):
                    raise RuntimeError('History database scope changed during import')
                if state['depth']:
                    if fallback is state['db']:
                        yield fallback
                    else:
                        with (opener(fallback) if fallback is not None else opener()) as db:
                            yield db
                    return
                if state['db'] is None:
                    state['db'] = state['stack'].enter_context(opener())
                state['depth'] += 1
                try:
                    with sqlite_scope(state['db'], 'Runtime.analytics'):
                        yield state['db']
                finally:
                    state['depth'] -= 1

            return connection()
        if fallback is not None:
            return nullcontext(fallback)
        return self.db()

    def analytics_history_init(self, db):
        with self.analytics_history_db(db) as analytics_db:
            analytics_db.execute("CREATE TABLE IF NOT EXISTS analytics_history (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)")
        _history_worker_state(self)
        self._analytics_history_schema_ready = True

    def analytics_history_ensure_running(self):
        # Fake factories opt in explicitly through analytics_history_start.
        from codex_runtime import AppServer
        if getattr(self, 'factory', None) is not AppServer or self.closed:
            return False
        return self.analytics_history_start()

    def analytics_history_start(self):
        with self.lock:
            if self.closed:
                return False
            _history_worker_state(self)
            worker = getattr(self, 'analytics_history_thread', None)
            if worker is not None and worker.is_alive():
                return False

            def run():
                failures = 0
                reported_healthy = False
                while not self.closed:
                    try:
                        advanced = self.analytics_history_step()
                        startup_memory_mark("analytics-import-first-step")
                        startup_memory_mark("analytics-import-progress", once=False, interval_seconds=30)
                        if failures or not reported_healthy:
                            with self.analytics_history_db() as db:
                                row = db.execute("SELECT record FROM analytics_history WHERE id='importer'").fetchone()
                                if row:
                                    diagnostic = json.loads(row[0])
                                    if diagnostic.get('status') == 'error':
                                        diagnostic.update(status='current', lastError=diagnostic.get('error'),
                                                          error=None, updated=time.time())
                                        db.execute("UPDATE analytics_history SET record=? WHERE id='importer'", (json.dumps(diagnostic),))
                            reported_healthy = True
                        failures = 0
                        self.analytics_history_health = {'status': 'running', 'updated': time.time()}
                    except Exception as error:
                        # A failed error write must not kill the only importer.
                        # Keep a safe in-memory diagnostic until storage recovers.
                        failures += 1
                        self._analytics_history_schema_ready = False
                        detail = _history_worker_error(error, 'History importer failed')
                        self.analytics_history_health = {'status': 'error', 'updated': time.time(),
                            'error': detail, 'consecutiveFailures': failures,
                            'errorPersisted': False}
                        try:
                            with self.analytics_history_db() as db:
                                db.execute("INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)", (
                                    'importer', '', json.dumps({'status': 'error', 'error': detail,
                                                               'updated': time.time()})))
                            self.analytics_history_health['errorPersisted'] = True
                        except Exception as persistence_error:
                            self.analytics_history_health['errorPersistenceError'] = _history_worker_error(
                                persistence_error, 'Cannot store the history error')
                        advanced = False
                    delay = (min(5.0, .25 * 2 ** min(failures - 1, 5)) if failures else
                             5.0 if not advanced and self._analytics_history_cursor == 0 else 0)
                    deadline = time.monotonic() + delay
                    while not self.closed and time.monotonic() < deadline:
                        time.sleep(min(.25, max(0, deadline - time.monotonic())))

            new_worker = threading.Thread(target=run, daemon=True, name='analytics-history')
            self.analytics_history_thread = new_worker
            try:
                new_worker.start()
            except Exception as error:
                if self.analytics_history_thread is new_worker and not new_worker.is_alive():
                    del self.analytics_history_thread
                self.analytics_history_health = {'status': 'error', 'updated': time.time(),
                                                'error': _history_worker_error(error, 'History worker could not start')}
                return False
            return True

    def _analytics_rollout_path(self, home, thread_id):
        now = time.monotonic()
        cache = self._analytics_history_paths.get(str(home))
        if cache is None or now - cache[0] > 30:
            paths, directories = _history_rollout_index(home, cache)
            cache = (time.monotonic(), paths, directories)
            self._analytics_history_paths[str(home)] = cache
        matches = cache[1].get(thread_id, [])
        if len(matches) != 1:
            return None, "ambiguous" if matches else "missing"
        path = matches[0]
        try:
            if not path.resolve().is_relative_to(home.resolve()):
                return None, "outsideProfile"
        except OSError:
            return None, "unreadable"
        return path, None

    def analytics_history_step(self, max_bytes=1048576, max_records=128):
        """Import one fair batch. Return whether complete lines advanced."""
        background = getattr(self, 'analytics_history_thread', None) is threading.current_thread()
        started = (time.monotonic(), time.thread_time()) if background else None
        acquired = self._analytics_history_guard.acquire(blocking=False)
        try:
            if not acquired:
                return False
            with _history_step_connections(self):
                # Idle actors share setup, but every actor retains fresh reads.
                # Progress and round boundaries preserve the worker's idle delay.
                for _ in range(32 if background else 1):
                    if background and self.closed:
                        return False
                    advanced = self._analytics_history_one(max_bytes, max_records)
                    if advanced:
                        return True
                    if self._analytics_history_cursor == 0 or not self._analytics_history_ids:
                        break
                return False
        finally:
            if acquired:
                self._analytics_history_guard.release()
            if started is not None:
                # Each batch repays its CPU time after every import scope closes.
                deadline = started[0] + max(0, time.thread_time() - started[1]) / .15
                while not self.closed:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    time.sleep(min(.1, remaining))

    def _analytics_history_one(self, max_bytes, max_records):
        """Import one actor through the batch owner's separate SQL scopes."""
        if not getattr(self, "_analytics_history_schema_ready", False):
            with self.analytics_history_db() as db:
                self.analytics_history_init(db)
        prepare_budget_migration(self)
        with self.analytics_history_db() as db:
            repair_terminal_errors(db)
            # Runtime.put stores object records with unique JSON keys.
            # Project roster fields once per round; read each actor fresh.
            ids = getattr(self, "_analytics_history_ids", None)
            if not ids or self._analytics_history_cursor % len(ids) == 0:
                from sqlite3 import sqlite_version_info
                if sqlite_version_info < (3, 38, 0):
                    ids = [a["id"] for a in self.records(db, "agents") if a.get("threadId")]
                else:
                    ids = []
                    for raw_id, raw_thread in db.execute(
                            "SELECT record -> '$.id',record -> '$.threadId' "
                            "FROM runtime_agents ORDER BY rowid"):
                        if raw_thread is not None and json.loads(raw_thread):
                            if raw_id is None:
                                raise KeyError("id")
                            ids.append(json.loads(raw_id))
                self._analytics_history_ids = ids
            if not ids:
                return False
            self._analytics_history_cursor %= len(ids)
            agent_id = ids[self._analytics_history_cursor]
            self._analytics_history_cursor = (self._analytics_history_cursor + 1) % len(ids)
            try:
                a = self.agent(agent_id, db)
            except ValueError:
                a = None
        if a is None or not a.get("threadId"):
            return False
        key = a["id"] + ":" + a.get("accountKey", "default") + ":" + a["threadId"]
        with self.analytics_history_db() as db:
            if hasattr(self, "analytics_db"):
                budget_advanced = migrate_budget_usage(db, a, budget_db=self.db())
            else:
                budget_advanced = migrate_budget_usage(db, a)
            row = db.execute("SELECT record FROM analytics_history WHERE id=?", (key,)).fetchone()
        state = json.loads(row[0]) if row else {
            "id": key, "agent": a["id"], "accountKey": a.get("accountKey", "default"),
            "threadId": a["threadId"], "deletedAt": a.get("deletedAt"), "offset": 0, "importedRecords": 0, "malformedLines": 0,
            "context": {"threadId": a["threadId"]},
        }
        state["deletedAt"] = a.get("deletedAt")
        if a.get("provider") == "claude":
            # Claude sends live analytics and has no Codex rollout format.
            # Its saved usage still passes through the same budget migration.
            state.update(status="liveOnly", error=None)
            self._analytics_history_save(key, a, state)
            return budget_advanced
        try:
            try:
                home = Path(self.accounts.home(a.get("accountKey", "default")))
            except ValueError:
                state.update(status="profileUnavailable", error="The managed account profile is unavailable")
                return self._analytics_history_save(key, a, state)
            path, problem = self._analytics_rollout_path(home, a["threadId"])
            if problem:
                state.update(status=problem, error="Native rollout " + problem)
                return self._analytics_history_save(key, a, state)
            info = path.stat()
            identity = [info.st_dev, info.st_ino]
            previous_identity = state.get("filesystemIdentity", state.get("identity"))
            # Device numbers can change after remount. Preserve the original
            # analytics identity; continuity still requires the same inode,
            # recorded path, thread header and checkpoint bytes.
            remapped_device = bool(previous_identity and previous_identity != identity
                and previous_identity[1] == identity[1] and state.get("path") == str(path)
                and state.get("anchor") and state.get("offset", 0) > 0
                and info.st_size >= state["offset"])
            if previous_identity and (previous_identity != identity and not remapped_device
                                      or info.st_size < state["offset"]):
                state.update(status="identityChanged", error="Native rollout was replaced or truncated; previous checkpoint retained")
                return self._analytics_history_save(key, a, state)
            state.update(path=str(path), fileBytes=info.st_size)
            state.setdefault("identity", identity)
            records, consumed, partial, oversized = [], 0, False, False
            with path.open("rb") as handle:
                opened = os.fstat(handle.fileno())
                if [opened.st_dev, opened.st_ino] != identity:
                    state.update(status="identityChanged", error="Native rollout changed during open")
                    return self._analytics_history_save(key, a, state)
                if remapped_device or not state.get("validated"):
                    first = handle.readline(MAX_LINE_BYTES + 1)
                    try:
                        header = json.loads(first)
                        payload = header.get("payload", {})
                        valid = header.get("type") == "session_meta" and (payload.get("id") or payload.get("session_id")) == a["threadId"]
                    except (ValueError, AttributeError):
                        valid = False
                    if not valid:
                        state.update(status="identityChanged" if remapped_device else "wrongThread",
                                     error="Native rollout header does not match the managed thread")
                        return self._analytics_history_save(key, a, state)
                    state["validated"] = True
                if state.get("anchor"):
                    handle.seek(max(0, state["offset"] - 256))
                    anchor = hashlib.sha256(handle.read(min(state["offset"], 256))).hexdigest()
                    if anchor != state["anchor"]:
                        state.update(status="identityChanged", error="Native rollout checkpoint bytes changed")
                        return self._analytics_history_save(key, a, state)
                if remapped_device:
                    state["filesystemRemap"] = {"previous": previous_identity, "current": identity,
                        "offset": state["offset"], "at": time.time(), "proof": "samePathInodeHeaderAnchor"}
                    state["filesystemRemapCount"] = state.get("filesystemRemapCount", 0) + 1
                state["filesystemIdentity"] = identity
                handle.seek(state["offset"])
                while len(records) < max_records and consumed < max_bytes:
                    offset = handle.tell()
                    line = handle.readline(MAX_LINE_BYTES + 1)
                    if not line:
                        break
                    if len(line) > MAX_LINE_BYTES:
                        oversized = True
                        break
                    if not line.endswith(b"\n"):
                        partial = True
                        break
                    consumed += len(line)
                    try:
                        record = json.loads(line)
                        if not isinstance(record, dict):
                            raise ValueError()
                    except (ValueError, UnicodeDecodeError):
                        state["malformedLines"] += 1
                        record = None
                    records.append((offset, record))
                next_offset = state["offset"] + consumed
                handle.seek(max(0, next_offset - 256))
                state["anchor"] = hashlib.sha256(handle.read(min(next_offset, 256))).hexdigest()
            # Parsing and metrics computation can include large results; the
            # collector runs in bounded record groups to release the writer.
            context = state["context"]
            context["allowedSourceThreadIds"] = inherited_usage_threads(a)
            collected = []
            for offset, record in records:
                if record is not None:
                    identity_key = hashlib.sha256((key + ":" + str(state["identity"]) + ":" + str(offset)).encode()).hexdigest()
                    for action in rollout_actions(record, context, identity_key, info.st_mtime):
                        kind, method, p, at = action
                        measurements = (event_payload_measurements(method, p) if kind == 'event'
                                        else model_payload_measurements(p) if kind == 'payload' else None)
                        collected.append((action, dict(context), measurements))
            budget_values = {}
            if hasattr(self, 'analytics_db'):
                # Native callbacks take the runtime writer before analytics.
                # Save exact budget receipts in that order before analytics SQL.
                for index, ((action, method, p, at), event_context, _) in enumerate(collected):
                    if action != 'event' or method != 'thread/tokenUsage/updated':
                        continue
                    event_agent = {**a, 'turnId': event_context.get('turnId'),
                                   'model': event_context.get('model'), 'effort': event_context.get('effort')}
                    with self.lock, self.db() as budget_db:
                        current = self.agent(a['id'], budget_db)
                        if (current.get('threadId') != a['threadId']
                                or current.get('accountKey', 'default') != a.get('accountKey', 'default')):
                            return False
                        budget_values[index] = budget_capture(budget_db, event_agent, p, at=at, source='rollout')
            with self.analytics_history_db() as db:
                current = self.agent(a["id"], db)
                if current.get("threadId") != a["threadId"] or current.get("accountKey", "default") != a.get("accountKey", "default"):
                    return False
                for index, ((action, method, p, at), event_context, measurements) in enumerate(collected):
                    event_agent = {**a, "turnId": event_context.get("turnId"),
                                   "model": event_context.get("model"), "effort": event_context.get("effort")}
                    if action == "event":
                        budget_args = ({'budget_capture_value': budget_values[index]}
                                       if index in budget_values else {})
                        self.analytics_event(db, event_agent, method, p, at=at, source="rollout",
                                             measurements=measurements, **budget_args)
                    elif action == "payload":
                        self.analytics_model_payload(db, event_agent, p, at=at,
                            turn_id=p.get("_analyticsTurnId"), source="rollout",
                            measurements=measurements)
                    elif action == "coverage":
                        state[method] = state.get(method, 0) + 1
                state.update(offset=next_offset, importedRecords=state["importedRecords"] + len(records),
                             status="oversizedLine" if oversized else "partialLine" if partial else "catchingUp" if next_offset < info.st_size else "current")
                state["error"] = "Native rollout line exceeds 64 MiB; checkpoint retained" if oversized else None
                if state["malformedLines"] or state.get("wrongThreadRecord"):
                    state["coverage"] = "partial"
                else:
                    state["coverage"] = "availableRecords"
                self._analytics_history_record(db, key, a["id"], state)
            return bool(consumed) or budget_advanced
        except OSError:
            state.update(status="unreadable", error="Cannot read the managed account's native rollout")
            return self._analytics_history_save(key, a, state)

    def _analytics_history_save(self, key, a, state):
        with self.analytics_history_db() as db:
            self._analytics_history_record(db, key, a["id"], state)
        return False

    def _analytics_history_record(self, db, key, agent, state):
        # A missing/idle rollout is polled repeatedly. Its checkpoint is a
        # fixed-size derived row; do not replace it merely to refresh a clock.
        row = db.execute('SELECT record FROM analytics_history WHERE id=?', (key,)).fetchone()
        previous = json.loads(row[0]) if row else None
        if previous is not None and {k: v for k, v in previous.items() if k != 'updated'} == {
                k: v for k, v in state.items() if k != 'updated'}:
            return False
        state['updated'] = time.time()
        db.execute('INSERT INTO analytics_history VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET '
                   'agent=excluded.agent,record=excluded.record', (key, agent, json.dumps(state)))
        return True
