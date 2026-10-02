"""Own native Codex/Claude app-server processes across backend restarts.

The supervisor is opt-in. It is a small stdio relay with durable request and
output journals; the canvas database remains the source of work permissions.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import socketserver
import sqlite3
import subprocess
import sys
import threading
import time
import uuid

PROTOCOL = 1
MAX_STDERR_EVENT_BYTES = 256 * 1024
HANDLE_LIMIT = 256 * 1024 * 1024
SOCKET_TIMEOUT = 10


def _connect(path, timeout=SOCKET_TIMEOUT):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(timeout)
    try:
        client.connect(str(path))
    except Exception:
        client.close()
        raise
    return client


def _send(sock, value, lock=None):
    data = (json.dumps(value, separators=(",", ":"), ensure_ascii=False) + "\n").encode()
    if lock is None:
        sock.sendall(data)
    else:
        with lock:
            sock.sendall(data)


def _recv(sock, stream):
    while True:
        line = stream.readline()
        if not line:
            raise ConnectionError("Supervisor connection closed")
        try:
            return json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError("Supervisor sent an invalid frame") from error


class Journal:
    """Small per-state journal; FULL synchronous makes accepted writes durable."""

    def __init__(self, root):
        root = Path(root)
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if shutil.disk_usage(root).free < HANDLE_LIMIT + 16 * 1024 * 1024:
            raise OSError("Supervisor needs at least 272 MiB free before creating its bounded journal")
        self.path = root / "supervisor.sqlite3"
        self.lock = threading.RLock()
        with self.db() as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=FULL")
            db.execute(f"PRAGMA max_page_count={max(1, HANDLE_LIMIT // 4096)}")
            db.executescript("""
                CREATE TABLE IF NOT EXISTS handles(
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, pid INTEGER NOT NULL,
                    sequence INTEGER NOT NULL DEFAULT 0, acknowledged INTEGER NOT NULL DEFAULT 0,
                    rpc_sequence INTEGER NOT NULL DEFAULT 0, init_result TEXT, created REAL NOT NULL,
                    closed_at REAL, closed_reason TEXT, generation INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS operations(
                    handle TEXT NOT NULL, operation_id TEXT NOT NULL, digest TEXT NOT NULL,
                    native_id INTEGER, accepted REAL NOT NULL,
                    PRIMARY KEY(handle,operation_id));
                CREATE TABLE IF NOT EXISTS events(
                    handle TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL,
                    payload TEXT NOT NULL, size INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY(handle,sequence));
                CREATE TABLE IF NOT EXISTS child_identities(
                    handle TEXT PRIMARY KEY, pid INTEGER NOT NULL, pgid INTEGER NOT NULL,
                    start_time TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS recovery_events(
                    id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
                    handle TEXT NOT NULL, pid INTEGER NOT NULL, start_time TEXT NOT NULL,
                    outcome TEXT NOT NULL, detail TEXT);
                CREATE TABLE IF NOT EXISTS supervisor_state(
                    key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS degraded_handles(
                    handle TEXT PRIMARY KEY, prior_pid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, recovered_at REAL NOT NULL);
            """)
            columns = {row[1] for row in db.execute("PRAGMA table_info(handles)")}
            if "closed_at" not in columns:
                db.execute("ALTER TABLE handles ADD COLUMN closed_at REAL")
            if "closed_reason" not in columns:
                db.execute("ALTER TABLE handles ADD COLUMN closed_reason TEXT")
            if "generation" not in columns:
                db.execute("ALTER TABLE handles ADD COLUMN generation INTEGER NOT NULL DEFAULT 0")
                # Existing handles predate generation tracking. Their current
                # process is generation one, preserving the legacy initialized
                # operation identity when a backend reattaches to it.
                db.execute("UPDATE handles SET generation=1 WHERE pid>0")
            event_columns = {row[1] for row in db.execute("PRAGMA table_info(events)")}
            if "generation" not in event_columns:
                db.execute("ALTER TABLE events ADD COLUMN generation INTEGER NOT NULL DEFAULT 1")

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=5)
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA synchronous=FULL")
        db.execute("PRAGMA wal_autocheckpoint=100")
        db.execute(f"PRAGMA max_page_count={max(1, HANDLE_LIMIT // 4096)}")
        try:
            with db:
                yield db
        finally:
            db.close()

    def outstanding_bytes(self, db, handle):
        row = db.execute("SELECT COALESCE(sum(size),0) FROM events WHERE handle=?", (handle,)).fetchone()
        return int(row[0])

    def ensure_space(self, required=1024 * 1024):
        if shutil.disk_usage(self.path.parent).free < required:
            raise OSError("Insufficient free disk space for a durable supervisor receipt")


def process_start_time(pid):
    if type(pid) is not int or pid < 1:
        return None
    import sys
    if sys.platform == "darwin":
        import ctypes

        class ProcBsdInfo(ctypes.Structure):
            _fields_ = [
                ("flags", ctypes.c_uint32), ("status", ctypes.c_uint32),
                ("exit_status", ctypes.c_uint32), ("pid", ctypes.c_uint32),
                ("ppid", ctypes.c_uint32), ("uid", ctypes.c_uint32),
                ("gid", ctypes.c_uint32), ("ruid", ctypes.c_uint32),
                ("rgid", ctypes.c_uint32), ("svuid", ctypes.c_uint32),
                ("svgid", ctypes.c_uint32), ("reserved", ctypes.c_uint32),
                ("command", ctypes.c_char * 16), ("name", ctypes.c_char * 32),
                ("open_files", ctypes.c_uint32), ("pgid", ctypes.c_uint32),
                ("job_count", ctypes.c_uint32), ("tty_device", ctypes.c_uint32),
                ("tty_pgid", ctypes.c_uint32), ("nice", ctypes.c_int32),
                ("start_seconds", ctypes.c_uint64), ("start_microseconds", ctypes.c_uint64),
            ]

        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        library.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                        ctypes.c_void_p, ctypes.c_int]
        library.proc_pidinfo.restype = ctypes.c_int
        info = ProcBsdInfo()
        size = ctypes.sizeof(info)
        read = library.proc_pidinfo(pid, 3, 0, ctypes.byref(info), size)
        if read == 0 and ctypes.get_errno() == 3:  # ESRCH
            return None
        if read != size or info.pid != pid:
            error = ctypes.get_errno()
            raise RuntimeError(f"Cannot verify process identity for PID {pid}: libproc returned {error}")
        try:
            state = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "stat="],
                                            text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                return None
            raise RuntimeError(f"Cannot verify process state for PID {pid}") from error
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError(f"Cannot verify process state for PID {pid}") from error
        if info.status == 5 or state.startswith("Z"):  # SZOMB
            return None
        return f"{info.start_seconds}.{info.start_microseconds:06d}"
    try:
        value = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
        state = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "stat="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
    except subprocess.CalledProcessError as error:
        if error.returncode == 1:
            return None
        raise RuntimeError(f"Cannot verify process identity for PID {pid}") from error
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"Cannot verify process identity for PID {pid}") from error
    return value if value and not state.startswith("Z") else None


def process_start_matches(pid, expected, *, allow_legacy=False):
    actual = process_start_time(pid)
    if actual == expected:
        return True
    if not allow_legacy or actual is None:
        return False
    try:
        legacy = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
    except (OSError, subprocess.SubprocessError):
        return False
    return legacy == expected


class Child:
    def __init__(self, handle, process, signature):
        self.handle, self.process, self.signature = handle, process, signature
        self.lock = threading.RLock()
        self.output = threading.Condition(self.lock)
        self.stopping = threading.Event()
        self.paused = threading.Event()
        self.reader = threading.Thread(target=self.read_stdout, daemon=True,
                                       name="supervisor-stdout-" + handle[:8])
        self.reader.start()
        self.stderr = threading.Thread(target=self.read_stderr, daemon=True,
                                       name="supervisor-stderr-" + handle[:8])
        self.stderr.start()

    def append(self, kind, payload):
        raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
        while not self.stopping.is_set():
            try:
                self.process.supervisor.journal.ensure_space(len(raw.encode()) + 1024 * 1024)
            except OSError:
                self.paused.set()
                self.stopping.wait(.1)
                continue
            with self.lock, self.process.supervisor.journal.lock:
                with self.process.supervisor.journal.db() as db:
                    used = self.process.supervisor.journal.outstanding_bytes(db, self.handle)
                    if used + len(raw.encode()) <= HANDLE_LIMIT:
                        row = db.execute("SELECT sequence,generation FROM handles WHERE id=?", (self.handle,)).fetchone()
                        if not row:
                            return
                        sequence = row[0] + 1
                        db.execute("UPDATE handles SET sequence=? WHERE id=?", (sequence, self.handle))
                        db.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) "
                                   "VALUES (?,?,?,?,?,?)",
                                   (self.handle, sequence, kind, raw, len(raw.encode()), row[1]))
                        db.commit()
                        self.paused.clear()
                        self.output.notify_all()
                        return
                    self.paused.set()
            # Backpressure blocks the app-server pipe reader rather than dropping
            # output. The backend health endpoint reports this state separately.
            self.stopping.wait(.1)

    def read_stdout(self):
        try:
            for line in self.process.stdout:
                try:
                    value = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    value = {"supervisorRaw": line}
                if value.get("id") == 1 and "result" in value:
                    with self.process.supervisor.journal.db() as db:
                        db.execute("UPDATE handles SET init_result=? WHERE id=?",
                                   (json.dumps(value["result"]), self.handle))
                self.append("stdout", value)
        finally:
            self.append("exit", {"returnCode": self.process.poll()})

    def read_stderr(self):
        try:
            stream = getattr(self.process.stderr, "buffer", self.process.stderr)
            while True:
                data = stream.read1(65536)
                if not data:
                    return
                text = data.decode("utf-8", "replace") if isinstance(data, bytes) else data
                self.append("stderr", {"data": text})
        except (OSError, ValueError):
            return

    def write(self, operation_id, native_id, message):
        body = json.dumps(message, sort_keys=True, separators=(",", ":"))
        canonical = {key: value for key, value in message.items() if key != "id"}
        digest = hashlib.sha256(json.dumps(canonical, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        journal = self.process.supervisor.journal
        started = time.perf_counter_ns()
        journal.ensure_space(max(1024 * 1024, len(body.encode()) * 2))
        with self.lock, journal.lock, journal.db() as db:
            prior = db.execute("SELECT digest,native_id FROM operations WHERE handle=? AND operation_id=?",
                               (self.handle, operation_id)).fetchone()
            if prior:
                if prior["digest"] != digest:
                    raise ValueError("Supervisor operation id was reused with different content")
                return {"accepted": True, "duplicate": True,
                        "remoteId": prior["native_id"],
                        "durableMs": (time.perf_counter_ns() - started) / 1_000_000}
            used = journal.outstanding_bytes(db, self.handle)
            if used >= HANDLE_LIMIT:
                raise BufferError("Supervisor output journal is full; waiting for backend replay acknowledgement")
            # FULL synchronous commit is the acceptance point. Never write child
            # stdin before this commit succeeds.
            remote_id = native_id
            if "method" in message and isinstance(native_id, int):
                row = db.execute("SELECT rpc_sequence FROM handles WHERE id=?", (self.handle,)).fetchone()
                remote_id = row[0] + 1
                message = {**message, "id": remote_id}
                body = json.dumps(message, separators=(",", ":"))
                db.execute("UPDATE handles SET rpc_sequence=? WHERE id=?", (remote_id, self.handle))
            db.execute("INSERT INTO operations VALUES (?,?,?,?,?)",
                       (self.handle, operation_id, digest, remote_id, time.time()))
            db.commit()
            self.process.stdin.write(body + "\n")
            self.process.stdin.flush()
        return {"accepted": True, "duplicate": False, "remoteId": remote_id,
                "durableMs": (time.perf_counter_ns() - started) / 1_000_000}


class Supervisor:
    def __init__(self, root):
        self.root = Path(root).resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.socket_path = self.root / "supervisor.sock"
        self.journal = None
        self.children = {}
        self.lock = threading.RLock()
        self.owner = None
        self.owner_connections = 0
        self.recovery = {"degraded": False, "fallbackReady": False, "blocked": None}

    @staticmethod
    def signature(command, env, cwd):
        content = json.dumps({"command": command, "env": env, "cwd": cwd}, sort_keys=True)
        return hashlib.sha256(content.encode()).hexdigest()

    def open_handle(self, handle, command, env, cwd):
        if self.journal is None:
            self.journal = Journal(self.root)
        signature = self.signature(command, env, cwd)
        with self.lock:
            child = self.children.get(handle)
            if child is not None:
                if child.signature != signature or child.process.poll() is not None:
                    raise RuntimeError("Supervisor handle exists with an incompatible or stopped child")
                return child, True
            recovered = False
            with self.journal.db() as db:
                saved = db.execute("SELECT signature,pid,closed_at FROM handles WHERE id=?", (handle,)).fetchone()
                if saved:
                    if saved["closed_at"] is None or self.recovery.get("blocked"):
                        raise RuntimeError("Supervisor handle is orphaned; native outcome remains unknown")
                    if saved["signature"] != signature:
                        proof = db.execute(
                            "SELECT c.start_time FROM degraded_handles d "
                            "JOIN child_identities c ON c.handle=d.handle "
                            "WHERE d.handle=? AND d.prior_pid=? AND c.pid=d.prior_pid "
                            "AND c.start_time=d.start_time", (handle, saved["pid"])).fetchone()
                        if not proof or process_start_matches(saved["pid"], proof[0], allow_legacy=True):
                            raise RuntimeError("Supervisor handle is orphaned; native outcome remains unknown")
                    recovered = True
                else:
                    db.execute("INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?)",
                               (handle, signature, time.time()))
                    db.commit()
            proc = subprocess.Popen(command, env=env, cwd=cwd, stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                encoding="utf-8", bufsize=1, start_new_session=True)
            try:
                started = process_start_time(proc.pid)
            except Exception:
                proc.kill()
                proc.wait(timeout=2)
                raise
            if started is None:
                proc.kill()
                proc.wait(timeout=2)
                raise RuntimeError("Cannot prove the native child process identity")
            with self.journal.db() as db:
                db.execute("UPDATE handles SET pid=?,signature=?,rpc_sequence=0,"
                           "generation=generation+1,init_result=NULL,"
                           "closed_at=NULL,closed_reason=NULL WHERE id=?", (proc.pid, signature, handle))
                db.execute("INSERT INTO child_identities VALUES (?,?,?,?) ON CONFLICT(handle) DO UPDATE SET "
                           "pid=excluded.pid,pgid=excluded.pgid,start_time=excluded.start_time",
                           (handle, proc.pid, proc.pid, started))
                if recovered:
                    db.execute("DELETE FROM degraded_handles WHERE handle=?", (handle,))
            proc.supervisor = self
            child = Child(handle, proc, signature)
            self.children[handle] = child
            return child, False

    def handle(self, request):
        if request.get("action") == "health":
            with self.journal.db() as db:
                handles = []
                for row in db.execute("SELECT id,sequence,acknowledged,pid FROM handles WHERE closed_at IS NULL"):
                    child = self.children.get(row["id"])
                    handles.append({"id": row["id"], "pid": row["pid"],
                        "sequence": row["sequence"], "acknowledged": row["acknowledged"],
                        "bufferedBytes": self.journal.outstanding_bytes(db, row["id"]),
                        "backpressure": bool(child and child.paused.is_set())})
            return {"protocol": PROTOCOL, "stateDir": str(self.root), "handles": handles,
                    "journalLimitBytes": HANDLE_LIMIT, "durability": "sqlite-full-sync-per-request",
                    "recovery": self.recovery}
        if request.get("action") == "finishFallback":
            if self.recovery.get("blocked"):
                raise RuntimeError("Cannot finish supervisor fallback while child ownership is uncertain")
            self.recovery = {"degraded": False, "fallbackReady": False, "blocked": None}
            with self.journal.db() as db:
                db.execute("INSERT INTO supervisor_state VALUES ('recovery',?) "
                           "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (json.dumps(self.recovery),))
            return {"finished": True}
        handle = request.get("handle")
        if not isinstance(handle, str) or not handle or len(handle) > 180:
            raise ValueError("Invalid supervisor handle")
        if request.get("action") == "open":
            child, resumed = self.open_handle(handle, request["command"], request["env"], request["cwd"])
            with self.journal.db() as db:
                row = db.execute("SELECT init_result,acknowledged,sequence,generation FROM handles WHERE id=?",
                                 (handle,)).fetchone()
            return {"resumed": resumed, "initResult": json.loads(row[0]) if row[0] else None,
                    "acknowledged": row[1], "sequence": row[2], "generation": row[3],
                    "returnCode": child.process.poll()}
        child = self.children.get(handle)
        if child is None:
            raise RuntimeError("Unknown supervisor handle")
        action = request.get("action")
        if action == "write":
            return child.write(request["operationId"], request.get("nativeId"), request["message"])
        if action == "ack":
            sequence = request.get("sequence")
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                if type(sequence) is not int or sequence < row[1] or sequence > row[0]:
                    raise ValueError("Supervisor ACK cursor is outside the saved sequence range")
                db.execute("DELETE FROM events WHERE handle=? AND sequence<=?", (handle, sequence))
                db.execute("UPDATE handles SET acknowledged=? WHERE id=?", (sequence, handle))
                db.commit()
            with self.journal.db() as db:
                db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            return {"acknowledged": sequence}
        if action == "replay":
            cursor = request.get("cursor")
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                if type(cursor) is not int or cursor < row[1] or cursor > row[0]:
                    raise ValueError("Supervisor replay cursor is stale or ahead of the journal")
                return {"events": [dict(e) for e in db.execute(
                    "SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence",
                    (handle, cursor))], "sequence": row[0], "acknowledged": row[1],
                    "backpressure": child.paused.is_set()}
        if action == "status":
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                return {"sequence": row[0], "acknowledged": row[1], "backpressure": child.paused.is_set(),
                        "pid": child.process.pid, "returnCode": child.process.poll()}
        if action == "next":
            cursor = request.get("cursor")
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                if type(cursor) is not int or cursor < row[1] or cursor > row[0]:
                    raise ValueError("Supervisor replay cursor is stale or ahead of the journal")
                event = db.execute("SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT 1",
                                   (handle, cursor)).fetchone()
            if event:
                return {"event": dict(event), "returnCode": child.process.poll(),
                        "backpressure": child.paused.is_set()}
            return {"event": None, "returnCode": child.process.poll(),
                    "backpressure": child.paused.is_set()}
        if action == "detach":
            return {"detached": True}
        raise ValueError("Unknown supervisor action")

    def serve(self, wait_for_lease=False):
        self.lease = (self.root / "supervisor.lock").open("a+")
        announced_wait = False
        while True:
            try:
                fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError as error:
                if not wait_for_lease:
                    raise RuntimeError("Another supervisor owns this state directory") from error
                if not announced_wait:
                    self.lease.seek(0)
                    try:
                        owner_pid = json.loads(self.lease.read()).get("pid")
                    except (ValueError, AttributeError):
                        owner_pid = None
                    if isinstance(owner_pid, int) and owner_pid > 0:
                        print(f"supervisor waiting for owner {owner_pid}", file=sys.stderr, flush=True)
                        announced_wait = True
                time.sleep(.1)
                continue

            self.lease.seek(0)
            prior = self.lease.read().strip()
            previous_identity = None
            if prior:
                try:
                    previous_identity = json.loads(prior)
                    old_pid = previous_identity["pid"]
                    old_start = previous_identity["startTime"]
                    current_start = process_start_time(old_pid)
                except (ValueError, KeyError, TypeError) as error:
                    raise RuntimeError("The previous supervisor lease identity is invalid") from error
                if current_start == old_start:
                    if not wait_for_lease:
                        raise RuntimeError("The previous supervisor process is still alive")
                    if not announced_wait:
                        print(f"supervisor waiting for owner {old_pid}", file=sys.stderr, flush=True)
                        announced_wait = True
                    fcntl.flock(self.lease, fcntl.LOCK_UN)
                    time.sleep(.1)
                    continue
            self.journal = Journal(self.root)
            if prior:
                self.recover_orphaned_children()
            break
        own_start = process_start_time(os.getpid())
        if own_start is None:
            raise RuntimeError("Cannot prove the supervisor process identity")
        self.lease.seek(0)
        self.lease.truncate()
        self.lease.write(json.dumps({"pid": os.getpid(), "startTime": own_start}))
        self.lease.flush()
        os.fsync(self.lease.fileno())
        self.socket_path.unlink(missing_ok=True)
        server = socketserver.ThreadingUnixStreamServer(str(self.socket_path), Handler)
        server.daemon_threads = True
        server.supervisor = self
        os.chmod(self.socket_path, 0o600)
        server.serve_forever(poll_interval=.2)

    def recover_orphaned_children(self):
        with self.journal.db() as db:
            handles = list(db.execute("SELECT id,pid FROM handles WHERE closed_at IS NULL"))
            identities = {row["handle"]: dict(row) for row in db.execute(
                "SELECT c.* FROM child_identities c JOIN handles h ON h.id=c.handle WHERE h.closed_at IS NULL")}
        if not handles:
            return
        self.recovery = {"degraded": False, "fallbackReady": False, "blocked": None}
        if len(identities) != len(handles):
            self.recovery["blocked"] = "A native child is missing its recorded PID and start time"
        for row in handles:
            handle = row["id"]
            identity = identities.get(handle)
            if not identity:
                with self.journal.db() as db:
                    reason = "Native child PID and start time were not recorded"
                    db.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                               (time.time(), reason, handle))
                    db.execute("INSERT INTO recovery_events(at,handle,pid,start_time,outcome,detail) "
                               "VALUES (?,?,?,?,?,?)",
                               (time.time(), handle, row["pid"], "", "missing-identity", reason))
                continue
            pid, pgid, expected = identity["pid"], identity["pgid"], identity["start_time"]
            actual = process_start_time(pid)
            if actual is None:
                outcome, detail = "already-exited", "Native child was no longer running when the supervisor recovered"
            elif actual != expected:
                outcome, detail = "identity-mismatch", "PID belongs to a different process start time"
            else:
                self.recovery.update(
                    degraded=True,
                    notice="The process supervisor restarted; native work is using recovery.",
                )
                try:
                    os.killpg(pgid, signal.SIGTERM)
                    deadline = time.monotonic() + 1.5
                    while time.monotonic() < deadline and process_start_time(pid) == expected:
                        time.sleep(.05)
                    if process_start_time(pid) == expected:
                        # Verify ownership again immediately before escalation.
                        if process_start_time(pid) != expected:
                            outcome, detail = "identity-mismatch", "PID changed before KILL; no signal was sent"
                            self.recovery["blocked"] = f"Child PID {pid} changed identity during cleanup"
                        else:
                            os.killpg(pgid, signal.SIGKILL)
                            deadline = time.monotonic() + 1
                            while time.monotonic() < deadline and process_start_time(pid) == expected:
                                time.sleep(.05)
                            if process_start_time(pid) == expected:
                                outcome, detail = "termination-failed", "Process group survived TERM and KILL"
                                self.recovery["blocked"] = f"Could not terminate verified child PID {pid}"
                            else:
                                outcome, detail = "killed", "verified process group terminated"
                    else:
                        outcome, detail = "terminated", "verified process group exited after TERM"
                except ProcessLookupError:
                    outcome, detail = "already-exited", None
                except OSError as error:
                    outcome, detail = "termination-failed", str(error)[:300]
                    self.recovery["blocked"] = f"Could not terminate verified child PID {pid}"
            with self.journal.db() as db:
                db.execute("INSERT INTO recovery_events(at,handle,pid,start_time,outcome,detail) "
                           "VALUES (?,?,?,?,?,?)", (time.time(), handle, pid, expected, outcome, detail))
                reason = detail or "Native child stopped while the supervisor was unavailable"
                db.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                           (time.time(), reason, handle))
                if outcome in {"already-exited", "terminated", "killed"}:
                    db.execute("INSERT OR REPLACE INTO degraded_handles VALUES (?,?,?,?)",
                               (handle, pid, expected, time.time()))
        if self.recovery["degraded"] and self.recovery["blocked"] is None:
            self.recovery["fallbackReady"] = True
        with self.journal.db() as db:
            db.execute("INSERT INTO supervisor_state VALUES ('recovery',?) "
                       "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                       (json.dumps(self.recovery),))


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        owner = None
        try:
            first = json.loads(self.rfile.readline())
            if first.get("protocol") != PROTOCOL or first.get("stateDir") != str(self.server.supervisor.root):
                raise RuntimeError("Supervisor protocol or state directory mismatch")
            owner = None if first.get("probe") is True else first.get("backendId")
            supervisor = self.server.supervisor
            if owner is not None:
                with supervisor.lock:
                    if supervisor.owner and supervisor.owner != owner and supervisor.owner_connections:
                        raise RuntimeError("Another backend is attached to this supervisor")
                    supervisor.owner = owner
                    supervisor.owner_connections += 1
            self.wfile.write(b'{"ok":true}\n')
            while True:
                line = self.rfile.readline()
                if not line:
                    return
                try:
                    request = json.loads(line)
                    result = supervisor.handle(request)
                    response = {"requestId": request.get("requestId"), "result": result}
                except Exception as error:
                    response = {"requestId": request.get("requestId"),
                                "error": str(error)[:500]}
                self.wfile.write((json.dumps(response, separators=(",", ":")) + "\n").encode())
                self.wfile.flush()
        except Exception as error:
            try:
                self.wfile.write((json.dumps({"error": str(error)[:500]}) + "\n").encode())
                self.wfile.flush()
            except OSError:
                pass
        finally:
            if owner is not None:
                with self.server.supervisor.lock:
                    self.server.supervisor.owner_connections = max(0, self.server.supervisor.owner_connections - 1)
                    if not self.server.supervisor.owner_connections:
                        self.server.supervisor.owner = None


class _QueueStream:
    def __init__(self):
        self.queue = queue.Queue()

    def push(self, value):
        self.queue.put(value)

    def __iter__(self):
        while True:
            value = self.queue.get()
            if value is None:
                return
            yield value

    def close(self):
        self.push(None)


class _Input:
    def __init__(self, process):
        self.process = process
        self.buffer = ""

    def write(self, value):
        self.buffer += value
        return len(value)

    def flush(self):
        if not self.buffer:
            return
        line, self.buffer = self.buffer, ""
        try:
            request = json.loads(line)
        except json.JSONDecodeError as error:
            raise OSError("Malformed JSON RPC written to supervisor") from error
        self.process.send_write(request)

    def fileno(self):
        raise OSError("Supervisor input is a message channel")

    def close(self):
        self.process.detach()


def process_launch_environment(pid):
    """Read a verified child's launch environment without saving credentials."""
    import sys
    if sys.platform == 'darwin':
        import ctypes
        import struct
        libc = ctypes.CDLL(None, use_errno=True)
        mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2, PID
        size = ctypes.c_size_t()
        if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
        if not 4 <= size.value <= 4 * 1024 * 1024:
            raise ValueError('Native process launch data exceeds its limit')
        buffer = ctypes.create_string_buffer(size.value)
        if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
        raw = buffer.raw[:size.value]
        argc = struct.unpack('i', raw[:4])[0]
        if not 0 < argc <= 4096:
            raise ValueError('Invalid native process argument count')
        position = raw.index(b'\0', 4) + 1
        while position < len(raw) and raw[position] == 0:
            position += 1
        for _ in range(argc):
            position = raw.index(b'\0', position) + 1
        raw = raw[position:]
    elif sys.platform.startswith('linux'):
        with (Path('/proc') / str(pid) / 'environ').open('rb') as stream:
            raw = stream.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            raise ValueError('Native process launch data exceeds its limit')
    else:
        raise RuntimeError('Cannot verify a legacy native launch on this platform')
    environment = {}
    for entry in raw.split(b'\0'):
        if not entry:
            break  # macOS follows the environment with a separate Apple vector.
        key, separator, value = entry.partition(b'=')
        if not separator:
            raise ValueError('Invalid native process environment')
        environment[os.fsdecode(key)] = os.fsdecode(value)
    return environment


def native_launch_environment(root, handle, command, env, cwd):
    """Backend ownership is transport metadata, not a native launch setting."""
    clean = dict(env)
    clean.pop('CODEX_AGENTS_BACKEND_ID', None)
    path = Path(root) / 'supervisor.sqlite3'
    db = sqlite3.connect(path.absolute().as_uri() + '?mode=ro', uri=True)
    try:
        saved = db.execute('SELECT h.signature,h.pid,c.pid,c.start_time,h.closed_at FROM handles h '
                           'LEFT JOIN child_identities c ON c.handle=h.id WHERE h.id=?',
                           (handle,)).fetchone()
    finally:
        db.close()
    if not saved or saved[4] is not None or Supervisor.signature(command, clean, cwd) == saved[0]:
        # A closed generation has no environment to inspect. The supervisor
        # validates its durable recovery proof before accepting a new launch.
        return clean
    # Older supervisors hash the backend ID into the launch. Keep their exact
    # accepted signature, but only for the same verified native configuration.
    _, pid, identity_pid, started, _ = saved
    if not started or pid != identity_pid or not process_start_matches(pid, started, allow_legacy=True):
        raise RuntimeError('Cannot verify the existing supervisor child; native outcome remains unknown')
    original = process_launch_environment(pid)
    if Supervisor.signature(command, original, cwd) != saved[0]:
        # A macOS Python launcher can add this marker after a script's exec.
        original.pop('__PYVENV_LAUNCHER__', None)
    comparable = dict(original)
    comparable.pop('CODEX_AGENTS_BACKEND_ID', None)
    # Reattachment keeps the child's accepted launch, including its PATH and
    # locale. A backend launcher can supply different ambient values. These
    # values never replace the live child's settings; command, account,
    # credentials, provider options and process identity still match exactly.
    for key in ('PATH', 'LANG', '__PYVENV_LAUNCHER__'):
        comparable.pop(key, None)
    requested = {key: value for key, value in clean.items()
                 if key not in {'PATH', 'LANG', '__PYVENV_LAUNCHER__'}}
    if (comparable != requested or Supervisor.signature(command, original, cwd) != saved[0]
            or not process_start_matches(pid, started, allow_legacy=True)):
        raise RuntimeError('Supervisor native launch settings changed; existing work was preserved')
    return original


class ProcessProxy:
    """Popen-shaped transport consumed by codex_runtime.AppServer."""
    def __init__(self, root, handle, command, env, cwd, stderr_sink):
        self.root = Path(root).resolve()
        self.handle = handle
        self.backend_id = os.environ.setdefault("CODEX_AGENTS_BACKEND_ID", str(uuid.uuid4()))
        self.socket = _connect(self.root / "supervisor.sock")
        self.reader = self.socket.makefile("r", encoding="utf-8")
        self.write_lock = threading.Lock()
        self.event_lock = threading.RLock()
        self.request_id = 0
        self.stdout = _Stdout(self)
        self.stderr = None
        self.stderr_sink = stderr_sink
        self.stdin = _Input(self)
        self._returncode = None
        self.detached = False
        _send(self.socket, {"protocol": PROTOCOL, "stateDir": str(self.root), "backendId": self.backend_id}, self.write_lock)
        hello = _recv(self.socket, self.reader)
        if hello.get("error"):
            self.reader.close()
            self.socket.close()
            raise RuntimeError("Supervisor attach failed: " + hello["error"])
        try:
            env = native_launch_environment(self.root, handle, command, env, cwd)
            opened = self.call("open", command=command, env=env, cwd=cwd)
        except Exception:
            self.reader.close()
            self.socket.close()
            raise
        self.initialize_result = opened.get("initResult") if opened.get("resumed") else None
        # Expose the supervisor's exact open receipt to the runtime. Recovery
        # may resume persisted work only when this handle reused its live child.
        self.resumed = opened.get("resumed") is True
        self.generation = opened.get("generation", 1)
        self.cursor = opened["acknowledged"]
        self.read_cursor = self.cursor
        self.ack_pending = set()
        self.sequence = opened["sequence"]
        self.remote_to_local = {}

    def call(self, action, **values):
        with self.write_lock:
            self.request_id += 1
            identity = self.request_id
            _send(self.socket, {"requestId": identity, "handle": self.handle, "action": action, **values})
            answer = _recv(self.socket, self.reader)
        if answer.get("requestId") != identity:
            raise RuntimeError("Supervisor response identity mismatch")
        if answer.get("error"):
            raise RuntimeError("Supervisor " + action + " failed: " + answer["error"])
        return answer.get("result", {})

    def next_event(self):
        while not self.detached:
            result = self.call("next", cursor=self.read_cursor)
            event = result["event"]
            if event:
                with self.event_lock:
                    if self.detached:
                        return None
                    self.read_cursor = self.sequence = event["sequence"]
                    payload = json.loads(event["payload"])
                    prior_generation = event.get("generation", self.generation) < self.generation
                    if prior_generation and (event["kind"] == "exit"
                            or (event["kind"] == "stdout" and "id" in payload)):
                        # The old child is gone, so its RPC traffic cannot be
                        # delivered through the new proxy. Retire it without
                        # letting the old exit close the new connection.
                        self.ack(event["sequence"])
                        continue
                    if event["kind"] == "stdout":
                        if "method" not in payload and isinstance(payload.get("id"), int):
                            local_id = self.remote_to_local.get(payload["id"])
                            if local_id is None:
                                payload = {**payload, "id": "supervisor-orphan-" + str(payload["id"])}
                            else:
                                payload = {**payload, "id": local_id}
                        return event["sequence"], json.dumps(payload) + "\n"
                    if event["kind"] == "exit":
                        self._returncode = payload.get("returnCode")
                        self.ack(event["sequence"])
                        return None
                    if event["kind"] == "stderr":
                        data = payload.get("data")
                        if not isinstance(data, str) or len(data.encode("utf-8")) > MAX_STDERR_EVENT_BYTES:
                            raise RuntimeError("Supervisor stderr event exceeds its size limit")
                        if not callable(self.stderr_sink):
                            raise RuntimeError("Supervisor stderr event has no AppServer log sink")
                        # Keep stderr on the existing journal path. ACK only after
                        # the bounded RotatingLog accepts the complete payload.
                        self.stderr_sink(data)
                    self.ack(event["sequence"])
            if result["returnCode"] is not None and event is None:
                self._returncode = result["returnCode"]
                return None
            time.sleep(.05)
        return None

    def send_write(self, request, operation_id=None):
        method = request.get("method", "reply")
        params = request.get("params", {})
        identity = operation_id or request.get("operationId") or self._operation_identity(method, params, request)
        result = self.call("write", operationId=identity, nativeId=request.get("id"), message=request)
        self.last_durable_ms = result["durableMs"]
        if result.get("remoteId") is not None and isinstance(request.get("id"), int):
            self.remote_to_local[result["remoteId"]] = request["id"]

    def _operation_identity(self, method, params, request):
        stable = None
        if method in {"turn/start", "review/start"}:
            stable = params.get("clientUserMessageId") or params.get("clientRequestId")
        elif method == "command/exec":
            stable = params.get("processId")
        elif method == "process/spawn":
            stable = params.get("processHandle")
        elif method == "process/writeStdin":
            stable = params.get("requestId") or params.get("request_id")
        if stable is None:
            stable = "new:" + str(uuid.uuid4())
        return hashlib.sha256((self.handle + "\0" + method + "\0" + str(stable)).encode()).hexdigest()

    def ack(self, sequence):
        with self.__dict__.setdefault("_ack_lock", threading.RLock()):
            if sequence <= self.cursor:
                return
            if type(sequence) is not int or sequence > self.read_cursor:
                raise RuntimeError("Cannot ACK a supervisor event that has not been read")
            batch = self.__dict__.get("_event_batches", {}).get(sequence, ())
            self.ack_pending.update(batch)
            self.ack_pending.add(sequence)
            contiguous = self.cursor
            while contiguous + 1 in self.ack_pending:
                contiguous += 1
            if contiguous > self.cursor:
                self.call("ack", sequence=contiguous)
                for acknowledged in range(self.cursor + 1, contiguous + 1):
                    self.ack_pending.discard(acknowledged)
                self.cursor = contiguous
                for last in tuple(self.__dict__.get("_event_batches", {})):
                    if last <= contiguous:
                        self._event_batches.pop(last)

    def register_event_batch(self, sequences):
        with self.__dict__.setdefault("_ack_lock", threading.RLock()):
            if (len(sequences) < 2 or len(sequences) > 128
                    or any(type(s) is not int or s <= self.cursor or s > self.read_cursor for s in sequences)
                    or any(a >= b for a, b in zip(sequences, sequences[1:]))):
                raise RuntimeError("Invalid supervisor event batch")
            self.__dict__.setdefault("_event_batches", {})[sequences[-1]] = tuple(sequences)

    def ack_applied_deltas(self, applied):
        """Repair only read delta gaps covered by the runtime's durable cursor."""
        with self.__dict__.setdefault("_ack_lock", threading.RLock()):
            if not self.ack_pending or self.cursor + 1 in self.ack_pending:
                return 0
            uri = (self.root / "supervisor.sqlite3").as_uri() + "?mode=ro"
            db = sqlite3.connect(uri, uri=True)
            try:
                rows = db.execute(
                    "SELECT sequence,kind,payload FROM events WHERE handle=? "
                    "AND sequence>? AND sequence<=? ORDER BY sequence LIMIT 128",
                    (self.handle, self.cursor, self.read_cursor)).fetchall()
            finally:
                db.close()
            missing = []
            contiguous = self.cursor
            for sequence, kind, payload in rows:
                if sequence != contiguous + 1:
                    break
                if sequence not in self.ack_pending:
                    message = json.loads(payload)
                    if (kind != "stdout" or not isinstance(message, dict)
                            or "id" in message
                            or message.get("method") != "item/agentMessage/delta"):
                        break
                    missing.append(sequence)
                contiguous = sequence
            if not missing or not applied(missing[-1]):
                return 0
            self.ack_pending.update(missing)
            self.ack(contiguous)
            return len(missing)

    def poll(self):
        return self._returncode

    def wait(self, timeout=None):
        if not self.detached:
            self.detached = True
            return self._returncode
        return self._returncode

    def terminate(self):
        self.detach()

    def kill(self):
        self.detach()

    def detach(self):
        with self.event_lock:
            if self.detached:
                return
            self.detached = True
            try:
                self.call("detach")
            except Exception:
                pass
            try:
                self.socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.reader.close()
            self.socket.close()


class _Stdout:
    def __init__(self, process):
        self.process = process
        self.current_sequence = 0

    def __iter__(self):
        while True:
            event = self.process.next_event()
            if event is None:
                return
            sequence, line = event
            self.current_sequence = sequence
            yield line

    def close(self):
        self.process.detach()

    def fileno(self):
        raise OSError("Supervisor output is a message channel")


def attach(root, handle, command, env, cwd=None, *, stderr_sink=None):
    if os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") != "1":
        return None
    # Account and terminal roots own local logs and sessions. The backend's
    # state directory owns the one supervisor socket and its durable journal.
    state_root = os.environ.get("CODEX_AGENTS_STATE_DIR") or root
    return ProcessProxy(state_root, handle, command, env, cwd, stderr_sink)


def status(root):
    root = Path(root).resolve()
    client = _connect(root / "supervisor.sock", timeout=2)
    reader = client.makefile("r", encoding="utf-8")
    try:
        _send(client, {"protocol": PROTOCOL, "stateDir": str(root), "backendId": "preflight", "probe": True})
        hello = _recv(client, reader)
        if hello.get("error"):
            raise RuntimeError(hello["error"])
        _send(client, {"requestId": 1, "action": "health"})
        response = _recv(client, reader)
    finally:
        reader.close()
        client.close()
    result = response.get("result")
    if not result or result.get("protocol") != PROTOCOL or result.get("stateDir") != str(root):
        raise RuntimeError("Supervisor health identity is incompatible")
    return result


def finish_fallback(root):
    root = Path(root).resolve()
    client = _connect(root / "supervisor.sock", timeout=2)
    reader = client.makefile("r", encoding="utf-8")
    try:
        _send(client, {"protocol": PROTOCOL, "stateDir": str(root), "backendId": "recovery", "probe": True})
        _recv(client, reader)
        _send(client, {"requestId": 1, "action": "finishFallback"})
        response = _recv(client, reader)
    finally:
        reader.close()
        client.close()
    if response.get("error"):
        raise RuntimeError(response["error"])
    return response.get("result", {})


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--status-json", action="store_true")
    parser.add_argument("--finish-fallback", action="store_true")
    parser.add_argument("--wait-for-lease", action="store_true")
    args = parser.parse_args()
    if args.finish_fallback:
        finish_fallback(args.state)
    elif args.status_json:
        print(json.dumps(status(args.state), separators=(",", ":")))
    elif args.check:
        current = status(args.state)
        if current.get("recovery", {}).get("degraded"):
            raise RuntimeError("Supervisor crash recovery is active; start one fallback backend generation first")
    else:
        Supervisor(args.state).serve(wait_for_lease=args.wait_for_lease)


if __name__ == "__main__":
    main()
