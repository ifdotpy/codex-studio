"""Local canvas API. Existing status, rollout and mailbox formats stay unchanged."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from functools import lru_cache
import gzip
import hashlib
import json
import os
import re
import signal
import shlex
import sqlite3
import subprocess
import sys
import threading
import time
from pathlib import Path

from codex_state import state_dir, codex_home, read_threads, effective_status, process_is_alive
from codex_sqlite import connect as sqlite_connect, assert_clean as sqlite_assert_clean, scope as sqlite_scope
from codex_open_file_limit import raise_open_file_limit
from codex_layout import SERVER_SOURCE_ROOT, WEB_ROOT

SCRIPTS = SERVER_SOURCE_ROOT
WEB = WEB_ROOT / "dist"
COMPONENT = re.compile(r"[A-Za-z0-9._-]+\Z")
AGENT_ID = re.compile(r"[A-Za-z0-9._:/-]{1,200}\Z")
READ_LIMIT = 2 * 1024 * 1024
SHUTDOWN_THREAD_LIMIT = 16
SHUTDOWN_STACK_LIMIT = 32
SHUTDOWN_TIMEOUT_SECONDS = 60
SHUTDOWN_POLL_SECONDS = 0.05


HASHED_ASSET = re.compile(r"^assets/.+-[A-Za-z0-9_-]{8,}\.(?:js|css|png|svg|woff2?)$")


def accepts_gzip(value):
    encodings = {}
    for part in (value or "").split(","):
        name, *parameters = part.strip().lower().split(";")
        quality = 1.0
        for parameter in parameters:
            if parameter.strip().startswith("q="):
                try:
                    quality = float(parameter.strip()[2:])
                except ValueError:
                    quality = 0
        encodings[name] = quality
    return encodings.get("gzip", encodings.get("*", 0)) > 0


@lru_cache(maxsize=64)
def static_content(path, modified_ns, size):
    # Only versioned static files enter this cache. API data and tokens never do.
    data = Path(path).read_bytes()
    compressed = gzip.compress(data, compresslevel=3, mtime=0) if size >= 1024 else None
    return data, compressed if compressed and len(compressed) < len(data) else None


def identity(*parts):
    return hashlib.sha256(json.dumps(parts).encode()).hexdigest()[:24]


def tail_json(path, limit=READ_LIMIT):
    try:
        with path.open("rb") as handle:
            size = os.fstat(handle.fileno()).st_size
            start = max(0, size - limit)
            handle.seek(start)
            if start:
                handle.readline()
            data = handle.read(limit)
    except FileNotFoundError:
        return [], False
    rows = []
    for line in data.splitlines():
        try:
            row = json.loads(line)
            if isinstance(row, dict):
                rows.append(row)
        except (ValueError, UnicodeDecodeError):
            continue
    return rows, bool(start)


class Canvas:
    @property
    def runtime(self):
        return self._runtime

    @runtime.setter
    def runtime(self, runtime):
        self._runtime = runtime
        if runtime is not None:
            # Both components write the same database. Acquire one lock before
            # opening a transaction, including chat-to-runtime message delivery.
            self.lock = runtime.lock

    def __init__(self, root=None, read_only=False):
        self.root = root or state_dir()
        self.read_only = read_only
        self.lock = threading.RLock()
        self.cache = {}
        self.rollouts = {}
        self.db = self.root / "canvas.sqlite3"
        self.runtime = None
        if read_only:
            if not self.db.exists():
                raise ValueError('No canvas state exists. Create a chat or start the canvas first.')
            return
        self.root.mkdir(parents=True, exist_ok=True)
        with self.connect() as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS groups (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS messages (
                  id TEXT PRIMARY KEY, room TEXT NOT NULL, author TEXT NOT NULL,
                  text TEXT NOT NULL, at REAL NOT NULL, deliveries TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS messages_room ON messages(room, at);
                CREATE TABLE IF NOT EXISTS graph_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS graph_edges (
                  id TEXT PRIMARY KEY, source TEXT NOT NULL, target TEXT NOT NULL,
                  kind TEXT NOT NULL, UNIQUE(source, target, kind));
            """)
            from codex_sync_entities import ensure_tables as ensure_sync_entity_tables
            ensure_sync_entity_tables(db)
        os.chmod(self.db, 0o600)

    @contextmanager
    def connect(self):
        db = (sqlite3.connect(self.db.absolute().as_uri() + '?mode=ro', uri=True, timeout=10)
              if self.read_only else sqlite_connect(self.db, timeout=10, site="Canvas.connect"))
        db.row_factory = sqlite3.Row
        try:
            sqlite_assert_clean(db, "Canvas.connect reuse")
            from codex_sync_entities import register_functions, ensure_tables
            register_functions(db)
            if not self.read_only and not db.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities'").fetchone():
                ensure_tables(db)
            sqlite_assert_clean(db, "Canvas.connect setup")
            with sqlite_scope(db, "Canvas.connect"):
                yield db
        finally:
            db.close()

    def threads(self, runtime_agents=None, db=None, *, status_errors=None):
        if db is None:
            with self.connect() as own:
                own.execute("PRAGMA query_only=ON")
                own.execute("BEGIN")
                return self.threads(runtime_agents, db=own, status_errors=status_errors)
        rows = []
        for path in sorted(self.root.glob("codex-swarm-status.*.json")):
            wave = path.name.removeprefix("codex-swarm-status.").removesuffix(".json")
            try:
                rows.extend(read_threads(self.root, wave))
            except Exception as error:
                if status_errors is None:
                    raise
                status_errors.append((path.name, error))
        for row in rows:
            row["id"] = identity(row["wave"], row.get("runId"), row["threadId"], row["name"])
            row["status"] = effective_status(row)
            row["launcherAlive"] = process_is_alive(row.get("launcherPid"))
            row["canSend"] = row["launcherAlive"] and all(
                isinstance(row.get(k), str) and COMPONENT.fullmatch(row[k])
                for k in ("wave", "runId", "name"))
            row["kind"] = "agent"
            row["source"] = "app-server"
        if self.runtime:
            if runtime_agents is None:
                runtime_agents = [
                    self.runtime.agent_entity_view(db, agent)
                    for agent in self.runtime.records(db, "agents", shared=True)
                    if not agent.get("deletedAt")
                ]
            rows.extend(dict(agent) for agent in runtime_agents)
        else:
            if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_agents'").fetchone():
                for key, raw in db.execute("SELECT id,record FROM runtime_agents"):
                    try:
                        a = json.loads(raw)
                        if not isinstance(a, dict) or not isinstance(a.get("id"), str):
                            raise TypeError("agent record must be an object with an id")
                    except (TypeError, ValueError) as error:
                        from codex_sync_entities import _report_bad_entity
                        _report_bad_entity("agent", str(key), error)
                        continue
                    if a.get("deletedAt"):
                        continue
                    rows.append({**a, "kind": "agent", "source": "managed", "canSend": False,
                                 "launcherAlive": False, "wave": "Managed team"})
        by_thread = {t['threadId']: t for t in rows if t.get('threadId')}
        registered = []
        for key, raw in db.execute("SELECT id,record FROM graph_agents"):
            try:
                record = json.loads(raw)
                if not isinstance(record, dict) or not isinstance(record.get("id"), str):
                    raise TypeError("graph agent record must be an object with an id")
                registered.append(record)
            except (TypeError, ValueError) as error:
                from codex_sync_entities import _report_bad_entity
                _report_bad_entity("agent", str(key), error)
        for record in registered:
            existing = by_thread.get(record.get('threadId'))
            if existing:
                existing['graphAlias'] = record['id']
                existing['parentId'] = record.get('parentId')
            else:
                rows.append({**record, 'kind': 'agent', 'source': 'registered', 'canSend': False,
                             'launcherAlive': False, 'events': 0, 'tokensUsed': 0})
        by_id = {t['id']: t for t in rows}
        aliases = {t.get('graphAlias'): t['id'] for t in rows if t.get('graphAlias')}
        for row in list(rows):
            parent = row.get('orchestratorId') or row.get('parentId')
            if not parent:
                continue
            resolved = by_thread.get(parent, {}).get('id') or aliases.get(parent) or parent
            row['parentId'] = resolved
            if resolved not in by_id:
                local_thread = parent if re.fullmatch(r'[a-fA-F0-9-]{36}', parent) else None
                node = {'id': resolved, 'threadId': local_thread, 'name': row.get('orchestratorName') or 'Orchestrator',
                        'role': 'orchestrator', 'kind': 'agent', 'source': 'orchestrator-reference',
                        'status': 'unknown', 'canSend': False, 'launcherAlive': False}
                rows.append(node)
                by_id[resolved] = node
        return rows

    def chats(self, db=None):
        if db is None:
            with self.connect() as own:
                own.execute("PRAGMA query_only=ON")
                own.execute("BEGIN")
                return self.chats(db=own)
        rows = db.execute("""
            SELECT g.id, g.name,
              (SELECT json_group_array(source) FROM (
                SELECT source FROM graph_edges WHERE target=g.id AND kind='chat' ORDER BY source
              )) AS members,
              (SELECT text FROM messages WHERE room=g.id ORDER BY at DESC LIMIT 1) AS tail,
              (SELECT at FROM messages WHERE room=g.id ORDER BY at DESC LIMIT 1) AS last_message_at,
              (SELECT count(*) FROM messages WHERE room=g.id) AS message_count
            FROM groups AS g
        """)
        return [
            {'id': row['id'], 'name': row['name'], 'members': json.loads(row['members'] or '[]'),
             'kind': 'chat', 'messageCount': row['message_count'],
             'tail': row['tail'] if row['message_count'] else '',
             'lastMessageAt': row['last_message_at']}
            for row in rows
        ]

    def edges(self, threads=None, db=None):
        if db is None:
            with self.connect() as own:
                own.execute("PRAGMA query_only=ON")
                own.execute("BEGIN")
                return self.edges(threads, db=own)
        threads = threads if threads is not None else self.threads(db=db)
        edges = [dict(e) for e in db.execute('SELECT * FROM graph_edges')]
        for row in threads:
            if row.get('parentId'):
                edges.append({'id': identity('spawn', row['parentId'], row['id']), 'source': row['parentId'],
                              'target': row['id'], 'kind': 'spawn'})
        return edges

    def register_agent(self, key, name, parent=None, thread_id=None, status='unknown'):
        if not isinstance(key, str) or not AGENT_ID.fullmatch(key):
            raise ValueError('Use a stable host agent identity with at most 200 characters.')
        if not isinstance(name, str) or not 1 <= len(name.strip()) <= 100:
            raise ValueError('Use an agent name with 1 to 100 characters.')
        if thread_id is not None and (not isinstance(thread_id, str) or not COMPONENT.fullmatch(thread_id)):
            raise ValueError('Invalid thread identity')
        if status not in {'unknown', 'running', 'waiting', 'completed', 'blocked', 'failed', 'interrupted'}:
            raise ValueError('Invalid reported status')
        with self.lock, self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            old = db.execute('SELECT record FROM graph_agents WHERE id=?', (key,)).fetchone()
            previous = json.loads(old['record']) if old else {}
            if previous and (previous.get('parentId') != parent or previous.get('threadId') != thread_id):
                raise ValueError('Agent identity already has a different parent or thread.')
            agents = {t['id']: t for t in self.threads()}
            if key in agents and agents[key].get('source') == 'app-server':
                raise ValueError('The launcher owns this agent record.')
            if any(c['id'] == key for c in self.chats()):
                raise ValueError('This identity belongs to a chat.')
            if parent is not None and (parent not in agents or parent == key):
                raise ValueError('Register the parent agent first. An agent cannot be its own parent.')
            cursor = parent
            visited = set()
            while cursor:
                if cursor == key or cursor in visited:
                    raise ValueError('A parent connection cannot contain a cycle.')
                visited.add(cursor)
                cursor = agents.get(cursor, {}).get('parentId')
            record = {'id': key, 'name': name.strip(), 'parentId': parent, 'threadId': thread_id,
                      'status': status, 'reportedAt': time.time(), 'role': 'agent' if parent else 'orchestrator'}
            db.execute('INSERT INTO graph_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                       (key, json.dumps(record)))
            from codex_sync_entities import put as sync_entity_put
            sync_entity_put(db, "agent", key, {**record, "kind": "agent", "source": "registered",
                                                "canSend": False, "launcherAlive": False,
                                                "events": 0, "tokensUsed": 0})
        return record

    def _team_id(self, key, agents=None):
        agents = agents if agents is not None else {t["id"]: t for t in self.threads()}
        seen = set()
        while key and key not in seen:
            seen.add(key)
            agent = agents.get(key, {})
            if agent.get("rootId"):
                return ("managed", agent["rootId"])
            if agent.get("source") == "app-server" and agent.get("wave") and agent.get("runId"):
                return ("wave", agent["wave"], agent["runId"])
            if agent.get("source") == "registered":
                if not agent.get("parentId"):
                    return ("registered", key)
                key = agent["parentId"]
                continue
            break
        raise ValueError("The agent team cannot be verified. Use the current team's runtime tools.")

    def _check_chat_team(self, room, members, actor=None):
        # Historical participants bind the room too. Removing an edge must not
        # transfer old messages to another team.
        participants = set(members)
        with self.connect() as db:
            group = db.execute("SELECT members FROM groups WHERE id=?", (room,)).fetchone()
            if group:
                participants.update(json.loads(group["members"]))
            for row in db.execute("SELECT author, deliveries FROM messages WHERE room=?", (room,)):
                if row["author"] != "user":
                    participants.add(row["author"])
                participants.update(json.loads(row["deliveries"]))
        if actor:
            participants.add(actor)
        agents = {t["id"]: t for t in self.threads()}
        teams = {self._team_id(member, agents) for member in participants}
        if len(teams) > 1:
            raise ValueError("Communication is limited to one team. This chat contains another team.")
        return next(iter(teams), None)

    def agent_messages(self, room, actor):
        with self.lock:
            group = next((g for g in self.chats() if g["id"] == room), None)
            members = group["members"] if group else [room]
            if actor not in members:
                raise ValueError("The author is not a member of this group.")
            self._check_chat_team(room, members, actor)
            return self.messages(room)

    def agent_chats(self, actor):
        self._team_id(actor)
        result = []
        for group in self.chats():
            if actor not in group["members"]:
                continue
            try:
                self._check_chat_team(group["id"], group["members"], actor)
            except ValueError:
                continue
            result.append(group)
        return result

    def connect_chat(self, source, target, connected=True):
        if not isinstance(connected, bool):
            raise ValueError('connected must be a boolean')
        with self.lock, self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            if not any(c['id'] == target for c in self.chats()):
                raise ValueError('The target must be a chat.')
            if connected and not any(t['id'] == source for t in self.threads()):
                raise ValueError('The source must be a current agent.')
            if connected:
                members = next(c["members"] for c in self.chats() if c["id"] == target)
                self._check_chat_team(target, [*members, source])
            key = identity('chat', source, target)
            if connected:
                db.execute('INSERT OR IGNORE INTO graph_edges VALUES (?,?,?,?)', (key, source, target, 'chat'))
            else:
                db.execute('DELETE FROM graph_edges WHERE id=?', (key,))
            from codex_sync_entities import put as sync_entity_put
            sync_entity_put(db, "edge", key,
                            {"id": key, "source": source, "target": target, "kind": "chat"},
                            deleted=not connected)
        return {'id': key, 'connected': connected}

    def thread(self, key):
        matches = [t for t in self.threads() if t["id"] == key]
        if len(matches) != 1:
            raise ValueError("The agent is no longer in the current run. Refresh the canvas.")
        return matches[0]

    def transcript(self, key):
        if self.runtime:
            # A managed history needs no global snapshot or legacy discovery.
            with self.runtime.db() as db:
                managed = db.execute("SELECT 1 FROM runtime_agents WHERE id=?", (key,)).fetchone()
            if managed:
                return self.runtime.transcript(key)
        thread = self.thread(key)
        if thread.get("source") == "managed" and self.runtime:
            return self.runtime.transcript(key)
        tid = thread.get("threadId")
        if not tid:
            return {'items': [], 'truncated': False, 'unavailable': 'No local thread is attached to this agent.'}
        if not COMPONENT.fullmatch(tid):
            raise ValueError("Invalid thread identity")
        now = time.monotonic()
        found, checked = self.rollouts.get(tid, (None, 0))
        if found is None and (tid not in self.rollouts or now - checked > 5):
            paths = list(codex_home().glob(f"sessions/*/*/*/*{tid}*.jsonl"))
            found = max(paths, key=lambda p: p.stat().st_mtime) if paths else None
            self.rollouts[tid] = (found, now)
        if found is None:
            return {"items": [], "truncated": False, "unavailable": "No local transcript for this thread.", "tail": thread.get("tail", "")}
        stat = found.stat()
        signature = (str(found), stat.st_size, stat.st_mtime_ns)
        if key in self.cache and self.cache[key][0] == signature:
            return self.cache[key][1]
        rows, truncated = tail_json(found)
        items = []
        for row in rows:
            p = row.get("payload", {})
            if row.get("type") != "response_item" or not isinstance(p, dict):
                continue
            kind = p.get("type")
            text, role, title = "", "", ""
            if kind == "message" and p.get("role") in ("assistant", "user"):
                role = p["role"]
                text = "\n".join(c.get("text", "") for c in p.get("content", []) if isinstance(c, dict) and c.get("type") in ("text", "input_text", "output_text"))
                title = "Final answer" if p.get("phase") == "final_answer" else role.title()
            elif kind in ("function_call", "custom_tool_call"):
                role, title = "tool", p.get("name", "Tool")
                text = p.get("arguments", p.get("input", ""))
            elif kind in ("function_call_output", "custom_tool_call_output"):
                role, title, text = "output", "Tool result", p.get("output", "")
            if not isinstance(text, str):
                text = json.dumps(text, ensure_ascii=False)
            if text.strip():
                items.append({"id": identity(row.get("timestamp"), p.get("id"), p.get("call_id"), role, text),
                              "role": role, "title": title, "text": text[:20000],
                              "truncated": len(text) > 20000, "at": row.get("timestamp")})
        result = {"items": items[-120:], "truncated": truncated or len(items) > 120, "unavailable": None}
        self.cache[key] = (signature, result)
        return result

    def create_chat(self, name, members, key):
        if not isinstance(name, str) or not 1 <= len(name.strip()) <= 100:
            raise ValueError("Use a chat name with 1 to 100 characters.")
        if not isinstance(members, list) or len(members) > 100 or not all(isinstance(m, str) for m in members):
            raise ValueError("Select at most 100 agents.")
        members = sorted(set(members))
        if not set(members) <= {t["id"] for t in self.threads()}:
            raise ValueError("The selected agents are no longer available.")
        if not isinstance(key, str) or not re.fullmatch(r"[a-f0-9-]{36}", key):
            raise ValueError("Invalid request identity")
        with self.lock, self.connect() as db:
            self._check_chat_team(key, members)
            if any(t['id'] == key for t in self.threads()):
                raise ValueError('This identity belongs to an agent.')
            existing = db.execute("SELECT * FROM groups WHERE id=?", (key,)).fetchone()
            if existing:
                if existing["name"] != name.strip() or json.loads(existing["members"]) != members:
                    raise ValueError("The request identity already has different content.")
            else:
                db.execute("INSERT INTO groups VALUES (?,?,?)", (key, name.strip(), json.dumps(members)))
                for member in members:
                    edge_id = identity('chat', member, key)
                    db.execute('INSERT INTO graph_edges VALUES (?,?,?,?)', (edge_id, member, key, 'chat'))
                    from codex_sync_entities import put as sync_entity_put
                    sync_entity_put(db, "edge", edge_id,
                                    {"id": edge_id, "source": member, "target": key, "kind": "chat"})
                self._sync_chat_entity(db, key)
        return {"id": key}

    def _sync_chat_entity(self, db, key):
        row = db.execute("SELECT id,name FROM groups WHERE id=?", (key,)).fetchone()
        from codex_sync_entities import put as sync_entity_put
        if not row:
            sync_entity_put(db, "chat", key, {}, deleted=True)
            return
        members = [item[0] for item in db.execute(
            "SELECT source FROM graph_edges WHERE target=? AND kind='chat' ORDER BY source", (key,))]
        last = db.execute("SELECT text,at FROM messages WHERE room=? ORDER BY at DESC LIMIT 1", (key,)).fetchone()
        count = db.execute("SELECT count(*) FROM messages WHERE room=?", (key,)).fetchone()[0]
        sync_entity_put(db, "chat", key, {"id": key, "name": row["name"], "members": members,
                                           "kind": "chat", "messageCount": count,
                                           "tail": last["text"] if last else "",
                                           "lastMessageAt": last["at"] if last else None})

    def messages(self, room):
        with self.connect() as db:
            rows = db.execute("SELECT * FROM messages WHERE room=? ORDER BY at DESC LIMIT 200", (room,)).fetchall()
        return [{**dict(r), "deliveries": json.loads(r["deliveries"])} for r in reversed(rows)]

    @staticmethod
    def _delivery_status(receipt):
        if not isinstance(receipt, dict):
            return "unknown: Runtime returned an invalid delivery receipt."
        status = receipt.get("status")
        if status in {"queued", "pending", "reserved", "dispatching", "delivered"}:
            return "queued"
        if status in {"failed", "cancelled"}:
            return f"{status}: {receipt.get('error') or status}"
        return f"unknown: {receipt.get('error') or status or 'Runtime returned no delivery status.'}"

    def _managed_delivery(self, member, message, delivery_id):
        try:
            receipt = self.runtime.delivery_receipt(delivery_id)
        except Exception as error:
            return f"unknown: Delivery receipt could not be read: {error}"
        if receipt is None:
            receipt = self.runtime.send(member, message, delivery_id)
        return self._delivery_status(receipt)

    @staticmethod
    def _message_receipt(row):
        deliveries = list(row["deliveries"].values())
        if not deliveries:
            return {**row, "status": "delivered"}
        accepted = {"queued", "delivered", "accepted", "sent"}
        rejected = [value for value in deliveries
                    if isinstance(value, str) and value.startswith(("failed:", "cancelled:"))]
        if all(isinstance(value, str) and value in accepted for value in deliveries):
            return {**row, "status": "queued" if "queued" in deliveries else "delivered"}
        if len(rejected) == len(deliveries):
            return {**row, "status": "failed", "error": "; ".join(dict.fromkeys(rejected))}
        # A partial or unknown dispatch cannot authorize a new message identity.
        return {**row, "status": "uncertain",
                "error": "Delivery is unconfirmed for one or more recipients. Check the saved deliveries before sending again."}

    def post(self, room, text, key, author="user", notify=True):
        if not isinstance(text, str) or not 1 <= len(text.strip()) <= 12000:
            raise ValueError("Use a message with 1 to 12000 characters.")
        if not isinstance(key, str) or not re.fullmatch(r"[a-f0-9-]{36}", key):
            raise ValueError("Invalid request identity")
        with self.lock, self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            previous = db.execute("SELECT * FROM messages WHERE id=?", (key,)).fetchone()
            if previous:
                if (previous["room"], previous["author"], previous["text"]) != (room, author, text.strip()):
                    raise ValueError("The request identity already has different content.")
                deliveries = json.loads(previous["deliveries"])
                group = next((g for g in self.chats() if g["id"] == room), None)
                members = group["members"] if group else [room]
                if author != "user" and author not in members:
                    raise ValueError("The author is not a member of this group.")
                self._check_chat_team(room, [*members, *deliveries], None if author == "user" else author)
                row = {**dict(previous), "deliveries": deliveries}
                if not any(status == "pending" for status in deliveries.values()):
                    return self._message_receipt(row)
                group = next((g for g in self.chats() if g["id"] == room), None)
                db.commit()
            else:
                threads = self.threads()
                group = next((g for g in self.chats() if g["id"] == room), None)
                if group:
                    members = group["members"]
                elif any(t["id"] == room for t in threads):
                    members = [room]
                else:
                    raise ValueError("This chat is no longer available.")
                if author != "user" and author not in members:
                    raise ValueError("The author is not a member of this group.")
                self._check_chat_team(room, members, None if author == "user" else author)
                deliveries = {m: "pending" for m in members if notify and m != author}
                row = {"id": key, "room": room, "author": author, "text": text.strip(), "at": time.time(), "deliveries": deliveries}
                db.execute("INSERT INTO messages VALUES (?,?,?,?,?,?)", (key, room, author, row["text"], row["at"], json.dumps(deliveries)))
                if group:
                    self._sync_chat_entity(db, room)
                db.commit()
                # Persist before mailbox writes. A repeated HTTP request cannot resend work.
                # A process death during dispatch leaves a visible 'pending' result for recovery.
            message = row["text"]
            if group:
                command = f"CODEX_AGENTS_STATE_DIR={shlex.quote(str(self.root))} {shlex.quote(str(SCRIPTS / 'codex-chat'))}"
                message = (f"Group chat: {group['name']}\nUser message: {message}\n\n"
                           f"Read the shared chat: {command} read {shlex.quote(room)}\n"
                           f"Post a reply: {command} post {shlex.quote(room)} 'your reply'\n"
                           "Use your CODEX_AGENT_OWNER for authorship. Read peer replies before coordination decisions. "
                           "Posts are shared, but do not automatically start peer turns.")
            for member in deliveries:
                if deliveries[member] != "pending":
                    continue
                try:
                    target = self.thread(member)
                    if not target["canSend"]:
                        raise ValueError("No live mailbox for this agent. The message remains in the shared chat.")
                    if target.get("source") == "managed":
                        deliveries[member] = self._managed_delivery(member, message, key + ":" + member)
                        db.execute("UPDATE messages SET deliveries=? WHERE id=?", (json.dumps(deliveries), key))
                        db.commit()
                        continue
                    if previous:
                        deliveries[member] = "unknown: Delivery was interrupted before mailbox acknowledgement. Inspect the mailbox before resending."
                        db.execute("UPDATE messages SET deliveries=? WHERE id=?", (json.dumps(deliveries), key))
                        db.commit()
                        continue
                    # Reuse the installed mailbox writer, never a second app-server.
                    result = subprocess.run([str(SCRIPTS / "codex-steer"), "--wave", target["wave"],
                                             "--expected-run", target["runId"], "--expected-thread", target["threadId"],
                                             target["name"], "--", message],
                                            env={**os.environ, "CODEX_AGENTS_STATE_DIR": str(self.root)},
                                            capture_output=True, text=True, timeout=10)
                    deliveries[member] = "queued" if result.returncode == 0 else "failed: " + (result.stderr or result.stdout).strip()[:400]
                except subprocess.TimeoutExpired:
                    deliveries[member] = "unknown: mailbox command timed out. Inspect the mailbox before resending."
                except (ValueError, OSError) as error:
                    deliveries[member] = "failed: " + str(error)
                db.execute("UPDATE messages SET deliveries=? WHERE id=?", (json.dumps(deliveries), key))
                db.commit()
            row["deliveries"] = deliveries
            return self._message_receipt(row)


def make_server(canvas, port=0, public_origin=None, unix_socket=False):
    """Stable server factory backed by the typed FastAPI application."""
    from studio_api.server import make_server as create_server

    return create_server(canvas, port, public_origin, unix_socket)

def record_shutdown_threads():
    """Keep code locations for threads that can prevent process exit."""
    try:
        main_thread = threading.main_thread()
        threads = [thread for thread in threading.enumerate()
                   if thread is not main_thread and not thread.daemon
                   and thread.ident is not None and thread.is_alive()]
        if not threads:
            return
        frames = sys._current_frames()
        records = []
        try:
            for thread in threads[:SHUTDOWN_THREAD_LIMIT]:
                frame = frames.get(thread.ident)
                stack = []
                while frame is not None and len(stack) < SHUTDOWN_STACK_LIMIT:
                    stack.append({"function": frame.f_code.co_name,
                                  "file": frame.f_code.co_filename,
                                  "line": frame.f_lineno})
                    frame = frame.f_back
                records.append({"threadId": thread.ident,
                                "nativeThreadId": thread.native_id,
                                "stack": stack, "stackTruncated": frame is not None})
                frame = None
        finally:
            frames.clear()
        print(json.dumps({"event": "backend_shutdown_threads", "pid": os.getpid(),
                          "at": time.time(), "threads": records,
                          "threadsTruncated": len(threads) > SHUTDOWN_THREAD_LIMIT}),
              file=sys.stderr, flush=True)
    except Exception:
        # Evidence must not change the existing shutdown result or lease rules.
        try:
            print(json.dumps({"event": "backend_shutdown_thread_diagnostic_failed",
                              "pid": os.getpid()}), file=sys.stderr, flush=True)
        except Exception:
            pass


class BackendShutdown:
    """Defer signal work and bound the entire process shutdown."""

    def __init__(self):
        self.request = None
        self.logged = False
        self.finished = False
        self.timeout_seconds = SHUTDOWN_TIMEOUT_SECONDS

    def terminate(self, number, _frame):
        # A signal can interrupt a response, a lock, or stderr itself.
        # Do not raise, log, or acquire a lock in this handler.
        if self.request is None:
            self.request = (number, time.monotonic())

    def requested(self):
        request = self.request
        if request is None:
            return False
        if not self.logged:
            self.logged = True
            print(json.dumps({"event": "backend_shutdown", "pid": os.getpid(),
                              "signal": request[0], "at": time.time(),
                              "timeoutSeconds": self.timeout_seconds}),
                  file=sys.stderr, flush=True)
        return True

    def watch(self):
        while True:
            request = self.request
            if request is not None:
                if time.monotonic() - request[1] >= self.timeout_seconds:
                    # Cleanup, the event loop, or interpreter thread joins can
                    # stall. Keep this path independent of their locks and logs.
                    os._exit(1)
            elif self.finished:
                return
            time.sleep(SHUTDOWN_POLL_SECONDS)


def main():
    raise_open_file_limit()
    parser = argparse.ArgumentParser(description="Local canvas for Codex app-server waves")
    parser.add_argument("--port", type=int, default=4620)
    args = parser.parse_args()
    shutdown = BackendShutdown()
    # Prepare the watchdog before signals can request shutdown. A daemon still
    # runs while Python waits for non-daemon threads after main returns.
    threading.Thread(target=shutdown.watch, name="backend-shutdown", daemon=True).start()
    signal_numbers = [signal.SIGTERM, signal.SIGINT]
    if hasattr(signal, "SIGBREAK"):
        signal_numbers.append(signal.SIGBREAK)
    previous_handlers = {number: signal.getsignal(number) for number in signal_numbers}
    for number in previous_handlers:
        signal.signal(number, shutdown.terminate)
    runtime = None
    server = None
    updates = None
    unix_thread = None
    try:
        from codex_runtime import Runtime
        if shutdown.requested():
            return
        canvas = Canvas()
        # Bind first so a port collision cannot disturb an existing runtime.
        server = make_server(canvas, args.port, unix_socket=os.name != "nt")
        if shutdown.requested():
            return
        if server.unix_server:
            unix_thread = threading.Thread(target=server.unix_server.serve_forever,
                                           kwargs={"poll_interval": 0.5,
                                                   "shutdown_requested": shutdown.requested}, daemon=True)
            unix_thread.start()
        runtime = Runtime(canvas.root)
        canvas.runtime = runtime
        if shutdown.requested():
            return
        from codex_live_updates import start as start_updates
        updates = start_updates(runtime)
        print(f"Codex Canvas: http://127.0.0.1:{server.server_port}", flush=True)
        server.serve_forever(poll_interval=0.5, shutdown_requested=shutdown.requested)
    except KeyboardInterrupt:
        pass
    except (RuntimeError, OSError) as error:
        parser.exit(1, f"codex-canvas: {error}\n")
    finally:
        try:
            shutdown.requested()
            if server:
                server.shutdown()
            if unix_thread:
                unix_thread.join(timeout=15)
            if updates:
                updates.close()
            if server:
                server.server_close()
            if runtime:
                runtime.close()
            record_shutdown_threads()
        finally:
            shutdown.finished = True
            for number, handler in previous_handlers.items():
                signal.signal(number, handler)
