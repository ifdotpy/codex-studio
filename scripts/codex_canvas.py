"""Local canvas API. Existing status, rollout and mailbox formats stay unchanged."""
from __future__ import annotations

import base64
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
import re
import secrets
import signal
import shlex
import sqlite3
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from codex_state import state_dir, board_dir, codex_home, read_threads, effective_status, process_is_alive

SCRIPTS = Path(__file__).resolve().parent
WEB = SCRIPTS.parent / "web" / "dist"
COMPONENT = re.compile(r"[A-Za-z0-9._-]+\Z")
AGENT_ID = re.compile(r"[A-Za-z0-9._:/-]{1,200}\Z")
READ_LIMIT = 2 * 1024 * 1024


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
                CREATE TABLE IF NOT EXISTS canvas_migrations (name TEXT PRIMARY KEY);
            """)
            db.execute('BEGIN IMMEDIATE')
            if not db.execute("SELECT 1 FROM canvas_migrations WHERE name='chat-edges-v1'").fetchone():
                legacy = {r['room'] for r in db.execute("SELECT DISTINCT room FROM messages WHERE room LIKE 'wave-%'")}
                waves = {}
                for row in read_threads(self.root):
                    key = 'wave-' + identity(row['wave'], row.get('runId'))
                    wave = waves.setdefault(key, {'name': row['wave'], 'members': []})
                    wave['members'].append(identity(row['wave'], row.get('runId'), row['threadId'], row['name']))
                for key in legacy:
                    wave = waves.get(key, {'name': 'Previous wave chat', 'members': []})
                    db.execute('INSERT OR IGNORE INTO groups VALUES (?,?,?)', (key, wave['name'], json.dumps(wave['members'])))
                for group in db.execute("SELECT * FROM groups").fetchall():
                    for member in json.loads(group["members"]):
                        db.execute("INSERT OR IGNORE INTO graph_edges VALUES (?,?,?,?)",
                                   (identity('chat', member, group['id']), member, group['id'], 'chat'))
                db.execute("INSERT INTO canvas_migrations VALUES ('chat-edges-v1')")
        os.chmod(self.db, 0o600)

    @contextmanager
    def connect(self):
        db = (sqlite3.connect(self.db.absolute().as_uri() + '?mode=ro', uri=True, timeout=10)
              if self.read_only else sqlite3.connect(self.db, timeout=10))
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def threads(self):
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
            rows.extend(self.runtime.snapshot()["agents"])
        else:
            with self.connect() as db:
                if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_agents'").fetchone():
                    for item in db.execute("SELECT record FROM runtime_agents"):
                        a = json.loads(item[0])
                        if a.get("deletedAt"):
                            continue
                        rows.append({**a, "kind": "agent", "source": "managed", "canSend": False,
                                     "launcherAlive": False, "wave": "Managed team"})
        by_thread = {t['threadId']: t for t in rows if t.get('threadId')}
        with self.connect() as db:
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

    def chats(self):
        with self.connect() as db:
            chats = []
            for row in db.execute("SELECT * FROM groups"):
                members = [e['source'] for e in db.execute("SELECT source FROM graph_edges WHERE target=? AND kind='chat' ORDER BY source", (row['id'],))]
                last = db.execute("SELECT text, at FROM messages WHERE room=? ORDER BY at DESC LIMIT 1", (row['id'],)).fetchone()
                count = db.execute("SELECT count(*) FROM messages WHERE room=?", (row['id'],)).fetchone()[0]
                chats.append({'id': row['id'], 'name': row['name'], 'members': members, 'kind': 'chat',
                              'messageCount': count, 'tail': last['text'] if last else '', 'lastMessageAt': last['at'] if last else None})
        return chats

    def edges(self, threads=None):
        threads = threads if threads is not None else self.threads()
        with self.connect() as db:
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
        return record

    def connect_chat(self, source, target, connected=True):
        if not isinstance(connected, bool):
            raise ValueError('connected must be a boolean')
        with self.lock, self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            if not any(c['id'] == target for c in self.chats()):
                raise ValueError('The target must be a chat.')
            if connected and not any(t['id'] == source for t in self.threads()):
                raise ValueError('The source must be a current agent.')
            key = identity('chat', source, target)
            if connected:
                db.execute('INSERT OR IGNORE INTO graph_edges VALUES (?,?,?,?)', (key, source, target, 'chat'))
            else:
                db.execute('DELETE FROM graph_edges WHERE id=?', (key,))
        return {'id': key, 'connected': connected}

    def snapshot(self):
        threads = self.threads()
        board = {"claims": {}, "notes": [], "queue": {}}
        board_error = None
        try:
            path = board_dir(self.root) / "codex-board.json"
            if path.exists():
                board = json.loads(path.read_text())
                if (not isinstance(board, dict) or not isinstance(board.get("claims"), dict)
                        or not all(isinstance(c, dict) for c in board["claims"].values())
                        or not isinstance(board.get("notes", []), list)
                        or not isinstance(board.get("queue", {}), dict)):
                    raise ValueError("Invalid board data")
        except (ValueError, OSError) as error:
            board = {"claims": {}, "notes": [], "queue": {}}
            board_error = str(error)
        owners = {t.get("boardOwner"): t for t in threads}
        for claim in board["claims"].values():
            owner = owners.get(claim.get("worker"))
            claim["stale"] = bool(owner and not owner["launcherAlive"])
        chats = self.chats()
        return {"threads": threads, "chats": chats, 'nodes': threads + chats, 'edges': self.edges(threads), "board": board,
                "boardError": board_error, "at": time.time(), "stateDir": str(self.root)}

    def thread(self, key):
        matches = [t for t in self.threads() if t["id"] == key]
        if len(matches) != 1:
            raise ValueError("The agent is no longer in the current run. Refresh the canvas.")
        return matches[0]

    def transcript(self, key):
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
            if any(t['id'] == key for t in self.threads()):
                raise ValueError('This identity belongs to an agent.')
            existing = db.execute("SELECT * FROM groups WHERE id=?", (key,)).fetchone()
            if existing:
                if existing["name"] != name.strip() or json.loads(existing["members"]) != members:
                    raise ValueError("The request identity already has different content.")
            else:
                db.execute("INSERT INTO groups VALUES (?,?,?)", (key, name.strip(), json.dumps(members)))
                for member in members:
                    db.execute('INSERT INTO graph_edges VALUES (?,?,?,?)', (identity('chat', member, key), member, key, 'chat'))
        return {"id": key}

    def messages(self, room):
        with self.connect() as db:
            rows = db.execute("SELECT * FROM messages WHERE room=? ORDER BY at DESC LIMIT 200", (room,)).fetchall()
        return [{**dict(r), "deliveries": json.loads(r["deliveries"])} for r in reversed(rows)]

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
                return {**dict(previous), "deliveries": json.loads(previous["deliveries"])}
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
            deliveries = {m: "pending" for m in members if notify and m != author}
            row = {"id": key, "room": room, "author": author, "text": text.strip(), "at": time.time(), "deliveries": deliveries}
            db.execute("INSERT INTO messages VALUES (?,?,?,?,?,?)", (key, room, author, row["text"], row["at"], json.dumps(deliveries)))
            db.commit()
            # Persist before mailbox writes. A repeated HTTP request cannot resend work.
            # A process death during dispatch leaves a visible 'pending' result for review.
            message = row["text"]
            if group:
                command = f"CODEX_AGENTS_STATE_DIR={shlex.quote(str(self.root))} {shlex.quote(str(SCRIPTS / 'codex-chat'))}"
                message = (f"Group chat: {group['name']}\nUser message: {message}\n\n"
                           f"Read the shared chat: {command} read {shlex.quote(room)}\n"
                           f"Post a reply: {command} post {shlex.quote(room)} 'your reply'\n"
                           "Use your CODEX_BOARD_OWNER for authorship. Read peer replies before coordination decisions. "
                           "Posts are shared, but do not automatically start peer turns.")
            for member in deliveries:
                try:
                    target = self.thread(member)
                    if not target["canSend"]:
                        raise ValueError("No live mailbox for this agent. The message remains in the shared chat.")
                    if target.get("source") == "managed":
                        self.runtime.send(member, message, key + ":" + member)
                        deliveries[member] = "queued"
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
            return row


def make_server(canvas, port=0):
    token = secrets.token_urlsafe(32)
    terminal_manager = [None]
    cost_reader = [None]
    terminal_lock = threading.RLock()

    def terminals():
        if not canvas.runtime:
            raise ValueError("The agent runtime is unavailable")
        with terminal_lock:
            if terminal_manager[0] is None:
                from codex_terminals import TerminalManager

                terminal_manager[0] = TerminalManager(canvas.root)
            return terminal_manager[0]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def send(self, value, status=200, content_type="application/json"):
            data = value if isinstance(value, bytes) else json.dumps(value, ensure_ascii=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header(
                "Content-Security-Policy",
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; frame-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'",
            )
            self.end_headers()
            self.wfile.write(data)

        def trusted(self, write=False):
            host = self.headers.get("Host")
            allowed = {f"{name}:{self.server.server_port}" for name in ("127.0.0.1", "localhost")}
            if host not in allowed:
                return False
            origin = f"http://{host}"
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
            revision, previous = -1, {}
            try:
                while not runtime.closed:
                    current, data = runtime.wait_transcript(key, revision)
                    if current is None:
                        break
                    if data is None:
                        self.wfile.write(b": heartbeat\n\n")
                    else:
                        records = {item["id"]: item for item in data.pop("items")}
                        payload = {**data, "replace": revision == -1, "order": list(records),
                                   "items": [item for key, item in records.items() if previous.get(key) != item]}
                        self.wfile.write(("data: " + json.dumps(payload, ensure_ascii=False) + "\n\n").encode())
                        revision, previous = current, records
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

        def do_GET(self):
            if not self.trusted():
                return self.send({"error": "Local origin required"}, 403)
            path = urlparse(self.path)
            try:
                if path.path == "/api/state":
                    return self.send({**canvas.snapshot(), "token": token,
                                      "runtime": canvas.runtime.snapshot() if canvas.runtime else None})
                if path.path == "/api/costs":
                    with terminal_lock:
                        if cost_reader[0] is None:
                            from codex_costs import CostReader

                            cost_reader[0] = CostReader(canvas.root)
                    return self.send(cost_reader[0].snapshot())
                if path.path == "/api/desktop":
                    return self.send(
                        {
                            "application": "codex-agents",
                            "protocol": 1,
                            "pid": os.getpid(),
                            "stateDir": str(Path(canvas.root).resolve()),
                        }
                    )
                if path.path == "/api/terminals":
                    return self.send(terminals().listing())
                if path.path == "/api/terminals/output":
                    query = parse_qs(path.query)
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
                    if path.path == "/api/accounts":
                        return self.send(runtime.accounts.snapshot())
                    if path.path == "/api/workspace":
                        return self.send(runtime.workspace_snapshot(agent))
                    if path.path == "/api/work":
                        return self.send(runtime.work_action(agent, {"action": "list"}))
                    if path.path == "/api/queue":
                        return self.send(runtime.queue_action(agent))
                    if path.path == "/api/changes":
                        return self.send(runtime.changes(agent))
                    if path.path == "/api/plan":
                        return self.send(runtime.plan_action(agent))
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
                            }
                        )
                    if path.path == "/api/capabilities":
                        return self.send(runtime.capabilities(agent))
                    if path.path == "/api/user-tasks":
                        return self.send(runtime.user_tasks(agent))
                    if path.path == "/api/profiles":
                        return self.send(runtime.profiles())
                    if path.path == "/api/rules":
                        return self.send(runtime.rules())
                    if path.path == "/api/resources":
                        return self.send(runtime.resource_action())
                    if path.path == "/api/monitor/log":
                        return self.send(runtime.monitor_log(q.get("id")))
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
                    return self.send(canvas.runtime.limits(parse_qs(path.query).get("account_key", ["default"])[0]))
                if path.path == "/api/task" and canvas.runtime:
                    return self.send(canvas.runtime.task_detail(parse_qs(path.query).get("id", [""])[0]))
                if path.path == "/api/complaint" and canvas.runtime:
                    return self.send(canvas.runtime.complaint_detail(parse_qs(path.query).get("id", [""])[0]))
                if path.path == "/api/agent-chat" and canvas.runtime:
                    query = parse_qs(path.query)
                    before = int(query["before"][0]) if query.get("before") else None
                    return self.send(canvas.runtime.chat_read(query.get("room", [""])[0], before=before))
                if path.path == "/api/models" and canvas.runtime:
                    return self.send(canvas.runtime.catalog(parse_qs(path.query).get("account_key", ["default"])[0]))
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
                if asset.is_relative_to(WEB.resolve()) and asset.is_file() and (relative == "index.html" or relative.startswith("assets/")):
                    mime = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8"}.get(asset.suffix, "application/octet-stream")
                    return self.send(asset.read_bytes(), content_type=mime)
                if path.path == "/":
                    return self.send({"error": "Build the interface: cd web && npm ci && npm run build"}, 503)
                return self.send({"error": "Not found"}, 404)
            except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
                return self.send({"error": str(error)}, 400)

        def do_POST(self):
            if not self.trusted(write=True):
                return self.send({"error": "Local origin and session token required"}, 403)
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if (
                    not 0
                    < length
                    <= (28 * 1024 * 1024 if self.path == "/api/assets" else 262144)
                ):
                    return self.send({"error": "Invalid request size"}, 413)
                if self.headers.get_content_type() != "application/json":
                    return self.send({"error": "JSON required"}, 415)
                self.connection.settimeout(10)
                body = json.loads(self.rfile.read(length))
                if not isinstance(body, dict):
                    raise ValueError("JSON object required")
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
                    if self.path == "/api/accounts/discover":
                        return self.send(runtime.accounts.discover())
                    if self.path == "/api/accounts/register":
                        runtime.accounts.register(body.get("home"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/default":
                        runtime.accounts.default(body.get("account_key"))
                        return self.send(runtime.accounts.snapshot())
                    if self.path == "/api/accounts/login":
                        return self.send(runtime.accounts.start_login(runtime, body.get("request_id")))
                    if self.path == "/api/agents/account":
                        return self.send(runtime.set_account(body.get("id"), body.get("account_key")))
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
                        return self.send(
                            runtime.checkpoint_capture(
                                agent, body.get("label", "Checkpoint")
                            )
                        )
                    if self.path == "/api/checkpoint/preview":
                        return self.send(
                            runtime.checkpoint_preview(agent, body.get("checkpoint"))
                        )
                    if self.path == "/api/checkpoint/restore":
                        return self.send(runtime.restore_checkpoint(agent, body))
                    if self.path == "/api/user-tasks/complete":
                        return self.send(runtime.complete_user_task(body))
                    if self.path == "/api/profiles":
                        return self.send(runtime.profiles(body))
                    if self.path == "/api/rules":
                        return self.send(runtime.rules(body))
                    if self.path == "/api/resources":
                        return self.send(runtime.resource_action(body))
                    if self.path == "/api/monitor/input":
                        return self.send(runtime.monitor_input(body.get("id"), body))
                    if self.path == "/api/native-command":
                        return self.send(runtime.native_command_action(body))
                    if self.path == "/api/messages":
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
                        return self.send(canvas.runtime.complaint(body.get("lead"), {"action": "submit", "text": body.get("text")}, "user:" + key, user=True))
                    if self.path == "/api/conversation/delete":
                        return self.send(canvas.runtime.delete_conversation(body.get("id")))
                    if self.path == "/api/leads":
                        return self.send(canvas.runtime.new_lead(body))
                    if self.path == "/api/conversation":
                        return self.send(canvas.runtime.conversation_settings(body.get("id"), body))
                    if self.path == "/api/agents":
                        return self.send(
                            canvas.runtime.create(body, parent=body.get("parent"))
                        )
                    if self.path == "/api/configure":
                        return self.send(canvas.runtime.configure(body.get("id"), body))
                    if self.path == "/api/action":
                        return self.send(canvas.runtime.native_action(body.get("id"), body.get("action")))
                    if self.path == "/api/import":
                        return self.send(canvas.runtime.import_thread(body))
                    if self.path == "/api/stop":
                        return self.send(canvas.runtime.stop(body.get("id"), body.get("descendants", True)))
                    if self.path == "/api/monitor/cancel":
                        return self.send(canvas.runtime.cancel_monitor(body.get("id")))
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
        def server_close(self):
            if cost_reader[0] is not None:
                cost_reader[0].close()
            if terminal_manager[0] is not None:
                terminal_manager[0].close()
            super().server_close()

    server = LocalServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    return server


def main():
    parser = argparse.ArgumentParser(description="Local canvas for Codex app-server waves")
    parser.add_argument("--port", type=int, default=4620)
    args = parser.parse_args()
    def terminate(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    runtime = None
    server = None
    try:
        from codex_runtime import Runtime
        canvas = Canvas()
        # Bind first so a port collision cannot disturb an existing runtime.
        server = make_server(canvas, args.port)
        runtime = Runtime(canvas.root)
        canvas.runtime = runtime
        print(f"Codex Canvas: http://127.0.0.1:{server.server_port}", flush=True)
        server.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    except (RuntimeError, OSError) as error:
        parser.exit(1, f"codex-canvas: {error}\n")
    finally:
        if server:
            server.server_close()
        if runtime:
            runtime.close()
