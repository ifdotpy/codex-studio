"""User terminal sessions. These shells never start or wake a model turn."""

import base64
import codecs
from contextlib import contextmanager
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

HISTORY_LIMIT = 1024 * 1024


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
        with self.db() as db:
            db.execute(
                "CREATE TABLE IF NOT EXISTS user_terminals (id TEXT PRIMARY KEY, record TEXT NOT NULL, output TEXT NOT NULL, offset INTEGER NOT NULL)"
            )
            db.execute(
                "CREATE TABLE IF NOT EXISTS user_terminal_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL)"
            )
            for row in db.execute("SELECT id,record FROM user_terminals"):
                record = json.loads(row["record"])
                if record["status"] == "running":
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
            server = self.connect()
            shell = os.environ.get("SHELL", "/bin/zsh")
            if not os.path.isabs(shell) or not os.access(shell, os.X_OK):
                shell = "/bin/sh"
            pid_path = self.native_root / (key + ".pid")
            self.processes[key] = {"server": server, "connection": self.connection, "pid_path": pid_path,
                                   "decoder": codecs.getincrementaldecoder("utf-8")("replace")}
            db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (key, json.dumps(record), "", 0))
            self.save_receipt(db, data["id"], signature, record)
            db.commit()
            # Native handles belong to this dedicated connection, independent of
            # account switches. Restore the user's environment inside the shell.
            env = {name: os.environ.get(name) for name in ("CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY")}
            env.update(TERM="xterm-256color", COLORTERM="truecolor")
            try:
                params = {
                    "processHandle": key, "cwd": str(cwd), "tty": True,
                    "command": [sys.executable, "-B", str(Path(__file__).with_name("codex_terminal_child.py")), str(pid_path), shell],
                    "size": {"rows": rows, "cols": cols}, "env": env,
                    "timeoutMs": None, "outputBytesCap": None,
                }
                from codex_runtime import SubmissionUnknown
                try:
                    submitted = server.submit("process/spawn", params)
                except SubmissionUnknown as error:
                    submitted = error.submitted
                server.on_result(submitted, lambda future: self.spawn_result(key, data["id"], future))
                server.wait(submitted, timeout=10)
            except Exception as error:
                from codex_runtime import NativeRpcError, SubmissionRejected
                rejected = isinstance(error, (NativeRpcError, SubmissionRejected))
                record.update(error=str(error), status="exited" if rejected else "running")
                db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
                db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?", (json.dumps(record), data["id"]))
                if rejected:
                    self.processes.pop(key, None)
            return record

    def spawn_result(self, key, request_id, future):
        from codex_runtime import NativeRpcError
        failure = None
        try:
            future.result()
        except NativeRpcError as error:
            failure = str(error)
        except Exception:
            return  # Disconnect cleanup preserves an unknown outcome.
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM user_terminals WHERE id=?", (key,)).fetchone()
            record = json.loads(row[0])
            if record["status"] == "running":
                record.pop("error", None)
                if failure:
                    record.update(status="exited", error=failure, updated=time.time())
                    self.processes.pop(key, None)
                db.execute("UPDATE user_terminals SET record=? WHERE id=?", (json.dumps(record), key))
            db.execute("UPDATE user_terminal_receipts SET result=? WHERE id=?", (json.dumps(record), request_id))

    def connect(self):
        from codex_runtime import AppServer
        if self.server is not None:
            if not (self.server.closed or self.server.proc.poll() is not None or self.server.transport_error):
                return self.server
            if self.processes:
                raise ValueError("The terminal connection closed. Wait for process cleanup, then create a new terminal.")
            self.server.close()
            self.close_streams(self.server)
        home = self.native_root / "home"
        home.mkdir(mode=0o700, exist_ok=True)
        (home / "config.toml").write_text(
            '[features]\nplugins = false\nremote_plugin = false\napps = false\nskip_host_skill_discovery = true\n')
        connection = self.connection = object()
        self.server = AppServer(self.native_root, self.notification, lambda _: None,
                                lambda: self.disconnected(connection), home=home, isolated=True)
        return self.server

    def notification(self, message):
        method, params = message.get("method"), message.get("params", {})
        key = params.get("processHandle")
        with self.lock:
            owned = self.processes.get(key)
            if owned is None:
                return
            if method == "process/outputDelta":
                self.append(key, owned["decoder"].decode(base64.b64decode(params["deltaBase64"], validate=True)))
                return
        if method == "process/exited":
            # Codex kills one process group. Interactive jobs use other groups.
            self.terminate(owned)
            self.finish(key, params["exitCode"])

    def finish(self, key, code=None, error=None):
        with self.lock, self.db() as db:
            owned = self.processes.get(key)
            if owned is None:
                return
            self.append(key, owned["decoder"].decode(b"", final=True))
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

    def disconnected(self, connection=None):
        with self.lock:
            active = [(key, owned) for key, owned in self.processes.items()
                      if connection is None or owned["connection"] is connection]
        for key, owned in active:
            self.terminate(owned)
            self.finish(key, error="The terminal connection closed. The exit code is unknown.")

    def append(self, key, text):
        if not text:
            return
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT output,offset FROM user_terminals WHERE id=?", (key,)
            ).fetchone()
            value = row["output"] + text
            dropped = max(0, len(value) - HISTORY_LIMIT)
            db.execute(
                "UPDATE user_terminals SET output=?,offset=? WHERE id=?",
                (value[dropped:], row["offset"] + dropped, key),
            )

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
        if action == "input":
            server = owned["server"]
            from codex_runtime import SubmissionUnknown
            try:
                submitted = server.submit("process/writeStdin", {
                    "processHandle": key, "deltaBase64": base64.b64encode(text.encode()).decode(),
                })
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
        elif action == "close" and owned:
            self.terminate(owned)
            # The helper may not have written its PID yet. Native kill also
            # handles that startup interval, without a second command spawn.
            from codex_runtime import NativeRpcError
            try:
                owned["server"].call("process/kill", {"processHandle": key}, timeout=3)
            except NativeRpcError as error:
                if not any(text in str(error) for text in ("no active process for process handle", "is no longer running")):
                    raise
        return record

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
        for owned in active:
            self.terminate(owned)
        if self.server:
            self.server.close()
            if not self.server.join_callbacks():
                raise RuntimeError("Terminal callbacks did not drain")
            self.close_streams(self.server)
        self.disconnected()

    @staticmethod
    def close_streams(server):
        for stream in (server.proc.stdin, server.proc.stdout, server.proc.stderr):
            if stream is not None:
                stream.close()
