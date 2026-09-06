"""User terminal sessions. These shells never start or wake a model turn."""

import codecs
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import termios
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
            master, slave = os.openpty()
            os.set_blocking(master, False)
            process = None
            try:
                fcntl.ioctl(
                    slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
                )
                shell = os.environ.get("SHELL", "/bin/zsh")
                if not os.path.isabs(shell) or not os.access(shell, os.X_OK):
                    shell = "/bin/sh"
                env = {**os.environ, "TERM": "xterm-256color", "COLORTERM": "truecolor"}
                process = subprocess.Popen(
                    [
                        sys.executable,
                        "-B",
                        str(Path(__file__).with_name("codex_terminal_child.py")),
                        shell,
                    ],
                    cwd=cwd,
                    env=env,
                    stdin=slave,
                    stdout=slave,
                    stderr=slave,
                    start_new_session=True,
                    close_fds=True,
                )
                os.close(slave)
                slave = -1
                db.execute(
                    "INSERT INTO user_terminals VALUES (?,?,?,?)",
                    (key, json.dumps(record), "", 0),
                )
                self.save_receipt(db, data["id"], signature, record)
                db.commit()
                self.processes[key] = (process, master)
                threading.Thread(
                    target=self.read, args=(key, process, master), daemon=True
                ).start()
                return record
            except BaseException:
                if process:
                    process.kill()
                    process.wait(timeout=5)
                os.close(master)
                if slave >= 0:
                    os.close(slave)
                raise

    def read(self, key, process, master):
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        try:
            while True:
                ready, _, _ = select.select([master], [], [], 0.25)
                if ready:
                    try:
                        raw = os.read(master, 65536)
                    except OSError:
                        break
                    if not raw:
                        break
                    self.append(key, decoder.decode(raw))
                elif process.poll() is not None:
                    break
            self.append(key, decoder.decode(b"", final=True))
        finally:
            # A shell can exit while its background jobs still own the PTY.
            self.terminate(process)
            code = process.wait(timeout=3)
            with self.lock, self.db() as db:
                row = db.execute(
                    "SELECT record FROM user_terminals WHERE id=?", (key,)
                ).fetchone()
                record = json.loads(row[0])
                if record["status"] != "closed":
                    record.update(status="exited")
                record.update(exitCode=code, updated=time.time())
                db.execute(
                    "UPDATE user_terminals SET record=? WHERE id=?",
                    (json.dumps(record), key),
                )
                self.processes.pop(key, None)
                try:
                    os.close(master)
                except OSError:
                    pass

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
        process_to_close = None
        input_write = None
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM user_terminals WHERE id=?", (key,)
            ).fetchone()
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
                if owned:
                    process_to_close = owned[0]
            elif action in {"input", "resize"}:
                if action == "input":
                    text = data.get("text")
                    if not isinstance(text, str) or len(text.encode()) > 65536:
                        raise ValueError("Terminal input is limited to 64 KiB")
                    signature, prior = self.receipt(
                        db, data.get("request_id"), {"action": action, **data}
                    )
                    if prior is not None:
                        return prior
                if (
                    record["status"] != "running"
                    or not owned
                    or owned[0].poll() is not None
                ):
                    raise ValueError("This terminal is no longer running")
                if action == "resize":
                    cols, rows = self.dimensions(data)
                    fcntl.ioctl(
                        owned[1],
                        termios.TIOCSWINSZ,
                        struct.pack("HHHH", rows, cols, 0, 0),
                    )
                else:
                    # Persist intent before an irreversible write. A response loss
                    # must never cause the same keystrokes to execute twice.
                    result = {
                        "ok": False,
                        "delivery": "uncertain",
                        "error": "Input delivery is uncertain. Check the terminal before sending it again.",
                    }
                    self.save_receipt(db, data["request_id"], signature, result)
                    db.commit()
                    input_write = (os.dup(owned[1]), text.encode())
            else:
                raise ValueError("Unknown terminal action")
            db.execute(
                "UPDATE user_terminals SET record=? WHERE id=?",
                (json.dumps(record), key),
            )
        if input_write:
            descriptor, raw = input_write
            sent = 0
            deadline = time.monotonic() + 3
            try:
                while sent < len(raw):
                    if time.monotonic() >= deadline:
                        raise RuntimeError(
                            "Input delivery is uncertain. Check the terminal before sending it again."
                        )
                    _, ready, _ = select.select([], [descriptor], [], 0.1)
                    if not ready:
                        continue
                    try:
                        sent += os.write(descriptor, raw[sent:])
                    except BlockingIOError:
                        continue
            finally:
                os.close(descriptor)
            result = {"ok": True, "delivery": "sent"}
            with self.lock, self.db() as db:
                db.execute(
                    "UPDATE user_terminal_receipts SET result=? WHERE id=?",
                    (json.dumps(result), data["request_id"]),
                )
            return result
        if process_to_close:
            self.terminate(process_to_close)
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

    def terminate(self, process):
        # The reader and a close request can both reach this method. Retain the
        # PTY until all groups in its original session have received cleanup.
        with self.termination_lock:
            self.signal_session(process.pid, signal.SIGHUP)
            deadline = time.monotonic() + 1
            while self.session_groups(process.pid):
                if time.monotonic() >= deadline:
                    self.signal_session(process.pid, signal.SIGKILL)
                    break
                time.sleep(0.05)
            process.poll()

    def close(self):
        with self.lock:
            self.closed = True
            active = [process for process, _ in self.processes.values()]
        for process in active:
            self.terminate(process)
