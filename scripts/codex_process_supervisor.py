"""Own native Codex/Claude app-server processes across backend restarts.

The supervisor is opt-in. It is a small stdio relay with durable request and
output journals; the canvas database remains the source of work permissions.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from codex_file_lock import flock, LOCK_EX, LOCK_NB, LOCK_UN
from codex_private_paths import ensure_private_dir
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

from codex_open_file_limit import raise_open_file_limit

PROTOCOL = 1
MAX_STDERR_EVENT_BYTES = 256 * 1024
HANDLE_LIMIT = 256 * 1024 * 1024
SOCKET_TIMEOUT = 10


def _retryable_storage_error(error):
    if isinstance(error, OSError):
        return True
    code = getattr(error, "sqlite_errorcode", None)
    if isinstance(code, int):
        return code & 255 in {sqlite3.SQLITE_FULL, sqlite3.SQLITE_BUSY,
                             sqlite3.SQLITE_LOCKED, sqlite3.SQLITE_IOERR}
    return isinstance(error, sqlite3.OperationalError) and str(error).lower() in {
        "database or disk is full", "database is locked", "database table is locked", "disk i/o error"}


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
        self._db_lock = threading.Lock()
        self._db_idle = []
        self._db_closed = False
        with self.db() as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=FULL")
            db.executescript("""
                CREATE TABLE IF NOT EXISTS handles(
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, pid INTEGER NOT NULL,
                    sequence INTEGER NOT NULL DEFAULT 0, acknowledged INTEGER NOT NULL DEFAULT 0,
                    rpc_sequence INTEGER NOT NULL DEFAULT 0, init_result TEXT, created REAL NOT NULL,
                    closed_at REAL, closed_reason TEXT, generation INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS operations(
                    handle TEXT NOT NULL, operation_id TEXT NOT NULL, digest TEXT NOT NULL,
                    native_id INTEGER, accepted REAL NOT NULL, generation INTEGER,
                    response TEXT, response_sequence INTEGER,
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
            operation_columns = {row[1] for row in db.execute("PRAGMA table_info(operations)")}
            if "generation" not in operation_columns:
                db.execute("ALTER TABLE operations ADD COLUMN generation INTEGER")
            if "response" not in operation_columns:
                db.execute("ALTER TABLE operations ADD COLUMN response TEXT")
            if "response_sequence" not in operation_columns:
                db.execute("ALTER TABLE operations ADD COLUMN response_sequence INTEGER")
            db.execute("CREATE INDEX IF NOT EXISTS operations_monitor_native ON operations("
                       "handle,generation,native_id) WHERE substr(operation_id,1,8)='monitor:'")

    @contextmanager
    def db(self):
        with self._db_lock:
            if self._db_closed:
                raise RuntimeError("Supervisor journal is closed")
            db = self._db_idle.pop() if self._db_idle else None
        if db is None:
            try:
                db = sqlite3.connect(self.path, timeout=5, check_same_thread=False)
                db.row_factory = sqlite3.Row
                db.execute("PRAGMA synchronous=FULL")
                db.execute("PRAGMA wal_autocheckpoint=100")
            except BaseException:
                if db is not None:
                    db.close()
                raise
        failed = True
        try:
            with db:
                yield db
            failed = False
        finally:
            with self._db_lock:
                reuse = not failed and not self._db_closed and len(self._db_idle) < 4
                if reuse:
                    self._db_idle.append(db)
            if not reuse:
                db.close()

    def close(self):
        with self._db_lock:
            self._db_closed = True
            idle, self._db_idle = self._db_idle, []
        # Active leases retain their transaction and close when they return.
        for db in idle:
            db.close()

    def __del__(self):
        try:
            self.close()
        except BaseException:
            pass

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
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes

        class FileTime(ctypes.Structure):
            _fields_ = [("low", wintypes.DWORD), ("high", wintypes.DWORD)]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        open_process = kernel32.OpenProcess
        open_process.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        open_process.restype = wintypes.HANDLE
        get_times = kernel32.GetProcessTimes
        get_times.argtypes = [wintypes.HANDLE, ctypes.POINTER(FileTime), ctypes.POINTER(FileTime), ctypes.POINTER(FileTime), ctypes.POINTER(FileTime)]
        get_times.restype = wintypes.BOOL
        close_handle = kernel32.CloseHandle
        handle = open_process(0x1000, False, pid)
        if not handle:
            if ctypes.get_last_error() in (87, 1168):
                return None
            raise RuntimeError(f"Cannot verify process identity for PID {pid}")
        try:
            created, exited, kernel, user = FileTime(), FileTime(), FileTime(), FileTime()
            if not get_times(handle, ctypes.byref(created), ctypes.byref(exited), ctypes.byref(kernel), ctypes.byref(user)):
                if ctypes.get_last_error() in (87, 1168):
                    return None
                raise RuntimeError(f"Cannot verify process identity for PID {pid}")
            ticks = (created.high << 32) | created.low
            if exited.high or exited.low:
                return None
            return f"{ticks // 10_000_000}.{(ticks // 10) % 1_000_000:06d}"
        finally:
            close_handle(handle)
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


def _valid_process_start(value):
    if not isinstance(value, str) or not value:
        return False
    parts = value.split('.')
    if (len(parts) == 2 and all(part.isascii() and part.isdigit() for part in parts)
            and int(parts[0]) > 0 and len(parts[1]) == 6):
        return True
    try:
        return time.strptime(value, '%a %b %d %H:%M:%S %Y').tm_year >= 1970
    except ValueError:
        return False


class Child:
    def __init__(self, handle, process, signature):
        self.handle, self.process, self.signature = handle, process, signature
        self.lock = threading.RLock()
        self.append_lock = threading.Lock()
        self.output = threading.Condition(self.lock)
        self.stopping = threading.Event()
        self.paused = threading.Event()
        self.persistence_errors = {}
        self.stdout_error = None
        self.reader = threading.Thread(target=self.read_stdout, daemon=True,
                                       name="supervisor-stdout-" + handle[:8])
        self.reader.start()
        self.stderr = threading.Thread(target=self.read_stderr, daemon=True,
                                       name="supervisor-stderr-" + handle[:8])
        self.stderr.start()

    def append(self, kind, payload):
        raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
        size = len(raw.encode())
        # One frame owns its candidate sequence until storage confirms it. ACK
        # and request handling use other locks and can continue during a retry.
        with self.append_lock:
            self._append_frame(kind, payload, raw, size)

    def _append_frame(self, kind, payload, raw, size):
        pending = None
        while not self.stopping.is_set():
            try:
                journal = self.process.supervisor.journal
                journal.ensure_space(size + 1024 * 1024)
                with self.lock, journal.lock, journal.db() as db:
                    used = journal.outstanding_bytes(db, self.handle)
                    row = db.execute("SELECT sequence,generation FROM handles WHERE id=?", (self.handle,)).fetchone()
                    if not row:
                        return
                    if pending and row[1] != pending[1]:
                        raise RuntimeError("Supervisor output generation changed before its receipt")
                    if pending and row[0] == pending[0]:
                        # The full transaction committed before a storage
                        # error was reported. This frame is already durable.
                        self.persistence_errors.pop(kind, None)
                        if not self.persistence_errors:
                            self.paused.clear()
                        self.output.notify_all()
                        return
                    if pending and row[0] != pending[0] - 1:
                        raise RuntimeError("Supervisor output sequence changed before its receipt")
                    if used + size <= HANDLE_LIMIT:
                        sequence = row[0] + 1
                        pending = (sequence, row[1])
                        db.execute("UPDATE handles SET sequence=? WHERE id=?", (sequence, self.handle))
                        db.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) "
                                   "VALUES (?,?,?,?,?,?)",
                                   (self.handle, sequence, kind, raw, size, row[1]))
                        if kind == "stdout" and isinstance(payload, dict):
                            if payload.get("id") == 1 and "result" in payload:
                                db.execute("UPDATE handles SET init_result=? WHERE id=?",
                                           (json.dumps(payload["result"]), self.handle))
                            if type(payload.get("id")) is int and "method" not in payload:
                                db.execute("UPDATE operations SET response=?,response_sequence=? WHERE handle=? "
                                           "AND native_id=? AND generation=? AND substr(operation_id,1,8)='monitor:' "
                                           "AND response IS NULL",
                                           (raw, sequence, self.handle, payload["id"], row[1]))
                        db.commit()
                        self.persistence_errors.pop(kind, None)
                        if not self.persistence_errors:
                            self.paused.clear()
                        self.output.notify_all()
                        return
                    self.paused.set()
            except (sqlite3.Error, OSError) as error:
                if not _retryable_storage_error(error):
                    raise
                with self.lock:
                    self.persistence_errors[kind] = type(error).__name__ + ": " + str(error)[:256]
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
                if isinstance(value, dict) and isinstance(value.get("method"), str):
                    value["_studioSupervisorReceivedAt"] = time.time()
                self.append("stdout", value)
        except Exception as error:
            self.stdout_error = type(error).__name__ + ": " + str(error)[:256]
        finally:
            self.append("exit", {"returnCode": self.process.wait()})

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
            rpc_request = "method" in message and isinstance(native_id, int)
            if rpc_request:
                row = db.execute("SELECT rpc_sequence,generation FROM handles WHERE id=?", (self.handle,)).fetchone()
                remote_id = row[0] + 1
                message = {**message, "id": remote_id}
                body = json.dumps(message, separators=(",", ":"))
                db.execute("UPDATE handles SET rpc_sequence=? WHERE id=?", (remote_id, self.handle))
            else:
                row = db.execute("SELECT generation FROM handles WHERE id=?", (self.handle,)).fetchone()
            db.execute("INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) "
                       "VALUES (?,?,?,?,?,?)",
                       (self.handle, operation_id, digest, remote_id, time.time(), row[1] if rpc_request else row[0]))
            db.commit()
            self.process.stdin.write(body + "\n")
            self.process.stdin.flush()
        return {"accepted": True, "duplicate": False, "remoteId": remote_id,
                "durableMs": (time.perf_counter_ns() - started) / 1_000_000}


class Supervisor:
    def __init__(self, root):
        self.root = ensure_private_dir(Path(root).resolve())
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
                return_code = child.process.poll()
                if return_code is None and child.signature != signature:
                    raise RuntimeError(
                        f"Supervisor handle {handle!r} is active with PID {child.process.pid} and different launch settings; "
                        "wait for it to exit or use the operator CLI to close a verified test/unknown handle."
                    )
                if return_code is None:
                    return child, True
                child.stopping.set()
                with child.output:
                    child.output.notify_all()
                self.children.pop(handle, None)
                if self.recovery.get("blocked"):
                    raise RuntimeError("Supervisor recovery is blocked; native outcome remains unknown")
                with self.journal.db() as db:
                    db.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                               (time.time(), f"Native child exited with status {return_code}", handle))
                    db.commit()
                stopped_in_process = True
            else:
                stopped_in_process = False
            recovered = stopped_in_process
            with self.journal.db() as db:
                saved = db.execute("SELECT signature,pid,closed_at,closed_reason FROM handles WHERE id=?", (handle,)).fetchone()
                if saved and not stopped_in_process:
                    if saved["closed_at"] is None or self.recovery.get("blocked"):
                        raise RuntimeError("Supervisor handle is orphaned; native outcome remains unknown")
                    if saved["signature"] != signature:
                        proof = db.execute(
                            "SELECT c.start_time FROM degraded_handles d "
                            "JOIN child_identities c ON c.handle=d.handle "
                            "WHERE d.handle=? AND d.prior_pid=? AND c.pid=d.prior_pid "
                            "AND c.start_time=d.start_time", (handle, saved["pid"])).fetchone()
                        if (not proof and saved["closed_reason"]
                                == "Closed by operator after verified test/unknown-client ownership"):
                            proof = db.execute(
                                "SELECT start_time FROM child_identities WHERE handle=? AND pid=?",
                                (handle, saved["pid"])).fetchone()
                        if (not proof or not _valid_process_start(proof[0])
                                or process_start_matches(saved["pid"], proof[0], allow_legacy=True)):
                            raise RuntimeError("Supervisor handle is orphaned; native outcome remains unknown")
                    recovered = True
                elif not saved:
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
                    identity = db.execute("SELECT start_time FROM child_identities WHERE handle=?",
                                          (row["id"],)).fetchone()
                    signature = db.execute("SELECT signature FROM handles WHERE id=?",
                                           (row["id"],)).fetchone()[0]
                    handles.append({"id": row["id"], "pid": row["pid"],
                        "signature": signature, "startTime": identity[0] if identity else None,
                        "sequence": row["sequence"], "acknowledged": row["acknowledged"],
                        "bufferedBytes": self.journal.outstanding_bytes(db, row["id"]),
                        "backpressure": bool(child and child.paused.is_set()),
                        "stdoutReaderAlive": bool(child and child.reader.is_alive()),
                        "stdoutReaderError": child.stdout_error if child else None,
                        "persistenceErrors": dict(child.persistence_errors) if child else {}})
            return {"protocol": PROTOCOL, "stateDir": str(self.root), "handles": handles,
                    "journalLimitBytes": HANDLE_LIMIT, "outputLimitBytesPerHandle": HANDLE_LIMIT,
                    "durability": "sqlite-full-sync-per-request",
                    "recovery": self.recovery}
        if request.get("action") == "finishFallback":
            if self.recovery.get("blocked"):
                raise RuntimeError("Cannot finish supervisor fallback while child ownership is uncertain")
            self.recovery = {"degraded": False, "fallbackReady": False, "blocked": None}
            with self.journal.db() as db:
                db.execute("INSERT INTO supervisor_state VALUES ('recovery',?) "
                           "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (json.dumps(self.recovery),))
            return {"finished": True}
        if request.get("action") == "adminCloseHandle":
            if (not isinstance(request.get("expectedPid"), int)
                    or not isinstance(request.get("expectedStartTime"), str)
                    or not request.get("expectedStartTime")
                    or not isinstance(request.get("expectedSignature"), str)):
                raise ValueError("Operator close requires expected PID, start time, and launch signature")
            handle = request.get("handle")
            with self.lock, self.journal.db() as db:
                row = db.execute("SELECT signature,pid,closed_at FROM handles WHERE id=?", (handle,)).fetchone()
                child_identity = db.execute(
                    "SELECT pid,pgid,start_time FROM child_identities WHERE handle=?", (handle,)
                ).fetchone()
                if not row or not child_identity or row["closed_at"] is not None:
                    raise RuntimeError("Operator close refused: handle is missing or already closed")
                if (row["signature"] != request["expectedSignature"]
                        or row["pid"] != request["expectedPid"]
                        or child_identity["pid"] != request["expectedPid"]
                        or child_identity["start_time"] != request["expectedStartTime"]
                        or process_start_time(request["expectedPid"]) != request["expectedStartTime"]):
                    raise RuntimeError("Operator close refused: handle identity changed; refresh status and verify ownership")
                child = self.children.get(handle)
                if child is None:
                    raise RuntimeError("Operator close refused: no in-memory child owner exists")
                child.stopping.set()
                try:
                    os.killpg(child_identity["pgid"], signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    child.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    if process_start_time(request["expectedPid"]) != request["expectedStartTime"]:
                        raise RuntimeError("Operator close refused escalation: child identity changed")
                    os.killpg(child_identity["pgid"], signal.SIGKILL)
                    child.process.wait(timeout=2)
                self.children.pop(handle, None)
                now = time.time()
                db.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                           (now, "Closed by operator after verified test/unknown-client ownership", handle))
                db.commit()
            result = {"closed": True, "handle": handle, "pid": request["expectedPid"]}
            return result
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
        if action == "retireCommand":
            if not handle.startswith('server-command:'):
                raise ValueError('Only completed server command handles can be retired')
            with self.lock, child.lock, self.journal.lock, self.journal.db() as db:
                row = db.execute('SELECT generation,sequence,acknowledged FROM handles WHERE id=?', (handle,)).fetchone()
                if (not row or row['generation'] != request.get('generation')
                        or child.process.poll() is None or child.reader.is_alive() or child.stderr.is_alive()
                        or row['sequence'] != row['acknowledged']):
                    raise RuntimeError('The command adapter is not ready for retirement')
                for stream in (child.process.stdin, child.process.stdout, child.process.stderr):
                    if stream is not None:
                        stream.close()
                for table in ('events', 'operations', 'child_identities', 'degraded_handles'):
                    db.execute('DELETE FROM ' + table + ' WHERE handle=?', (handle,))
                db.execute('DELETE FROM handles WHERE id=?', (handle,))
                db.commit()
                self.children.pop(handle, None)
            return {'retired': True}
        if action == "write":
            return child.write(request["operationId"], request.get("nativeId"), request["message"])
        if action == "operationStatus":
            operation_id = request.get("operationId")
            if not isinstance(operation_id, str) or not operation_id.startswith("monitor:"):
                raise ValueError("Invalid monitor operation identity")
            with self.journal.db() as db:
                row = db.execute("SELECT native_id,generation,response,response_sequence FROM operations "
                                 "WHERE handle=? AND operation_id=?", (handle, operation_id)).fetchone()
                current, acknowledged = db.execute(
                    "SELECT generation,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
            if not row or row[1] != current or type(row[0]) is not int:
                return {"accepted": False, "reason": (
                    "operation_missing" if not row else
                    "native_generation_changed" if row[1] != current else "rpc_identity_missing")}
            return {"accepted": True, "nativeId": row[0],
                    "response": json.loads(row[2]) if row[2] and type(row[3]) is int
                    and row[3] <= acknowledged else None}
        if action == "ack":
            sequence = request.get("sequence")
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                if type(sequence) is not int or sequence < row[1] or sequence > row[0]:
                    raise ValueError("Supervisor ACK cursor is outside the saved sequence range")
                db.execute("DELETE FROM events WHERE handle=? AND sequence<=?", (handle, sequence))
                db.execute("UPDATE handles SET acknowledged=? WHERE id=?", (sequence, handle))
                db.commit()
            # SQLite checkpoints the WAL automatically. A synchronous TRUNCATE
            # after every event makes the single replay stream wait for disk I/O
            # before it can deliver later RPC replies.
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
            # Reader completion is monotonic. Sample it before the journal
            # query so a just-finished reader cannot hide an event committed
            # after an empty SELECT.
            stdout_reader_alive = child.reader.is_alive()
            stderr_reader_alive = child.stderr.is_alive()
            with self.journal.db() as db:
                row = db.execute("SELECT sequence,acknowledged FROM handles WHERE id=?", (handle,)).fetchone()
                if type(cursor) is not int or cursor < row[1] or cursor > row[0]:
                    raise ValueError("Supervisor replay cursor is stale or ahead of the journal")
                event = db.execute("SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT 1",
                                   (handle, cursor)).fetchone()
            if event:
                return {"event": dict(event), "returnCode": child.process.poll(),
                        "backpressure": child.paused.is_set(),
                        "stdoutReaderAlive": stdout_reader_alive,
                        "stderrReaderAlive": stderr_reader_alive}
            return {"event": None, "returnCode": child.process.poll(),
                    "backpressure": child.paused.is_set(),
                    "stdoutReaderAlive": stdout_reader_alive,
                    "stderrReaderAlive": stderr_reader_alive}
        if action == "detach":
            return {"detached": True}
        raise ValueError("Unknown supervisor action")

    def serve(self, wait_for_lease=False):
        self.lease = (self.root / "supervisor.lock").open("a+")
        announced_wait = False
        while True:
            try:
                flock(self.lease, LOCK_EX | LOCK_NB)
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
                    flock(self.lease, LOCK_UN)
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
            operator = first.get("operator") is True
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
                    if request.get("action") == "adminCloseHandle" and not operator:
                        raise RuntimeError("adminCloseHandle is available only through the operator CLI")
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


def process_launch_command(pid):
    """Read bounded OS arguments without executing or changing the process."""
    if sys.platform == 'darwin':
        import ctypes
        import struct
        libc = ctypes.CDLL(None, use_errno=True)
        mib = (ctypes.c_int * 3)(1, 49, pid)
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
        command = []
        for _ in range(argc):
            end = raw.index(b'\0', position)
            command.append(os.fsdecode(raw[position:end]))
            position = end + 1
    elif sys.platform.startswith('linux'):
        with (Path('/proc') / str(pid) / 'cmdline').open('rb') as stream:
            raw = stream.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024 or not raw.endswith(b'\0'):
            raise ValueError('Invalid native process launch data')
        command = [os.fsdecode(part) for part in raw[:-1].split(b'\0')]
        if not 0 < len(command) <= 4096:
            raise ValueError('Invalid native process argument count')
    else:
        raise RuntimeError('Cannot verify a retained native launch on this platform')
    if not command or not command[0]:
        raise ValueError('Invalid native executable')
    return command


def supervisor_launch_snapshot(root, handle):
    """Read one exact handle and its saved process identity."""
    path = Path(root) / 'supervisor.sqlite3'
    if not path.exists():
        return None
    db = sqlite3.connect(path.absolute().as_uri() + '?mode=ro', uri=True, timeout=.25)
    db.row_factory = sqlite3.Row
    try:
        row = db.execute('SELECT h.signature,h.pid,h.generation,h.closed_at,h.init_result,'
                         'h.sequence,h.acknowledged,c.pid AS identity_pid,c.start_time '
                         'FROM handles h LEFT JOIN child_identities c ON c.handle=h.id '
                         'WHERE h.id=?', (handle,)).fetchone()
        return dict(row) if row else None
    finally:
        db.close()


def retained_native_launch(root, handle, command, env, cwd=None):
    """Prove an existing launch before selecting its executable for reattachment."""
    saved = supervisor_launch_snapshot(root, handle)
    if not saved or saved['closed_at'] is not None:
        return None
    pid, started = saved['pid'], saved['start_time']
    if started and pid == saved['identity_pid'] and process_start_time(pid) is None:
        return None
    if not started or pid != saved['identity_pid'] or not process_start_matches(pid, started, allow_legacy=True):
        raise RuntimeError('Cannot verify the existing supervisor child; native outcome remains unknown')
    actual = process_launch_command(pid)
    if actual[1:] != command[1:]:
        raise RuntimeError('Supervisor native launch settings changed; existing work was preserved')
    native_launch_environment(root, handle, actual, env, cwd)
    current = supervisor_launch_snapshot(root, handle)
    keys = ('signature', 'pid', 'identity_pid', 'start_time', 'generation', 'closed_at')
    if (current is None or any(current[key] != saved[key] for key in keys)
            or not process_start_matches(pid, started, allow_legacy=True)):
        raise RuntimeError('Cannot verify the retained supervisor child; native outcome remains unknown')
    return {'command': actual, 'handle': handle, **{key: current[key] for key in keys}}


def retained_open_receipt(proxy, expected, command, env, cwd):
    """Use the existing status action, which cannot create a native process."""
    if expected.get('handle') != proxy.handle:
        raise RuntimeError('Cannot verify the retained supervisor handle')
    status = proxy.call('status')
    saved = supervisor_launch_snapshot(proxy.root, proxy.handle)
    keys = ('signature', 'pid', 'identity_pid', 'start_time', 'generation', 'closed_at')
    if (saved is None or any(saved[key] != expected.get(key) for key in keys)
            or saved['closed_at'] is not None or status.get('pid') != saved['pid']
            or status.get('returnCode') is not None
            or Supervisor.signature(command, env, cwd) != saved['signature']
            or process_launch_command(saved['pid']) != command
            or not process_start_matches(saved['pid'], saved['start_time'], allow_legacy=True)):
        raise RuntimeError('Cannot verify the retained supervisor child; native outcome remains unknown')
    try:
        initialized = json.loads(saved['init_result'])
    except (TypeError, ValueError):
        initialized = None
    if not isinstance(initialized, dict):
        raise RuntimeError('The retained native initialization receipt is unavailable; native outcome remains unknown')
    return {'resumed': True, 'initResult': initialized, 'generation': saved['generation'],
            'acknowledged': saved['acknowledged'], 'sequence': saved['sequence']}


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


# Backend diagnostics, ownership, and the desktop Node runtime for federation.
# A native child does not need these values in its launch identity.
BACKEND_ONLY_ENVIRONMENT = ('CODEX_AGENTS_BACKEND_ID', 'CODEX_RUNTIME_LOCK_METRICS',
                            'CODEX_AGENTS_PROVIDER_CAPTURE', 'CODEX_AGENTS_PROVIDER_CAPTURE_FILE',
                            'CODEX_NODE')

# Shell and desktop launchers can supply different ambient values after an
# update. Retained children keep their original values and exact signature.
RETAINED_LAUNCHER_ENVIRONMENT = frozenset({
    'PATH', 'LANG', 'LC_CTYPE', 'LC_ALL', '__PYVENV_LAUNCHER__', 'COMMAND_MODE',
    'MallocNanoZone', 'XPC_SERVICE_NAME', '__CFBundleIdentifier',
})


def native_launch_environment(root, handle, command, env, cwd):
    """Exclude backend ownership and diagnostics from native launch settings."""
    clean = dict(env)
    for key in BACKEND_ONLY_ENVIRONMENT:
        clean.pop(key, None)
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
    if started and pid == identity_pid and process_start_time(pid) is None:
        # An exited child has no environment to recover. Let open_handle check
        # its owned Popen exit or durable recovery proof before replacement.
        # An orphaned handle still fails there; this is not replay permission.
        return clean
    if not started or pid != identity_pid or not process_start_matches(pid, started, allow_legacy=True):
        raise RuntimeError('Cannot verify the existing supervisor child; native outcome remains unknown')
    original = process_launch_environment(pid)
    if Supervisor.signature(command, original, cwd) != saved[0]:
        # A macOS Python launcher can add this marker after a script's exec.
        original.pop('__PYVENV_LAUNCHER__', None)
    comparable = dict(original)
    for key in BACKEND_ONLY_ENVIRONMENT:
        comparable.pop(key, None)
    # Reattachment keeps the child's accepted launch, including its desktop
    # and locale settings. A launcher can supply different ambient values. These
    # values never replace the live child's settings; command, account,
    # credentials, provider options and process identity still match exactly.
    for key in RETAINED_LAUNCHER_ENVIRONMENT:
        comparable.pop(key, None)
    requested = {key: value for key, value in clean.items()
                 if key not in RETAINED_LAUNCHER_ENVIRONMENT}
    if (comparable != requested or Supervisor.signature(command, original, cwd) != saved[0]
            or not process_start_matches(pid, started, allow_legacy=True)):
        raise RuntimeError('Supervisor native launch settings changed; existing work was preserved')
    return original


class ProcessProxy:
    """Popen-shaped transport consumed by codex_runtime.AppServer."""
    def __init__(self, root, handle, command, env, cwd, stderr_sink, expected=None):
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
            if expected is not None:
                opened = retained_open_receipt(self, expected, command, env, cwd)
            else:
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
            readers_known = (type(result.get("stdoutReaderAlive")) is bool
                             and type(result.get("stderrReaderAlive")) is bool)
            readers_alive = (result.get("stdoutReaderAlive") is True
                             or result.get("stderrReaderAlive") is True)
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
                        if not readers_known:
                            return None
                        continue
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
            if (result["returnCode"] is not None and event is None
                    and (not readers_known or not readers_alive)):
                # The child readers append synchronously before they exit. This
                # is the terminal fence if a reader could not journal its exit.
                self._returncode = result["returnCode"]
                return None
            time.sleep(.05)
        return None

    def send_write(self, request, operation_id=None):
        method = request.get("method", "reply")
        params = request.get("params", {})
        identity = operation_id or request.get("operationId") or self._operation_identity(method, params, request)
        # A native reply can enter the journal before the write receipt returns.
        # Publish its local ID before next_event can classify that reply.
        with self.event_lock:
            result = self.call("write", operationId=identity, nativeId=request.get("id"), message=request)
            self.last_durable_ms = result["durableMs"]
            if "method" in request and result.get("remoteId") is not None and isinstance(request.get("id"), int):
                self.remote_to_local[result["remoteId"]] = request["id"]
        return result

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


def attach(root, handle, command, env, cwd=None, *, stderr_sink=None, expected=None):
    if os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") != "1":
        return None
    root = Path(root).expanduser().resolve()
    configured = os.environ.get("CODEX_AGENTS_STATE_DIR")
    if configured and Path(configured).expanduser().resolve() != root:
        raise RuntimeError(
            "Supervisor state root mismatch: the caller selected " + str(root)
            + " but CODEX_AGENTS_STATE_DIR selects " + str(Path(configured).expanduser().resolve())
            + ". Start Studio with matching state settings; refusing to contact another supervisor."
        )
    return ProcessProxy(root, handle, command, env, cwd, stderr_sink, expected=expected)


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


def retire_command(root, handle, *, persisted=False):
    """Retire one completed command without opening or killing a process."""
    saved = supervisor_launch_snapshot(root, handle)
    if saved is None:
        return
    client = _connect(Path(root) / 'supervisor.sock', timeout=3)
    reader = client.makefile('r', encoding='utf-8')
    try:
        _send(client, {'protocol': PROTOCOL, 'stateDir': str(Path(root).resolve()),
                      'backendId': os.environ.setdefault('CODEX_AGENTS_BACKEND_ID', str(uuid.uuid4()))})
        hello = _recv(client, reader)
        if hello.get('error'):
            raise RuntimeError('Supervisor retirement connection was refused')
        if persisted:
            # A detached adapter can complete before reattachment. The backend
            # has saved its final snapshot, so older metadata frames need no
            # replay. Use the existing ACK fence before releasing this handle.
            _send(client, {'requestId': 0, 'action': 'ack', 'handle': handle,
                          'sequence': saved['sequence']})
            acknowledged = _recv(client, reader)
            if acknowledged.get('error'):
                raise RuntimeError('Supervisor final command acknowledgement was refused')
        _send(client, {'requestId': 1, 'action': 'retireCommand', 'handle': handle,
                      'generation': saved['generation']})
        result = _recv(client, reader)
        if result.get('error'):
            raise RuntimeError('Supervisor command retirement was refused')
    finally:
        reader.close()
        client.close()


def admin_close_handle(root, handle, expected_pid, expected_start_time, expected_signature):
    root = Path(root).expanduser().resolve()
    # The owner can wait three seconds for TERM and two seconds for KILL.
    client = _connect(root / "supervisor.sock", timeout=10)
    reader = client.makefile("r", encoding="utf-8")
    try:
        _send(client, {"protocol": PROTOCOL, "stateDir": str(root), "operator": True, "probe": True})
        hello = _recv(client, reader)
        if hello.get("error"):
            raise RuntimeError(hello["error"])
        request_id = uuid.uuid4().hex
        _send(client, {"requestId": request_id, "action": "adminCloseHandle", "handle": handle,
                       "expectedPid": expected_pid, "expectedStartTime": expected_start_time,
                       "expectedSignature": expected_signature})
        response = _recv(client, reader)
    finally:
        reader.close()
        client.close()
    if response.get("error"):
        raise RuntimeError(response["error"])
    return response.get("result", {})


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
    raise_open_file_limit()
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--status-json", action="store_true")
    parser.add_argument("--finish-fallback", action="store_true")
    parser.add_argument("--admin-close-handle")
    parser.add_argument("--expected-pid", type=int)
    parser.add_argument("--expected-start-time")
    parser.add_argument("--expected-signature")
    parser.add_argument("--wait-for-lease", action="store_true")
    args = parser.parse_args()
    if args.admin_close_handle:
        if (args.expected_pid is None or not args.expected_start_time or not args.expected_signature):
            parser.error("--admin-close-handle requires --expected-pid, --expected-start-time, and --expected-signature")
        print(json.dumps(admin_close_handle(args.state, args.admin_close_handle, args.expected_pid,
                                            args.expected_start_time, args.expected_signature)))
    elif args.finish_fallback:
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
