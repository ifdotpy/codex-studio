"""Discover and approve installed native executables without account access."""
from __future__ import annotations

import hashlib
import fcntl
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import stat
import struct
import subprocess
import tempfile
import time

CHATGPT_CODEX = Path('/Applications/ChatGPT.app/Contents/Resources/codex')
APPROVAL_REVISION = 2
REQUIRED_COMPANIONS = ('codex-code-mode-host',)
MAX_OUTPUT = 4 * 1024 * 1024
REQUIRED_METHODS = (
    'initialize', 'config/read', 'thread/loaded/list', 'turn/start',
    'thread/metadata/update', 'thread/queue/list',
    'thread/backgroundTerminals/list', 'thread/settings/update', 'model/list',
)


class ProtocolIncompatible(ValueError):
    """The executable schema cannot implement the current Studio contract."""


def version_key(version):
    """Order semantic versions, including numeric alpha suffixes and releases."""
    match = re.fullmatch(r'(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?', version)
    if not match:
        raise ValueError('Native executable did not report a semantic version')
    prerelease = match[4]
    parts = tuple((0, int(part)) if part.isdigit() else (1, part)
                  for part in (prerelease or '').split('.'))
    return (*map(int, match.group(1, 2, 3)), prerelease is None, parts)


def file_identity(path):
    resolved = Path(path).expanduser().resolve(strict=True)
    info = resolved.stat()
    if not stat.S_ISREG(info.st_mode) or not os.access(resolved, os.X_OK):
        raise ValueError('Native candidate is not an executable file')
    return {'path': str(resolved), 'size': info.st_size, 'mtimeNs': info.st_mtime_ns,
            'device': info.st_dev, 'inode': info.st_ino}


def companion_identities(path):
    directory = Path(path).expanduser().resolve(strict=True).parent
    companions = {}
    for name in REQUIRED_COMPANIONS:
        try:
            identity = file_identity(directory / name)
        except (OSError, ValueError):
            raise ValueError(f'Native candidate lacks required executable {name}') from None
        if Path(identity['path']).parent != directory:
            raise ValueError(f'Native companion {name} is outside its source directory')
        companions[name] = identity
    return companions


def bundle_digest(hashes):
    return hashlib.sha256(json.dumps(hashes, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def _environment(home):
    # A clean HOME also prevents project/global config and keychain auth fallback.
    env = {key: os.environ[key] for key in ('PATH', 'SYSTEMROOT', 'LANG', 'LC_ALL', 'TMPDIR')
           if key in os.environ}
    env.update(HOME=str(home), CODEX_HOME=str(home), XDG_CONFIG_HOME=str(home),
               XDG_DATA_HOME=str(home), XDG_CACHE_HOME=str(home))
    return env


def _reap(proc):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
    else:
        proc.wait()
    for pipe in (proc.stdin, proc.stdout, proc.stderr):
        if pipe:
            pipe.close()


def _run(command, home, timeout=15, *, input_bytes=None):
    proc = subprocess.Popen(command, cwd=home, env=_environment(home),
                            stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    output = bytearray()
    total = 0
    deadline = time.monotonic() + timeout
    try:
        if input_bytes is not None:
            os.set_blocking(proc.stdin.fileno(), False)
            if os.write(proc.stdin.fileno(), input_bytes) != len(input_bytes):
                raise RuntimeError('Native validation input did not drain')
            proc.stdin.close()
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ, True)
            selector.register(proc.stderr, selectors.EVENT_READ, False)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('Native validation command timed out')
                for key, _ in selector.select(min(remaining, .25)):
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    total += len(chunk)
                    if total > MAX_OUTPUT:
                        raise ValueError('Native validation output exceeded the size limit')
                    if key.data:
                        output.extend(chunk)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Native validation command timed out')
            if proc.wait(timeout=remaining) != 0:
                raise RuntimeError('Native validation command failed')
        return bytes(output)
    finally:
        _reap(proc)


def _version(path, home):
    output = _run([str(path), '--version'], home).decode('utf-8', errors='replace').strip()
    match = re.fullmatch(r'codex-cli\s+(\S+)', output)
    if not match:
        raise ValueError('Native executable did not report a Codex CLI version')
    version_key(match[1])
    return match[1]


def discover_candidates(*, env=None):
    """Return newest-first candidates plus errors; CODEX_BIN is not a version pin."""
    env = os.environ if env is None else env
    paths = []
    if env.get('CODEX_BIN'):
        explicit = env['CODEX_BIN']
        paths.append(shutil.which(explicit, path=env.get('PATH', '')) or explicit)
    paths.extend(str(Path(entry or '.') / 'codex') for entry in env.get('PATH', '').split(os.pathsep)
                 if (Path(entry or '.') / 'codex').exists())
    if CHATGPT_CODEX.exists():
        paths.append(str(CHATGPT_CODEX))
    rows, seen = [], set()
    with tempfile.TemporaryDirectory(prefix='studio-native-discovery-') as home:
        for path in paths:
            resolved = str(Path(path).expanduser().resolve())
            if resolved in seen:
                continue
            seen.add(resolved)
            try:
                identity = file_identity(resolved)
                companions = companion_identities(resolved)
                identity['companions'] = companions
                rows.append({'path': resolved, 'status': 'discovered', 'version': _version(resolved, home),
                             'identity': identity, 'companionIdentity': companions})
            except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
                # Never return candidate output or environment values as diagnostics.
                rows.append({'path': resolved, 'status': 'rejected', 'error': _safe_error(error)})
    return sorted(rows, key=lambda row: (not bool(row.get('error')),
                  version_key(row['version']) if row.get('version') else ()), reverse=True)


def _safe_error(error):
    if isinstance(error, (ValueError, RuntimeError, TimeoutError)):
        return str(error)[:400]
    return f'Native validation failed ({type(error).__name__})'


def _schema_check(directory):
    def read(name):
        path = Path(directory) / name
        if not path.is_file() or path.stat().st_size > 32 * 1024 * 1024:
            raise ProtocolIncompatible('Native protocol schema is missing or too large')
        try:
            result = json.loads(path.read_text())
        except (ValueError, UnicodeError):
            raise ProtocolIncompatible('Native protocol schema is invalid') from None
        if not isinstance(result, dict):
            raise ProtocolIncompatible('Native protocol schema is invalid')
        return result

    requests = read('ClientRequest.json')
    methods = {method for variant in requests.get('oneOf', [])
               for method in variant.get('properties', {}).get('method', {}).get('enum', [])}
    missing = sorted(set(REQUIRED_METHODS) - methods)
    if missing:
        raise ProtocolIncompatible('Native protocol lacks required methods: ' + ', '.join(missing))
    turn = read('v2/TurnStartParams.json')
    if 'cyberAccessProgram' not in turn.get('properties', {}):
        raise ProtocolIncompatible('Native turn/start lacks cyberAccessProgram')
    programs = turn.get('definitions', {}).get('CyberAccessProgram', {}).get('enum', [])
    if not {'standard', 'daybreakBlue', 'daybreakRed'}.issubset(programs):
        raise ProtocolIncompatible('Native turn/start lacks required Daybreak programs')
    models = read('v2/ModelListResponse.json')
    model = models.get('definitions', {}).get('Model', {})
    if 'availableAccessPrograms' not in model.get('properties', {}):
        raise ProtocolIncompatible('Native model/list lacks availableAccessPrograms for Daybreak')
    return {'methods': list(REQUIRED_METHODS), 'daybreak': True}


def _smoke(path, home, timeout=20):
    command = [str(path), 'app-server', '--listen', 'stdio://',
               '-c', 'cli_auth_credentials_store="file"']
    proc = subprocess.Popen(command, cwd=home, env=_environment(home), stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    deadline = time.monotonic() + timeout
    buffer = bytearray()
    total = 0
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ, True)
            selector.register(proc.stderr, selectors.EVENT_READ, False)
            os.set_blocking(proc.stdin.fileno(), False)

            def send(frame):
                data = json.dumps(frame).encode() + b'\n'
                if os.write(proc.stdin.fileno(), data) != len(data):
                    raise RuntimeError('Native smoke input did not drain')

            def request(number, method, params):
                nonlocal total
                send({'id': number, 'method': method, 'params': params})
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError('Native protocol smoke timed out')
                    while b'\n' in buffer:
                        line, _, tail = buffer.partition(b'\n')
                        buffer[:] = tail
                        try:
                            frame = json.loads(line)
                        except (ValueError, UnicodeError):
                            raise ValueError('Native smoke returned invalid JSON') from None
                        if not isinstance(frame, dict):
                            raise ValueError('Native smoke returned an invalid frame')
                        if 'method' in frame:
                            if 'id' in frame:
                                raise RuntimeError('Native smoke requested an unsupported action')
                            continue
                        if frame.get('id') != number or 'error' in frame or 'result' not in frame:
                            raise RuntimeError(f'Native {method} smoke failed')
                        return frame['result']
                    for key, _ in selector.select(min(remaining, .25)):
                        chunk = os.read(key.fileobj.fileno(), 65536)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            if key.data:
                                raise RuntimeError('Native smoke closed its output')
                            continue
                        total += len(chunk)
                        if total > MAX_OUTPUT:
                            raise ValueError('Native smoke output exceeded the size limit')
                        if key.data:
                            buffer.extend(chunk)

            initialize = request(1, 'initialize', {'clientInfo': {'name': 'codex_studio_update',
                                 'version': '1.0.0'}, 'capabilities': {'experimentalApi': True}})
            if not isinstance(initialize, dict) or not isinstance(initialize.get('userAgent'), str):
                raise ValueError('Native initialize returned an invalid result')
            send({'method': 'initialized'})
            config = request(2, 'config/read', {'includeLayers': False})
            if not isinstance(config, dict) or not isinstance(config.get('config'), dict):
                raise ValueError('Native config/read returned an invalid result')
            loaded = request(3, 'thread/loaded/list', {})
            if not isinstance(loaded, dict) or loaded.get('data') != []:
                raise ValueError('Native isolated smoke contains unexpected loaded threads')
        return ['initialize', 'config/read', 'thread/loaded/list']
    finally:
        _reap(proc)


def _digest(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def _host_smoke(path, home):
    hello = json.dumps({'type': 'connection/hello', 'supportedVersions': [1],
                        'requiredCapabilities': [], 'optionalCapabilities': []}).encode()
    output = _run([str(path), '--listen', 'stdio'], home,
                  input_bytes=struct.pack('<I', len(hello)) + hello)
    if len(output) < 4 or struct.unpack('<I', output[:4])[0] != len(output) - 4:
        raise ValueError('Native Code Mode host returned an invalid IPC frame')
    try:
        response = json.loads(output[4:])
    except (ValueError, UnicodeError):
        raise ValueError('Native Code Mode host returned invalid JSON') from None
    if (not isinstance(response, dict) or response.get('type') != 'connection/ready'
            or response.get('selectedVersion') != 1 or not isinstance(response.get('capabilities'), list)):
        raise ValueError('Native Code Mode host failed its IPC handshake')
    return ['--help', 'connection/hello']


def approve_candidate(path, root):
    """Validate an exact copied executable, then publish it without replacement.

    A successful schema and isolated smoke proves protocol compatibility only.
    No account credentials, model request, existing process, or package is used.
    """
    identity = file_identity(path)
    source = Path(identity['path'])
    companions = companion_identities(source)
    identity['companions'] = companions
    builds = Path(root).resolve() / 'native-runtime' / 'builds'
    builds.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.validate-', dir=builds) as staging:
        staging = Path(staging)
        bundle = staging / 'bundle'
        bundle.mkdir()
        binary = bundle / 'codex'
        shutil.copy2(source, binary)
        for name, companion in companions.items():
            shutil.copy2(companion['path'], bundle / name)
        current = {**file_identity(source), 'companions': companion_identities(source)}
        if current != identity:
            raise RuntimeError('Native candidate changed during its snapshot')
        hashes = {name: _digest(bundle / name) for name in ('codex', *REQUIRED_COMPANIONS)}
        digest = hashes['codex']
        bundle_sha = bundle_digest(hashes)
        home = staging / 'home'
        home.mkdir()
        version = _version(binary, home)
        companion_checks = {}
        for name in REQUIRED_COMPANIONS:
            help_text = _run([str(bundle / name), '--help'], home).decode('utf-8', errors='replace')
            if f'Usage: {name}' not in help_text or '--listen' not in help_text:
                raise ValueError(f'Native companion {name} failed its help smoke')
            companion_checks[name] = _host_smoke(bundle / name, home)
        schemas = staging / 'schemas'
        _run([str(binary), 'app-server', 'generate-json-schema', '--experimental', '--out', str(schemas)],
             home, timeout=30)
        checks = {'schema': _schema_check(schemas), 'smoke': _smoke(binary, home),
                  'companions': companion_checks}
        for name, expected in hashes.items():
            if _digest(bundle / name) != expected:
                raise RuntimeError('Native executable bundle changed during validation')
        target = builds / bundle_sha / 'codex'
        # The lock protects concurrent publishers; rename exposes the complete
        # directory at once. An existing directory, including a partial one,
        # must pass every check and is never replaced or repaired in place.
        with (builds.parent / 'publication.lock').open('a') as publication:
            fcntl.flock(publication, fcntl.LOCK_EX)
            if os.path.lexists(target.parent):
                if target.parent.is_symlink() or not target.parent.is_dir():
                    raise RuntimeError('Approved native bundle has an unexpected path')
                for name, expected in hashes.items():
                    existing = target.parent / name
                    if existing.is_symlink() or not existing.is_file() or _digest(existing) != expected:
                        raise RuntimeError('Approved native executable has an unexpected digest')
                    file_identity(existing)
            else:
                os.rename(bundle, target.parent)
        return {'path': str(target), 'sourcePath': str(source), 'version': version,
                'sha256': digest, 'validatedAt': time.time(), 'checks': checks,
                'bundleSha256': bundle_sha,
                'companions': {name: {'path': str(target.parent / name),
                                     'sourcePath': companion['path'], 'sha256': hashes[name],
                                     'sourceIdentity': companion}
                               for name, companion in companions.items()},
                'approvalRevision': APPROVAL_REVISION,
                'sourceIdentity': identity}
