"""Shared work, result acceptance, search, and conversation controls."""

import hashlib
import json
import os
import re
import sqlite3
import subprocess
import threading
import tempfile
import time
import unicodedata
import uuid
import shutil
from pathlib import Path

from codex_agent_management import management_tools, manage_agent, _worktree_check


def text_field(value, name, maximum=32000, empty=False):
    if (
        not isinstance(value, str)
        or len(value) > maximum
        or (not empty and not value.strip())
    ):
        raise ValueError(
            f"Supply {name} with {'0' if empty else '1'} to {maximum} characters"
        )
    return value.strip()


def work_tools(tool, text):
    return management_tools(tool, text) + [
        tool(
            "orchestration_task",
            "Manage team assignments and review decisions. list returns brief items with nextCursor; get reads one task; history pages earlier evidence. "
            "claim reserves ready work atomically. submit saves evidence, sets review, and notifies the lead. "
            "Only the lead reviews results. accept records approval and releases dependent work. "
            "reject takes the reason and required corrections in result, sets ready, and delivers those instructions to the owner as work_decision. "
            "cancel closes a task without requiring a submit; give a reason. Only the lead or task creator can cancel. "
            "Cancellation releases the assignment and sends work_decision to a live owner. "
            "The owner continues from that event when automatic continuation is enabled. Explicit stops and native failure holds remain in effect. "
            "Mutations return brief receipts; the server owns state changes and event delivery.",
            {
                "action": {
                    "type": "string",
                    "enum": [
                        "list",
                        "get",
                        "history",
                        "create",
                        "claim",
                        "update",
                        "submit",
                        "accept",
                        "reject",
                        "cancel",
                    ],
                },
                "task_id": text,
                "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                "cursor": text,
                "state": text,
                "title": text,
                "description": text,
                "owner": text,
                "dependencies": {"type": "array", "items": text},
                "status": {"type": "string", "enum": ["ready", "blocked"]},
                "result": text,
                "reason": {"type": "string", "minLength": 1, "maxLength": 32000},
                "checks": text,
                "revision": text,
                "files": {"type": "array", "items": text},
                "version": {"type": "integer"},
            },
            ["action"],
        ),
        tool(
            "orchestration_search",
            "Search your own messages and tool results, plus shared team work and complaints. Private rooms remain visible only to their participants. Results include exact record references.",
            {"query": text, "limit": {"type": "integer", "minimum": 1, "maximum": 100}},
            ["query"],
        ),
    ]


class WorkMixin:
    def work_action(self, agent_id, data, key=None, actor=None, epoch=None):
        result = self._work_action(agent_id, data, key, actor, epoch)
        if data.get('action') == 'submit' and result.get('results'):
            self._archive_work_result(data.get('task_id'), result['results'][-1]['id'])
        if data.get('action') != 'accept' or result.get('status') != 'accepted':
            return result
        if 'archive' in result:
            return result
        lock = self.__dict__.setdefault('_accepted_archive_lock', threading.RLock())
        with lock:
            return self._finish_accepted_action(agent_id, result, key, epoch)

    def _finish_accepted_action(self, agent_id, result, key, epoch):
        with self.lock, self.db() as db:
            stored = db.execute('SELECT record FROM runtime_work WHERE id=?', (result['id'],)).fetchone()
            prior = json.loads(stored[0]).get('archive') if stored else None
        try:
            archive = prior or self._archive_accepted_owner(agent_id, result, epoch)
        except Exception as error:
            archive = {'status': 'kept', 'reason': 'The archive check failed: ' + str(error)[:180]}
        with self.lock, self.db() as db:
            row = db.execute('SELECT record FROM runtime_work WHERE id=?', (result['id'],)).fetchone()
            if not row:
                raise ValueError('Accepted work item disappeared during archive check')
            work = json.loads(row[0])
            if work['status'] != 'accepted':
                raise ValueError('Accepted work item changed during archive check')
            if not work.get('archive'):
                work['archive'] = archive
                self.put(db, 'work', work)
            result['archive'] = work['archive']
            if key:
                db.execute('UPDATE runtime_operation_receipts SET result=? WHERE id=?',
                           (json.dumps(result), key))
            if (result['archive']['status'] != 'archived' and work.get('owner')
                    and work['owner'] != work['rootId']):
                owner = self.agent(work['owner'], db)
                if not owner.get('deletedAt'):
                    decision = work['decisions'][-1]
                    self.enqueue(db, owner, 'work_decision', json.dumps({
                        'task': work['id'], 'decision': 'accept', 'reason': decision['reason']}),
                        'work-decision:' + work['id'] + ':' + str(work['version'] - 1))
        return result

    def _archive_work_result(self, task_id, result_id):
        """Write the committed result once, then return its stable absolute path."""
        with self.lock, self.db() as db:
            row = db.execute('SELECT record FROM runtime_work WHERE id=?', (task_id,)).fetchone()
            if not row:
                raise ValueError('Submitted task result is unavailable')
            work = json.loads(row[0])
            result = next((item for item in work.get('results', []) if item['id'] == result_id), None)
            if not result:
                raise ValueError('Committed task result is unavailable')
            path = Path(self.root).absolute() / 'results' / work['id'] / (result['id'] + '.md')
            result['resultFile'] = str(path)
            if not work.get('results')[-1].get('resultFile'):
                work['results'][-1]['resultFile'] = str(path)
                self.put(db, 'work', work)
        # The database transaction above commits before any artifact write.
        committed = result
        body = (f"# {work['title']}\n\n"
                f"Result ID: `{result_id}`\n\n"
                "## Result\n\n" + committed['text'] + "\n\n"
                "## Checks\n\n" + committed['checks'] + "\n\n"
                "## Revision\n\n" + committed['revision'] + "\n\n"
                "## Files\n\n" + ("\n".join(f"- `{item}`" for item in committed['files'])
                                      if committed['files'] else "None") + "\n")
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        descriptor, temporary = tempfile.mkstemp(prefix='.result-', dir=path.parent)
        try:
            with os.fdopen(descriptor, 'w', encoding='utf-8', newline='') as stream:
                stream.write(body)
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(temporary, path)
            except FileExistsError:
                if path.read_text(encoding='utf-8') != body:
                    raise ValueError('The saved task result file does not match its committed record')
            finally:
                os.unlink(temporary)
        except BaseException:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass
            raise
        return str(path)

    def _archive_accepted_owner(self, agent_id, result, epoch):
        owner_id = result.get('owner')
        if not owner_id or owner_id == result['rootId']:
            return {'status': 'skipped', 'reason': 'The task owner is the lead or is unassigned'}
        with self.lock, self.db() as db:
            owner = self.agent(owner_id, db)
            work = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                         (result['id'],)).fetchone()[0])
            open_tasks = [row[0] for row in db.execute(
                "SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=? "
                "AND json_extract(record,'$.status') NOT IN ('accepted','cancelled')",
                (owner_id,))]
        if open_tasks:
            return {'status': 'kept', 'reason': 'The owner has another open task', 'tasks': open_tasks[:20]}
        if owner.get('agentArchive'):
            if owner['agentArchive'].get('reason') == 'Accepted task result is on main':
                cleaned = owner.get('cleanedWorktree')
                return {'status': 'archived', 'worktree': {'state': 'removed',
                        'bytes': cleaned.get('bytes')} if cleaned else {'state': 'none', 'bytes': 0}}
            return {'status': 'kept', 'reason': 'The owner was archived for another reason'}
        if owner.get('deletedAt'):
            return {'status': 'kept', 'reason': 'The owner is already removed'}
        revision = work['results'][-1].get('revision', '')
        if not re.fullmatch(r'[0-9a-fA-F]{7,64}', revision):
            return {'status': 'kept', 'reason': 'The submitted revision is not a commit ID'}
        try:
            cwd = Path(owner['cwd']).resolve()
            repo = subprocess.run(['git', '-C', str(cwd), 'rev-parse', '--show-toplevel'],
                                  check=True, capture_output=True, timeout=30).stdout.decode().strip()
            submitted = subprocess.run(['git', '-C', repo, 'rev-parse', '--verify',
                                        revision + '^{commit}'], check=True, capture_output=True,
                                       timeout=30).stdout.decode().strip()
            main = subprocess.run(['git', '-C', repo, 'rev-parse', '--verify',
                                   'refs/heads/main^{commit}'], check=True, capture_output=True,
                                  timeout=30).stdout.decode().strip()
            reached = subprocess.run(['git', '-C', repo, 'merge-base', '--is-ancestor', submitted, main],
                                     capture_output=True, timeout=30)
        except (KeyError, OSError, subprocess.SubprocessError, UnicodeError):
            return {'status': 'kept', 'reason': 'The result commit or main branch cannot be checked'}
        if reached.returncode != 0:
            return {'status': 'kept', 'reason': 'The result commit is not reachable from main'}
        if owner.get('worktreeReady'):
            _, reason = _worktree_check(self, agent_id, owner_id, epoch)
            if reason:
                return {'status': 'kept', 'reason': reason}
        try:
            archived = manage_agent(self, agent_id, {'action': 'archive', 'agent_id': owner_id,
                                                      'reason': 'Accepted task result is on main'}, epoch)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            return {'status': 'kept', 'reason': str(error)[:180]}
        if archived['status'] != 'archived':
            blockers = archived.get('blockers') or []
            return {'status': 'kept', 'reason': ', '.join(item['kind'] for item in blockers) or archived['status']}
        cleanup = archived.get('worktree') or {}
        if cleanup.get('state') == 'kept':
            return {'status': 'archived', 'worktree': cleanup,
                    'reason': cleanup.get('reason', 'The worktree was kept')}
        return {'status': 'archived', 'worktree': cleanup}

    def setup_work(self, db):
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
            CREATE INDEX IF NOT EXISTS runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
            CREATE TABLE IF NOT EXISTS runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
        """)
        state = db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()
        if not state:
            db.execute("INSERT INTO runtime_search_rollout VALUES (1,'building',0,?)", (time.time(),))
            state = ("building",)
        phase = state[0]
        old_exists = db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search'").fetchone()
        if phase not in {"dropping", "complete"} and not old_exists:
            db.execute("CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize='unicode61')")
        if phase not in {"dropping", "complete"}:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_search_indexed (id TEXT PRIMARY KEY)")
        if phase not in {"dropping", "complete"} and db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search'").fetchone():
            self.setup_search_rows(db)

    def setup_search_rows(self, db):
        # FTS UNINDEXED columns cannot support an equality lookup. Keep the
        # document address in an ordinary indexed table, including legacy rows.
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search_rows'").fetchone():
            return
        db.execute("CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE)")
        db.execute("CREATE TABLE IF NOT EXISTS runtime_search_rows_rollout (id INTEGER PRIMARY KEY CHECK(id=1),cursor INTEGER NOT NULL DEFAULT 0)")
        db.execute("INSERT OR IGNORE INTO runtime_search_rows_rollout(id,cursor) VALUES(1,0)")

    @staticmethod
    def work_records(db, root_id):
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.rootId')=?", (root_id,))]

    @staticmethod
    def latest_work_result_file(db, agent_id):
        rows = db.execute("SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?",
                          (agent_id,)).fetchall()
        results = [item for row in rows for item in json.loads(row[0]).get('results', [])
                   if item.get('agent') == agent_id and item.get('resultFile')]
        return max(results, key=lambda item: item.get('created', 0))['resultFile'] if results else None

    @staticmethod
    def work_by_id(db, task_id, root_id):
        row = db.execute("SELECT record FROM runtime_work WHERE id=?", (task_id,)).fetchone()
        task = json.loads(row[0]) if row else None
        return task if task and task.get('rootId') == root_id else None

    @staticmethod
    def work_dependency_statuses(db, root_id, dependencies):
        if not dependencies:
            return {}
        marks = ",".join("?" for _ in dependencies)
        return {row[0]: row[1] for row in db.execute(
            "SELECT json_extract(record,'$.id'),json_extract(record,'$.status') FROM runtime_work "
            "WHERE id IN (" + marks + ") AND json_extract(record,'$.rootId')=?",
            (*dependencies, root_id))}

    @staticmethod
    def work_dependent_records(db, root_id, dependency_id):
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.rootId')=? "
            "AND json_extract(record,'$.owner') IS NOT NULL AND EXISTS "
            "(SELECT 1 FROM json_each(runtime_work.record,'$.dependencies') WHERE value=?)",
            (root_id, dependency_id))]

    def _search_phase(self, db):
        row = db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()
        return row[0] if row else "legacy"

    def search_is_indexed(self, db, key):
        if self._search_phase(db) in {"active", "dropping", "complete"}:
            return db.execute("SELECT 1 FROM runtime_search_next_meta WHERE id=?", (key,)).fetchone() is not None
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search_rows'").fetchone():
            return db.execute("SELECT 1 FROM runtime_search_rows WHERE id=?", (key,)).fetchone() is not None
        return False

    def _index_search_next(self, db, key, agent, kind, body):
        previous = db.execute("SELECT search_rowid FROM runtime_search_next_meta WHERE id=?", (key,)).fetchone()
        if previous:
            db.execute("DELETE FROM runtime_search_next WHERE rowid=?", (previous[0],))
            rowid = previous[0]
            db.execute("UPDATE runtime_search_next_meta SET agent=?,kind=? WHERE id=?", (agent, kind, key))
        else:
            rowid = db.execute("SELECT coalesce(max(search_rowid),0)+1 FROM runtime_search_next_meta").fetchone()[0]
            db.execute("INSERT INTO runtime_search_next_meta VALUES (?,?,?,?)", (key, agent, kind, rowid))
        db.execute("INSERT INTO runtime_search_next(rowid,body) VALUES (?,?)", (rowid, body))

    def _delete_search_next(self, db, key):
        previous = db.execute("SELECT search_rowid FROM runtime_search_next_meta WHERE id=?", (key,)).fetchone()
        if previous:
            db.execute("DELETE FROM runtime_search_next WHERE rowid=?", (previous[0],))
            db.execute("DELETE FROM runtime_search_next_meta WHERE id=?", (key,))

    def index_item(self, db, key, agent, kind, body=None):
        from codex_search_text import search_text
        body = search_text(db, key)
        phase = self._search_phase(db)
        if phase not in {"active", "dropping", "complete"}:
            row = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
            if row:
                db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
            cursor = db.execute(
                "INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                (key, agent, kind, body),
            )
            db.execute("INSERT INTO runtime_search_rows VALUES (?,?) ON CONFLICT(id) DO UPDATE SET search_rowid=excluded.search_rowid",
                       (key, cursor.lastrowid))
            db.execute("INSERT OR IGNORE INTO runtime_search_indexed VALUES (?)", (key,))
        if phase in {"building", "active", "dropping", "complete"} and db.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search_next'"
        ).fetchone():
            self._index_search_next(db, key, agent, kind, body)

    def delete_search_item(self, db, key):
        phase = self._search_phase(db)
        if phase not in {"active", "dropping", "complete"}:
            row = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
            if row:
                db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
                db.execute("DELETE FROM runtime_search_rows WHERE id=?", (key,))
                db.execute("DELETE FROM runtime_search_indexed WHERE id=?", (key,))
        if phase in {"building", "active", "dropping", "complete"}:
            self._delete_search_next(db, key)

    def search_migration_start(self):
        if getattr(self, "search_migration_thread", None) and self.search_migration_thread.is_alive():
            return False
        phase = None
        with self.db() as db:
            phase = self._search_phase(db)
        if phase == "complete":
            return False
        worker = threading.Thread(target=self._search_migration_run, daemon=True,
                                  name="runtime-search-migration")
        self.search_migration_thread = worker
        worker.start()
        return True

    def _search_migration_run(self):
        while not self.closed:
            delay = 0.001
            try:
                blocked_for_space = False
                with self.lock, self.db() as db:
                    phase = self._search_phase(db)
                    if phase == "active":
                        db.execute("UPDATE runtime_search_rollout SET phase='dropping',updated=? WHERE id=1", (time.time(),))
                        phase = "dropping"
                    elif phase in {"building", "waiting_for_space"}:
                        if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search_next'").fetchone():
                            free = shutil.disk_usage(self.db_path.parent).free
                            # Free pages inside the database are reused before the file grows.
                            free += (db.execute("PRAGMA freelist_count").fetchone()[0]
                                     * db.execute("PRAGMA page_size").fetchone()[0])
                            db_bytes = self.db_path.stat().st_size
                            reserve = max(16 * 1024**3, db_bytes // 2)
                            if free < reserve:
                                db.execute("UPDATE runtime_search_rollout SET phase='waiting_for_space',updated=? WHERE id=1", (time.time(),))
                                blocked_for_space = True
                            else:
                                self._check_search_rows_batch(db)
                                if sqlite3.sqlite_version_info < (3, 43, 0):
                                    raise RuntimeError("SQLite 3.43 or later is required for contentless FTS deletes")
                                db.execute("CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61')")
                                db.execute("UPDATE runtime_search_rollout SET phase='building',updated=? WHERE id=1", (time.time(),))
                        if not blocked_for_space:
                            reusable = (db.execute("PRAGMA freelist_count").fetchone()[0]
                                        * db.execute("PRAGMA page_size").fetchone()[0])
                            if shutil.disk_usage(self.db_path.parent).free + reusable < 8 * 1024**3:
                                db.execute("UPDATE runtime_search_rollout SET phase='waiting_for_space',updated=? WHERE id=1", (time.time(),))
                                blocked_for_space = True
                            else:
                                if phase == "waiting_for_space":
                                    db.execute("UPDATE runtime_search_rollout SET phase='building',updated=? WHERE id=1", (time.time(),))
                                if self._check_search_rows_batch(db):
                                    pass
                                elif not self._search_migration_batch(db):
                                    self._search_migration_verify_and_switch(db)
                    elif phase == "dropping":
                        self._search_cleanup_batch(db)
                    else:
                        return
                self.search_migration_error = None
                if blocked_for_space:
                    delay = 30
                time.sleep(delay)
            except Exception as error:
                self.search_migration_error = str(error)[:500]
                time.sleep(5)

    def _search_migration_batch(self, db, batch_size=5, max_bytes=64 * 1024):
        state = db.execute("SELECT cursor FROM runtime_search_rollout WHERE id=1").fetchone()
        cursor = int(state[0])
        rows = db.execute(
            "SELECT search_rowid,id FROM runtime_search_rows WHERE search_rowid>? ORDER BY search_rowid LIMIT ?",
            (cursor, batch_size),
        ).fetchall()
        if not rows:
            return False
        from codex_search_text import search_texts
        refs = [row["id"] for row in rows]
        bodies = search_texts(db, refs)
        total_bytes = 0
        processed = 0
        for row in rows:
            ref = row["id"]
            body = bodies.get(ref, "")
            body_bytes = len(body.encode("utf-8"))
            if processed and total_bytes + body_bytes > max_bytes:
                break
            total_bytes += body_bytes
            item = db.execute("SELECT agent,record FROM runtime_items WHERE id=?", (ref,)).fetchone()
            if item:
                record = json.loads(item["record"])
                if record.get("truncated"):
                    # Transfer the legacy full body before the old FTS row can be removed.
                    db.execute("INSERT INTO runtime_item_fulltext VALUES (?,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body", (ref, body))
                    from codex_search_text import search_text
                    if search_text(db, ref) != body:
                        raise RuntimeError("Full-text transfer verification failed for " + ref)
            legacy = db.execute("SELECT agent,kind FROM runtime_search WHERE rowid=?", (row["search_rowid"],)).fetchone()
            kind, agent = legacy["kind"], legacy["agent"]
            self._index_search_next(db, ref, agent, kind, body)
            processed += 1
        if not processed:
            return False
        db.execute("UPDATE runtime_search_rollout SET cursor=?,updated=? WHERE id=1",
                   (rows[processed - 1]["search_rowid"], time.time()))
        self.search_migration_last_batch_bytes = total_bytes
        return True

    def _check_search_rows_batch(self, db, batch_size=500):
        # The check table is dropped when the check completes.
        if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' "
                          "AND name='runtime_search_rows_rollout'").fetchone():
            return False
        state = db.execute("SELECT cursor FROM runtime_search_rows_rollout WHERE id=1").fetchone()
        if not state:
            return False
        cursor = int(state[0])
        rows = db.execute("SELECT rowid,id FROM runtime_search WHERE rowid>? ORDER BY rowid LIMIT ?",
                          (cursor, batch_size)).fetchall()
        if not rows:
            db.execute("DROP TABLE runtime_search_rows_rollout")
            return False
        db.executemany("INSERT OR IGNORE INTO runtime_search_rows VALUES (?,?)",
                       ((row["id"], row["rowid"]) for row in rows))
        db.execute("UPDATE runtime_search_rows_rollout SET cursor=? WHERE id=1", (rows[-1]["rowid"],))
        return True

    def _search_migration_verify_and_switch(self, db):
        missing = db.execute("SELECT r.id FROM runtime_search_rows r LEFT JOIN runtime_search_next_meta n ON n.id=r.id WHERE n.id IS NULL LIMIT 1").fetchone()
        mismatch = db.execute("SELECT n.id FROM runtime_search_next_meta n LEFT JOIN runtime_search_rows r ON r.id=n.id WHERE r.id IS NULL LIMIT 1").fetchone()
        if missing or mismatch:
            db.execute("UPDATE runtime_search_rollout SET phase='building',cursor=0,updated=? WHERE id=1", (time.time(),))
            return
        truncated_count = db.execute(
            "SELECT count(*) FROM runtime_items WHERE json_extract(record,'$.truncated')=1"
        ).fetchone()[0]
        copied_count = db.execute("""SELECT count(*) FROM runtime_item_fulltext f
            JOIN runtime_items i ON i.id=f.id
            WHERE json_extract(i.record,'$.truncated')=1""").fetchone()[0]
        if truncated_count != copied_count:
            db.execute("UPDATE runtime_search_rollout SET phase='building',cursor=0,updated=? WHERE id=1", (time.time(),))
            return
        sample = db.execute("""SELECT i.id,r.search_rowid FROM runtime_items i
            JOIN runtime_search_rows r ON r.id=i.id
            WHERE json_extract(i.record,'$.truncated')=1 ORDER BY i.id LIMIT 20""").fetchall()
        for item in sample:
            old_body = db.execute(
                "SELECT body FROM runtime_search WHERE rowid=?", (item["search_rowid"],)
            ).fetchone()
            new_body = db.execute(
                "SELECT body FROM runtime_item_fulltext WHERE id=?", (item["id"],)
            ).fetchone()
            if (old_body is None or new_body is None or
                    hashlib.sha256((old_body[0] or "").encode("utf-8")).digest() !=
                    hashlib.sha256((new_body[0] or "").encode("utf-8")).digest()):
                raise RuntimeError("Full-text transfer hash verification failed for " + item["id"])
        db.execute("UPDATE runtime_search_rollout SET phase='dropping',updated=? WHERE id=1", (time.time(),))

    def _search_cleanup_batch(self, db, batch_size=5):
        rows = db.execute("SELECT search_rowid,id FROM runtime_search_rows ORDER BY search_rowid LIMIT ?", (batch_size,)).fetchall()
        if rows:
            for row in rows:
                db.execute("DELETE FROM runtime_search WHERE rowid=?", (row["search_rowid"],))
                db.execute("DELETE FROM runtime_search_rows WHERE id=?", (row["id"],))
                db.execute("DELETE FROM runtime_search_indexed WHERE id=?", (row["id"],))
            return
        db.execute("DROP TABLE IF EXISTS runtime_search")
        db.execute("DROP TABLE IF EXISTS runtime_search_rows")
        db.execute("DROP TABLE IF EXISTS runtime_search_indexed")
        db.execute("UPDATE runtime_search_rollout SET phase='complete',updated=? WHERE id=1", (time.time(),))
    def checked_actor(self, db, agent_id, actor=None):
        a = self.agent(agent_id, db)
        if a.get("deletedAt"):
            raise ValueError("This agent was deleted")
        if actor:
            caller = self.agent(actor, db)
            if caller.get("deletedAt") or not caller["autoWake"]:
                raise ValueError("The caller is stopped")
            if caller["rootId"] != a["rootId"]:
                raise ValueError("This record belongs to another team")
        return a

    def operation_receipt(self, db, key, body):
        signature = hashlib.sha256(
            json.dumps(body, sort_keys=True).encode()
        ).hexdigest()
        row = (
            db.execute(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                (key,),
            ).fetchone()
            if key
            else None
        )
        if row and row["signature"] != signature:
            raise ValueError("This request id has different content")
        return signature, json.loads(row["result"]) if row else None

    def save_receipt(self, db, key, signature, result):
        if key:
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES (?,?,?)",
                (key, signature, json.dumps(result)),
            )
        return result

    def _work_action(self, agent_id, data, key=None, actor=None, epoch=None):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id, actor)
            if epoch is not None and self.agent(actor, db)["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            signature, previous = self.operation_receipt(
                db, key, {"agent": agent_id, "actor": actor, "body": data}
            )
            if previous is not None:
                return previous
            action = data.get("action", "list")
            if action == "list":
                works = self.work_records(db, a["rootId"])
                statuses = {work["id"]: work["status"] for work in works}
                return {
                    "items": [self.work_view(w, statuses) for w in works],
                    "tasks": [self.work_view(w, statuses) for w in works],
                }
            if action == "create":
                if actor and a["id"] != a["rootId"]:
                    raise ValueError("Only the lead can create work")
                w = {
                    "id": str(uuid.uuid4()),
                    "rootId": a["rootId"],
                    "title": text_field(data.get("title"), "a title", 160),
                    "description": text_field(
                        data.get("description", ""), "a description", empty=True
                    ),
                    "owner": None,
                    "dependencies": [],
                    "status": "ready",
                    "created": time.time(),
                    "updated": time.time(),
                    "version": 0,
                    "results": [],
                    "decisions": [],
                    "createdBy": actor or a["id"],
                }
            else:
                w = self.work_by_id(db, data.get("task_id"), a["rootId"])
                if not w:
                    raise ValueError("Unknown work item")
                if data.get("version") is not None and data["version"] != w["version"]:
                    raise ValueError("This work item changed. Reload before editing")
            leader = not actor or self.agent(actor, db)["id"] == a["rootId"]
            if action in {"create", "update"}:
                if not leader:
                    raise ValueError("Only the lead can change work assignments")
                if w["status"] == "accepted":
                    raise ValueError("Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence")
                if w["status"] == "review":
                    raise ValueError("Work under review cannot be reassigned; reject its result before editing")
                if "title" in data:
                    w["title"] = text_field(data["title"], "a title", 160)
                if "description" in data:
                    w["description"] = text_field(
                        data["description"], "a description", empty=True
                    )
                if "owner" in data:
                    owner = data["owner"] or None
                    if owner and self.checked_actor(db, owner)["rootId"] != a["rootId"]:
                        raise ValueError("Owner belongs to another team")
                    w["owner"] = owner
                if "status" in data:
                    if data["status"] not in {"ready", "blocked"}:
                        raise ValueError("Choose ready or blocked")
                    w["status"] = data["status"]
                if w.get("owner") and w["owner"] != a["rootId"]:
                    from codex_agent_modes import assert_delegation
                    assert_delegation(self.agent(a["rootId"], db))
                if "dependencies" in data:
                    deps = data["dependencies"]
                    if (
                        not isinstance(deps, list)
                        or len(deps) > 100
                        or any(not isinstance(d, str) for d in deps)
                    ):
                        raise ValueError("Supply up to 100 dependency ids")
                    w["dependencies"] = list(dict.fromkeys(deps))
                    works = self.work_records(db, a["rootId"])
                    graph = {t["id"]: t["dependencies"] for t in works}
                    graph[w["id"]] = w["dependencies"]
                    visited = set()

                    def visit(node, path):
                        if node in path:
                            raise ValueError("Task dependencies contain a cycle")
                        if node in visited:
                            return
                        if node not in graph:
                            raise ValueError("Dependency is outside this team")
                        for child in graph[node]:
                            visit(child, path | {node})
                        visited.add(node)

                    visit(w["id"], set())
            elif action == "claim":
                claimant = actor or data.get("owner") or a["id"]

                if claimant != a["rootId"] and w.get("owner") != claimant:
                    from codex_agent_modes import assert_delegation
                    assert_delegation(self.agent(a["rootId"], db))
                if self.checked_actor(db, claimant)["rootId"] != a["rootId"]:
                    raise ValueError("Claimant belongs to another team")
                if (
                    w["status"] not in {"ready", "running"}
                    or self.work_view(w, self.work_dependency_statuses(
                        db, a["rootId"], w["dependencies"]))["blockedBy"]
                ):
                    raise ValueError("This work item is not ready")
                if w["owner"] and w["owner"] != claimant:
                    raise ValueError("Another agent owns this work item")
                w.update(owner=claimant, status="running")
            elif action == "submit":
                if w["status"] not in {"running", "ready", "blocked", "review"}:
                    raise ValueError("This result is already accepted")
                if actor and w["owner"] != actor:
                    raise ValueError("Only the assigned worker can submit its result")
                if self.work_view(w, self.work_dependency_statuses(
                        db, a["rootId"], w["dependencies"]))["blockedBy"]:
                    raise ValueError("Dependencies have not been accepted")
                files = data.get("files", [])
                if (
                    not isinstance(files, list)
                    or len(files) > 50
                    or any(not isinstance(f, str) or len(f) > 4096 for f in files)
                ):
                    raise ValueError("Supply up to 50 file paths")
                submitter = w.get("owner") or a["id"]
                for path in files:
                    self.workspace_path(submitter, path)
                result = {
                    "id": str(uuid.uuid4()),
                    "agent": submitter,
                    "text": text_field(data.get("result"), "a result"),
                    "checks": text_field(data.get("checks"), "test evidence"),
                    "revision": text_field(
                        data.get("revision"), "a source revision", 200
                    ),
                    "files": files,
                    "created": time.time(),
                }
                result['resultFile'] = str(Path(self.root).absolute() / 'results' / w['id'] / (result['id'] + '.md'))
                w["results"].append(result)
                w["status"] = "review"
                if actor != a["rootId"]:
                    self.enqueue(
                        db,
                        self.agent(a["rootId"], db),
                        "work_review",
                        json.dumps(
                            {"task": w["id"], "title": w["title"], "result": result},
                            ensure_ascii=False,
                        ),
                        "work-result:" + result["id"],
                    )
            elif action == "cancel":
                if not leader and actor != w.get("createdBy"):
                    raise ValueError("Only the lead or task creator can cancel this task")
                reason = text_field(data.get("reason"), "a cancellation reason")
                if w["status"] == "cancelled":
                    return self.save_receipt(db, key, signature, self.work_view(
                        w, self.work_dependency_statuses(db, a["rootId"], w["dependencies"])))
                if w["status"] == "accepted":
                    raise ValueError("Cannot cancel accepted work")
                owner_id = w.get("owner")
                w["decisions"].append({
                    "decision": "cancel", "reason": reason,
                    "by": actor or a["id"], "owner": owner_id,
                    "created": time.time(),
                })
                w.update(status="cancelled", owner=None)
                owner_row = (db.execute("SELECT record FROM runtime_agents WHERE id=?", (owner_id,)).fetchone()
                             if owner_id else None)
                owner = json.loads(owner_row[0]) if owner_row else None
                if (owner and not owner.get("deletedAt")
                        and owner.get("status") not in {"completed", "failed"}):
                    self.enqueue(db, owner, "work_decision", json.dumps(
                        {"task": w["id"], "decision": "cancel", "reason": reason}),
                        "work-decision:" + w["id"] + ":cancel")
            elif action in {"accept", "reject"}:
                if not leader:
                    raise ValueError("Only the lead can accept or reject a result")
                if w["status"] == "accepted":
                    raise ValueError("This work is already accepted. Create a follow-up task with this task ID and the new evidence")
                if w["status"] != "review" or not w["results"]:
                    raise ValueError("Submit a result before review")
                if action == "reject" and w.get("owner") and w["owner"] != a["rootId"]:
                    from codex_agent_modes import assert_delegation
                    assert_delegation(self.agent(a["rootId"], db))
                reason = text_field(data.get("result"), "a review decision")
                w["decisions"].append(
                    {
                        "resultId": w["results"][-1]["id"],
                        "decision": action,
                        "reason": reason,
                        "by": actor or "user",
                        "created": time.time(),
                    }
                )
                w["status"] = "accepted" if action == "accept" else "ready"
                db.execute("UPDATE runtime_events SET status='stored_only',error=? "
                           "WHERE id=? AND kind='work_review' AND status='pending' AND agent=?",
                           ('The exact result already has a decision',
                            "work-result:" + w["results"][-1]["id"], a["rootId"]))
                if (w.get('owner') and w['owner'] != actor
                        and (action == 'reject' or w['owner'] == a['rootId'])):
                    self.enqueue(
                        db,
                        self.agent(w["owner"], db),
                        "work_decision",
                        json.dumps(
                            {"task": w["id"], "decision": action, "reason": reason}
                        ),
                        "work-decision:" + w["id"] + ":" + str(w["version"]),
                )
                if action == "accept":
                    for other in self.work_dependent_records(db, a["rootId"], w["id"]):
                        statuses = self.work_dependency_statuses(
                            db, a["rootId"], other["dependencies"])
                        statuses[w["id"]] = "accepted"
                        if not self.work_view(other, statuses)["blockedBy"]:
                            self.enqueue(
                                db,
                                self.agent(other["owner"], db),
                                "work_ready",
                                json.dumps({"task": other["id"], "title": other["title"]}),
                                "work-ready:" + other["id"] + ":" + w["id"],
                            )
            else:
                raise ValueError("Unknown work action")
            w.update(version=w["version"] + 1, updated=time.time())
            self.put(db, "work", w)
            return self.save_receipt(
                db,
                key,
                signature,
                self.work_view(w, self.work_dependency_statuses(
                    db, a["rootId"], w["dependencies"])),
            )

    def release_failed_work(self, db, agents, force=False):
        """Return unfinished work of failed or deleted workers to the board.

        A failed worker cannot submit, so its claim would block the task forever.
        Work under review keeps its owner: the lead still reviews the evidence.
        dispatch calls this under Runtime.lock, so it runs at most every 30
        seconds and reads only the matching rows, never the whole board.
        """
        now = time.monotonic()
        if not force and now < getattr(self, "_release_work_after", 0):
            return []
        self._release_work_after = now + 30
        gone = {
            a["id"]: ("The worker was deleted" if a.get("deletedAt")
                      else a.get("error") or "The worker failed")
            for a in agents
            if a.get("parentId") and (a.get("deletedAt") or a.get("status") == "failed")
        }
        if not gone:
            return []
        marks = ",".join("?" * len(gone))
        rows = db.execute(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner') IN (" + marks + ")"
            " AND json_extract(record,'$.status') IN ('ready','running','blocked')", list(gone)).fetchall()
        released = []
        for (raw,) in rows:
            w = json.loads(raw)
            owner = w["owner"]
            reason = str(gone[owner])[:500]
            w.setdefault("releases", []).append(
                {"agent": owner, "reason": reason, "created": time.time()})
            w.update(owner=None, status="blocked" if w["status"] == "blocked" else "ready",
                     version=w["version"] + 1, updated=time.time())
            self.put(db, "work", w)
            lead = self.agent(w["rootId"], db)
            if not lead.get("deletedAt"):
                self.enqueue(db, lead, "work_released",
                             json.dumps({"task": w["id"], "title": w["title"], "agent": owner,
                                         "reason": reason}, ensure_ascii=False),
                             "work-released:" + w["id"] + ":" + str(w["version"]))
            released.append(w["id"])
        return released

    @staticmethod
    def work_view(w, works):
        statuses = works if isinstance(works, dict) else {t["id"]: t["status"] for t in works}
        blocked = [d for d in w["dependencies"] if statuses.get(d) != "accepted"]
        return {
            **w,
            "blockedBy": blocked,
            "displayStatus": (
                "blocked"
                if blocked and w["status"] in {"ready", "running"}
                else w["status"]
            ),
        }

    def search_work(self, query, agent_id=None, limit=50):
        query = text_field(query, "a search query", 500)
        limit = max(1, min(100, int(limit)))
        # Quote each word; user input never becomes FTS operators or SQL.
        match = " AND ".join(
            '"' + word.replace('"', '""') + '"' for word in query.split()
        )
        with self.read_db() as db:
            caller = self.checked_actor(db, agent_id) if agent_id else None
            allowed = {
                a["id"]
                for a in self.records(db, "agents")
                if not a.get("deletedAt")
                and (not caller or a["rootId"] == caller["rootId"])
            }
            found = []
            next_index = self._search_phase(db) in {"active", "dropping", "complete"}
            if next_index:
                query_rows = db.execute(
                    "SELECT m.id,m.agent,m.kind FROM runtime_search_next "
                    "JOIN runtime_search_next_meta m ON m.search_rowid=runtime_search_next.rowid "
                    "JOIN runtime_items i ON i.id=m.id "
                    "WHERE runtime_search_next MATCH ? AND json_extract(i.record,'$.afterRestore') IS NULL "
                    "ORDER BY rank LIMIT 1000", (match,)
                )
            else:
                query_rows = db.execute(
                    "SELECT runtime_search.id,runtime_search.agent,runtime_search.kind "
                    "FROM runtime_search JOIN runtime_items i ON i.id=runtime_search.id "
                    "WHERE runtime_search MATCH ? AND json_extract(i.record,'$.afterRestore') IS NULL "
                    "ORDER BY rank LIMIT 1000", (match,)
                )
            for row in query_rows:
                if row["agent"] in allowed and (
                    not caller or row["agent"] == caller["id"]
                ):
                    found.append(
                        {**dict(row), "type": "message", "reference": row["id"]}
                    )
                if len(found) >= limit:
                    break
            if found:
                from codex_search_text import search_texts
                bodies = search_texts(db, (row["id"] for row in found))
                for row in found:
                    row["excerpt"] = self._search_excerpt(bodies.get(row["id"], ""), query)
            needle = query.casefold()
            for table, kind, field in [
                ("work", "work", "title"),
                ("complaints", "complaint", "title"),
                ("plans", "plan", "text"),
            ]:
                for row in self.records(db, table):
                    root = row.get("rootId") or row.get("leadId") or row.get("id")
                    if caller and root != caller["rootId"]:
                        continue
                    if root not in allowed:
                        continue
                    body = json.dumps(row, ensure_ascii=False)
                    if needle in body.casefold():
                        found.append(
                            {
                                "id": row["id"],
                                "agent": root,
                                "type": kind,
                                "kind": kind,
                                "excerpt": str(row.get(field, ""))[:500],
                                "reference": row["id"],
                            }
                        )
            readable = {
                r["id"]
                for r in self.chat_rooms(db, caller["id"] if caller else None)
            }
            for row in db.execute(
                "SELECT id,room,sender,text FROM runtime_chat_messages WHERE instr(lower(text),lower(?))>0 ORDER BY seq DESC LIMIT 1000",
                (query,),
            ):
                if row["room"] in readable:
                    found.append(
                        {
                            "id": row["id"],
                            "agent": row["sender"],
                            "room": row["room"],
                            "type": "room",
                            "kind": "Agent chat",
                            "excerpt": row["text"][:500],
                            "reference": row["id"],
                        }
                    )
            return {
                "results": [{**r, "text": r.get("excerpt", "")} for r in found[:limit]],
                "query": query,
                "limit": limit,
            }

    @staticmethod
    def _search_excerpt(body, query, token_limit=30):
        """Make a plain-text result window for the contentless index."""
        tokens = list(re.finditer(r"[^\W_]+", body, flags=re.UNICODE))
        if len(tokens) <= token_limit:
            return body
        normalize = lambda value: "".join(
            char for char in unicodedata.normalize("NFD", value.casefold())
            if unicodedata.category(char) != "Mn"
        )
        terms = {normalize(word) for word in re.findall(r"[^\W_]+", query, flags=re.UNICODE)}
        hits = [index for index, token in enumerate(tokens) if normalize(token.group()) in terms]
        center = hits[0] if hits else 0
        start = max(0, min(center - (token_limit // 2 - 1), len(tokens) - token_limit))
        end = min(len(tokens), start + token_limit)
        excerpt = body[tokens[start].start():tokens[end - 1].end()]
        if start:
            excerpt = " … " + excerpt
        if end < len(tokens):
            excerpt += " … "
        return excerpt

    def chat_organization(self, key, data):
        from codex_workspace import active_monitors
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            allowed = {'id', 'read_state', 'project_folder', 'project_path', 'expected_revision', 'expected_folder', 'pinned', 'archived', 'project'}
            if set(data) - allowed:
                raise ValueError('Unknown chat organization field')
            if 'read_state' in data:
                from codex_chat_read_state import read_state
                return read_state(self, db, a, data)
            if 'project_folder' in data:
                from codex_project_folders import folder_for
                if not a.get('isLead') or data.get('project_path') != a.get('cwd'):
                    raise ValueError('Select a chat in this project')
                desired = folder_for(self, db, a['cwd'], data['project_folder'])
                revision = data.get('expected_revision')
                current = a.get('projectFolderRevision', 0)
                if type(revision) is not int or revision < 0:
                    raise ValueError('Supply the current chat folder revision')
                replay = a.get('projectFolder') == desired and revision in (current, current - 1)
                if not replay and (revision != current or data.get('expected_folder') != a.get('projectFolder')):
                    raise ValueError('The chat folder changed. Reload it before moving')
                if a.get('projectFolder') != desired:
                    a['projectFolderRevision'] = current + 1
                a['projectFolder'] = desired
            for field in ("pinned", "archived"):
                if field in data:
                    if not isinstance(data[field], bool):
                        raise ValueError("Supply a boolean for " + field)
                    if (
                        field == "archived"
                        and data[field]
                        and (
                            a.get("inFlight")
                            or any(
                                m["agent"] == key
                                and m["status"] in {"running", "approval", "starting"}
                                for m in active_monitors(db)
                            )
                        )
                    ):
                        raise ValueError(
                            "Stop active work before archiving this conversation"
                        )
                    a[field] = data[field]
            if "project" in data:
                a["project"] = text_field(
                    data["project"], "a project name", 100, empty=True
                )
            self.put(db, "agents", a)
            return a

    def plan_action(self, key, data=None):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            row = db.execute(
                "SELECT record FROM runtime_plans WHERE id=?", (key,)
            ).fetchone()
            plan = (
                json.loads(row[0])
                if row
                else {
                    "id": key,
                    "rootId": a["rootId"],
                    "text": "",
                    "version": 0,
                    "updated": None,
                    "steps": [],
                }
            )
            if data is None:
                return plan
            if data.get("version") != plan["version"]:
                raise ValueError("The plan changed. Reload before saving")
            plan.update(
                text=text_field(data.get("text"), "a plan", 64000, empty=True),
                version=plan["version"] + 1,
                updated=time.time(),
            )
            self.put(db, "plans", plan)
            self.enqueue(
                db,
                a,
                "plan_update",
                "The user updated the shared plan:\n" + plan["text"],
                "plan:" + key + ":" + str(plan["version"]),
            )
            return plan

    def queue_action(self, agent_id, data=None):
        import math

        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)

            def snapshot():
                rows = []
                for stored in db.execute(
                    "SELECT e.id,e.text,e.kind,e.status,e.created,m.record AS metadata "
                    "FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id "
                    "WHERE e.agent=? AND e.epoch=? AND e.status='pending' "
                    "AND e.kind IN ('user','followup') ORDER BY e.created,e.rowid",
                    (agent_id, a["epoch"]),
                ):
                    row = dict(stored)
                    metadata = json.loads(row.pop("metadata") or "{}")
                    row.update(
                        assets=[self.asset_view(self.asset_record(v)) for v in metadata.get("assets", [])],
                        delivery=metadata.get("delivery", "queue"),
                        requestedDelivery=metadata.get("requestedDelivery", metadata.get("delivery", "queue")),
                    )
                    if "acceptedAt" in metadata:
                        row["acceptedAt"] = metadata["acceptedAt"]
                    rows.append(row)
                revision = hashlib.sha256(json.dumps(
                    [a["epoch"], a.get("queueMutationRevision", 0), rows], sort_keys=True,
                ).encode()).hexdigest()
                return {"items": rows, "revision": revision,
                        "capabilities": {"reorder": True, "receipts": True}}

            if data is None:
                return snapshot()
            action = data.get("action")
            request_id = data.get("request_id")
            if request_id is not None:
                request_id = text_field(request_id, "a request ID", 200)
            if action == "reorder" and not request_id:
                raise ValueError("Supply a request ID to change queue delivery")
            receipt_key = "queue:" + agent_id + ":" + request_id if request_id else None
            signature, previous = self.operation_receipt(
                db, receipt_key, {"agent": agent_id, "body": data},
            )
            if previous is not None:
                return previous
            current = snapshot()
            if (("expected_revision" in data or action == "reorder")
                    and data.get("expected_revision") != current["revision"]):
                raise ValueError("This queue changed. Reload before editing or reordering")
            rows = current["items"]
            if action == "reorder":
                ordered = data.get("ordered_ids")
                if (not isinstance(ordered, list) or any(not isinstance(v, str) for v in ordered)
                        or len(ordered) != len(rows) or len(set(ordered)) != len(ordered)
                        or set(ordered) != {r["id"] for r in rows}):
                    raise ValueError("Supply every current queued message ID exactly once")
                # Keep system events in their slots. Unique queue timestamps also
                # make dispatch deterministic when old messages have equal clocks.
                pending = db.execute(
                    "SELECT id,kind,created FROM runtime_events WHERE agent=? AND epoch=? "
                    "AND status='pending' ORDER BY created,rowid", (agent_id, a["epoch"]),
                ).fetchall()
                slots = []
                run = []
                lower = -math.inf
                for event in [*pending, {"kind": None, "created": math.inf}]:
                    if event["kind"] in {"user", "followup"}:
                        run.append(event["created"])
                        continue
                    upper = event["created"]
                    values = []
                    last = lower
                    for original in run:
                        last = max(original, math.nextafter(last, math.inf))
                        values.append(last)
                    if values and values[-1] >= upper:
                        values = []
                        last = upper
                        for original in reversed(run):
                            last = min(original, math.nextafter(last, -math.inf))
                            values.append(last)
                        values.reverse()
                    if values and (values[0] <= lower or values[-1] >= upper
                                   or any(not math.isfinite(v) for v in values)):
                        raise ValueError("Queue timestamps overlap. The queue cannot be reordered safely")
                    slots.extend(values)
                    run = []
                    lower = upper
                for message_id, created in zip(ordered, slots):
                    db.execute("UPDATE runtime_events SET created=? WHERE id=?", (created, message_id))
            else:
                row = next((r for r in rows
                            if r["id"] == (data.get("message_id") or data.get("id"))), None)
                if not row:
                    raise ValueError("This message already left the queue")
                if data.get("expectedText") is not None and data["expectedText"] != row["text"]:
                    raise ValueError("This queued message changed")
                if action == "cancel":
                    db.execute("UPDATE runtime_events SET status='cancelled' WHERE id=?", (row["id"],))
                elif action == "edit":
                    text = text_field(data.get("text"), "a message", empty=bool(row["assets"]))
                    db.execute("UPDATE runtime_events SET text=? WHERE id=?", (text, row["id"]))
                    if row["text"] != text:
                        saved = db.execute(
                            "SELECT record FROM runtime_event_meta WHERE id=?", (row["id"],),
                        ).fetchone()
                        metadata = json.loads(saved[0]) if saved else {}
                        metadata["acceptedAt"] = time.time()
                        db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)",
                                   (row["id"], json.dumps(metadata)))
                elif action == "first":
                    first = min(r["created"] for r in rows)
                    db.execute("UPDATE runtime_events SET created=? WHERE id=?", (first - 0.001, row["id"]))
                else:
                    raise ValueError("Choose edit, cancel, first, or reorder")
            a["queueMutationRevision"] = a.get("queueMutationRevision", 0) + 1
            self.put(db, "agents", a)
            updated = snapshot()
            result = self.save_receipt(db, receipt_key, signature, {
                "status": "updated", "revision": updated["revision"],
                "capabilities": updated["capabilities"],
            })
            self.changed.set()
            return result

    def annotate(self, agent_id, data):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            signature, previous = self.operation_receipt(
                db, data.get("id"), {"agent": agent_id, **data}
            )
            if previous is not None:
                return previous
            path = text_field(data.get("path"), "a file path", 4096)
            self.workspace_path(agent_id, path)
            line = data.get("line")
            if not isinstance(line, int) or line < 1:
                raise ValueError("Supply a positive line number")
            note = {
                "id": data.get("id") or str(uuid.uuid4()),
                "agent": agent_id,
                "rootId": a["rootId"],
                "path": path,
                "line": line,
                "text": text_field(data.get("text"), "a comment"),
                "created": time.time(),
            }
            turn_id = data.get("turnId")
            if turn_id is not None:
                note["turnId"] = text_field(turn_id, "a turn ID", 200)
            self.put(db, "annotations", note)
            self.enqueue(
                db,
                a,
                "user",
                f"Review comment at {path}:{line}"
                + (f" (reported turn {note['turnId']})" if turn_id is not None else "")
                + f"\n{note['text']}",
                "annotation:" + note["id"],
            )
            return self.save_receipt(db, data.get("id"), signature, note)

    def search_item(self, key):
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT agent,record FROM runtime_items WHERE id=?", (key,)
            ).fetchone()
            if row:
                a = self.checked_actor(db, row["agent"])
                item = json.loads(row["record"])
                if item.get("afterRestore"):
                    raise ValueError("This item belongs to history before restore")
                from codex_search_text import search_text
                full = search_text(db, key)
                return {
                    **item,
                    "text": full or item["text"],
                    "agent": a["id"],
                    "kind": "message",
                }
            row = db.execute(
                "SELECT * FROM runtime_chat_messages WHERE id=?", (key,)
            ).fetchone()
            if row:
                room = next(
                    (r for r in self.chat_rooms(db) if r["id"] == row["room"]), None
                )
                if not room:
                    raise ValueError("This chat is unavailable")
                return {**dict(row), "kind": "room", "agent": row["sender"]}
            for table in ("work", "plans", "complaints"):
                row = db.execute(
                    f"SELECT record FROM runtime_{table} WHERE id=?", (key,)
                ).fetchone()
                if row:
                    item = json.loads(row[0])
                    owner = item.get("rootId") or item.get("leadId") or item["id"]
                    self.checked_actor(db, owner)
                    return {
                        **item,
                        "agent": owner,
                        "kind": {
                            "work": "work",
                            "plans": "plan",
                            "complaints": "complaint",
                        }[table],
                        "text": item.get("text")
                        or json.dumps(item, indent=2, ensure_ascii=False),
                    }
            raise ValueError("This search result is unavailable")
