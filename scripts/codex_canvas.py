"""Local canvas API. Existing status, rollout and mailbox formats stay unchanged."""
from __future__ import annotations

import base64
import argparse
from contextlib import contextmanager
from functools import lru_cache
import gzip
import hashlib
import json
import os
import re
import secrets
import signal
import shlex
import socket
import sqlite3
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from codex_backend_identity import BACKEND_BUILD
from codex_state import state_dir, codex_home, read_threads, effective_status, process_is_alive
from codex_startup_memory import mark as startup_memory_mark
from codex_sqlite import connect as sqlite_connect, assert_clean as sqlite_assert_clean, scope as sqlite_scope
import codex_http_traces

SCRIPTS = Path(__file__).resolve().parent
WEB = SCRIPTS.parent / "web" / "dist"
COMPONENT = re.compile(r"[A-Za-z0-9._-]+\Z")
AGENT_ID = re.compile(r"[A-Za-z0-9._:/-]{1,200}\Z")
READ_LIMIT = 2 * 1024 * 1024


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

    def threads(self, runtime_agents=None, db=None):
        if db is None:
            with self.connect() as own:
                own.execute("PRAGMA query_only=ON")
                own.execute("BEGIN")
                return self.threads(runtime_agents, db=own)
        rows = read_threads(self.root)
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
            rows.extend(dict(agent) for agent in (runtime_agents if runtime_agents is not None
                                                else self.runtime.snapshot(include_work=False)["agents"]))
        else:
            if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_agents'").fetchone():
                for item in db.execute("SELECT record FROM runtime_agents"):
                    a = json.loads(item[0])
                    if a.get("deletedAt"):
                        continue
                    rows.append({**a, "kind": "agent", "source": "managed", "canSend": False,
                                 "launcherAlive": False, "wave": "Managed team"})
        by_thread = {t['threadId']: t for t in rows if t.get('threadId')}
        registered = [json.loads(r['record']) for r in db.execute("SELECT record FROM graph_agents")]
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
        chats = []
        for row in db.execute("SELECT * FROM groups"):
            members = [e['source'] for e in db.execute("SELECT source FROM graph_edges WHERE target=? AND kind='chat' ORDER BY source", (row['id'],))]
            last = db.execute("SELECT text, at FROM messages WHERE room=? ORDER BY at DESC LIMIT 1", (row['id'],)).fetchone()
            count = db.execute("SELECT count(*) FROM messages WHERE room=?", (row['id'],)).fetchone()[0]
            chats.append({'id': row['id'], 'name': row['name'], 'members': members, 'kind': 'chat',
                          'messageCount': count, 'tail': last['text'] if last else '', 'lastMessageAt': last['at'] if last else None})
        return chats

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

    def snapshot(self, runtime_snapshot=None, db=None):
        if db is None:
            with self.connect() as own:
                own.execute("PRAGMA query_only=ON")
                own.execute("BEGIN")
                return self.snapshot(runtime_snapshot, db=own)
        threads = self.threads(runtime_snapshot["agents"] if runtime_snapshot is not None else None, db=db)
        chats = self.chats(db=db)
        return {"threads": threads, "chats": chats, 'nodes': threads + chats,
                'edges': self.edges(threads, db=db), "at": time.time(), "stateDir": str(self.root)}

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
    from codex_remote import RemoteAccess
    remote = RemoteAccess(canvas.root, public_origin)
    token = secrets.token_urlsafe(32)
    terminal_manager = [None]
    cost_reader = [None]
    pricing_catalog = [None]
    session_cost_reader = [None]
    sync_store = [None]
    terminal_lock = threading.RLock()

    def terminals():
        if not canvas.runtime:
            raise ValueError("The agent runtime is unavailable")
        with terminal_lock:
            if terminal_manager[0] is None:
                from codex_terminals import TerminalManager

                terminal_manager[0] = TerminalManager(canvas.root)
            return terminal_manager[0]

    def snapshot(include_work=True):
        if canvas.runtime:
            with canvas.runtime.read_db() as db:
                runtime = canvas.runtime.snapshot(include_work=include_work, db=db)
                return {**canvas.snapshot(runtime_snapshot=runtime, db=db), "runtime": runtime}
        return {**canvas.snapshot(), "runtime": None}

    def sync():
        with terminal_lock:
            if sync_store[0] is None:
                from codex_sync import SyncStore

                def state_signature():
                    runtime = canvas.runtime
                    connected = False
                    volatile = None
                    if runtime:
                        # Native startup holds start_lock across potentially slow
                        # initialization. Signature sampling is on the SSE/HTTP
                        # hot path, so skip this tick instead of waiting for it.
                        if not runtime.start_lock.acquire(blocking=False):
                            return None
                        try:
                            if not runtime.lock.acquire(blocking=False):
                                return None
                            try:
                                connected = bool(set(runtime.servers) - runtime.offline_accounts) and not runtime.closed
                                rate_limits = json.loads(json.dumps(runtime.rate_limits, sort_keys=True))
                                rate_limits_by_account = {
                                    key: json.loads(json.dumps(runtime.rate_limits_for(key), sort_keys=True))
                                    for key in runtime.rate_limits_by_account
                                }
                                connection_ids = dict(runtime.connection_ids)
                                version_monitor = getattr(runtime, "provider_version_monitor", None)
                                provider_warnings = version_monitor.status()["warnings"] if version_monitor else []
                                volatile = json.dumps({
                                    "rateLimits": rate_limits,
                                    "rateLimitsByAccount": rate_limits_by_account,
                                    "connectionIds": connection_ids,
                                    "providerWarnings": provider_warnings,
                                }, sort_keys=True, separators=(",", ":"))
                            finally:
                                runtime.lock.release()
                        finally:
                            runtime.start_lock.release()
                    # Legacy app-server threads come from status files and launcher
                    # liveness. They have no SQLite trigger, so fold file identity
                    # and process liveness into the scoped state revision.
                    files = []
                    for path in sorted(canvas.root.glob("codex-swarm-status.*.json")):
                        try:
                            stat = path.stat()
                        except FileNotFoundError:
                            continue
                        files.append((path.name, stat.st_ino, stat.st_size, stat.st_mtime_ns))
                    liveness = tuple(sorted((
                        (row.get("launcherPid"), process_is_alive(row.get("launcherPid")))
                        for row in read_threads(canvas.root)
                        if row.get("launcherPid") is not None
                    ), key=lambda item: str(item[0])))
                    return connected, volatile, tuple(files), liveness

                sync_store[0] = SyncStore(canvas.connect, snapshot, canvas.transcript,
                                          chat_snapshot=lambda: snapshot(include_work=False),
                                          state_signature=state_signature)
                if canvas.runtime is not None:
                    canvas.runtime.sync_store = sync_store[0]
            return sync_store[0]

    class Handler(BaseHTTPRequestHandler):
        def parse_request(self):
            accepted = BaseHTTPRequestHandler.parse_request(self)
            if accepted and getattr(self, "_http_trace_enabled", False):
                try:
                    self._http_trace = codex_http_traces.begin(self.command, self.path)
                except Exception as error:
                    codex_http_traces.report_failure(error)
            return accepted

        def handle_one_request(self):
            self._http_trace = None
            self._http_status = None
            self._http_trace_enabled = True
            outcome = "complete"
            try:
                BaseHTTPRequestHandler.handle_one_request(self)
            except Exception:
                outcome = "error"
                raise
            finally:
                self._http_trace_enabled = False
                try:
                    codex_http_traces.finish(self._http_trace, self._http_status, outcome)
                except Exception as error:
                    codex_http_traces.report_failure(error)
                    try:
                        codex_http_traces.discard(self._http_trace)
                    except Exception as cleanup_error:
                        codex_http_traces.report_failure(cleanup_error)

        def log_request(self, code="-", size="-"):
            self._http_status = code

        def log_message(self, *_args):
            pass

        def send(self, value, status=200, content_type="application/json", cache_control="no-store", compressed=None, etag=False, weak_etag_fields=(), server_timing=None):
            serialize_started = time.perf_counter() if server_timing else None
            baseline = getattr(self, "sync_entities_after", None)
            if (baseline is not None and 200 <= status < 300 and isinstance(value, dict)
                    and "_syncEntities" not in value):
                with self.server.sync_store().connect() as db:
                    changed = db.execute("""SELECT collection,id,seq,payload,deleted FROM sync_entities
                                            WHERE seq>? AND collection NOT LIKE 'transcript:%'
                                            ORDER BY seq""", (baseline,)).fetchall()
                if changed:
                    value = {**value, "_syncEntities": [
                        {"id": "entity:" + row[0] + ":" + row[1], "seq": row[2],
                         "payload": row[3], "_deleted": bool(row[4])} for row in changed]}

            data = value if isinstance(value, bytes) else json.dumps(value, ensure_ascii=False).encode()
            serialize_ms = (time.perf_counter() - serialize_started) * 1000 if server_timing else None
            compressible = content_type.startswith(("application/json", "application/manifest+json", "text/", "image/svg+xml"))
            encoded = False
            compress_started = time.perf_counter() if server_timing else None
            if accepts_gzip(self.headers.get("Accept-Encoding")) and compressible and len(data) >= 1024:
                candidate = compressed if compressed is not None else gzip.compress(data, compresslevel=3, mtime=0)
                if len(candidate) < len(data):
                    data, encoded = candidate, True
            compress_ms = (time.perf_counter() - compress_started) * 1000 if server_timing else None
            validator_data = data
            if weak_etag_fields and isinstance(value, dict):
                validator_data = json.dumps(
                    {key: entry for key, entry in value.items() if key not in weak_etag_fields},
                    ensure_ascii=False,
                    sort_keys=True,
                ).encode()
            validator = (
                ("W/" if weak_etag_fields else "")
                + '"'
                + hashlib.sha256(validator_data).hexdigest()
                + '"'
                if etag
                else None
            )
            not_modified = bool(
                validator
                and any(
                    tag.strip() == "*"
                    or tag.strip().removeprefix("W/")
                    == validator.removeprefix("W/")
                    for tag in self.headers.get("If-None-Match", "").split(",")
                )
            )
            if not_modified:
                status = 304
                data = b""
                encoded = False
            self.send_response(status)
            if not not_modified:
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", cache_control)
            if compressible:
                self.send_header("Vary", "Accept-Encoding")
            if validator:
                self.send_header("ETag", validator)
            if encoded:
                self.send_header("Content-Encoding", "gzip")
            if server_timing:
                metrics = [f"{name};dur={duration:.2f}" for name, duration in server_timing.items()]
                metrics.extend((f"response-json;dur={serialize_ms:.2f}",
                                f"response-gzip;dur={compress_ms:.2f}"))
                self.send_header("Server-Timing", ", ".join(metrics))
            if not not_modified:
                self.send_header("X-Content-Type-Options", "nosniff")
                self.send_header("Referrer-Policy", "no-referrer")
                self.send_header(
                    "Content-Security-Policy",
                    "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'",
                )
            self.end_headers()
            self.wfile.write(data)

        def send_monitor_log(self, download):
            path = download["path"]
            try:
                descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
                stream = os.fdopen(descriptor, "rb")
                size = os.fstat(stream.fileno()).st_size
            except FileNotFoundError:
                stream = None
                content = download.get("fallback") or b""
                size = len(content)
            start, end, status = 0, size, 200
            value = self.headers.get("Range")
            if value:
                match = re.fullmatch(r"bytes=(\d*)-(\d*)", value.strip())
                if not match or (not match.group(1) and not match.group(2)):
                    if stream:
                        stream.close()
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if match.group(1):
                    start = int(match.group(1))
                    end = min(size, int(match.group(2)) + 1) if match.group(2) else size
                else:
                    suffix = int(match.group(2))
                    start, end = max(0, size - suffix), size
                if start >= size or end <= start:
                    if stream:
                        stream.close()
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                status = 206
            length = end - start
            self.send_response(status)
            self.send_header("Content-Type", download.get("mime", "text/plain"))
            self.send_header("Content-Length", str(length))
            name = re.sub(r"[^A-Za-z0-9._-]", "_", download["name"])
            self.send_header("Content-Disposition", f'attachment; filename="{name}"')
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("X-Log-Truncated", "true" if download.get("truncated") else "false")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end - 1}/{size}")
            self.end_headers()
            try:
                if stream:
                    stream.seek(start)
                    remaining = length
                    while remaining:
                        chunk = stream.read(min(65536, remaining))
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        remaining -= len(chunk)
                else:
                    self.wfile.write(content[start:end])
            finally:
                if stream:
                    stream.close()

        def trusted(self, write=False):
            if getattr(self.server, "address_family", None) == socket.AF_UNIX:
                return not write or secrets.compare_digest(self.headers.get("X-Canvas-Token", ""), token)
            origin = remote.request_origin(self.headers, self.client_address[0], self.server.server_port)
            if origin is None:
                return False
            if self.headers.get("Origin") not in (None, origin):
                return False
            if self.headers.get("Sec-Fetch-Site") == "cross-site":
                return False
            return not write or secrets.compare_digest(self.headers.get("X-Canvas-Token", ""), token)

        def stream_transcript(self, key):
            runtime = canvas.runtime
            runtime.transcript(key)  # Validate before sending streaming headers.
            self.connection.settimeout(20)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream; charset=utf-8")
            self.send_header("Cache-Control", "no-cache, no-transform")
            self.send_header("X-Accel-Buffering", "no")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            revision, previous, version, previous_order = -1, {}, 0, None
            try:
                while not runtime.closed:
                    if not self.trusted():
                        break
                    current, data = runtime.wait_transcript(key, revision)
                    if not self.trusted():
                        break
                    if current is None:
                        break
                    if data is None:
                        self.wfile.write(b": heartbeat\n\n")
                    else:
                        records = {item["id"]: item for item in data.pop("items")}
                        order = list(records)
                        changed = []
                        for key, item in records.items():
                            old = previous.get(key)
                            if old == item:
                                continue
                            if (old and isinstance(old.get("text"), str) and
                                    isinstance(item.get("text"), str) and
                                    item["text"].startswith(old["text"]) and
                                    {k: v for k, v in old.items() if k != "text"} ==
                                    {k: v for k, v in item.items() if k != "text"}):
                                changed.append({"id": key, "append": item["text"][len(old["text"]):]})
                            else:
                                changed.append({"id": key, "replace": item})
                        version += 1
                        payload = {**data, "version": version, "replace": revision == -1,
                                   "items": changed}
                        if revision == -1 or order != previous_order:
                            payload["order"] = order
                        self.wfile.write(("data: " + json.dumps(payload, ensure_ascii=False) + "\n\n").encode())
                        revision, previous, previous_order = current, records, order
                    self.wfile.flush()
                    time.sleep(.08)  # Coalesce fast deltas without polling an idle model.
            except OSError:
                pass
            except (ValueError, RuntimeError, sqlite3.Error) as error:
                try:
                    self.wfile.write(("event: unavailable\ndata: " + json.dumps({"error": str(error)}) + "\n\n").encode())
                    self.wfile.flush()
                except OSError:
                    pass
            self.close_connection = True

        def stream_sync(self):
            store = sync()
            query = parse_qs(urlparse(self.path).query)
            if query.get("protocol") == ["1"] or self.headers.get("X-Codex-Sync-Protocol") == "1":
                scope = query.get("scope", [""])[0]
                raw_cursor = self.headers.get("Last-Event-ID") or query.get("after", ["0"])[0]
                try:
                    cursor = int(raw_cursor)
                    if cursor < 0 or cursor > 9007199254740991:
                        raise ValueError
                    first = store.stream_batch(scope, cursor)
                except ValueError:
                    return self.send({"error": "Invalid sync scope or cursor"}, 400)
                self.connection.settimeout(20)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                self.send_header("Cache-Control", "no-cache, no-transform")
                self.send_header("X-Accel-Buffering", "no")
                self.send_header("X-Codex-Sync-Protocol", "1")
                self.end_headers()
                previous_heartbeat = time.monotonic()
                observed_generation = None
                try:
                    while not (canvas.runtime and canvas.runtime.closed):
                        if not self.trusted():
                            break
                        if scope == "state:entities:v1" or scope.startswith("transcript:"):
                            current_generation = store.generation()
                            if current_generation != observed_generation:
                                store.pull(scope, cursor, 100, reset_support=True)
                                observed_generation = store.generation()
                        batch = first
                        first = None
                        if batch is None:
                            batch = store.stream_batch(scope, cursor)
                        kind = batch["kind"]
                        if kind in ("reset", "cursor-ahead"):
                            payload = {"protocolVersion": 1, "workspaceId": store.identity()["workspaceId"],
                                       "scope": scope, **batch}
                            event = "reset" if kind == "reset" else "cursor-ahead"
                            self.wfile.write((f"event: {event}\ndata: " + json.dumps(payload) + "\n\n").encode())
                            self.wfile.flush()
                            break
                        if kind == "changes":
                            payload = {"protocolVersion": 1, "workspaceId": store.identity()["workspaceId"],
                                       "scope": scope, "documents": batch["documents"], "cursor": batch["cursor"]}
                            encoded = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
                            if len(encoded.encode("utf-8")) > 1024 * 1024:
                                reset = {"protocolVersion": 1, "workspaceId": store.identity()["workspaceId"],
                                         "scope": scope, "kind": "reset", "reason": "event-too-large",
                                         "floor": batch.get("floor", 0), "maxSeq": batch["maxSeq"]}
                                self.wfile.write(("event: reset\ndata: " + json.dumps(reset) + "\n\n").encode())
                                self.wfile.flush()
                                break
                            cursor = batch["cursor"]
                            self.wfile.write((f"id: {cursor}\nevent: changes\ndata: {encoded}\n\n").encode())
                            self.wfile.flush()
                        elif time.monotonic() - previous_heartbeat >= 15:
                            self.wfile.write(b": heartbeat\n\n")
                            self.wfile.flush()
                            previous_heartbeat = time.monotonic()
                        time.sleep(0.25)
                except (OSError, sqlite3.Error):
                    pass
                self.close_connection = True
                return
            shared_stream = query.get("protocol") == ["2"]
            entity_stream = query.get("scope") == ["state:entities:v1"]
            draft_stream = query.get("scope") == ["drafts"]
            transcript_scope = query.get("scope", [""])[0]
            transcript_id = (
                transcript_scope[len("transcript:"):]
                if transcript_scope.startswith("transcript:")
                and len(transcript_scope) < 300
                else None
            )
            self.connection.settimeout(20)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream; charset=utf-8")
            self.send_header("Cache-Control", "no-cache, no-transform")
            self.send_header("X-Accel-Buffering", "no")
            self.end_headers()
            previous = None
            observed_generation = None
            rate_previous = object()
            try:
                while not (canvas.runtime and canvas.runtime.closed):
                    if not self.trusted():
                        break
                    if entity_stream or transcript_id is not None:
                        current_generation = store.generation()
                        if current_generation != observed_generation:
                            scope = "state:entities:v1" if entity_stream else "transcript:" + transcript_id
                            store.pull(scope, 0, 100, reset_support=True)
                            observed_generation = store.generation()
                    current = (store.generation_state() if shared_stream else
                               store.entity_sequence() if entity_stream else
                               store.draft_sequence() if draft_stream else
                               store.transcript_revision(transcript_id) if transcript_id is not None else
                               store.generation())
                    if current is None:
                        current = store.generation()
                    if current != previous:
                        data = (
                            json.dumps(current)
                            if shared_stream or entity_stream or draft_stream or transcript_id is not None
                            else '"RESYNC"'
                        )
                        self.wfile.write(("data: " + data + "\n\n").encode())
                    else:
                        self.wfile.write(b": heartbeat\n\n")
                    if shared_stream and canvas.runtime:
                        from codex_token_rate import token_rates
                        rates = token_rates(canvas.runtime).workspace_snapshot()
                        if rates != rate_previous:
                            payload = {**rates, 'protocol': 2, 'workspaceId': current['workspaceId']}
                            self.wfile.write(("event: token-rates\ndata: " + json.dumps(payload) + "\n\n").encode())
                            rate_previous = rates
                    self.wfile.flush()
                    previous = current
                    time.sleep(1)
            except (OSError, sqlite3.Error):
                pass
            self.close_connection = True

        def do_GET(self):
            path = urlparse(self.path)
            startup_memory_mark("first-renderer-request")
            if not self.trusted():
                return self.send({"error": "Local origin required"}, 403)
            try:
                if path.path == "/api/session":
                    return self.send({"token": token})
                if path.path == "/api/sync/identity":
                    return self.send(sync().identity())
                if path.path == "/api/sync/protocol":
                    return self.send({"protocolVersion": 1, "supportedVersions": [1, 2],
                                      "capabilities": ["pull", "stream", "streamChanges", "entityReset"] +
                                                      (["unixSocket"] if (Path(canvas.root) / "canvas.sock").exists() else []),
                                      "scopes": ["state:entities:v1", "transcript:<agent-id>", "drafts"],
                                      "pullEndpoint": "/api/sync/pull", "streamEndpoint": "/api/sync/stream",
                                      "maxEntityPage": 500, "maxOtherPage": 100,
                                      "maxStreamDocuments": 100, "maxStreamBytes": 1048576})
                if path.path == "/api/sync/pull":
                    q = {k: v[0] for k, v in parse_qs(path.query).items()}
                    store = sync()
                    projection = store.pull(q.get("scope", "state"), q.get("after", 0),
                                            q.get("limit", 100), q.get("fresh") == "1",
                                            q.get("initialHigh", 0), q.get("reset") == "1", q.get("priorityId"))
                    return self.send({**projection, "generation": store.generation()})
                if path.path == "/api/sync/generations":
                    return self.send(sync().generation_state())
                if path.path == "/api/sync/stream":
                    version = parse_qs(path.query).get("protocol", [None])[0]
                    header_version = self.headers.get("X-Codex-Sync-Protocol")
                    if version not in (None, "1", "2") or header_version not in (None, "1", "2"):
                        return self.send({"error": "Unsupported sync protocol version",
                                          "supportedVersions": [1, 2]}, 426)
                    return self.stream_sync()
                if path.path == "/api/state":
                    return self.send({**snapshot(include_work=parse_qs(path.query).get("view") != ["chat"]),
                                      "token": token})
                if path.path == "/api/worktree-disk" and canvas.runtime:
                    from codex_worktree_disk import scanner
                    query = parse_qs(path.query)
                    requested = query.get("workers", [""])[0][:24000]
                    worker_ids = [value for value in requested.split(",")
                                  if value and len(value) <= 128][:500]
                    return self.send(scanner(canvas.root).snapshot(worker_ids))
                if path.path == "/api/costs":
                    with terminal_lock:
                        if cost_reader[0] is None:
                            from codex_costs import AccountCostReader
                            from codex_pricing import PricingCatalog

                            if not canvas.runtime:
                                raise ValueError("The agent runtime is unavailable")
                            pricing_catalog[0] = PricingCatalog(canvas.root)
                            cost_reader[0] = AccountCostReader(canvas.root, canvas.runtime.accounts,
                                                               pricing=pricing_catalog[0])
                    query = parse_qs(path.query)
                    return self.send(cost_reader[0].snapshot(query.get("account_key", ["default"])[0]))
                if path.path == "/api/session-cost":
                    query = parse_qs(path.query)
                    agent_id = query.get("agent", [None])[0]
                    if not agent_id or not AGENT_ID.fullmatch(agent_id):
                        raise ValueError("Select a chat")
                    with terminal_lock:
                        if pricing_catalog[0] is None:
                            from codex_pricing import PricingCatalog
                            pricing_catalog[0] = PricingCatalog(canvas.root)
                        if session_cost_reader[0] is None:
                            from codex_session_costs import SessionCostReader
                            session_cost_reader[0] = SessionCostReader(canvas.root / "canvas.sqlite3",
                                                                        pricing_catalog[0],
                                                                        accounts=getattr(canvas.runtime, "accounts", None),
                                                                        state_root=canvas.root)
                    return self.send(session_cost_reader[0].snapshot(agent_id))
                if path.path == "/api/desktop":
                    from codex_native_runtime import status as native_runtime_status
                    from codex_browser import diagnostics as browser_diagnostics
                    try:
                        saved_recovery = json.loads(
                            (Path(canvas.root) / "background-recovery.json").read_text()
                        )
                    except FileNotFoundError:
                        saved_recovery = {}
                    supervisor_required = saved_recovery.get("supervisorEnabled") is True
                    supervisor_error = (
                        "Supervisor mode is enabled in background-recovery.json, but this backend did not start in supervisor mode. AppServers are not being started; restart through the supervisor recovery service."
                        if supervisor_required
                        and os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") != "1"
                        and os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") != "1"
                        else None
                    )
                    supervisor_status = None
                    if (os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1"
                            or (Path(canvas.root) / "supervisor.sock").exists()):
                        from codex_process_supervisor import status as process_supervisor_status
                        try:
                            supervisor_status = process_supervisor_status(canvas.root)
                        except (OSError, RuntimeError, ValueError) as error:
                            supervisor_status = {"error": str(error)[:300]}
                    browser_account = parse_qs(path.query).get("account_key", ["default"])[0]
                    return self.send(
                        {
                            "application": "codex-agents",
                            "protocol": 1,
                            "mobileProtocol": 1,
                            "backendBuild": BACKEND_BUILD,
                            "nativeRuntime": native_runtime_status(canvas.runtime),
                            "browser": browser_diagnostics(canvas.runtime, browser_account),
                            "liveUpdate": (canvas.runtime.live_updates.status()
                                           if getattr(canvas.runtime, "live_updates", None) else None),
                            "restartEnvironment": {key: os.environ[key] for key in (
                                "CODEX_HOME", "CODEX_CANVAS_CWD",
                                "CODEX_CANVAS_CONCURRENCY", "CODEX_BIN", "SHELL", "LANG", "LC_ALL",
                                "CODEX_AGENTS_SUPERVISOR_MODE")
                                if key in os.environ},
                            "publicOrigin": remote.origin(),
                            "pid": os.getpid(),
                            "supervisorMode": os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1",
                            "supervisorRequired": supervisor_required,
                            "supervisorError": supervisor_error,
                            "supervisor": supervisor_status,
                            "supervisorFallback": os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1",
                            "supervisorNotice": ("The process supervisor stopped unexpectedly. Studio applied normal recovery to turns, monitors, and terminals with reduced restart protection; accepted or uncertain operations were not resubmitted."
                                                 if os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1" else None),
                            "stateDir": str(Path(canvas.root).resolve()),
                        }
                    )
                if path.path == "/api/diagnostics" and canvas.runtime:
                    from codex_diagnostics import snapshot as diagnostics_snapshot
                    diagnostics = diagnostics_snapshot(canvas.runtime)
                    diagnostics["supervisor"] = {
                        "mode": os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1",
                        "fallback": os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1",
                        "notice": ("The process supervisor stopped unexpectedly. Studio applied normal recovery to turns, monitors, and terminals with reduced restart protection; accepted or uncertain operations were not resubmitted."
                                   if os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1" else None),
                    }
                    if diagnostics["supervisor"]["mode"]:
                        try:
                            from codex_process_supervisor import status as process_supervisor_status
                            diagnostics["supervisor"]["health"] = process_supervisor_status(canvas.root)
                        except (OSError, RuntimeError, ValueError) as error:
                            diagnostics["supervisor"]["health"] = {"error": str(error)[:300]}
                    return self.send(diagnostics)
                if path.path == "/api/terminals":
                    return self.send(terminals().listing())
                if path.path == "/api/terminals/output":
                    query = parse_qs(path.query)
                    if query.get("history") == ["1"]:
                        return self.send(terminals().history_output(
                            query.get("id", [None])[0], query.get("offset", [0])[0],
                            query.get("limit", [65536])[0]))
                    return self.send(
                        terminals().output(
                            query.get("id", [None])[0], query.get("offset", [0])[0]
                        )
                    )
                if path.path == "/api/directories":
                    directory = Path(parse_qs(path.query).get("path", [os.getcwd()])[0]).expanduser().resolve()
                    if not directory.is_dir():
                        raise ValueError("This directory is unavailable")
                    folders = sorted((p for p in directory.iterdir() if p.is_dir() and not p.name.startswith('.')), key=lambda p: p.name.lower())
                    return self.send({"path": str(directory), "parent": str(directory.parent) if directory != directory.parent else None,
                        "directories": [{"name": p.name, "path": str(p)} for p in folders[:500]]})
                if canvas.runtime:
                    runtime = canvas.runtime
                    q = {k: v[0] for k, v in parse_qs(path.query).items()}
                    agent = q.get("agent")
                    if path.path == "/api/tool-requests":
                        request_id = q.get("request_id")
                        return self.send(runtime.request_action(agent,
                            {"action": "get", "request_id": request_id} if request_id else {"action": "list"}))
                    if path.path == "/api/analytics":
                        if q.get("export") == "1":
                            chunks = runtime.analytics_export_chunks(**q)
                            first = next(chunks)
                            self.send_response(200)
                            self.send_header("Content-Type", "application/json; charset=utf-8")
                            self.send_header("Content-Disposition", 'attachment; filename="codex-studio-analytics.json"')
                            self.send_header("Cache-Control", "no-store")
                            self.send_header("X-Content-Type-Options", "nosniff")
                            self.end_headers()
                            self.close_connection = True
                            try:
                                self.wfile.write(first)
                                for chunk in chunks:
                                    self.wfile.write(chunk)
                            except (BrokenPipeError, ConnectionResetError):
                                pass
                            except (sqlite3.Error, ValueError, RuntimeError) as error:
                                print(f"Analytics export interrupted: {error}", file=sys.stderr)
                            finally:
                                chunks.close()
                            return
                        result = runtime.analytics(**q)
                        timing = result.pop("__serverTiming", None)
                        return self.send(result, server_timing=timing)
                    if path.path == "/api/accounts/claude/login":
                        from codex_claude_login import manager
                        return self.send(manager(runtime).status(q.get("request_id")))
                    if path.path == "/api/accounts":
                        return self.send(runtime.accounts_snapshot())
                    if path.path == "/api/projects":
                        return self.send(runtime.projects())
                    if path.path == "/api/questions":
                        return self.send(runtime.question_history(agent))
                    if path.path == "/api/workspace":
                        view = q.get("view", "full")
                        value = (runtime.workspace_part(agent, view) if view in {"work", "inbox", "annotations"}
                                 else runtime.workspace_snapshot(agent, view=view))
                        return self.send(value, etag=True)
                    if path.path == "/api/workspace/tasks":
                        cursor = json.loads(q["cursor"]) if q.get("cursor") else None
                        before = json.loads(q["before"]) if q.get("before") else None
                        started = time.perf_counter() if q.get("timing") == "1" else None
                        result = runtime.workspace_task_feed(
                            agent, cursor=cursor, before=before, limit=q.get("limit", 100)
                        )
                        timing = ({"task-feed": (time.perf_counter() - started) * 1000}
                                  if started is not None else None)
                        return self.send(result, etag=True, server_timing=timing)
                    if path.path == "/api/work":
                        return self.send(runtime.work_action(agent, {"action": "list"}))
                    if path.path == "/api/queue":
                        return self.send(runtime.queue_action(agent))
                    if path.path == "/api/messages/receipts":
                        return self.send(runtime.user_delivery_receipts(agent, json.loads(q.get("ids", "[]"))))
                    if path.path == "/api/changes":
                        return self.send(runtime.changes(agent, scope=q.get("scope")), etag=True)
                    if path.path == "/api/plan":
                        return self.send(runtime.plan_action(agent), etag=True)
                    if path.path == "/api/transcript/page":
                        return self.send(runtime.transcript(q.get("id"), before=q.get("before"), around=q.get("around"), after=q.get("after"), limit=q.get("limit", 120)))
                    if path.path == "/api/transcript/item":
                        from codex_transcript_history import history_item
                        return self.send(history_item(runtime, q.get("id"), q.get("message_id")))
                    if path.path == "/api/transcript/search":
                        from codex_transcript_history import search_history
                        return self.send(search_history(runtime, q.get("id"), q.get("q"), q.get("limit", 100)))
                    if path.path == "/api/search/item":
                        return self.send(runtime.search_item(q.get("id")))
                    if path.path == "/api/search":
                        return self.send(
                            runtime.search_work(q.get("q"), limit=q.get("limit", 50))
                        )
                    if path.path == "/api/checkpoints":
                        return self.send(
                            {
                                "checkpoints": runtime.workspace_snapshot(agent)[
                                    "checkpoints"
                                ]
                            }, etag=True
                        )
                    if path.path == "/api/capabilities":
                        return self.send(runtime.capabilities(agent), etag=True, weak_etag_fields=("at",))
                    if path.path == "/api/skills":
                        return self.send(runtime.skill_catalog(agent))
                    if path.path == "/api/panel":
                        return self.send(runtime.get_panel(agent))
                    if path.path == "/api/profiles":
                        return self.send(runtime.profiles())
                    if path.path == "/api/rules":
                        return self.send(runtime.rules(), etag=True)
                    if path.path == "/api/monitor/log":
                        return self.send_monitor_log(runtime.monitor_log(q.get("id")))
                    if path.path == "/api/file-info":
                        return self.send(runtime.file_info(agent, q.get("path"), q.get("asset")))
                    if path.path == "/api/file":
                        content, mime, name = runtime.file_content(
                            agent, q.get("path"), q.get("asset")
                        )
                        return self.send(
                            {
                                "name": name,
                                "mime": mime,
                                "base64": base64.b64encode(content).decode(),
                            }
                        )
                if path.path == "/api/limits" and canvas.runtime:
                    query = parse_qs(path.query)
                    account_key = query.get("account_key", ["default"])[0]
                    cached = canvas.runtime.rate_limits_for(account_key)
                    if query.get("cached") == ["1"] and (cached.get("data") is not None or cached.get("error")):
                        return self.send(cached)
                    # An empty cache after a restart needs one ordinary read.
                    return self.send(canvas.runtime.limits(account_key))
                if path.path == "/api/task" and canvas.runtime:
                    return self.send(canvas.runtime.task_detail(parse_qs(path.query).get("id", [""])[0]))
                if path.path == "/api/complaint" and canvas.runtime:
                    return self.send(canvas.runtime.complaint_detail(parse_qs(path.query).get("id", [""])[0]))
                if path.path == "/api/agent-chat" and canvas.runtime:
                    query = parse_qs(path.query)
                    before = int(query["before"][0]) if query.get("before") else None
                    after = int(query["after"][0]) if query.get("after") else None
                    limit = int(query["limit"][0]) if query.get("limit") else 100
                    return self.send(canvas.runtime.chat_read(query.get("room", [""])[0],
                                                             before=before, after=after, limit=limit))
                if path.path == "/api/models" and canvas.runtime:
                    query = parse_qs(path.query)
                    account = query.get("account_key", ["default"])[0]
                    from codex_catalog import CatalogPending, DISPLAY_READ
                    # A settings list may show an expired catalog while it refreshes.
                    display = DISPLAY_READ.set(True)
                    try:
                        if query.get("workers") == ["1"]:
                            from codex_worker_accounts import catalog
                            return self.send(catalog(canvas.runtime, account))
                        return self.send(canvas.runtime.catalog(account))
                    except CatalogPending as error:
                        return self.send({"error": str(error), "catalogPending": True}, 400)
                    finally:
                        DISPLAY_READ.reset(display)
                if path.path == "/api/import" and canvas.runtime:
                    query = parse_qs(path.query)
                    return self.send(canvas.runtime.import_list(query.get("cursor", [None])[0], account_key=query.get("account_key", ["default"])[0]))
                if path.path == "/api/transcript/stream" and canvas.runtime:
                    return self.stream_transcript(parse_qs(path.query).get("id", [""])[0])
                if path.path == "/api/transcript":
                    return self.send(canvas.transcript(parse_qs(path.query).get("id", [""])[0]))
                if path.path == "/api/messages":
                    return self.send(canvas.messages(parse_qs(path.query).get("room", [""])[0]))
                relative = "index.html" if path.path == "/" else path.path.lstrip("/")
                asset = (WEB / relative).resolve()
                if asset.is_relative_to(WEB.resolve()) and asset.is_file() and (relative in {"index.html", "studio-sw.js", "manifest.webmanifest", "apple-touch-icon.png", "icon.svg", "icon-192.png", "icon-512.png"} or relative.startswith("assets/")):
                    mime = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".webmanifest": "application/manifest+json", ".png": "image/png", ".svg": "image/svg+xml"}.get(asset.suffix, "application/octet-stream")
                    if HASHED_ASSET.fullmatch(relative):
                        stat = asset.stat()
                        data, compressed = static_content(str(asset), stat.st_mtime_ns, stat.st_size)
                        return self.send(data, content_type=mime, compressed=compressed,
                                         cache_control="private, max-age=31536000, immutable")
                    return self.send(asset.read_bytes(), content_type=mime,
                                     cache_control="no-cache" if relative == "studio-sw.js" else "no-store")
                if path.path == "/":
                    return self.send({"error": "Build the interface: cd web && npm ci && npm run build"}, 503)
                return self.send({"error": "Not found"}, 404)
            except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
                return self.send({"error": str(error)}, 400)

        def do_POST(self):
            federation_path = self.path in {
                "/api/federation/v1/pair", "/api/federation/v1/status",
                "/api/federation/v1/message", "/api/federation/v1/pull",
            }
            if not federation_path and not self.trusted(write=True):
                return self.send({"error": "Local origin and session token required"}, 403)
            try:
                if federation_path:
                    from codex_remote import RemoteAccess
                    remote = RemoteAccess(canvas.root)
                    if remote.request_origin(self.headers, self.client_address[0], self.server.server_port) is None:
                        return self.send({"error": "Tailscale Serve origin required"}, 403)
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= 256 * 1024:
                        return self.send({"error": "Invalid federation request size"}, 413)
                    if self.headers.get_content_type() != "application/json":
                        return self.send({"error": "JSON required"}, 415)
                    self.connection.settimeout(10)
                    raw = self.rfile.read(length)
                    if len(raw) != length:
                        return self.send({"error": "Incomplete federation request"}, 400)
                    action = self.path.rsplit("/", 1)[-1]
                    try:
                        result = canvas.runtime.federation().route(
                            action, self.headers, self.client_address[0], raw)
                    except PermissionError as error:
                        return self.send({"error": str(error)}, 403)
                    return self.send(result)
                length = int(self.headers.get("Content-Length", "0"))
                if (
                    not 0
                    < length
                    <= (28 * 1024 * 1024 if self.path == "/api/assets" else 6 * 1024 * 1024 if self.path == "/api/voice/audio" else 262144)
                ):
                    return self.send({"error": "Invalid request size"}, 413)
                if self.headers.get_content_type() != "application/json":
                    return self.send({"error": "JSON required"}, 415)
                self.connection.settimeout(10)
                body = json.loads(self.rfile.read(length))
                if not isinstance(body, dict):
                    raise ValueError("JSON object required")
                self.sync_entities_after = sync().entity_sequence()
                workspace = self.headers.get("X-Canvas-Workspace")
                if (workspace is not None or self.path == "/api/sync/drafts") and workspace != sync().identity()["workspaceId"]:
                    return self.send({"error": "The server workspace changed. Reload before sending."}, 409)
                if self.path.startswith("/api/voice/") and canvas.runtime:
                    voice = canvas.runtime.voice()
                    action = self.path.removeprefix("/api/voice/")
                    fields = {
                        "status": (), "start": ("session_id", "sdp"), "end": ("session_id",),
                        "record": ("session_id", "event_id", "kind", "text", "item_id", "previous_item_id", "payload"),
                        "records": ("after",), "speech": ("record_id", "session_id"),
                        "submit": ("message_id", "record_ids", "edited_text"),
                        "audio": ("session_id", "chunk_id", "audio", "mime"),
                        "approvals": (), "approval_speech": ("request_id",),
                        "approve": ("session_id", "speech_id", "transcript_id"),
                    }
                    if action not in fields:
                        raise ValueError("Unknown voice action")
                    optional = {"record": {"text", "item_id", "previous_item_id", "payload"},
                                "records": {"after"}, "submit": {"edited_text"}}
                    if any(key not in body for key in fields[action] if key not in optional.get(action, set())):
                        raise ValueError("Missing voice request fields")
                    args = {key: body[key] for key in fields[action] if key in body}
                    return self.send(getattr(voice, action)(body.get("agent"), **args))
                if self.path == "/api/sync/drafts":
                    return self.send(sync().push_drafts(body.get("rows")))
                if self.path == "/api/limits/reset" and canvas.runtime:
                    from codex_limit_resets import consume_reset

                    return self.send(consume_reset(canvas.runtime, body))
                if self.path == "/api/terminals/create":
                    return self.send(terminals().create(canvas.runtime, body))
                if self.path in {
                    "/api/terminals/input",
                    "/api/terminals/resize",
                    "/api/terminals/rename",
                    "/api/terminals/close",
                }:
                    return self.send(
                        terminals().action(self.path.rsplit("/", 1)[1], body)
                    )
                if canvas.runtime:
                    runtime = canvas.runtime
                    agent = body.get("agent")
                    if self.path == "/api/federation":
                        from codex_federation import FederationService
                        service = runtime.federation()
                        action = body.get("action")
                        if action == "approve_peer":
                            return self.send(service.approve_peer(
                                body.get("state_id"), body.get("accept_missing_whois", False)))
                        return self.send(service.action(body))
                    if self.path == "/api/panel/layout":
                        from codex_progress_layout import LayoutConflict, record_layout
                        try:
                            return self.send(record_layout(runtime, body))
                        except LayoutConflict as error:
                            return self.send({"error": str(error)}, 409)
                    if self.path == "/api/peer-teams":
                        from codex_peer_teams import manage
                        return self.send(manage(runtime, body))
                    if self.path == "/api/projects":
                        return self.send(runtime.projects(body))
                    if self.path in {"/api/accounts/claude/login", "/api/accounts/claude/login/code", "/api/accounts/claude/login/cancel"}:
                        from codex_claude_login import manager
                        login = manager(runtime)
                        if self.path.endswith("/code"):
                            return self.send(login.code(body.get("request_id"), body.get("code")))
                        if self.path.endswith("/cancel"):
                            return self.send(login.cancel(body.get("request_id")))
                        return self.send(login.start(body.get("account_key"), body.get("request_id")))
                    if self.path == "/api/accounts/discover":
                        return self.send(runtime.accounts.discover())
                    if self.path == "/api/accounts/register":
                        runtime.accounts.register(body.get("home"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/default":
                        runtime.accounts.default(body.get("account_key"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/login/cancel":
                        return self.send(runtime.accounts.cancel_login(runtime, body.get("request_id")))
                    if self.path == "/api/accounts/delete":
                        runtime.accounts.delete(body.get("account_key"), body.get("request_id"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/disconnect":
                        runtime.accounts.disconnect(body.get("account_key"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/reconnect":
                        runtime.accounts.reconnect(body.get("account_key"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/login":
                        return self.send(runtime.accounts.start_login(runtime, body.get("request_id"), body.get("account_key")))
                    if self.path == "/api/claude/profiles":
                        from codex_claude_controls import profile
                        return self.send(profile(runtime, body))
                    if self.path == "/api/claude/session":
                        from codex_claude_controls import action
                        return self.send(action(runtime, body))
                    if self.path == "/api/agents/account-transfer":
                        from codex_account_transfer import transfer_store
                        transfers = transfer_store(runtime)
                        if body.get("action"):
                            return self.send(transfers.action(body.get("request_id"), body["action"]))
                        return self.send(transfers.request(body.get("id"), body.get("account_key"),
                                                           body.get("request_id"), body.get("scope", "team")))
                    if self.path == "/api/agents/account":
                        selected = runtime.set_account(body.get("id"), body.get("account_key"), body.get("cwd"))
                        with runtime.lock, runtime.db() as db:
                            selected = runtime.agent(selected["id"], db)
                            selected["empty"] = runtime.empty_lead(db, selected)
                        return self.send(selected)
                    if self.path == "/api/work":
                        return self.send(
                            runtime.work_action(agent, body, body.get("id"))
                        )
                    if self.path == "/api/queue":
                        return self.send(runtime.queue_action(agent, body))
                    if self.path == "/api/plan":
                        return self.send(runtime.plan_action(agent, body))
                    if self.path == "/api/annotation":
                        return self.send(runtime.annotate(agent, body))
                    if self.path == "/api/organization":
                        return self.send(
                            runtime.chat_organization(body.get("id"), body)
                        )
                    if self.path == "/api/assets":
                        return self.send(runtime.upload_asset(body))
                    if self.path == "/api/branch":
                        return self.send(runtime.branch_conversation(agent, body))
                    if self.path == "/api/checkpoint":
                        return self.send(runtime.checkpoint_summary(
                            runtime.checkpoint_capture(
                                agent, body.get("label", "Checkpoint")
                            )
                        ))
                    if self.path == "/api/checkpoint/preview":
                        return self.send(
                            runtime.checkpoint_preview(agent, body.get("checkpoint"))
                        )
                    if self.path == "/api/checkpoint/restore":
                        return self.send(runtime.restore_checkpoint(agent, body))
                    if self.path == "/api/tool-requests/cancel":
                        if set(body) != {"agent", "request_id"}:
                            raise ValueError("Supply agent and request_id")
                        return self.send(runtime.request_action(body["agent"],
                            {"action": "cancel", "request_id": body["request_id"]}))
                    if self.path == "/api/profiles":
                        return self.send(runtime.profiles(body))
                    if self.path == "/api/rules":
                        return self.send(runtime.rules(body))
                    if self.path == "/api/monitor/input":
                        return self.send(runtime.monitor_input(body.get("id"), body))
                    if self.path == "/api/native-command":
                        return self.send(runtime.native_command_action(body))
                    if self.path == "/api/messages":
                        if runtime.federation().has_room(body.get("room")):
                            result = runtime.federation().user_message(
                                body.get("room"), body.get("text", ""), body.get("id"))
                            result["id"] = body.get("id")
                            return self.send(result)
                        with runtime.db() as db:
                            managed = db.execute(
                                "SELECT 1 FROM runtime_agents WHERE id=?",
                                (body.get("room"),),
                            ).fetchone()
                        if managed:
                            result=runtime.send(body['room'],body.get('text',''),body.get('id'),delivery=body.get('delivery','queue'),assets=body.get('assets',[]))
                            # Keep the existing message-reader API as a mirror. Runtime owns delivery and retries.
                            with canvas.lock,canvas.connect() as db:
                                db.execute('INSERT OR IGNORE INTO messages VALUES (?,?,?,?,?,?)',(result['id'],body['room'],'user',body.get('text','').strip(),time.time(),json.dumps({body['room']:result['status']})))
                            return self.send(result)
                    if self.path == "/api/rename":
                        return self.send(canvas.runtime.rename(body.get("id"), body.get("name")))
                    if self.path == "/api/room/delete":
                        return self.send(canvas.runtime.hide_room(body.get("id")))
                    if self.path == "/api/complaints":
                        key = body.get("id")
                        if not isinstance(key, str) or not 1 <= len(key) <= 200:
                            raise ValueError("A complaint request id is required")
                        if body.get("action", "submit") == "respond":
                            from codex_runtime import ComplaintConflict
                            try:
                                return self.send(canvas.runtime.complaint_response_from_user(body, "user:" + key))
                            except ComplaintConflict as error:
                                return self.send({"error": str(error)}, 409)
                        if body.get("action", "submit") != "submit":
                            raise ValueError("Choose submit or respond")
                        return self.send(canvas.runtime.complaint(body.get("lead"), {"action": "submit", "text": body.get("text")}, "user:" + key, user=True))
                    if self.path == "/api/conversation/delete":
                        return self.send(canvas.runtime.delete_conversation(body.get("id")))
                    if self.path == "/api/leads":
                        created = canvas.runtime.new_lead(body)
                        # The creation receipt is shown before its snapshot arrives.
                        with runtime.lock, runtime.db() as db:
                            created = runtime.agent(created["id"], db)
                            created["empty"] = runtime.empty_lead(db, created)
                        return self.send(created)
                    if self.path == "/api/conversation":
                        if any(field in body for field in ("agent_mode", "expected_mode_revision", "subagent_concurrency")):
                            try:
                                result = canvas.runtime.conversation_settings(body.get("id"), body)
                            except ValueError as error:
                                return self.send({"error": str(error), "outcome": "not_applied"}, 400)
                            return self.send(result)
                        return self.send(canvas.runtime.conversation_settings(body.get("id"), body))
                    if self.path == "/api/agents":
                        return self.send(
                            canvas.runtime.create(body, parent=body.get("parent"))
                        )
                    if self.path == "/api/configure":
                        return self.send(canvas.runtime.configure(body.get("id"), body))
                    if self.path == "/api/connection-recovery":
                        from codex_connection_recovery import recover
                        return self.send(recover(runtime, body.get("id")))
                    if self.path == "/api/context-repair":
                        from codex_context_repair import repair_idle
                        repaired = repair_idle(runtime, body.get("id"))
                        return self.send({"id": repaired["id"], "repair": repaired.get("contextRepair")})
                    if self.path == "/api/capacity-retry":
                        return self.send(canvas.runtime.capacity_retry(body.get("id"), body.get("retry_id"), body.get("action")))
                    if self.path == "/api/usage-resume":
                        return self.send(canvas.runtime.usage_resume_action(body.get("id"), body.get("resume_id"), body.get("enabled")))
                    if self.path == "/api/action":
                        action = body.get("action")
                        if isinstance(action, dict) and "safety" in action:
                            return self.send(canvas.runtime.native_action(body.get("id"), action))
                        request_id = body.get("request_id")
                        if not isinstance(request_id, str) or not 1 <= len(request_id) <= 200:
                            return self.send({"error": "Reload Studio before Review or Compact. This client has no durable action request ID.",
                                              "outcome": "not_applied"}, 400)
                        if set(body) - {"id", "action", "request_id", "context"}:
                            return self.send({"error": "Invalid native action fields", "outcome": "not_applied"}, 400)
                        try:
                            result = canvas.runtime.native_action(body.get("id"), action, request_id, body.get("context", {}))
                        except ValueError as error:
                            return self.send({"error": str(error), "outcome": "not_applied"}, 400)
                        return self.send(result)
                    if self.path == "/api/import":
                        return self.send(canvas.runtime.import_thread(body))
                    if self.path == "/api/stop":
                        return self.send(canvas.runtime.stop(body.get("id"), body.get("descendants", True)))
                    if self.path == "/api/monitor/cancel":
                        return self.send(canvas.runtime.cancel_monitor(body.get("id")))
                    if self.path == "/api/questions/delete":
                        return self.send(runtime.delete_question(body.get("id")))
                    if self.path == "/api/questions/defer":
                        return self.send(canvas.runtime.defer_question(body.get("id"), body.get("deferred", True)))
                    if self.path == "/api/answer":
                        return self.send(canvas.runtime.answer(body.get("id"), body))
                if self.path == "/api/chats":
                    return self.send(canvas.create_chat(body.get("name"), body.get("members", []), body.get("id")))
                if self.path == "/api/connections":
                    return self.send(canvas.connect_chat(body.get('source'), body.get('target'), body.get('connected', True)))
                if self.path == "/api/messages":
                    return self.send(canvas.post(body.get("room"), body.get("text"), body.get("id")))
                return self.send({"error": "Not found"}, 404)
            except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
                return self.send({"error": str(error)}, 400)

    class LocalServer(ThreadingHTTPServer):
        # A diagram can request many module chunks while transcript streams stay open.
        request_queue_size = socket.SOMAXCONN
        voice_pruned_at = 0

        def service_actions(self):
            if time.monotonic() - self.voice_pruned_at >= 3600:
                self.voice_pruned_at = time.monotonic()
                if canvas.runtime:
                    from codex_voice import prune_audio
                    from codex_execution import maintenance
                    maintenance(canvas.runtime)
                    try:
                        prune_audio(canvas.root)
                    except OSError as error:
                        print(f"Voice audio cleanup failed: {error}", file=sys.stderr)

        def server_close(self):
            if cost_reader[0] is not None:
                cost_reader[0].close()
            if terminal_manager[0] is not None:
                terminal_manager[0].close()
            super().server_close()

    class LocalUnixServer(ThreadingHTTPServer):
        address_family = socket.AF_UNIX
        daemon_threads = True
        allow_reuse_address = False

        def server_bind(self):
            socket_path = Path(self.server_address)
            socket_path.parent.mkdir(parents=True, exist_ok=True)
            if socket_path.exists():
                if not socket_path.is_socket():
                    raise OSError(f"Canvas socket path is occupied: {socket_path}")
                probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                probe.settimeout(0.2)
                try:
                    probe.connect(str(socket_path))
                except ConnectionRefusedError:
                    socket_path.unlink()
                finally:
                    probe.close()
            super().server_bind()
            os.chmod(socket_path, 0o600)
            stat = socket_path.stat()
            self.owned_socket_identity = (stat.st_dev, stat.st_ino)

        def server_close(self):
            ThreadingHTTPServer.server_close(self)
            socket_path = Path(self.server_address)
            try:
                stat = socket_path.stat()
                if (stat.st_dev, stat.st_ino) == getattr(self, "owned_socket_identity", None):
                    socket_path.unlink()
            except FileNotFoundError:
                pass

    server = LocalServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    # Explicit patch points for an already-running server instance. A live
    # patch can replace RequestHandlerClass.send/do_GET/do_POST/stream_sync and
    # resolve these services without constructing a second backend.
    server.canvas = canvas
    server.sync_store = sync
    server.snapshot_state = snapshot
    server.unix_server = None
    if unix_socket:
        try:
            local_socket = LocalUnixServer(str(Path(canvas.root) / "canvas.sock"), Handler)
        except Exception:
            server.server_close()
            raise
        local_socket.daemon_threads = True
        local_socket.canvas = canvas
        local_socket.sync_store = sync
        local_socket.snapshot_state = snapshot
        server.unix_server = local_socket
    return server


def raise_open_file_limit(target=65536):
    """Raise the soft open-file limit before native processes start.

    launchd starts the backend with a soft limit of 256. Each loaded Codex
    thread keeps pipes to its MCP servers, so a busy app-server reached that
    limit and could not start commands (OS error 24). Children inherit the
    raised limit. The hard limit is never changed.
    """
    import resource
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    wanted = target if hard == resource.RLIM_INFINITY else min(target, hard)
    if soft != resource.RLIM_INFINITY and soft < wanted:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (wanted, hard))
        except (ValueError, OSError):
            pass
    return resource.getrlimit(resource.RLIMIT_NOFILE)[0]


def main():
    raise_open_file_limit()
    parser = argparse.ArgumentParser(description="Local canvas for Codex app-server waves")
    parser.add_argument("--port", type=int, default=4620)
    args = parser.parse_args()
    def terminate(_signal, _frame):
        print(json.dumps({"event": "backend_shutdown", "pid": os.getpid(),
                          "signal": _signal, "at": time.time()}), file=sys.stderr, flush=True)
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    runtime = None
    server = None
    updates = None
    try:
        from codex_runtime import Runtime
        canvas = Canvas()
        # Bind first so a port collision cannot disturb an existing runtime.
        server = make_server(canvas, args.port, unix_socket=True)
        if server.unix_server:
            unix_thread = threading.Thread(target=server.unix_server.serve_forever,
                                           kwargs={"poll_interval": 0.5}, daemon=True)
            unix_thread.start()
        runtime = Runtime(canvas.root)
        canvas.runtime = runtime
        from codex_live_updates import start as start_updates
        updates = start_updates(runtime)
        print(f"Codex Canvas: http://127.0.0.1:{server.server_port}", flush=True)
        server.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    except (RuntimeError, OSError) as error:
        parser.exit(1, f"codex-canvas: {error}\n")
    finally:
        if updates:
            updates.close()
        if server:
            if server.unix_server:
                server.unix_server.shutdown()
                server.unix_server.server_close()
            server.server_close()
        if runtime:
            runtime.close()
