"""Shared local-delivery journal and artifact identity. No installer or process owner."""
from contextlib import closing, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import tempfile
import uuid

import desktop_updates as updates

# Shared owner of delivery journal names and format values. Units: file names and format versions.
CONTRACT = json.loads(Path(__file__).with_name('delivery-contract.json').read_text())
PHASES = CONTRACT['phases']
JOURNAL_DIRECTORY = CONTRACT['journalDirectory']
STARTUP_INTENT_FILE = CONTRACT['startupIntentFile']
STARTUP_INTENT_VERSION = CONTRACT['startupIntentVersion']
# Maximum bytes in one state-owned delivery record. Unit: bytes.
DELIVERY_RECORD_MAX_BYTES = 2 * 1024 * 1024


class DeliveryError(RuntimeError):
    """The local delivery record or artifact cannot be proved."""


def read_delivery_record(path):
    if not path.exists() and not path.is_symlink():
        return None
    if path.is_symlink():
        raise DeliveryError('The delivery receipt must not be a symlink')
    try:
        record = read_private_json(path)
    except (OSError, ValueError) as error:
        raise DeliveryError('The saved delivery receipt is unreadable') from error
    if not isinstance(record, dict):
        raise DeliveryError('The saved delivery receipt is unreadable')
    request, handles = record.get('request'), record.get('handles')
    if (record.get('phase') not in ('validated', *PHASES.values())
            or type(record.get('beforePid')) is not int or record['beforePid'] <= 0
            or not isinstance(request, dict) or not isinstance(handles, dict)
            or not isinstance(record.get('id'), str)):
        raise DeliveryError('The saved delivery receipt has an invalid phase or record')
    build = updates.valid_build(request.get('build'))
    if (not build or request.get('commit') != build['commit']
            or any(not isinstance(request.get(key), str) or not Path(request[key]).is_absolute()
                   for key in ('stateDir', 'installed'))
            or any(not isinstance(request.get(key), str) or len(request[key]) != 64
                   or any(char not in '0123456789abcdef' for char in request[key])
                   for key in ('bundleHash', 'backendHash'))
            or any(not isinstance(key, str) or not isinstance(value, list) or len(value) != 2
                   or any(type(number) is not int or number <= 0 for number in value)
                   for key, value in handles.items())):
        raise DeliveryError('The saved delivery receipt has invalid request fields')
    if 'startupBackend' in request:
        if 'installedDelivery' in request:
            raise DeliveryError('A delivery cannot select two recovery proof kinds')
        validate_startup_proof(request['startupBackend'])
        if request['startupBackend']['pid'] != record['beforePid']:
            raise DeliveryError('The startup proof names another backend')
    if 'installedDelivery' in request:
        validate_installed_proof(request['installedDelivery'])
    operation = str(uuid.uuid5(uuid.NAMESPACE_URL,
                              'studio-delivery:' + request['stateDir'] + ':' + request['commit']))
    if record['id'] != operation:
        raise DeliveryError('The saved delivery receipt has an invalid request identity')
    return record


def file_hash(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def bundle_hash(app):
    """Bind a retry to every sealed file, path, mode, and symlink of its source bundle."""
    digest = hashlib.sha256()
    for path in sorted(app.rglob('*')):
        mode = path.lstat().st_mode
        digest.update(json.dumps([str(path.relative_to(app)), stat.S_IMODE(mode)],
                                 ensure_ascii=True).encode())
        if stat.S_ISLNK(mode):
            digest.update(b'link:' + os.fsencode(os.readlink(path)))
        elif stat.S_ISREG(mode):
            digest.update(b'file:' + file_hash(path).encode())
        elif stat.S_ISDIR(mode):
            digest.update(b'directory:')
        else:
            raise DeliveryError('The bundle contains an unsupported file type')
        digest.update(b'\0')
    return digest.hexdigest()


def read_handles(state):
    database = state / 'supervisor.sqlite3'
    with closing(sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=5)) as connection:
        rows = connection.execute(
            'SELECT id, pid, generation FROM handles WHERE closed_at IS NULL ORDER BY id').fetchall()
    if any(not isinstance(key, str) or type(pid) is not int or pid <= 0
           or type(generation) is not int or generation <= 0 for key, pid, generation in rows):
        raise DeliveryError('An unresolved provider reservation prevents delivery')
    return {key: [pid, generation] for key, pid, generation in rows}


def journal_path(state, operation):
    folder = state / JOURNAL_DIRECTORY
    if folder.is_symlink():
        raise DeliveryError('The delivery journal directory must not be a symlink')
    return folder / (operation + '.json')



def request_hash(request):
    return hashlib.sha256(json.dumps(request, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def require_startup_target(app, *, installed_recovery=False):
    """The signed target must declare this protocol and supply its recovery resources."""
    resources = app / 'Contents/Resources'
    try:
        contract = read_private_json(resources / 'delivery-contract.json')
        if (not isinstance(contract, dict) or type(contract.get('startupIntentVersion')) is not int
                or any(contract.get(key) != CONTRACT[key] for key in (
                    'receiptFile', 'phases', 'journalDirectory', 'startupIntentFile',
                    'startupIntentVersion', 'startupResources'))):
            raise DeliveryError('The target does not support this startup delivery protocol')
        if installed_recovery and (type(contract.get('installedRecoveryVersion')) is not int
                                   or contract['installedRecoveryVersion'] != CONTRACT['installedRecoveryVersion']):
            raise DeliveryError('The target does not support installed-delivery recovery')
        for name in CONTRACT['startupResources']:
            path = resources / name
            if path.is_symlink() or not path.is_file():
                raise DeliveryError('The target lacks a required startup recovery resource')
    except (OSError, ValueError) as error:
        raise DeliveryError('The target startup delivery contract is unavailable') from error


def valid_hash(value):
    return isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdef' for c in value)


def valid_file_identity(value):
    return (isinstance(value, dict) and set(value) == {'device', 'inode'}
            and type(value['device']) is int and value['device'] >= 0
            and type(value['inode']) is int and value['inode'] > 0)


def valid_executable_identity(value):
    return (isinstance(value, dict) and set(value) == {'device', 'inode', 'sha256'}
            and valid_file_identity({key: value[key] for key in ('device', 'inode')})
            and valid_hash(value['sha256']))


def validate_startup_proof(proof):
    """A process observation, never an HTTP identity or permission to select another PID."""
    from desktop_process import STARTUP_ENVIRONMENT_KEYS, STARTUP_STATE_FILES
    keys = {'version', 'kind', 'pid', 'birth', 'argv', 'environment', 'executable', 'files',
            'previousBuild', 'previousBundleHash', 'supervisorHandles'}
    if (not isinstance(proof, dict) or set(proof) != keys
            or type(proof['version']) is not int or proof['version'] != STARTUP_INTENT_VERSION
            or proof['kind'] != 'startup-backend' or type(proof['pid']) is not int or proof['pid'] < 2
            or not isinstance(proof['birth'], str) or len(proof['birth']) > 80
            or not isinstance(proof['argv'], list) or len(proof['argv']) != 3
            or any(not isinstance(arg, str) or not arg or '\0' in arg for arg in proof['argv'])
            or not isinstance(proof['environment'], dict)
            or not set(proof['environment']).issubset(STARTUP_ENVIRONMENT_KEYS)
            or any(not isinstance(value, str) for value in proof['environment'].values())
            or not valid_executable_identity(proof['executable'])
            or not isinstance(proof['files'], dict) or set(proof['files']) != set(STARTUP_STATE_FILES)
            or any(not valid_file_identity(value) for value in proof['files'].values())
            or not updates.valid_build(proof['previousBuild']) or not valid_hash(proof['previousBundleHash'])
            or not isinstance(proof['supervisorHandles'], list)):
        raise DeliveryError('The startup backend proof is invalid')
    try:
        seconds, micros = proof['birth'].split(':')
        if not seconds.isdecimal() or not micros.isdecimal() or not 0 <= int(micros) < 1_000_000:
            raise ValueError('birth')
    except ValueError as error:
        raise DeliveryError('The startup backend birth identity is invalid') from error
    validate_supervisor_handles(proof['supervisorHandles'])
    return proof


def validate_supervisor_handles(handles):
    ids = set()
    for handle in handles:
        if (not isinstance(handle, dict) or set(handle) != {'id', 'pid', 'signature', 'startTime'}
                or not isinstance(handle['id'], str) or not handle['id'] or handle['id'] in ids
                or type(handle['pid']) is not int or handle['pid'] < 2
                or not valid_hash(handle['signature'])
                or not isinstance(handle['startTime'], str) or not handle['startTime']):
            raise DeliveryError('The startup supervisor proof is invalid')
        ids.add(handle['id'])


def validate_installed_proof(proof):
    """Artifact and retained-owner proof. It contains no process signal target."""
    from desktop_process import STARTUP_STATE_FILES
    fields = {'version', 'kind', 'priorId', 'priorRequestHash', 'priorReceiptHash', 'configHash',
              'previousBuild', 'previousBundleHash', 'executable', 'files', 'supervisorHandles'}
    if isinstance(proof, dict) and proof.get('version') == CONTRACT['installedRecoveryVersion']:
        fields.add('stateOwner')
    if (not isinstance(proof, dict) or set(proof) != fields
            or type(proof['version']) is not int or proof['version'] not in (1, CONTRACT['installedRecoveryVersion'])
            or proof['kind'] != 'installed-delivery' or not valid_operation(proof['priorId'])
            or any(not valid_hash(proof[key]) for key in (
                'priorRequestHash', 'priorReceiptHash', 'configHash', 'previousBundleHash'))
            or not valid_executable_identity(proof['executable'])
            or not updates.valid_build(proof['previousBuild'])
            or not isinstance(proof['files'], dict) or set(proof['files']) != set(STARTUP_STATE_FILES)
            or any(not valid_file_identity(value) for value in proof['files'].values())
            or not isinstance(proof['supervisorHandles'], list)):
        raise DeliveryError('The installed-delivery recovery proof is invalid')
    if proof['version'] == CONTRACT['installedRecoveryVersion']:
        owner = proof['stateOwner']
        if (not isinstance(owner, dict) or set(owner) != {'id','requestHash','backendBuild'}
                or not valid_operation(owner['id']) or not valid_hash(owner['requestHash'])
                or not valid_hash(owner['backendBuild'])):
            raise DeliveryError('The installed-delivery state-owner proof is invalid')
    validate_supervisor_handles(proof['supervisorHandles'])
    return proof


def valid_operation(value):
    try:
        return isinstance(value, str) and str(uuid.UUID(value)) == value
    except ValueError:
        return False


def read_private_bytes(path):
    """Read one bounded owner file without following a link or accepting another user's write."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_mode & 0o022 or info.st_size > DELIVERY_RECORD_MAX_BYTES):
            raise DeliveryError('The delivery owner file is not a bounded, protected regular file')
        body = stream.read(DELIVERY_RECORD_MAX_BYTES + 1)
        if len(body) > DELIVERY_RECORD_MAX_BYTES:
            raise DeliveryError('The delivery owner file exceeds its byte limit')
        return body


def read_private_json(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise DeliveryError('The delivery owner file contains a duplicate field')
            result[key] = value
        return result
    return json.loads(read_private_bytes(path), object_pairs_hook=unique)


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def publish_startup_intent(state, record, *, predecessor=None):
    """Publish only a pointer to the durable request; never replace an unresolved request."""
    path = state / STARTUP_INTENT_FILE
    intent = {'version': STARTUP_INTENT_VERSION, 'id': record['id'],
              'requestHash': request_hash(record['request'])}
    if path.exists() or path.is_symlink():
        saved = read_private_json(path)
        if saved == intent:
            return
        if predecessor is None or saved != predecessor:
            raise DeliveryError('Another startup delivery intent is unresolved')
    descriptor, name = tempfile.mkstemp(dir=state, prefix='.startup-delivery-')
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, 'w') as stream:
            json.dump(intent, stream, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        if predecessor is None:
            os.link(temporary, path)  # Publish complete bytes; never replace a competing request.
        else:
            # The caller holds delivery.lock. Recheck the exact installed predecessor before CAS.
            if read_private_json(path) != predecessor:
                raise DeliveryError('The installed predecessor changed before intent publication')
            os.replace(temporary, path)
        sync_directory(state)
    finally:
        temporary.unlink(missing_ok=True)


def save_journal(state, record):
    path = journal_path(state, record['id'])
    if path.is_symlink() or not path.parent.is_dir():
        raise DeliveryError('The delivery journal is unavailable or a symlink')
    updates.write_json(path, record)
    sync_directory(path.parent)


def server_path(app):
    return app / 'Contents/Resources/workspace/bin/studio-runtime-server'



def clear_startup_intent(state, record):
    with delivery_lease(state):
        path = state / STARTUP_INTENT_FILE
        if not path.exists() and not path.is_symlink():
            return
        expected = {'version': STARTUP_INTENT_VERSION, 'id': record['id'],
                    'requestHash': request_hash(record['request'])}
        if read_private_json(path) != expected:
            raise DeliveryError('Another startup delivery intent replaced the completed request')
        path.unlink()
        sync_directory(state)


@contextmanager
def delivery_lease(state):
    """The existing local-delivery lease serializes both publication and final pointer cleanup."""
    descriptor = os.open(state / 'delivery.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'a+') as lease:
        info = os.fstat(lease.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise DeliveryError('The local delivery lease is not a protected regular file')
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise DeliveryError('Another local delivery is active') from error
        try:
            yield
        finally:
            fcntl.flock(lease, fcntl.LOCK_UN)
