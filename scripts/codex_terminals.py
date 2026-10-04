"""User terminal sessions. These shells never start or wake a model turn."""

import base64
import codecs
from contextlib import contextmanager, nullcontext
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import uuid

from studio_api.sync.resources.models import ResourceRef, TerminalResource, TerminalsResource

HISTORY_LIMIT = 1024 * 1024
ARCHIVE_CHUNK = 65536


class TerminalManager:
    def __init__(self, root):
        self.root = Path(root)
        self.db_path = self.root / "canvas.sqlite3"
        self.lock = threading.RLock()
        self.processes = {}
        self.termination_lock = threading.RLock()
        self.closed = False
        self.server = None
        self.connection = None
        self.native_root = self.root / "terminal-server"
        self.native_root.mkdir(mode=0o700, exist_ok=True)
        self.supervisor_mode = os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1"
        restarted: list[str] = []
        with self.db() as db:
            db.execute(
                "CREATE TABLE IF NOT EXISTS user_terminals (id TEXT PRIMARY KEY, record TEXT NOT NULL, output TEXT NOT NULL, offset INTEGER NOT NULL)"
            )
            db.execute(
                "CREATE TABLE IF NOT EXISTS user_terminal_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL)"
            )
            db.execute(
                "CREATE TABLE IF NOT EXISTS user_terminal_output "
                "(terminal TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, "
                "text TEXT NOT NULL, PRIMARY KEY (terminal,start))"
            )
            db.execute("CREATE INDEX IF NOT EXISTS user_terminal_output_end "
                       "ON user_terminal_output (terminal,end)")
            db.execute("CREATE TABLE IF NOT EXISTS user_terminal_event_cursor "
                       "(terminal TEXT PRIMARY KEY, sequence INTEGER NOT NULL)")
            db.execute("CREATE TABLE IF NOT EXISTS user_terminal_decoder_state "
                       "(terminal TEXT PRIMARY KEY, pending BLOB NOT NULL)")
            for row in db.execute("SELECT id,record FROM user_terminals"):
                record = json.loads(row["record"])
                if record["status"] == "running" and not self.supervisor_mode:
                    record.update(
                        status="exited",
                        error="The server restarted. This shell cannot be resumed.",
                        updated=time.time(),
                        exitCode=None,
                    )
                    db.execute(
                        "UPDATE user_terminals SET record=? WHERE id=?",
                        (json.dumps(record), row["id"]),
                    )
                    restarted.append(row["id"])
                elif record["status"] == "running":
                    decoder = codecs.getincrementaldecoder("utf-8")("replace")
                    pending = db.execute("SELECT pending FROM user_terminal_decoder_state WHERE terminal=?",
                                         (row["id"],)).fetchone()
                    if pending:
                        decoder.setstate((bytes(pending[0]), 0))
                    self.processes[row["id"]] = {
                        "server": None, "connection": None,
                        "pid_path": self.native_root / (row["id"] + ".pid"),
                        "decoder": decoder,
                        "eventSequence": 0,
                        "output_lock": threading.RLock(),
                    }
        if restarted:
            self._publish(
                *(ResourceRef(TerminalResource(kind="terminal", terminalId=key)) for key in restarted),
                ResourceRef(TerminalsResource(kind="terminals")),
            )

    def _publish(self, *resources: ResourceRef) -> None:
        """Publish committed resource invalidations when the sync hub is present."""
        try:
            from studio_api.sync.resources.hub import publish_resources
        except ModuleNotFoundError as error:
            if error.name != "studio_api.sync.resources.hub":
                raise
            return
        publish_resources(self.root, *resources)

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.db_path, timeout=10)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def receipt(self, db, key, body):
        if not isinstance(key, str) or not 1 <= len(key) <= 200:
            raise ValueError("A request id is required")
        signature = hashlib.sha256(
            json.dumps(body, sort_keys=True).encode()
        ).hexdigest()
        row = db.execute(
            "SELECT * FROM user_terminal_receipts WHERE id=?", (key,)
        ).fetchone()
        if row and row["signature"] != signature:
            raise ValueError("This request id has different content")
        return signature, json.loads(row["result"]) if row else None

    def save_receipt(self, db, key, signature, result):
        db.execute(
            "INSERT INTO user_terminal_receipts VALUES (?,?,?)",
            (key, signature, json.dumps(result)),
        )
        return result

    def listing(self):
        with self.lock, self.db() as db:
            return {
                "items": [
                    json.loads(r[0])
                    for r in db.execute(
                        "SELECT record FROM user_terminals ORDER BY json_extract(record,'$.created') DESC"
                    )
                    if json.loads(r[0])["status"] != "closed"
                ]
            }

    @staticmethod
    def dimensions(data):
        cols, rows = data.get("cols", 100), data.get("rows", 28)
        if any(
            isinstance(v, bool) or not isinstance(v, int) for v in (cols, rows)
        ) or not (2 <= cols <= 1000 and 1 <= rows <= 500):
            raise ValueError("Invalid terminal size")
        return cols, rows

    def create(self, runtime, data):
        cols, rows = self.dimensions(data)
        with runtime.lock, runtime.db() as db:
            agent = runtime.checked_actor(db, data.get("agent"))
            cwd = Path(agent["cwd"]).resolve(strict=True)
            if not cwd.is_dir():
                raise ValueError("The project directory is unavailable")
        with self.lock, self.db() as db:
            if self.closed:
                raise ValueError("The terminal server is closing")
            signature, prior = self.receipt(db, data.get("id"), data)
            if prior is not None:
                return prior
        # Connection setup and native I/O must not hold the output lock.
        server = self.connect()
        with self.lock, self.db() as db:
            if self.closed:
                raise ValueError("The terminal server is closing")
            signature, prior = self.receipt(db, data.get("id"), data)
            if prior is not None:
                return prior
            if server is not self.server or server.closed or server.transport_error or server.proc.poll() is not None:
                raise ValueError("The terminal connection closed before creation")
            key = str(uuid.uuid4())
            now = time.time()
            record = {
                "id": key,
                "agent": agent["id"],
                "title": "Terminal",
                "cwd": str(cwd),
                "status": "running",
                "created": now,
                "updated": now,
                "exitCode": None,
            }
            shell = os.environ.get("SHELL", "/bin/zsh")
            if not os.path.isabs(shell) or not os.access(shell, os.X_OK):
                shell = "/bin/sh"
            pid_path = self.native_root / (key + ".pid")
            owned = self.processes[key] = {
                "server": server, "connection": self.connection, "pid_path": pid_path,
                "decoder": codecs.getincrementaldecoder("utf-8")("replace"),
                "spawn_lock": threading.RLock(),
                "output_lock": threading.RLock(),
            }
            db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (key, json.dumps(record), "", 0))
            self.save_receipt(db, data["id"], signature, record)
        self._publish(ResourceRef(TerminalsResource(kind="terminals")))
        # Native handles belong to this dedicated connection, independent of
        # account switches. Restore the user's environment inside the shell.
        env = {name: os.environ.get(name) for name in ("CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY")}
        env.update(TERM="xterm-256color", COLORTERM="truecolor")
        params = {
            "processHandle": key, "cwd": str(cwd), "tty": True,
            "command": [sys.executable, "-B", str(Path(__file__).with_name("codex_terminal_child.py")), str(pid_path), shell],
            "size": {"rows": rows, "cols": cols}, "env": env,
            "timeoutMs": None, "outputBytesCap": None,
        }
        submitted = None
        try:
            from codex_runtime import SubmissionUnknown
            # Close can mark this terminal closed while submission is pending.
            # Its kill waits only for this terminal's write, not its receipt.
            early_result = None
            changed = False
            with owned["spawn_lock"]:
                with self.lock, self.db() as db:
                    row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
                    current = json.loads(row[0])
                    if self.closed or current["status"] != "running" or self.processes.get(key) is not owned:
                        if current["status"] == "running":
                            current.update(status="exited", error="The terminal server is closing", updated=time.time())
                            self.processes.pop(key, None)
                            db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(current), key))
                            changed = True
                        db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?", (json.dumps(current), data["id"]))
                        early_result = current
                if early_result is None:
                    try:
                        submitted = server.submit("process/spawn", params,
                                                  operation_id="terminal-spawn:" + key)
                    except SubmissionUnknown as error:
                        submitted = error.submitted
            if early_result is not None:
                if changed:
                    self._publish(
                        ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
                        ResourceRef(TerminalsResource(kind="terminals")),
                    )
                return early_result
            server.on_result(submitted, lambda future: self.spawn_result(key, data["id"], future, owned))
            server.wait(submitted, timeout=10)
        except Exception as error:
            from codex_runtime import NativeRpcError, SubmissionRejected
            rejected = isinstance(error, (NativeRpcError, SubmissionRejected))
            changed = False
            with self.lock, self.db() as db:
                row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
                record = json.loads(row[0])
                if record["status"] == "running" and self.processes.get(key) is owned:
                    record.update(error=str(error), status="exited" if rejected else "running")
                    db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
                    changed = True
                    if rejected:
                        self.processes.pop(key, None)
                db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?", (json.dumps(record), data["id"]))
            if changed:
                self._publish(
                    ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
                    ResourceRef(TerminalsResource(kind="terminals")),
                )
            # A response can arrive between a timeout and its database update.
            # Reconcile that exact future after the update without resubmission.
            if submitted is not None and submitted[2].done():
                self.spawn_result(key, data["id"], submitted[2], owned)
        with self.lock, self.db() as db:
            return json.loads(db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()[0])

    def spawn_result(self, key, request_id, future, owned=None):
        from codex_runtime import NativeRpcError
        failure = None
        try:
            future.result()
        except NativeRpcError as error:
            failure = str(error)
        except Exception:
            return  # Disconnect cleanup preserves an unknown outcome.
        changed = False
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
            if not row:
                return
            record = json.loads(row[0])
            current = self.processes.get(key)
            if owned is not None and current is not owned and (current is not None or record["status"] == "running"):
                return
            stop = current if record["status"] == "closed" and not failure else None
            if record["status"] == "running":
                previous = record.copy()
                record.pop("error", None)
                if failure:
                    record.update(status="exited", error=failure, updated=time.time())
                    self.processes.pop(key, None)
                changed = record != previous
                db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
            db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?", (json.dumps(record), request_id))
        if changed:
            self._publish(
                ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
                ResourceRef(TerminalsResource(kind="terminals")),
            )
        if stop:
            self.stop_process(key, stop)

    def start_guard(self):
        # Existing managers receive this field lazily after a live update.
        with self.lock:
            guard = getattr(self, "_start_lock", None)
            if guard is None:
                guard = self._start_lock = threading.RLock()
            return guard

    def connect(self):
        from codex_runtime import AppServer
        with self.start_guard():
            with self.lock:
                if self.closed:
                    raise ValueError("The terminal server is closing")
                previous = self.server
                if previous is not None:
                    if not (previous.closed or previous.proc.poll() is not None or previous.transport_error):
                        return previous
                    if self.processes:
                        raise ValueError("The terminal connection closed. Wait for process cleanup, then create a new terminal.")
            if previous is not None:
                previous.close()
                self.close_streams(previous)
            home = self.native_root / "home"
            home.mkdir(mode=0o700, exist_ok=True)
            (home / "config.toml").write_text(
                '[features]\nplugins = false\nremote_plugin = false\napps = false\nskip_host_skill_discovery = true\n')
            with self.lock:
                if self.closed:
                    raise ValueError("The terminal server is closing")
                connection = self.connection = object()
                for owned in self.processes.values():
                    owned["connection"] = connection
            server = AppServer(self.native_root, self.notification, lambda _: None,
                               lambda: self.disconnected(connection), home=home, isolated=True,
                               supervisor_handle="terminals", supervisor_root=self.root)
            with self.lock:
                closed = self.closed
                if not closed:
                    self.server = server
                    for owned in self.processes.values():
                        if owned["connection"] is connection:
                            owned["server"] = server
            if closed:
                server.close()
                if not server.join_callbacks():
                    raise RuntimeError("Terminal callbacks did not drain")
                self.close_streams(server)
                raise ValueError("The terminal server is closing")
            return server

    def notification(self, message):
        method, params = message.get("method"), message.get("params", {})
        key = params.get("processHandle")
        with self.lock:
            owned = self.processes.get(key)
        if owned is None:
            return
        if method == "process/outputDelta":
            sequence = message.get("_studioSupervisorSequence")
            self.output_delta(key, owned, params["deltaBase64"], sequence)
            return
        if method == "process/exited":
            # Codex kills one process group. Interactive jobs use other groups.
            self.terminate(owned)
            self.finish(key, params["exitCode"])

    def finish(self, key, code=None, error=None):
        with self.lock:
            owned = self.processes.get(key)
        if owned is None:
            return
        with owned.setdefault("output_lock", threading.RLock()):
            with self.lock:
                if self.processes.get(key) is not owned:
                    return
                final_text = owned["decoder"].decode(b"", final=True)
            if final_text:
                self._append_record(key, final_text)
            with self.lock, self.db() as db:
                row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
                record = json.loads(row[0])
                if record["status"] != "closed":
                    record["status"] = "exited"
                record.update(exitCode=code, updated=time.time())
                record.pop("error", None)
                if error:
                    record["error"] = error
                db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
                self.processes.pop(key)
                owned["pid_path"].unlink(missing_ok=True)
                owned["pid_path"].with_suffix(".tmp").unlink(missing_ok=True)
        self._publish(
            ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
            ResourceRef(TerminalsResource(kind="terminals")),
        )

    def disconnected(self, connection=None):
        with self.lock:
            active = [(key, owned) for key, owned in self.processes.items()
                      if connection is None or owned["connection"] is connection]
        for key, owned in active:
            self.terminate(owned)
            self.finish(key, error="The terminal connection closed. The exit code is unknown.")

    def append(self, key, text, *, supervisor_sequence=None, decoder_pending=None):
        if not text and supervisor_sequence is None and decoder_pending is None:
            return
        with self.lock:
            owned = self.processes.get(key)
        guard = owned.setdefault("output_lock", threading.RLock()) if owned else nullcontext()
        with guard:
            changed = self._append_record(
                key, text, supervisor_sequence=supervisor_sequence, decoder_pending=decoder_pending
            )
        if changed:
            self._publish(ResourceRef(TerminalResource(kind="terminal", terminalId=key)))

    def output_delta(self, key, owned, delta_base64, supervisor_sequence=None):
        with owned.setdefault("output_lock", threading.RLock()):
            with self.lock:
                if self.processes.get(key) is not owned:
                    return
                decoder = owned["decoder"]
                text = decoder.decode(base64.b64decode(delta_base64, validate=True))
                pending = decoder.getstate()[0]
            changed = self._append_record(
                key, text, supervisor_sequence=supervisor_sequence, decoder_pending=pending
            )
        if changed:
            self._publish(ResourceRef(TerminalResource(kind="terminal", terminalId=key)))

    def _append_record(self, key, text, *, supervisor_sequence=None, decoder_pending=None):
        """Commit one output delta and report whether visible output changed."""
        with self.lock, self.db() as db:
            if supervisor_sequence is not None:
                row = db.execute("SELECT sequence FROM user_terminal_event_cursor WHERE terminal=?", (key,)).fetchone()
                if row and supervisor_sequence <= row[0]:
                    return False
            if not text:
                if decoder_pending is not None:
                    db.execute("INSERT INTO user_terminal_decoder_state VALUES (?,?) "
                               "ON CONFLICT(terminal) DO UPDATE SET pending=excluded.pending",
                               (key, sqlite3.Binary(decoder_pending)))
                if supervisor_sequence is not None:
                    db.execute("INSERT INTO user_terminal_event_cursor VALUES (?,?) "
                               "ON CONFLICT(terminal) DO UPDATE SET sequence=excluded.sequence",
                               (key, supervisor_sequence))
                return False
            row = db.execute(
                "SELECT output,offset FROM user_terminals WHERE id=?", (key,)
            ).fetchone()
            if decoder_pending is not None:
                db.execute("INSERT INTO user_terminal_decoder_state VALUES (?,?) "
                           "ON CONFLICT(terminal) DO UPDATE SET pending=excluded.pending",
                           (key, sqlite3.Binary(decoder_pending)))
            self.seed_archive(db, key, row)
            start = row["offset"] + len(row["output"])
            self.archive_text(db, key, start, text)
            value = row["output"] + text
            dropped = max(0, len(value) - HISTORY_LIMIT)
            db.execute(
                "UPDATE user_terminals SET output=?,offset=? WHERE id=?",
                (value[dropped:], row["offset"] + dropped, key),
            )
            if supervisor_sequence is not None:
                db.execute("INSERT INTO user_terminal_event_cursor VALUES (?,?) "
                           "ON CONFLICT(terminal) DO UPDATE SET sequence=excluded.sequence",
                           (key, supervisor_sequence))
        return True

    @staticmethod
    def archive_text(db, key, start, text):
        previous = db.execute("SELECT start,end,text FROM user_terminal_output "
                              "WHERE terminal=? ORDER BY start DESC LIMIT 1", (key,)).fetchone()
        if previous and previous["end"] == start and len(previous["text"]) < ARCHIVE_CHUNK:
            count = min(len(text), ARCHIVE_CHUNK - len(previous["text"]))
            db.execute("UPDATE user_terminal_output SET end=?,text=? WHERE terminal=? AND start=?",
                       (start + count, previous["text"] + text[:count], key, previous["start"]))
            start += count
            text = text[count:]
        for position in range(0, len(text), ARCHIVE_CHUNK):
            chunk = text[position:position + ARCHIVE_CHUNK]
            db.execute("INSERT INTO user_terminal_output VALUES (?,?,?,?)",
                       (key, start + position, start + position + len(chunk), chunk))

    def seed_archive(self, db, key, row):
        if row["output"] and not db.execute(
                "SELECT 1 FROM user_terminal_output WHERE terminal=? LIMIT 1", (key,)).fetchone():
            # Earlier releases retained only this suffix. Preserve its absolute
            # cursor; an unavailable prefix must remain visibly unavailable.
            self.archive_text(db, key, row["offset"], row["output"])

    def history_output(self, key, offset=0, limit=ARCHIVE_CHUNK):
        try:
            offset, limit = int(offset), int(limit)
        except (TypeError, ValueError):
            raise ValueError("Invalid terminal history cursor")
        if offset < 0 or not 1 <= limit <= HISTORY_LIMIT:
            raise ValueError("Invalid terminal history range")
        with self.lock, self.db() as db:
            row = db.execute("SELECT * FROM user_terminals WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown terminal")
            self.seed_archive(db, key, row)
            first = db.execute("SELECT start FROM user_terminal_output WHERE terminal=? "
                               "ORDER BY start LIMIT 1", (key,)).fetchone()
            start = first[0] if first else row["offset"]
            available = row["offset"] + len(row["output"])
            cursor = min(max(offset, start), available)
            end = min(cursor + limit, available)
            text = ''.join(chunk["text"][max(0, cursor - chunk["start"]):end - chunk["start"]]
                           for chunk in db.execute(
                               "SELECT start,text FROM user_terminal_output "
                               "WHERE terminal=? AND end>? AND start<? ORDER BY start LIMIT ?",
                               (key, cursor, end, limit // ARCHIVE_CHUNK + 2)))
            record = json.loads(row["record"])
            return {"text": text, "offset": cursor + len(text),
                    "availableOffset": available, "historyStart": start,
                    "hasMore": cursor + len(text) < available, "truncated": offset < start,
                    "status": record["status"], "exitCode": record.get("exitCode"),
                    "error": record.get("error")}

    def output(self, key, offset=0):
        try:
            offset = int(offset)
        except (TypeError, ValueError):
            raise ValueError("Invalid terminal cursor")
        if offset < 0:
            raise ValueError("Invalid terminal cursor")
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT * FROM user_terminals WHERE id=?", (key,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown terminal")
            record = json.loads(row["record"])
            start = row["offset"]
            end = start + len(row["output"])
            return {
                "text": row["output"][max(0, min(offset, end) - start) :],
                "offset": end,
                "truncated": offset < start,
                "status": record["status"],
                "exitCode": record.get("exitCode"),
                "error": record.get("error"),
            }

    def action(self, action, data):
        key = data.get("id")
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown terminal")
            record = json.loads(row[0])
            owned = self.processes.get(key)
            if action == "rename":
                title = data.get("title")
                if not isinstance(title, str) or not title.strip() or len(title) > 120:
                    raise ValueError("Use a terminal name of 1 to 120 characters")
                record.update(title=title.strip(), updated=time.time())
            elif action == "close":
                record.update(status="closed", updated=time.time())
            elif action in {"input", "resize"}:
                if action == "input":
                    text = data.get("text")
                    if not isinstance(text, str) or len(text.encode()) > 65536:
                        raise ValueError("Terminal input is limited to 64 KiB")
                    signature, prior = self.receipt(db, data.get("request_id"), {"action": action, **data})
                    if prior is not None:
                        return prior
                if record["status"] != "running" or not owned:
                    raise ValueError("This terminal is no longer running")
                if action == "resize":
                    cols, rows = self.dimensions(data)
                else:
                    # Persist before native submission. Timeout never authorizes
                    # replay. A late native response updates this same receipt.
                    self.save_receipt(db, data["request_id"], signature, {
                        "ok": False, "delivery": "uncertain",
                        "error": "Input delivery is uncertain. Check the terminal before sending it again.",
                    })
            else:
                raise ValueError("Unknown terminal action")
            db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
        if action == "rename":
            resources = [
                ResourceRef(TerminalsResource(kind="terminals")),
                ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
            ]
            self._publish(*resources)
        if action == "input":
            server = owned["server"]
            from codex_runtime import SubmissionUnknown
            try:
                submitted = server.submit("process/writeStdin", {
                    "processHandle": key, "deltaBase64": base64.b64encode(text.encode()).decode(),
                }, operation_id="terminal-input:" + data["request_id"])
            except SubmissionUnknown as error:
                submitted = error.submitted
            def delivered(future):
                try:
                    future.result()
                except Exception:
                    return
                with self.lock, self.db() as db:
                    db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?",
                               (json.dumps({"ok": True, "delivery": "sent"}), data["request_id"]))
            server.on_result(submitted, delivered)
            server.wait(submitted, timeout=3)
            delivered(submitted[2])
            return {"ok": True, "delivery": "sent"}
        if action == "resize":
            owned["server"].call("process/resizePty", {"processHandle": key, "size": {"rows": rows, "cols": cols}}, timeout=3)
        elif action == "close":
            try:
                if owned:
                    self.stop_process(key, owned)
            finally:
                self._publish(
                    ResourceRef(TerminalsResource(kind="terminals")),
                    ResourceRef(TerminalResource(kind="terminal", terminalId=key)),
                )
        return record

    def stop_process(self, key, owned):
        with self.lock:
            guard = owned.get("spawn_lock")
            if guard is None:
                guard = owned["spawn_lock"] = threading.RLock()
        with guard:
            self.terminate(owned)
            # The helper may not have written its PID yet. Native kill also
            # handles that startup interval, without a second command spawn.
            from codex_runtime import NativeRpcError
            server = owned["server"]
            if server is None:
                return
            try:
                server.call("process/kill", {"processHandle": key}, timeout=3)
            except NativeRpcError as error:
                if not any(text in str(error) for text in ("no active process for process handle", "is no longer running")):
                    raise

    @staticmethod
    def session_groups(session):
        # macOS ps does not expose the numeric POSIX session ID. Use getsid
        # against each live PID, never a command-name or parent-name match.
        rows = subprocess.check_output(
            ["/bin/ps", "-axo", "pid=,stat="], text=True, timeout=5
        )
        groups = {}
        for line in rows.splitlines():
            fields = line.split()
            if len(fields) != 2 or "Z" in fields[1]:
                continue
            pid = int(fields[0])
            try:
                if os.getsid(pid) == session:
                    group = os.getpgid(pid)
                    if group > 1:
                        groups[group] = pid
            except ProcessLookupError:
                continue
        return groups

    def signal_session(self, session, sig):
        groups = self.session_groups(session)
        # Signal jobs before the shell, which can exit immediately on SIGHUP.
        for group, member in sorted(
            groups.items(), key=lambda item: item[0] == session
        ):
            try:
                if os.getsid(member) == session and os.getpgid(member) == group:
                    os.killpg(group, sig)
            except ProcessLookupError:
                continue

    def terminate(self, owned):
        # Exit notifications and close requests can arrive together. Check only
        # groups in the session recorded by our native child bootstrap.
        with self.termination_lock:
            try:
                session = int(owned["pid_path"].read_text())
            except FileNotFoundError:
                return
            self.signal_session(session, signal.SIGHUP)
            deadline = time.monotonic() + 1
            while self.session_groups(session):
                if time.monotonic() >= deadline:
                    self.signal_session(session, signal.SIGKILL)
                    break
                time.sleep(0.05)

    def close(self):
        with self.lock:
            self.closed = True
            active = list(self.processes.values())
        # A concurrent constructor must finish or discard its transport before
        # shutdown closes the published connection. Neither step holds lock.
        with self.start_guard():
            if not self.supervisor_mode:
                for owned in active:
                    self.terminate(owned)
            if self.server:
                self.server.close()
                if not self.server.join_callbacks():
                    raise RuntimeError("Terminal callbacks did not drain")
                self.close_streams(self.server)
            if not self.supervisor_mode:
                self.disconnected()

    @staticmethod
    def close_streams(server):
        for stream in (server.proc.stdin, server.proc.stdout, server.proc.stderr):
            if stream is not None:
                stream.close()
