"""Owner-server commands with durable start claims and bounded output."""
from __future__ import annotations

from dataclasses import dataclass, field
import codecs
import hashlib
import secrets
import json
import os
from pathlib import Path, PureWindowsPath
import re
import queue
import sys
import uuid
import selectors
import select
import signal
import subprocess
import threading
import time
from typing import Any

from codex_private_paths import protect_temp_file

# The kqueue API exists only in the BSD and macOS builds of select.
KQUEUE: Any = select

MAX_OUTPUT = 4 * 1024 * 1024
READ_OUTPUT = 64 * 1024
SYNC_SECONDS = 5


def output_expiry(value: dict[str, Any]) -> float | None:
    """Find the command expiry in a server receipt or an output-reference page."""
    if (isinstance(value.get('outputExpiresAt'), (float, int))
            and ('handle' in value or 'outputRef' in value)):
        return float(value['outputExpiresAt'])
    for field in ('value', 'result'):
        nested = value.get(field)
        if isinstance(nested, dict):
            expiry = output_expiry(nested)
            if expiry is not None:
                return expiry
    return None


def expire_tool_output(result: dict[str, Any]) -> dict[str, Any]:
    """Keep command pages out of cached tool replies after their expiry."""
    expiry = result.get('serverOutputExpiresAt')
    if isinstance(expiry, (float, int)) and time.time() >= expiry:
        return {'success': True, 'contentItems': [{'type': 'inputText', 'text':
            json.dumps({'outcome': 'unknown', 'expired': True,
                        'detail': 'The command output has expired. Do not run the command again.'})}]}
    return result


def validate(payload: dict[str, Any], *, allow_foreign_windows_path: bool = False) -> dict[str, Any]:
    cwd, command = payload.get('cwd'), payload.get('command')
    if not isinstance(cwd, str) or '\0' in cwd:
        raise ValueError('Supply an absolute cwd on the selected server')
    native_absolute = Path(cwd).is_absolute()
    foreign_absolute = allow_foreign_windows_path and PureWindowsPath(cwd).is_absolute()
    if not (native_absolute or foreign_absolute):
        raise ValueError('Supply an absolute cwd on the selected server')
    if '..' in Path(cwd).parts or '..' in PureWindowsPath(cwd).parts:
        raise ValueError('The cwd must not contain parent traversal')
    if isinstance(command, str):
        if not command or '\0' in command or len(command.encode()) > 128 * 1024:
            raise ValueError('Supply a command of 1 to 128 KiB without NUL')
    elif (not isinstance(command, list) or not 1 <= len(command) <= 256
          or any(not isinstance(v, str) or '\0' in v for v in command)
          or not command[0] or sum(len(v.encode()) for v in command) > 128 * 1024):
        raise ValueError('Supply command argv of 1 to 256 strings, at most 128 KiB')
    environment = payload.get('env', {})
    if (not isinstance(environment, dict) or any(
            not isinstance(k, str) or not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', k)
            or not isinstance(v, str) or '\0' in v for k, v in environment.items())
            or len(json.dumps(environment).encode()) > 128 * 1024):
        raise ValueError('Supply env additions with valid names and string values, at most 128 KiB')
    timeout, limit = payload.get('timeout', 120), payload.get('output_limit', 256 * 1024)
    if type(timeout) is not int or not 1 <= timeout <= 1800:
        raise ValueError('Supply timeout seconds from 1 to 1800')
    if type(limit) is not int or not 2 <= limit <= MAX_OUTPUT:
        raise ValueError('Supply output_limit bytes from 2 to 4194304')
    return {**payload, 'cwd': cwd, 'command': command, 'env': environment,
            'timeout': timeout, 'output_limit': limit}


@dataclass
class Buffer:
    limit: int
    head: bytearray = field(default_factory=bytearray)
    tail: bytearray = field(default_factory=bytearray)
    total: int = 0

    def append(self, data: bytes) -> None:
        self.total += len(data)
        head_size = (self.limit + 1) // 2
        take = min(len(data), head_size - len(self.head))
        self.head.extend(data[:take])
        self.tail.extend(data[take:])
        tail_size = self.limit - head_size
        if len(self.tail) > tail_size:
            del self.tail[:len(self.tail) - tail_size]

    def read(self, offset: int, limit: int, *, final: bool = True) -> dict[str, Any]:
        if self.total <= self.limit:
            start, data, gap = offset, (self.head + self.tail)[offset:offset + limit], 0
        elif offset < len(self.head):
            start, data, gap = offset, self.head[offset:offset + limit], 0
        else:
            start = max(offset, self.total - len(self.tail))
            data, gap = self.tail[start - (self.total - len(self.tail)):][:limit], start - offset
        # Carry a partial UTF-8 character into the next byte page. A retained
        # tail can start inside a discarded character; account for that gap.
        if gap:
            while data and data[0] & 0xc0 == 0x80:
                data = data[1:]
                start += 1
                gap += 1
        decoder = codecs.getincrementaldecoder('utf-8')(errors='replace')
        text = decoder.decode(bytes(data), final=final and start + len(data) == self.total)
        pending = decoder.getstate()[0]
        if pending:
            data = data[:-len(pending)]
            if not data and self.total > self.limit and offset < len(self.head):
                page = self.read(len(self.head), limit, final=final)
                return page | {'gapBytes': page['offset'] - offset}
        return {'offset': start, 'nextOffset': start + len(data), 'gapBytes': gap,
                'text': text, 'totalBytes': self.total,
                'truncated': self.total > self.limit}


class MarkerRedactor:
    """Do not publish the private marker, even across stream chunks."""
    def __init__(self, secret: str) -> None:
        self.secret = secret.encode()
        self.pending = b''

    def feed(self, data: bytes, *, final: bool = False) -> bytes:
        value = self.pending + data
        size = 0
        if not final:
            for length in range(1, min(len(value), len(self.secret) - 1) + 1):
                if value.endswith(self.secret[:length]):
                    size = length
        self.pending = value[-size:] if size else b''
        return (value[:-size] if size else value).replace(self.secret, b'[private marker]')


def atomic_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex)
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            protect_temp_file(temporary)
            with os.fdopen(fd, 'w', encoding='utf-8') as output:
                json.dump(value, output, ensure_ascii=False)
                output.flush()
                os.fsync(output.fileno())
        except BaseException:
            try:
                os.close(fd)
            except OSError:
                pass
            raise
        os.replace(temporary, path)
        if os.name != 'nt':
            directory = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def job_path(root: Path, key: str, suffix: str) -> Path:
    return root / (hashlib.sha256(key.encode()).hexdigest() + suffix)


def descendant_birth(pid: int) -> str | None:
    if sys.platform.startswith('linux'):
        try:
            # Linux start ticks avoid the second-resolution ps birth stamp.
            fields = Path('/proc/' + str(pid) + '/stat').read_text().rsplit(')', 1)[1].split()
            return None if fields[0] == 'Z' else fields[19]
        except FileNotFoundError:
            return None
    from codex_process_supervisor import process_start_time
    began: str | None = process_start_time(pid)
    return began


def descendant_matches(pid: int, began: str) -> bool:
    return descendant_birth(pid) == began


class Descendants:
    """Track fork ancestry, including children that create another session."""
    def __init__(self, key: str, started_at: float, marker_secret: str | None = None) -> None:
        self.key, self.started_at = key, started_at
        self.marker_secret = marker_secret or secrets.token_hex(32)
        self.known: dict[int, str] = {}
        self.kqueue: Any = None
        self.library: Any = None
        self.root = 0
        self.collected_at = 0.0
        self.stopped: set[int] = set()
        self.unproved: set[int] = set()
        self.forks: dict[int, int] = {}
        self.parents: dict[int, int] = {}
        self.job = None
        if os.name == 'nt':
            from codex_windows_supervisor import create_job
            self.job = create_job()
        elif sys.platform.startswith('linux'):
            import ctypes
            # Orphans remain children of this adapter after their parent exits.
            if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0):
                raise RuntimeError('Cannot retain command descendant ownership')
        elif sys.platform == 'darwin':
            import ctypes
            self.library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
            self.library.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
            self.kqueue = KQUEUE.kqueue()
        else:
            raise ValueError('Command descendant cleanup is unavailable on this platform')

    def bind(self, process: subprocess.Popen[bytes]) -> None:
        process_start_time = descendant_birth
        pid = process.pid
        self.root = pid
        started = process_start_time(pid)
        if not started:
            process.kill()
            process.wait(timeout=2)
            raise RuntimeError('Cannot prove command process identity')
        self.known[pid] = started
        if self.job is not None:
            from codex_windows_supervisor import assign_process, resume_process
            try:
                assign_process(process, self.job)
                resume_process(process)
            except BaseException:
                try:
                    process.kill()
                except OSError:
                    pass
                raise
        if self.kqueue is not None:
            self.kqueue.control([KQUEUE.kevent(pid, filter=KQUEUE.KQ_FILTER_PROC,
                flags=KQUEUE.KQ_EV_ADD | KQUEUE.KQ_EV_ENABLE,
                fflags=KQUEUE.KQ_NOTE_FORK | KQUEUE.KQ_NOTE_EXIT)], 0, 0)

    def collect(self, *, force: bool = False) -> None:
        if self.job is not None:
            return
        process_start_time, process_start_matches = descendant_birth, descendant_matches
        events = self.kqueue.control(None, 4096, 0) if self.kqueue is not None else []
        now = time.monotonic()
        if not force and not events and now - self.collected_at < .1:
            return
        self.collected_at = now
        if self.kqueue is not None:
            for event in events:
                if event.fflags & KQUEUE.KQ_NOTE_FORK:
                    self.forks[event.ident] = self.forks.get(event.ident, 0) + 1
            import ctypes
            pending = list(self.known)
            while pending:
                parent = pending.pop()
                if not process_start_matches(parent, self.known[parent]):
                    continue
                buffer = (ctypes.c_int * 4096)()
                size = self.library.proc_listchildpids(parent, buffer, ctypes.sizeof(buffer))
                if size < 0:
                    raise RuntimeError('The command process tree cannot be read')
                if size >= ctypes.sizeof(buffer):
                    raise RuntimeError('The command process tree exceeds the cleanup limit')
                for pid in buffer[:max(0, size) // ctypes.sizeof(ctypes.c_int)]:
                    if pid and pid not in self.known:
                        began = process_start_time(pid)
                        if began and process_start_matches(parent, self.known[parent]):
                            # Recheck the parent link after reading the child
                            # birth stamp. A PID reused by another parent is not
                            # proof of a command descendant.
                            current = (ctypes.c_int * 4096)()
                            length = self.library.proc_listchildpids(parent, current, ctypes.sizeof(current))
                            if length < 0 or length >= ctypes.sizeof(current):
                                raise RuntimeError('The command child identity cannot be read')
                            if pid not in current[:length // ctypes.sizeof(ctypes.c_int)] or not process_start_matches(pid, began):
                                continue
                            self.known[pid] = began
                            self.parents[pid] = parent
                            try:
                                self.kqueue.control([KQUEUE.kevent(pid, filter=KQUEUE.KQ_FILTER_PROC,
                                    flags=KQUEUE.KQ_EV_ADD | KQUEUE.KQ_EV_ENABLE,
                                    fflags=KQUEUE.KQ_NOTE_FORK | KQUEUE.KQ_NOTE_EXIT)], 0, 0)
                            except ProcessLookupError:
                                if process_start_matches(pid, began):
                                    raise
                            pending.append(pid)
            return
        processes: dict[int, tuple[int, str]] = {}
        for path in Path('/proc').iterdir():
            if not path.name.isdecimal():
                continue
            try:
                fields = (path / 'stat').read_text().rsplit(')', 1)[1].split()
                if fields[0] != 'Z':
                    processes[int(path.name)] = (int(fields[1]), fields[19])
            except (FileNotFoundError, ProcessLookupError):
                continue
        roots = {os.getpid(), *(pid for pid, began in self.known.items() if process_start_matches(pid, began))}
        for _ in range(len(processes)):
            added = set()
            for pid, (parent, began) in processes.items():
                if (parent in roots and pid not in roots and parent in processes
                        and process_start_matches(parent, processes[parent][1]) and process_start_matches(pid, began)):
                    added.add(pid)
                    self.known[pid] = began
                    self.parents[pid] = parent
            if not added:
                break
            roots.update(added)

    def unknown_forks(self) -> int:
        if self.job is not None:
            return 0
        unresolved = len(self.unproved)
        for parent, count in self.forks.items():
            stopped = 0
            for pid, source in self.parents.items():
                if source == parent and (pid in self.stopped or not descendant_matches(pid, self.known[pid])):
                    stopped += 1
            unresolved += max(0, count - stopped)
        return unresolved

    def signal_process(self, pid: int, began: str, signum: int) -> bool:
        if sys.platform.startswith('linux'):
            # Pin the PID before checking its birth. A reused PID cannot change
            # the process addressed by this descriptor after the check.
            fd = os.pidfd_open(pid)
            try:
                if not descendant_matches(pid, began):
                    return False
                signal.pidfd_send_signal(fd, signum)
                return True
            finally:
                os.close(fd)
        if descendant_matches(pid, began):
            os.kill(pid, signum)
            return True
        return False

    def stop(self) -> int:
        if self.job is not None:
            active = self.job.active_processes()
            if active:
                self.job.terminate()
                if not self.job.wait_empty(timeout=3):
                    raise RuntimeError('The Windows command job did not stop before the cleanup deadline')
            self.stopped.update(self.known)
            return max(0, active - 1)
        process_start_matches = descendant_matches
        deadline = time.monotonic() + 3
        while True:
            try:
                self.collect(force=True)
                if self.kqueue is not None:
                    self.marked_processes()
            except (OSError, RuntimeError, subprocess.SubprocessError):
                # A failed scan cannot justify killing an unproved PID. Still
                # stop every process whose identity was already proved.
                self.unproved.add(0)
            alive = {pid: began for pid, began in self.known.items() if process_start_matches(pid, began)}
            if not alive:
                return len(self.stopped - {self.root})
            # Stop forking before the second tree walk. Check the birth time
            # immediately before each signal. Linux also pins the PID with pidfd.
            for pid, began in alive.items():
                try:
                    self.signal_process(pid, began, signal.SIGSTOP)
                except ProcessLookupError:
                    pass
            try:
                self.collect(force=True)
            except (OSError, RuntimeError, subprocess.SubprocessError):
                self.unproved.add(0)
            for pid, began in self.known.items():
                try:
                    if self.signal_process(pid, began, signal.SIGKILL):
                        self.stopped.add(pid)
                except ProcessLookupError:
                    pass
            if time.monotonic() >= deadline:
                raise RuntimeError('Command descendants remain after the cleanup deadline')
            time.sleep(.02)

    def marked_processes(self) -> None:
        from codex_process_supervisor import process_launch_environment, process_start_time, process_start_matches
        rows = subprocess.check_output(['/bin/ps', '-axo', 'pid=,uid='], text=True, timeout=2)
        for line in rows.splitlines():
            pid, uid = map(int, line.split())
            if uid != os.getuid() or pid in self.known or pid == os.getpid():
                continue
            try:
                environment = process_launch_environment(pid)
                if environment.get('STUDIO_EXEC_ID') != self.marker_secret:
                    continue
                began = process_start_time(pid)
                if began and float(began) >= self.started_at and process_start_matches(pid, began):
                    self.known[pid] = began
            except (OSError, RuntimeError, ValueError):
                continue

    def close(self) -> None:
        if self.job is not None:
            self.job.close()
            self.job = None
        if self.kqueue is not None:
            self.kqueue.close()


class ServerExec:
    """The backend attaches to command adapters, never directly to commands."""
    def __init__(self, service: Any) -> None:
        self.service, self.runtime = service, service.runtime
        self.lock = threading.RLock()
        self.active: dict[str, Any] = {}
        self.threads: list[threading.Thread] = []
        self.closed = False
        self.folder = self.runtime.root / 'server-command-output'
        self.folder.mkdir(mode=0o700, exist_ok=True)
        with self.runtime.db() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS runtime_server_exec (
                    id TEXT PRIMARY KEY, owner TEXT NOT NULL, actor TEXT NOT NULL,
                    record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_server_exec_audit (
                    id TEXT PRIMARY KEY, actor TEXT NOT NULL, source_server TEXT NOT NULL,
                    server TEXT NOT NULL, cwd TEXT NOT NULL, argv_hash TEXT NOT NULL,
                    start REAL NOT NULL, end REAL, exit_code INTEGER, signal INTEGER);
                CREATE TABLE IF NOT EXISTS runtime_server_exec_input (
                    id TEXT PRIMARY KEY, handle TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS runtime_server_exec_input_handle
                    ON runtime_server_exec_input(handle);
            ''')
            rows = db.execute("SELECT * FROM runtime_server_exec WHERE json_extract(record,'$.status') IN ('starting','running','unknown')").fetchall()
        for row in rows:
            self._restore(row['owner'], row['actor'], row['id'])
        with self.runtime.read_db() as db:
            completed = db.execute("SELECT id FROM runtime_server_exec WHERE json_extract(record,'$.status') NOT IN ('starting','running','unknown')").fetchall()
        for row in completed:
            self._retire(row['id'])

    def _retire(self, key: str) -> None:
        from codex_process_supervisor import retire_command
        try:
            with self.runtime.read_db() as db:
                saved = db.execute('SELECT record FROM runtime_server_exec WHERE id=?', (key,)).fetchone()
            persisted = bool(saved and json.loads(saved[0])['status'] not in {'starting', 'running'})
            retire_command(self.runtime.root, 'server-command:' + hashlib.sha256(key.encode()).hexdigest(), persisted=persisted)
        except Exception as error:
            self.service._unknown_diagnostic(key, 'exec_retirement', error)

    def _record(self, principal: str, actor: str, key: str) -> dict[str, Any]:
        with self.runtime.read_db() as db:
            row = db.execute('SELECT * FROM runtime_server_exec WHERE id=?', (key,)).fetchone()
        if not row or row['owner'] != principal or row['actor'] != actor:
            raise PermissionError('The command handle belongs to another actor or server')
        return json.loads(row['record'])  # type: ignore[no-any-return]

    def _save(self, principal: str, actor: str, key: str, record: dict[str, Any]) -> None:
        with self.runtime.db() as db:
            previous = db.execute('SELECT record FROM runtime_server_exec WHERE id=?', (key,)).fetchone()
            if (previous and json.loads(previous[0])['status'] not in {'starting', 'running'}
                    and record['status'] in {'starting', 'running'}):
                return
            db.execute('UPDATE runtime_server_exec SET record=? WHERE id=?', (json.dumps(record), key))
            if record['status'] not in {'starting', 'running'}:
                db.execute('UPDATE runtime_server_exec_audit SET end=?,exit_code=?,signal=? WHERE id=?',
                           (record.get('finishedAt'), record.get('exitCode'), record.get('signal'), key))
        if record['status'] not in {'starting', 'running'}:
            self.service.exec_completed(principal, actor, record)

    def _snapshot(self, key: str) -> dict[str, Any] | None:
        path = job_path(self.folder, key, '.output.json')
        return json.loads(path.read_text(encoding='utf-8')) if path.exists() else None

    def _attach(self, principal: str, actor: str, key: str, config: dict[str, Any], *, restore: bool) -> Any:
        from codex_process_supervisor import ProcessProxy, retained_native_launch
        handle = 'server-command:' + hashlib.sha256(key.encode()).hexdigest()
        command = config['adapter']
        expected = retained_native_launch(self.runtime.root, handle, command, config['launchEnv'], str(self.folder)) if restore else None
        if restore and expected is None:
            raise RuntimeError('The retained command adapter is unavailable; outcome unknown')
        proxy = ProcessProxy(self.runtime.root, handle, command, config['launchEnv'], str(self.folder),
                             stderr_sink=lambda _text: None, expected=expected)
        with self.lock:
            if self.closed:
                proxy.detach()
                raise RuntimeError('The command backend is detached')
            self.active[key] = proxy
            thread = threading.Thread(target=self._watch, args=(principal, actor, key, proxy),
                                      daemon=True, name='server-command-' + key[:8])
            self.threads = [t for t in self.threads if t.is_alive()]
            self.threads.append(thread)
            thread.start()
        return proxy

    def _restore(self, principal: str, actor: str, key: str) -> None:
        try:
            snapshot = self._snapshot(key)
            if snapshot and snapshot['record']['status'] not in {'starting', 'running'}:
                self._save(principal, actor, key, snapshot['record'])
                self._retire(key)
                return
            config = json.loads(job_path(self.folder, key, '.config.json').read_text(encoding='utf-8'))
            self._attach(principal, actor, key, config, restore=True)
        except Exception as error:
            self.service._unknown_diagnostic(key, 'exec_restore', error)
            # The adapter can finish between the snapshot read and the native
            # proof. Reconcile that final file without opening another process.
            try:
                snapshot = self._snapshot(key)
                if snapshot and snapshot['record']['status'] not in {'starting', 'running'}:
                    self._save(principal, actor, key, snapshot['record'])
                    self._retire(key)
                    return
            except (OSError, ValueError):
                pass
            record = self._record(principal, actor, key)
            record.update(status='unknown', finishedAt=record.get('finishedAt') or time.time(),
                          error='Command outcome unknown; inspect the target server. The command was not run again.')
            self._save(principal, actor, key, record)

    def start(self, principal: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
        p = validate(payload)
        actor = p.get('actor')
        if not isinstance(actor, str) or not actor or len(actor) > 200:
            raise ValueError('Supply the lead actor identity')
        if not Path(p['cwd']).is_dir():
            raise ValueError('The cwd is not an existing folder on this server')
        from codex_process_supervisor import status, process_launch_command
        try:
            health = status(self.runtime.root)
            if health.get('recovery', {}).get('blocked'):
                raise RuntimeError('Supervisor recovery is blocked')
        except Exception as error:
            raise ValueError('The process supervisor is unavailable. No command was started.') from error
        from codex_shell import monitor_command
        argv = (monitor_command(None, p['command'], p['cwd'], config={
            'shell_environment_policy': {'set': p['env']}})
            if isinstance(p['command'], str) else p['command'])
        record = {'handle': key, 'serverId': self.service.server_id, 'status': 'starting',
                  'startedAt': time.time(), 'finishedAt': None, 'exitCode': None, 'signal': None,
                  'duration': None, 'timedOut': False, 'outputLimit': p['output_limit']}
        record['outputExpiresAt'] = record['startedAt'] + 7 * 86400
        config_path = job_path(self.folder, key, '.config.json')
        # macOS framework launchers replace argv[0] after exec. Launch the
        # current interpreter's OS-reported executable directly so retained
        # command and environment signatures remain exact across restart.
        executable = process_launch_command(os.getpid())[0] if sys.platform == 'darwin' else sys.executable
        config = {'payload': p, 'argv': argv, 'record': record, 'launchEnv': dict(os.environ),
                  'markerSecret': secrets.token_hex(32),
                  'adapter': [executable, '-B', str(Path(__file__).resolve()), '--child', str(config_path)]}
        with self.lock:
            if self.closed or self.runtime.closed:
                raise ValueError('The command service is closed')
            if len(self.active) >= 32:
                raise ValueError('The server already has 32 command handles in progress')
            # The claim precedes supervisor open and the adapter's own Popen fence.
            with self.runtime.db() as db:
                db.execute('INSERT INTO runtime_server_exec VALUES(?,?,?,?)',
                           (key, principal, actor, json.dumps(record)))
                db.execute('INSERT INTO runtime_server_exec_audit VALUES(?,?,?,?,?,?,?,NULL,NULL,NULL)',
                           (key, actor, principal, self.service.server_id, p['cwd'],
                            hashlib.sha256(json.dumps(argv, ensure_ascii=False).encode()).hexdigest(), record['startedAt']))
            atomic_json(config_path, config)
            self._attach(principal, actor, key, config, restore=False)
        if p['timeout'] <= SYNC_SECONDS:
            deadline = time.monotonic() + SYNC_SECONDS + 3
            while time.monotonic() < deadline:
                value = self.read(principal, {'actor': actor, 'handle': key})
                if value['status'] not in {'starting', 'running'}:
                    snapshot = self._snapshot(key)
                    self._save(principal, actor, key, snapshot['record'] if snapshot else record)
                    return value
                time.sleep(.02)
        return record

    def _watch(self, principal: str, actor: str, key: str, proxy: Any) -> None:
        try:
            while not self.closed:
                event = proxy.next_event()
                if event is None:
                    break
                sequence, raw = event
                frame = json.loads(raw)
                if frame.get('method') == 'server_command/status':
                    self._save(principal, actor, key, frame['params'])
                proxy.ack(sequence)
            if not self.closed:
                snapshot = self._snapshot(key)
                record = snapshot['record'] if snapshot else self._record(principal, actor, key)
                if record['status'] in {'starting', 'running'}:
                    record.update(status='unknown', finishedAt=time.time(),
                                  error='The command adapter exited without a result; outcome unknown')
                self._save(principal, actor, key, record)
                proxy.call('retireCommand', generation=proxy.generation)
        except Exception as error:
            if not self.closed:
                self.service._unknown_diagnostic(key, 'exec', error)
                record = self._record(principal, actor, key)
                if record['status'] in {'starting', 'running'}:
                    record.update(status='unknown', finishedAt=time.time(),
                                  error='Command observation failed; inspect the target server. The command was not run again.')
                    self._save(principal, actor, key, record)
        finally:
            proxy.detach()
            with self.lock:
                self.active.pop(key, None)

    def read(self, principal: str, p: dict[str, Any]) -> dict[str, Any]:
        key, actor = p.get('handle'), p.get('actor')
        if not isinstance(key, str) or not isinstance(actor, str):
            raise ValueError('Supply a command handle and actor')
        record = self._record(principal, actor, key)
        if time.time() >= record.get('outputExpiresAt', record['startedAt'] + 7 * 86400):
            raise ValueError('The command output has expired')
        snapshot = self._snapshot(key)
        if snapshot:
            if record['status'] != 'unknown' or snapshot['record']['status'] not in {'starting', 'running'}:
                record = snapshot['record']
        result = dict(record)
        result.setdefault('outputExpiresAt', record['startedAt'] + 7 * 86400)
        for name in ('stdout', 'stderr'):
            offset = p.get(name + '_offset', 0)
            if type(offset) is not int or offset < 0:
                raise ValueError('Supply nonnegative output byte offsets')
            saved = snapshot.get(name, {}) if snapshot else {}
            buffer = Buffer(saved.get('limit', 1))
            buffer.head = bytearray.fromhex(saved.get('head', ''))
            buffer.tail = bytearray.fromhex(saved.get('tail', ''))
            buffer.total = saved.get('total', 0)
            if offset > buffer.total:
                raise ValueError('Output offset exceeds the retained output')
            value = buffer.read(offset, READ_OUTPUT // 2, final=bool(snapshot and snapshot['record'].get('finishedAt')))
            result[name] = value['text']
            result[name + 'Offset'] = value['offset']
            result[name + 'NextOffset'] = value['nextOffset']
            result[name + 'GapBytes'] = value['gapBytes']
            result[name + 'Bytes'] = value['totalBytes']
            result[name + 'Truncated'] = value['truncated']
        return result

    def control(self, principal: str, action: str, p: dict[str, Any], request_id: str) -> dict[str, Any]:
        key, actor = p.get('handle'), p.get('actor')
        if not isinstance(key, str) or not isinstance(actor, str):
            raise ValueError('Supply a command handle and actor')
        record = self._record(principal, actor, key)
        if action == 'exec_input':
            if 'input' not in p and p.get('close_stdin') is True:
                p = {**p, 'input': ''}
            if not isinstance(p.get('input'), str) or len(p['input'].encode()) > 64 * 1024:
                raise ValueError('Supply input text of at most 64 KiB')
            if 'close_stdin' in p and type(p['close_stdin']) is not bool:
                raise ValueError('Supply a Boolean close_stdin value')
        with self.lock:
            proxy = self.active.get(key)
            if proxy is None:
                return {'handle': key, 'status': record['status'], 'accepted': False}
            if action == 'exec_input':
                # At most 32 frames can enter the bounded adapter queue. Never
                # block its stdin reader, which would also block the supervisor
                # journal and cancellation. The claim precedes pipe delivery.
                with self.runtime.db() as db:
                    count = db.execute('SELECT count(*) FROM runtime_server_exec_input WHERE handle=?', (key,)).fetchone()[0]
                    if count >= 32:
                        raise ValueError('The command has accepted 32 input requests. No more input can be sent.')
                    db.execute('INSERT INTO runtime_server_exec_input VALUES(?,?)', (request_id, key))
        proxy.send_write({'method': action, 'params': p}, operation_id='server-command-control:' + request_id)
        return {'handle': key, 'accepted': True}

    def close(self) -> None:
        with self.lock:
            self.closed = True
            proxies, threads = list(self.active.values()), list(self.threads)
        # Closing a backend never cancels the supervised command.
        for proxy in proxies:
            proxy.detach()
        for thread in threads:
            thread.join(timeout=12)
            if thread.is_alive():
                raise RuntimeError('Command observer did not detach; preserve state ownership')

    def prune(self, now: float) -> None:
        with self.runtime.read_db() as db:
            completed = db.execute("SELECT id FROM runtime_server_exec WHERE json_extract(record,'$.status') NOT IN ('starting','running')").fetchall()
        for row in completed:
            self._retire(row['id'])
        with self.lock, self.runtime.db() as db:
            rows = db.execute("SELECT id FROM runtime_server_exec WHERE json_extract(record,'$.startedAt')<?",
                              (now - 7 * 86400,)).fetchall()
            for row in rows:
                if row['id'] in self.active:
                    continue
                for suffix in ('.config.json', '.output.json', '.started'):
                    job_path(self.folder, row['id'], suffix).unlink(missing_ok=True)
                db.execute('DELETE FROM runtime_server_exec_input WHERE handle=?', (row['id'],))
                db.execute('DELETE FROM runtime_server_exec WHERE id=?', (row['id'],))
            db.execute('DELETE FROM runtime_server_exec_audit WHERE end<?', (now - 90 * 86400,))


def child(config_path: Path) -> None:
    config = json.loads(config_path.read_text(encoding='utf-8'))
    p, record = config['payload'], config['record']
    key = record['handle']
    folder = config_path.parent
    output_path = job_path(folder, key, '.output.json')
    stdout = Buffer((p['output_limit'] + 1) // 2)
    stderr = Buffer(p['output_limit'] // 2)
    controls: queue.Queue[dict[str, Any]] = queue.Queue(32)
    shutdown = threading.Event()
    cancel = threading.Event()
    signal.signal(signal.SIGTERM, lambda _signum, _frame: shutdown.set())

    def inputs() -> None:
        for line in sys.stdin:
            control = json.loads(line)
            if control['method'] == 'exec_cancel':
                cancel.set()
            else:
                controls.put(control)

    def publish() -> None:
        atomic_json(output_path, {'record': record, **{name: {
            'limit': buffer.limit, 'head': buffer.head.hex(), 'tail': buffer.tail.hex(), 'total': buffer.total}
            for name, buffer in (('stdout', stdout), ('stderr', stderr))}})
        print(json.dumps({'method': 'server_command/status', 'params': record}), flush=True)

    # The supervisor saves this initialization proof for safe reattachment.
    print(json.dumps({'id': 1, 'result': {'serverCommand': key}}), flush=True)
    began = time.monotonic()
    process = None
    # Legacy adapters that used a public handle cannot use marker-only proof.
    secret = config.get('markerSecret') or secrets.token_hex(32)
    tree = Descendants(key, record['startedAt'], secret)
    redactors = {name: MarkerRedactor(secret) for name in ('stdout', 'stderr')}
    try:
        marker = job_path(folder, key, '.started')
        fd = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.fsync(fd)
        os.close(fd)
        if os.name != 'nt':
            directory = os.open(folder, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        publish()
        threading.Thread(target=inputs, daemon=True).start()
        environment = {**config['launchEnv'], **p['env'], 'STUDIO_EXEC_ID': secret}
        if os.name == 'nt':
            process = subprocess.Popen(config['argv'], cwd=p['cwd'], env=environment,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                creationflags=int(getattr(subprocess, 'CREATE_SUSPENDED', 0x4))
                | int(getattr(subprocess, 'CREATE_NEW_PROCESS_GROUP', 0x200)))
            tree.bind(process)
            assert process.stdin is not None and process.stdout is not None and process.stderr is not None
            stdin_stream = process.stdin
            stdout_stream = process.stdout
            stderr_stream = process.stderr
            record['status'] = 'running'
            publish()
            output_events: queue.Queue[tuple[str, bytes | None]] = queue.Queue(64)
            reader_stop = threading.Event()
            writer_queue: queue.Queue[bytes | None] = queue.Queue(32)

            def emit_output(event: tuple[str, bytes | None]) -> bool:
                while not reader_stop.is_set():
                    try:
                        output_events.put(event, timeout=.1)
                        return True
                    except queue.Full:
                        continue
                return False

            def read_pipe(name: str, stream: Any) -> None:
                try:
                    while True:
                        data = os.read(stream.fileno(), 8192)
                        if not emit_output((name, data or None)) or not data:
                            return
                except (OSError, ValueError):
                    emit_output((name, None))

            def write_pipe() -> None:
                try:
                    while True:
                        data = writer_queue.get()
                        if data is None:
                            break
                        stdin_stream.write(data)
                        stdin_stream.flush()
                except (OSError, ValueError):
                    pass
                finally:
                    try:
                        stdin_stream.close()
                    except OSError:
                        pass

            readers = [threading.Thread(target=read_pipe, args=('stdout', stdout_stream),
                                        daemon=True, name='server-exec-stdout'),
                       threading.Thread(target=read_pipe, args=('stderr', stderr_stream),
                                        daemon=True, name='server-exec-stderr')]
            writer = threading.Thread(target=write_pipe, daemon=True, name='server-exec-stdin')
            for thread in (*readers, writer):
                thread.start()
            ended: set[str] = set()
            close_input = False
            cancelled = False
            killed_at: float | None = None
            win_pending: queue.Queue[bytes] = queue.Queue(32)
            last_publish = time.monotonic()
            while len(ended) < 2 or process.poll() is None:
                now = time.monotonic()
                while not controls.empty():
                    control = controls.get_nowait()
                    if control['method'] == 'exec_cancel':
                        cancelled = True
                    elif control['method'] == 'exec_input':
                        value = control['params'].get('input', '').encode()
                        try:
                            win_pending.put_nowait(value)
                        except queue.Full:
                            record.update(status='unknown', error='Command input queue is full')
                            cancelled = True
                        close_input |= control['params'].get('close_stdin', False)
                cancelled |= shutdown.is_set() or cancel.is_set()
                if killed_at is None and (cancelled or now >= began + p['timeout'] or process.poll() is not None):
                    timed_out = not cancelled and now >= began + p['timeout']
                    record['timedOut'] = timed_out
                    record['stoppedDescendants'] = tree.stop()
                    killed_at = now
                    close_input = True
                while not win_pending.empty() and not writer_queue.full():
                    writer_queue.put_nowait(win_pending.get_nowait())
                if close_input and win_pending.empty() and writer_queue.empty():
                    try:
                        writer_queue.put_nowait(None)
                    except queue.Full:
                        pass
                    close_input = False
                try:
                    name, data = output_events.get(timeout=.05)
                    buffer = stdout if name == 'stdout' else stderr
                    redactor = redactors[name]
                    buffer.append(redactor.feed(data or b'', final=data is None))
                    if data is None:
                        ended.add(name)
                except queue.Empty:
                    pass
                if now - last_publish >= .5:
                    publish()
                    last_publish = now
                if killed_at is not None and now - killed_at > 2:
                    break
            code = process.wait(timeout=3)
            record.update(status='cancelled' if cancelled else 'completed', exitCode=code,
                          signal=None)
            for child_stream in (process.stdin, process.stdout, process.stderr):
                try:
                    if child_stream is not None:
                        child_stream.close()
                except OSError:
                    pass
            for thread in (*readers, writer):
                thread.join(timeout=1)
            reader_stop.set()
        else:
            process = subprocess.Popen([sys.executable, '-B', str(Path(__file__).resolve()), '--command', str(config_path)],
                cwd=p['cwd'], env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, start_new_session=True)
            # The bootstrap stops before exec. Register kernel fork tracking before
            # the command can create and orphan a child in another process group.
            tree.bind(process)
        assert process is not None
        if os.name != 'nt':
            last_publish = time.monotonic()
            while True:
                waited, state = os.waitpid(process.pid, os.WUNTRACED | os.WNOHANG)
                if waited:
                    if not os.WIFSTOPPED(state):
                        raise RuntimeError('The command bootstrap did not stop before execution')
                    break
                cancelled = shutdown.is_set() or cancel.is_set()
                now = time.monotonic()
                if cancelled or now >= began + p['timeout']:
                    record.update(status='cancelled' if cancelled else 'completed',
                                  timedOut=not cancelled, signal=signal.SIGKILL)
                    tree.stop()
                    return
                tree.collect()
                if now - last_publish >= .5:
                    publish()
                    last_publish = now
                time.sleep(.02)
            os.kill(process.pid, signal.SIGCONT)
            assert process.stdin is not None and process.stdout is not None and process.stderr is not None
            os.set_blocking(process.stdin.fileno(), False)
            record['status'] = 'running'
            publish()
            pending_bytes = bytearray()
            close_input = False
            cancelled = False
            killed_at = None
            last_publish = time.monotonic()
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ, (stdout, redactors['stdout']))
                selector.register(process.stderr, selectors.EVENT_READ, (stderr, redactors['stderr']))
                while selector.get_map() or process.poll() is None:
                    tree.collect()
                    now = time.monotonic()
                    while len(pending_bytes) < 64 * 1024 and not controls.empty():
                        control = controls.get_nowait()
                        if control['method'] == 'exec_cancel':
                            cancelled = True
                        elif control['method'] == 'exec_input':
                            pending_bytes.extend(control['params']['input'].encode())
                            close_input |= control['params'].get('close_stdin', False)
                    cancelled |= shutdown.is_set() or cancel.is_set()
                    if killed_at is None and (cancelled or now >= began + p['timeout'] or process.poll() is not None):
                        record['timedOut'] = not cancelled and now >= began + p['timeout']
                        record['stoppedDescendants'] = tree.stop()
                        killed_at = now
                    if killed_at is not None and now - killed_at > 2:
                        break
                    if pending_bytes and not process.stdin.closed:
                        try:
                            count = os.write(process.stdin.fileno(), pending_bytes[:8192])
                            del pending_bytes[:count]
                        except BlockingIOError:
                            pass
                        except BrokenPipeError:
                            pending_bytes.clear()
                    if close_input and not pending_bytes and not process.stdin.closed:
                        process.stdin.close()
                    for selected, _ in selector.select(.05):
                        data = os.read(selected.fd, 8192)
                        buffer, redactor = selected.data
                        buffer.append(redactor.feed(data, final=not data))
                        if not data:
                            selector.unregister(selected.fileobj)
                    if now - last_publish >= .5:
                        publish()
                        last_publish = now
            code = process.wait(timeout=2)
            record.update(status='cancelled' if cancelled else 'completed', exitCode=code if code >= 0 else None,
                          signal=-code if code < 0 else None)
    except FileExistsError:
        record.update(status='unknown', error='A prior command start exists. The command was not run again.')
    except OSError as error:
        record.update(status='failed', error=os.strerror(error.errno) if error.errno else 'Command start failed')
    except Exception:
        record.update(status='unknown', error='Command outcome unknown; inspect the target server')
    finally:
        if process is not None:
            try:
                record['stoppedDescendants'] = tree.stop()
                record['cleanupUnknownForks'] = tree.unknown_forks()
                if record['cleanupUnknownForks']:
                    record.update(status='unknown', error='A descendant fork lost its ancestry; inspect the target server')
            except Exception:
                record.update(status='unknown', error='Descendant cleanup is incomplete; inspect the target server',
                              stoppedDescendants=len(tree.stopped - {tree.root}),
                              cleanupUnknownForks=max(1, tree.unknown_forks()))
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                record.update(status='unknown', error='The command process still runs; inspect the target server',
                              cleanupUnknownForks=max(1, tree.unknown_forks()))
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
        tree.close()
        record.update(duration=time.monotonic() - began, finishedAt=time.time())
        publish()


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--command':
        config = json.loads(Path(sys.argv[2]).read_text(encoding='utf-8'))
        os.kill(os.getpid(), signal.SIGSTOP)
        os.execvpe(config['argv'][0], config['argv'], os.environ)
    if len(sys.argv) != 3 or sys.argv[1] != '--child':
        raise SystemExit('This adapter is started by the process supervisor')
    child(Path(sys.argv[2]))
