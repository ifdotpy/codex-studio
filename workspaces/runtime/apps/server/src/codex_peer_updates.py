"""Updates from a paired server (docs/peer-updates.md).

The backend of the installed app asks paired servers for their build stamp, pulls a newer
build over the signed orchestration channel, verifies it and stages it in the state directory.
It never replaces the installed app: the recovery LaunchAgent runs
`workspaces/client/apps/desktop/install_update.py`, which imports the shared checks below.
"""
from __future__ import annotations

import base64
from collections.abc import Callable
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import threading
import time
from typing import Any, BinaryIO, Protocol, cast
import uuid

from codex_multi_server_transport_limits import MULTI_SERVER_MAX_RESPONSE_BYTES

# The only app path that takes or installs updates. The recovery LaunchAgent exists only for it.
INSTALLED_APP = Path('/Applications/Codex Studio.app')
APP_NAME = INSTALLED_APP.name
BUNDLE_ID = 'local.codex.agents'
# Written by workspaces/client/apps/desktop/package.mjs, relative to the app bundle.
BUILD_STAMP = Path('Contents/Resources/studio-build.json')

# Raw archive bytes in one update.chunk answer. Base64 makes it 4/3 larger; the answer must
# stay below the signed response limit (checked below).
CHUNK_BYTES = 512 * 1024
# Largest app archive a server offers or a puller accepts (an installed app is about 430 MB).
MAX_ARCHIVE_BYTES = 1024 * 1024 * 1024
# Seconds between update checks, and before the first check after a backend start.
PULL_INTERVAL_SECONDS = 600
FIRST_PULL_SECONDS = 120
# Seconds before a build that failed is tried again.
FAILURE_RETRY_SECONDS = 3600
# Seconds that one update.export request waits for the archive before it answers "preparing".
EXPORT_WAIT_SECONDS = 90
# Seconds that a puller waits in total for a peer to prepare its archive.
EXPORT_PREPARE_SECONDS = 1200
# Seconds between two update.export requests while the peer prepares the archive.
EXPORT_POLL_SECONDS = 10
# Seconds for one ditto run (archive or extract an app).
DITTO_SECONDS = 1800
# Seconds for one codesign run.
CODESIGN_SECONDS = 300
# Seconds for the update.offer and update.release requests.
SHORT_REQUEST_SECONDS = 20
# Seconds for one update.chunk request.
CHUNK_REQUEST_SECONDS = 60
# Seconds for the whole archive as one stream over the QUIC link.
STREAM_SECONDS = 900

if 4 * math.ceil(CHUNK_BYTES / 3) + 4096 >= MULTI_SERVER_MAX_RESPONSE_BYTES:
    raise RuntimeError('An update chunk answer would exceed the signed response limit')

# State directory files. The installer reads and writes them too.
PENDING_FILE = 'pending-update.json'
STATUS_FILE = 'peer-update.json'
RECEIPT_FILE = 'update-receipt.json'
UPDATES_DIR = 'updates'
# Name prefix of the private folder in UPDATES_DIR that one pull extracts its archive into.
EXTRACT_PREFIX = '.extract-'
EXPORTS_DIR = 'server-exports'
EXPORT_SUFFIX = '.zip'
EXPORT_METADATA_SUFFIX = '.json'

DITTO = '/usr/bin/ditto'
CODESIGN = '/usr/bin/codesign'
UNKNOWN_ACTION = 'Unknown cross-server operation'
ACTIONS = frozenset({'update.offer', 'update.export', 'update.chunk', 'update.release'})

Runner = Callable[[list[str], float], 'subprocess.CompletedProcess[str]']
Build = dict[str, Any]


class UpdateError(Exception):
    """A step failed. The puller records the reason and skips the build for an hour."""


class Refused(Exception):
    """The paired server refused the request (an older server does not know the action)."""


class Stopped(Exception):
    """The backend stops. The pull ends without a failure record."""


class Transport(Protocol):
    def servers(self) -> list[dict[str, Any]]: ...
    def request(self, server: str, envelope: dict[str, Any], *, timeout: int) -> dict[str, Any]: ...


def run_tool(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False,
                          stdin=subprocess.DEVNULL)


def valid_build(value: object) -> Build | None:
    """The build stamp, or None when the value is not a complete stamp."""
    if not isinstance(value, dict) or value.get('version') != 1:
        return None
    built, commit, commit_time, branch = (value.get('builtAt'), value.get('commit'),
                                          value.get('commitTime'), value.get('branch'))
    if (isinstance(built, bool) or not isinstance(built, (int, float)) or not math.isfinite(built) or built <= 0
            or not isinstance(commit, str) or len(commit) not in (40, 64)
            or any(c not in '0123456789abcdef' for c in commit)
            or type(commit_time) is not int or commit_time <= 0
            or not isinstance(branch, str) or len(branch) > 255):
        return None
    return {'version': 1, 'builtAt': built, 'commit': commit, 'commitTime': commit_time, 'branch': branch}


def read_build(app: Path) -> Build | None:
    try:
        return valid_build(json.loads((app / BUILD_STAMP).read_text()))
    except (OSError, ValueError):
        return None


def installed_app() -> Path | None:
    """The installed app this backend runs from, or None for a source checkout or another copy."""
    parents = Path(__file__).resolve().parents
    if platform.system() != 'Darwin' or len(parents) < 5:
        return None
    # <app>/Contents/Resources/workspace/scripts/codex_peer_updates.py
    app = parents[4]
    if app != INSTALLED_APP or read_build(app) is None:
        return None
    return app


def build_key(build: Build) -> str:
    return f"{build['builtAt']!r}:{build['commit']}"


def build_folder(root: Path, build: Build) -> Path:
    return root / UPDATES_DIR / str(build['builtAt'])


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor, name = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.')
    try:
        with os.fdopen(descriptor, 'w') as output:
            json.dump(value, output, sort_keys=True)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def read_pending(root: Path) -> dict[str, Any] | None:
    """The staged update, or None. The app must be a staged copy in this state directory."""
    value = read_json(root / PENDING_FILE)
    if value is None:
        return None
    build = valid_build(value.get('build'))
    app, source, key = value.get('app'), value.get('from'), value.get('id')
    if build is None or not isinstance(app, str) or not isinstance(source, str) or not isinstance(key, str):
        return None
    if Path(app).resolve() != (build_folder(root, build) / APP_NAME).resolve():
        return None
    return {'id': key, 'app': app, 'build': build, 'from': source}


def remove_tree(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
    elif path.exists():
        shutil.rmtree(path, ignore_errors=True)


def _tail(result: subprocess.CompletedProcess[str]) -> str:
    text = ((result.stderr or '') + (result.stdout or '')).strip()
    return text[-300:]


def requirement(app: Path, run: Runner = run_tool) -> str:
    """The designated requirement of a signed app: it names the signing certificate."""
    result = run([CODESIGN, '-d', '-r-', str(app)], CODESIGN_SECONDS)
    if result.returncode == 0:
        for line in ((result.stdout or '') + '\n' + (result.stderr or '')).splitlines():
            if line.startswith('designated => '):
                return line.removeprefix('designated => ').strip()
    raise UpdateError(f'The designated requirement of {app.name} is unavailable')


def verify_app(app: Path, installed: Path, build: Build, run: Runner = run_tool) -> None:
    """Accept only a complete app signed by the certificate of the installed app, with the
    offered build stamp."""
    if app.is_symlink() or not app.is_dir():
        raise UpdateError('The staged app is missing')
    result = run([CODESIGN, '--verify', '--deep', '--strict', str(app)], CODESIGN_SECONDS)
    if result.returncode != 0:
        raise UpdateError('The signature check failed: ' + _tail(result))
    if requirement(app, run) != requirement(installed, run):
        raise UpdateError('Another certificate signed the build')
    if read_build(app) != build:
        raise UpdateError('The build stamp of the app differs from the offer')


# Offering side: the update.* orchestration actions.

_EXPORT_LOCK = threading.Lock()
_EXPORT_THREADS: dict[str, threading.Thread] = {}
_EXPORT_ERRORS: dict[str, str] = {}
_EXPORT_HOLDERS: dict[str, set[str]] = {}


def export_id(build: Build) -> str:
    return str(uuid.uuid5(uuid.NAMESPACE_URL, 'studio-peer-update:' + build_key(build)))


def _export_paths(root: Path, export: str) -> tuple[Path, Path, Path]:
    folder = root / EXPORTS_DIR
    name = str(uuid.UUID(export))
    return (folder / (name + EXPORT_SUFFIX), folder / (name + EXPORT_METADATA_SUFFIX),
            folder / (name + EXPORT_SUFFIX + '.partial'))


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while data := stream.read(1024 * 1024):
            digest.update(data)
    return digest.hexdigest()


def remove_bytecode(app: Path) -> None:
    """Remove Python bytecode that a process without -B wrote into the installed app. The files
    are caches outside the signature seal; the package makes each cache folder read-only, so
    only an app from before that guard has them."""
    workspace = app / 'Contents' / 'Resources' / 'workspace'
    if not workspace.is_dir():
        return
    for cache in workspace.rglob('__pycache__'):
        if cache.is_symlink() or not cache.is_dir():
            continue
        for path in cache.iterdir():
            if path.suffix == '.pyc' and path.is_file() and not path.is_symlink():
                path.unlink(missing_ok=True)


def _make_export(root: Path, app: Path, build: Build, export: str, run: Runner) -> None:
    archive, metadata, partial = _export_paths(root, export)
    try:
        archive.parent.mkdir(exist_ok=True)
        if shutil.disk_usage(archive.parent).free < MAX_ARCHIVE_BYTES:
            raise UpdateError('Not enough free disk space for the app archive')
        partial.unlink(missing_ok=True)
        remove_bytecode(app)
        # A paired Mac verifies the same seal; an app with changed files cannot pass it on.
        result = run([CODESIGN, '--verify', '--deep', '--strict', str(app)], CODESIGN_SECONDS)
        if result.returncode != 0:
            raise UpdateError('The installed app fails its signature check: ' + _tail(result))
        result = run([DITTO, '-c', '-k', '--keepParent', str(app), str(partial)], DITTO_SECONDS)
        if result.returncode != 0:
            raise UpdateError('ditto could not archive the app: ' + _tail(result))
        size = partial.stat().st_size
        if not 0 < size <= MAX_ARCHIVE_BYTES:
            raise UpdateError('The app archive exceeds the size limit')
        digest = file_digest(partial)
        partial.replace(archive)
        write_json(metadata, {'export': export, 'size': size, 'sha256': digest, 'build': build})
    except (UpdateError, OSError, subprocess.SubprocessError) as error:
        partial.unlink(missing_ok=True)
        with _EXPORT_LOCK:
            _EXPORT_ERRORS[export] = str(error)[:300] or type(error).__name__


def _ready_export(root: Path, export: str, build: Build) -> dict[str, Any] | None:
    archive, metadata, _ = _export_paths(root, export)
    value = read_json(metadata)
    if value is None or value.get('build') != build or not archive.is_file():
        return None
    if archive.stat().st_size != value.get('size'):
        return None
    return {'export': export, 'size': value['size'], 'sha256': value['sha256']}


def receive(root: Path, principal: str, action: str, payload: dict[str, Any]) -> dict[str, Any]:
    """Answer one update.* action of a paired server. Reads only; the export is a cache file."""
    if action not in ACTIONS:
        raise ValueError(UNKNOWN_ACTION)
    app, run = installed_app(), run_tool
    build = read_build(app) if app is not None else None
    if action == 'update.offer':
        return {'build': build}
    if action == 'update.release':
        export = str(uuid.UUID(str(payload.get('export'))))
        with _EXPORT_LOCK:
            holders = _EXPORT_HOLDERS.setdefault(export, set())
            holders.discard(principal)
            if not holders and export not in _EXPORT_THREADS:
                _EXPORT_HOLDERS.pop(export, None)
                for path in _export_paths(root, export):
                    path.unlink(missing_ok=True)
        return {'released': True}
    if action == 'update.chunk':
        try:
            archive = _export_paths(root, str(payload.get('export')))[0]
        except ValueError:
            raise ValueError('Invalid update export') from None
        if not archive.is_file():
            raise ValueError('The update archive has expired or was released')
        offset = payload.get('offset')
        size = archive.stat().st_size
        if type(offset) is not int or not 0 <= offset < size:
            raise ValueError('Invalid update archive offset')
        with archive.open('rb') as stream:
            stream.seek(offset)
            data = stream.read(CHUNK_BYTES)
        return {'offset': offset, 'nextOffset': offset + len(data),
                'data': base64.b64encode(data).decode(), 'sha256': hashlib.sha256(data).hexdigest()}
    # update.export
    if app is None or build is None:
        raise ValueError('This server does not run an installed app')
    if payload.get('builtAt') != build['builtAt']:
        raise ValueError('This server runs another build')
    export = export_id(build)
    ready = _ready_export(root, export, build)
    if ready is None:
        with _EXPORT_LOCK:
            worker = _EXPORT_THREADS.get(export)
            if worker is None:
                _EXPORT_ERRORS.pop(export, None)
                worker = threading.Thread(target=_finish_export, args=(root, app, build, export, run),
                                          name='studio-update-export', daemon=True)
                _EXPORT_THREADS[export] = worker
                worker.start()
        worker.join(EXPORT_WAIT_SECONDS)
        with _EXPORT_LOCK:
            error = None if worker.is_alive() else _EXPORT_ERRORS.pop(export, None)
        if error:
            raise ValueError(error)
        ready = _ready_export(root, export, build)
        if ready is None:
            return {'export': export, 'preparing': True}
    with _EXPORT_LOCK:
        _EXPORT_HOLDERS.setdefault(export, set()).add(principal)
    return ready


def _finish_export(root: Path, app: Path, build: Build, export: str, run: Runner) -> None:
    try:
        _make_export(root, app, build, export, run)
    finally:
        with _EXPORT_LOCK:
            _EXPORT_THREADS.pop(export, None)


# Pulling side.

class Puller:
    """One pull pass at a time. It never writes outside the state directory."""

    def __init__(self, root: Path, app: Path, transport: Transport, enabled: Callable[[], bool], *,
                 run: Runner = run_tool, clock: Callable[[], float] = time.time,
                 stop: threading.Event | None = None) -> None:
        self.root, self.app, self.transport, self.enabled = root.resolve(), app, transport, enabled
        self.run, self.clock = run, clock
        self.stop = stop or threading.Event()
        self.lock = threading.Lock()

    def _call(self, server: str, action: str, payload: dict[str, Any], timeout: int) -> dict[str, Any]:
        # Every update.* action is a read, so a new request ID per call is safe.
        key = str(uuid.uuid4())
        reply = self.transport.request(server, {'requestId': key, 'action': action, 'payload': payload},
                                       timeout=timeout)
        if not isinstance(reply, dict) or reply.get('requestId') != key:
            raise UpdateError('The paired server returned an invalid receipt')
        if reply.get('outcome') == 'not_applied':
            raise Refused(str(reply.get('error') or 'The paired server refused the request'))
        value = reply.get('value')
        if reply.get('outcome') != 'applied' or not isinstance(value, dict):
            raise UpdateError('The paired server did not answer')
        return value

    def _stream(self, server: str, export: str, size: int, stream: BinaryIO, whole: Any) -> int:
        """Receive the archive as one stream over the QUIC link (docs/peer-link.md). Returns the
        bytes received; the chunk actions continue from there."""
        from codex_server_transport import receive_export

        def write(data: bytes) -> None:
            stream.write(data)
            whole.update(data)

        return receive_export(self.transport, server, export + EXPORT_SUFFIX, 0, size, write, STREAM_SECONDS)

    def offers(self, local: Build) -> list[tuple[Build, str]]:
        found = []
        for server in self.transport.servers():
            server_id = server.get('id')
            if not isinstance(server_id, str) or server.get('reachability') == 'unreachable':
                continue
            try:
                value = self._call(server_id, 'update.offer', {}, SHORT_REQUEST_SECONDS)
            except Exception:
                # An offline server, or an older one that refuses the action, offers nothing.
                continue
            build = valid_build(value.get('build'))
            if build is not None and build['builtAt'] > local['builtAt']:
                found.append((build, server_id))
        return sorted(found, key=lambda item: item[0]['builtAt'], reverse=True)

    def _recent_failure(self, status: dict[str, Any], build: Build) -> bool:
        now = self.clock()
        failure = status['failures'].get(build_key(build))
        if isinstance(failure, dict) and now - failure['at'] < FAILURE_RETRY_SECONDS:
            return True
        receipt = read_json(self.root / RECEIPT_FILE)
        return bool(receipt and receipt.get('outcome') == 'failed' and receipt.get('build') == build
                    and type(receipt.get('at')) in (int, float) and now - receipt['at'] < FAILURE_RETRY_SECONDS)

    def pull(self) -> dict[str, Any]:
        """One check of all paired servers. Returns what it did."""
        if not self.lock.acquire(blocking=False):
            return {'outcome': 'busy'}
        try:
            local = read_build(self.app)
            if local is None:
                return {'outcome': 'no-build'}
            if not self.enabled():
                return {'outcome': 'disabled'}
            offers = self.offers(local)
            status = read_json(self.root / STATUS_FILE) or {}
            status['checkedAt'] = self.clock()
            saved = status.get('failures')
            failures = {key: value for key, value in (saved.items() if isinstance(saved, dict) else ())
                        if isinstance(value, dict) and type(value.get('at')) in (int, float)
                        and self.clock() - value['at'] < 24 * 3600}
            status['failures'] = failures
            pending = read_pending(self.root)
            if pending and pending['build']['builtAt'] <= local['builtAt']:
                # Another install already brought this build or a newer one.
                (self.root / PENDING_FILE).unlink(missing_ok=True)
                remove_tree(Path(pending['app']).parent)
                pending = None
            choice = next(((build, server) for build, server in offers
                           if not self._recent_failure(status, build)), None)
            if choice is None or (pending and pending['build']['builtAt'] >= choice[0]['builtAt']):
                write_json(self.root / STATUS_FILE, status)
                return {'outcome': 'none'}
            build, server = choice
            try:
                app = self._fetch(server, build)
            except Stopped:
                return {'outcome': 'stopped'}
            except (UpdateError, Refused, OSError, ValueError, KeyError, TypeError,
                    subprocess.SubprocessError) as error:
                reason = str(error)[:300] or type(error).__name__
                failures[build_key(build)] = {'at': self.clock(), 'reason': reason, 'server': server}
                event: dict[str, Any] = {'at': self.clock(), 'server': server, 'build': build,
                                         'outcome': 'failed', 'reason': reason}
                status['lastPull'] = event
                write_json(self.root / STATUS_FILE, status)
                return event
            update_id = str(uuid.uuid5(uuid.NAMESPACE_URL, 'studio-peer-update:' + server + ':' + build_key(build)))
            write_json(self.root / PENDING_FILE, {'id': update_id, 'app': str(app), 'build': build, 'from': server})
            if pending and Path(pending['app']).parent != app.parent:
                remove_tree(Path(pending['app']).parent)
            event = {'at': self.clock(), 'server': server, 'build': build, 'outcome': 'staged', 'reason': None}
            status['lastPull'] = event
            write_json(self.root / STATUS_FILE, status)
            return event
        finally:
            self.lock.release()

    def _wait(self, seconds: float) -> None:
        if self.stop.wait(seconds):
            raise Stopped()

    def _fetch(self, server: str, build: Build) -> Path:
        updates = self.root / UPDATES_DIR
        updates.mkdir(mode=0o700, exist_ok=True)
        deadline = time.monotonic() + EXPORT_PREPARE_SECONDS
        while True:
            if self.stop.is_set():
                raise Stopped()
            meta = self._call(server, 'update.export', {'builtAt': build['builtAt']}, EXPORT_WAIT_SECONDS + 30)
            if not meta.get('preparing'):
                break
            if time.monotonic() >= deadline:
                raise UpdateError('The paired server did not prepare the app archive in time')
            self._wait(EXPORT_POLL_SECONDS)
        export, size, digest = meta.get('export'), meta.get('size'), meta.get('sha256')
        if not isinstance(export, str):
            raise UpdateError('Invalid update export')
        archive = updates / (str(build['builtAt']) + EXPORT_SUFFIX + '.partial')
        folder = build_folder(self.root, build)
        for leftover in updates.glob(EXTRACT_PREFIX + '*'):
            remove_tree(leftover)  # From a pull that a backend exit interrupted.
        extraction: Path | None = None
        try:
            uuid.UUID(export)
            if (type(size) is not int or not 0 < size <= MAX_ARCHIVE_BYTES or not isinstance(digest, str)
                    or len(digest) != 64):
                raise UpdateError('Invalid update archive size or checksum')
            if shutil.disk_usage(updates).free < size * 4:
                raise UpdateError('Not enough free disk space for the update')
            whole = hashlib.sha256()
            with archive.open('wb') as stream:
                offset = self._stream(server, export, size, stream, whole)
                while offset < size:
                    if self.stop.is_set():
                        raise Stopped()
                    chunk = self._call(server, 'update.chunk', {'export': export, 'offset': offset},
                                       CHUNK_REQUEST_SECONDS)
                    data = base64.b64decode(str(chunk.get('data')), validate=True)
                    if (not data or len(data) > CHUNK_BYTES or chunk.get('offset') != offset
                            or chunk.get('nextOffset') != offset + len(data) or offset + len(data) > size
                            or hashlib.sha256(data).hexdigest() != chunk.get('sha256')):
                        raise UpdateError('Invalid update archive chunk')
                    stream.write(data)
                    whole.update(data)
                    offset += len(data)
            if whole.hexdigest() != digest:
                raise UpdateError('The update archive checksum differs')
            remove_tree(folder)
            # A new private folder: only the archive can add entries to it. The build folder has
            # a predictable name, and another program can write into it during the extraction
            # (Finder writes .DS_Store into each folder that one of its windows shows).
            extraction = Path(tempfile.mkdtemp(prefix=EXTRACT_PREFIX, dir=updates))
            result = self.run([DITTO, '-x', '-k', str(archive), str(extraction)], DITTO_SECONDS)
            if result.returncode != 0:
                raise UpdateError('ditto could not extract the update: ' + _tail(result))
            entries = sorted(entry.name for entry in extraction.iterdir())
            if entries != [APP_NAME]:
                raise UpdateError(('The update archive does not hold only the app: '
                                   + ', '.join(repr(name) for name in entries))[:300])
            verify_app(extraction / APP_NAME, self.app, build, self.run)
            remove_tree(folder)
            folder.mkdir(mode=0o700)
            app = folder / APP_NAME
            (extraction / APP_NAME).rename(app)
            return app
        except BaseException:
            remove_tree(folder)
            raise
        finally:
            if extraction is not None:
                remove_tree(extraction)
            archive.unlink(missing_ok=True)
            try:
                self._call(server, 'update.release', {'export': export}, SHORT_REQUEST_SECONDS)
            except Exception:
                pass  # The peer removes old exports after a day.


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _text(value: object) -> str | None:
    return value[:300] if isinstance(value, str) else None


def _event(value: object) -> dict[str, Any] | None:
    if not isinstance(value, dict):
        return None
    return {'at': _number(value.get('at')), 'server': _text(value.get('server', value.get('from'))),
            'build': valid_build(value.get('build')), 'outcome': _text(value.get('outcome')),
            'reason': _text(value.get('reason'))}


def status(root: Path, app: Path | None = None) -> dict[str, Any]:
    """The update status for GET /api/desktop: the local build, the last pull and install."""
    app = installed_app() if app is None else app
    saved = read_json(root / STATUS_FILE) or {}
    pending = read_pending(root)
    return {
        'build': read_build(app) if app is not None else None,
        'checkedAt': _number(saved.get('checkedAt')),
        'lastPull': _event(saved.get('lastPull')),
        'pending': {'build': pending['build'], 'server': pending['from']} if pending else None,
        'lastInstall': _event(read_json(root / RECEIPT_FILE)),
    }


class Service:
    """The background puller of one runtime."""

    def __init__(self, puller: Puller) -> None:
        self.puller = puller
        self.thread = threading.Thread(target=self._loop, name='studio-peer-updates', daemon=True)

    def _loop(self) -> None:
        if self.puller.stop.wait(FIRST_PULL_SECONDS):
            return
        while True:
            try:
                self.puller.pull()
            except Exception:
                pass  # A failed pass leaves its record; the next pass runs on time.
            if self.puller.stop.wait(PULL_INTERVAL_SECONDS):
                return


_START_LOCK = threading.Lock()


def start(runtime: Any) -> None:
    """Start the puller in the backend of the installed app only."""
    app = installed_app()
    if app is None:
        return
    root = Path(runtime.root)

    def enabled() -> bool:
        return bool(runtime.paired_access().discovery().auto_update_enabled())

    with _START_LOCK:
        if runtime.__dict__.get('_peer_updates') is None:
            service = Service(Puller(root, app, cast(Transport, runtime.multi_server().transport), enabled))
            runtime._peer_updates = service
            service.thread.start()


def close(runtime: Any) -> None:
    service = runtime.__dict__.get('_peer_updates')
    if service is not None:
        service.puller.stop.set()
