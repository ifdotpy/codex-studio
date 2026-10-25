"""Stage a verified Mac bundle for the existing recovery owner, then check its result."""
from __future__ import annotations

import argparse
from contextlib import closing
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import sys
import tempfile
import time
from urllib.parse import urlparse
import urllib.request
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
if sys.version_info < (3, 11):
    raise SystemExit('Mac delivery needs Python 3.11 or newer, as the recovery helper does')
import desktop_updates as updates
import recover_backend as recovery

# Shared owner of the receipt filename and phases consumed by the Node command router.
CONTRACT = json.loads(Path(__file__).with_name('delivery-contract.json').read_text())
PHASES = CONTRACT['phases']
# Maximum wait for the existing recovery owner and its replacement backend, in seconds.
INSTALL_WAIT_SECONDS = 900
# Delay between observations of the same install request, in seconds.
POLL_SECONDS = 2
# Timeout for one loopback backend observation, in seconds.
HTTP_SECONDS = 10
# State-owned request journals survive cache loss and installer stage cleanup.
JOURNAL_DIRECTORY = 'local-deliveries'


class DeliveryError(Exception):
    """The delivery is refused, failed, or cannot yet be proved complete."""


class DeliveryPending(DeliveryError):
    """The receipt exists, but its replacement backend is not ready yet."""


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_receipt(path, record):
    updates.write_json(path, record)
    sync_directory(path.parent)


def journal_path(state, operation):
    folder = state / JOURNAL_DIRECTORY
    if folder.is_symlink():
        raise DeliveryError('The delivery journal directory must not be a symlink')
    return folder / (operation + '.json')


def save_record(state, record_path, record):
    journal = journal_path(state, record['id'])
    journal.parent.mkdir(mode=0o700, exist_ok=True)
    # Persist the new directory entry before a journal can authorize publication.
    sync_directory(state)
    write_receipt(journal, record)
    record_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    write_receipt(record_path, record)


def validate_receipt_directory(receipts, source, installed, state=None):
    protected = [source.resolve(), installed.resolve(), Path(__file__).resolve().parents[4]]
    if state is not None:
        protected.append(state.resolve())
    if any(receipts == folder or folder in receipts.parents for folder in protected):
        raise DeliveryError('The receipt directory must be outside bundles, runtime state, and the checkout')


def read_delivery_record(path):
    if not path.exists() and not path.is_symlink():
        return None
    if path.is_symlink():
        raise DeliveryError('The delivery receipt must not be a symlink')
    record = updates.read_json(path)
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


def server_path(app):
    return app / 'Contents/Resources/workspace/bin/studio-runtime-server'


def read_handles(state):
    database = state / 'supervisor.sqlite3'
    with closing(sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=5)) as connection:
        rows = connection.execute(
            'SELECT id, pid, generation FROM handles WHERE closed_at IS NULL ORDER BY id').fetchall()
    if any(not isinstance(key, str) or type(pid) is not int or pid <= 0
           or type(generation) is not int or generation <= 0 for key, pid, generation in rows):
        raise DeliveryError('An unresolved provider reservation prevents delivery')
    return {key: [pid, generation] for key, pid, generation in rows}


def observe_desktop(url):
    parsed = urlparse(url)
    if (parsed.scheme != 'http' or parsed.hostname not in ('127.0.0.1', 'localhost', '::1')
            or parsed.username or parsed.password or parsed.path not in ('', '/')
            or parsed.query or parsed.fragment):
        raise DeliveryError('The delivery URL must be a loopback HTTP origin')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), recovery.NoRedirect())
    with opener.open(url.rstrip('/') + '/api/desktop', timeout=HTTP_SECONDS) as response:
        data = json.load(response)
    if not isinstance(data, dict):
        raise DeliveryError('The desktop response is not an object')
    data['_deliveryPort'] = parsed.port or 80
    return data


def check_backend(identity, installed):
    installed = Path(installed).resolve()
    if (not isinstance(identity, dict) or identity.get('backendKind') != 'rust'
            or identity.get('supervisorMode') is not True
            or type(identity.get('pid')) is not int or identity['pid'] <= 0
            or not isinstance(identity.get('stateDir'), str)
            or not Path(identity['stateDir']).is_absolute()):
        raise DeliveryError('A verified Rust backend with its existing supervisor is required')
    try:
        recovery.validate_identity(identity, Path(identity['stateDir']))
    except RuntimeError as error:
        raise DeliveryError(str(error)) from error
    try:
        state = Path(identity['stateDir']).resolve(strict=True)
    except OSError as error:
        raise DeliveryError('The running state directory cannot be verified') from error
    try:
        config = recovery.load_config(state / 'background-recovery.json', state)
    except (KeyError, RuntimeError) as error:
        raise DeliveryError('The recovery configuration cannot be verified') from error
    # The native client passes resource_dir()/workspace to recovery; the
    # desktop recovery owner also supports the bundle's Resources root.
    resources = installed / 'Contents/Resources'
    recovery_resources = (resources, resources / 'workspace')
    if (not config or not isinstance(config.get('stateDir'), str)
            or Path(config['stateDir']).resolve() != state or config.get('backend') != 'rust'
            or config.get('supervisorEnabled') is not True
            or not isinstance(config.get('runtimeServer'), str)
            or Path(config['runtimeServer']).resolve() != server_path(installed)
            or not isinstance(config.get('resources'), str)
            or Path(config['resources']).resolve() not in recovery_resources):
        raise DeliveryError('The recovery configuration does not match the running state and installed app')
    if identity.get('backendBuild') != file_hash(server_path(installed)):
        raise DeliveryError('The running backend does not match the installed binary')
    if identity.get('_deliveryPort', config['port']) != config['port']:
        raise DeliveryError('The observed listener differs from the configured backend port')
    try:
        recovery.verify_backend_process(identity, config)
    except RuntimeError as error:
        raise DeliveryError(str(error)) from error
    return state


def publish_pending(state, pending):
    """Publish a complete request without replacing a concurrent or malformed pending file."""
    descriptor, name = tempfile.mkstemp(dir=state, prefix='.delivery-pending-')
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, 'w') as stream:
            json.dump(pending, stream, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, state / updates.PENDING_FILE)
        sync_directory(state)
    finally:
        temporary.unlink(missing_ok=True)


def stage(source, commit, installed, receipts, *, observe, run=updates.run_tool, dry_run=False):
    source, installed, receipts = Path(source), Path(installed), Path(receipts)
    if source.is_symlink() or installed.is_symlink():
        raise DeliveryError('The source and installed apps must not be symlinks')
    source, installed, receipts = source.resolve(), installed.resolve(), receipts.resolve()
    validate_receipt_directory(receipts, source, installed)
    if source == installed or source.name != updates.APP_NAME:
        raise DeliveryError('Provide a separate packaged Codex Studio.app')
    build = updates.read_build(source)
    if not build or build['commit'] != commit:
        raise DeliveryError('The package does not match the requested commit')
    updates.verify_app(source, installed, build, run)
    identity = observe()
    state = check_backend(identity, installed)
    validate_receipt_directory(receipts, source, installed, state)
    request = {'commit': commit, 'build': build, 'bundleHash': bundle_hash(source),
               'backendHash': file_hash(server_path(source)), 'stateDir': str(state),
               'installed': str(installed)}
    operation = str(uuid.uuid5(uuid.NAMESPACE_URL, 'studio-delivery:' + str(state) + ':' + commit))
    owner_receipt = updates.read_json(state / updates.RECEIPT_FILE) or {}
    if owner_receipt.get('id') == operation and owner_receipt.get('outcome') in ('failed', 'obsolete'):
        raise DeliveryError('The recovery owner reports delivery ' + str(owner_receipt['outcome']))
    record_path = receipts / CONTRACT['receiptFile']
    cached = read_delivery_record(record_path)
    durable_path = journal_path(state, operation)
    durable = read_delivery_record(durable_path)
    for saved in (cached, durable):
        if saved is not None and (saved.get('request') != request or saved.get('id') != operation):
            raise DeliveryError('The saved delivery request names a different package or target')
    previous = durable or cached
    if previous is not None and (previous.get('request') != request or previous.get('id') != operation):
        raise DeliveryError('The saved delivery request names a different package or target')
    pending_path = state / updates.PENDING_FILE
    if previous:
        pending = updates.read_pending(state)
        receipt = updates.read_json(state / updates.RECEIPT_FILE) or {}
        if previous.get('phase') == PHASES['installed'] or (receipt.get('id') == operation
                                                   and receipt.get('outcome') == 'installed'):
            return verify_completion(previous, installed, observe)
        if pending and pending['id'] == operation and pending['build'] == build:
            return previous
        if receipt.get('id') == operation and receipt.get('outcome') in ('failed', 'obsolete'):
            raise DeliveryError('The recovery owner reports delivery ' + str(receipt['outcome']))
        if previous.get('phase') in (PHASES['publishing'], PHASES['submitted']):
            raise DeliveryError('The delivery outcome is unknown; keep this request and inspect the recovery receipt')
    if pending_path.exists() or pending_path.is_symlink():
        raise DeliveryError('Another update is pending; inspect its receipt before delivery')
    handles = read_handles(state)
    record = previous or {'id': operation, 'request': request, 'beforePid': identity['pid'],
                          'handles': handles, 'phase': 'validated'}
    if dry_run:
        return {**record, 'phase': 'dry_run'}
    if (updates.read_build(installed) == build
            and identity.get('backendBuild') == request['backendHash']
            and bundle_hash(installed) == request['bundleHash']):
        return {**record, 'phase': 'already_installed'}
    receipts.mkdir(parents=True, exist_ok=True, mode=0o700)
    # This lease serializes local delivery commands. The pending link also protects other writers.
    with (state / 'delivery.lock').open('a+') as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise DeliveryError('Another local delivery is active') from error
        if pending_path.exists() or pending_path.is_symlink():
            raise DeliveryError('Another update is pending')
        # Recheck the cache inside the lease. Never replace a receipt saved by another invocation.
        saved = read_delivery_record(record_path)
        if saved != cached or read_delivery_record(durable_path) != durable:
            raise DeliveryError('The saved delivery changed; retry this request after inspection')
        save_record(state, record_path, record)
        folder = updates.build_folder(state, build)
        marker = folder / 'local-delivery.json'
        if folder.exists():
            if updates.read_json(marker) != {'id': operation, 'request': request}:
                raise DeliveryError('The stage folder belongs to another delivery')
        else:
            folder.mkdir(parents=True, mode=0o700)
            updates.write_json(marker, {'id': operation, 'request': request})
        staged = folder / updates.APP_NAME
        if not staged.exists():
            temporary = Path(tempfile.mkdtemp(dir=folder, prefix='.copy-'))
            copy = temporary / updates.APP_NAME
            result = run([updates.DITTO, str(source), str(copy)], updates.DITTO_SECONDS)
            if result.returncode:
                raise DeliveryError('The package copy failed; its partial stage is kept for inspection')
            updates.verify_app(copy, installed, build, run)
            if bundle_hash(copy) != request['bundleHash']:
                raise DeliveryError('The copied package differs from the verified request')
            os.rename(copy, staged)
            temporary.rmdir()
        updates.verify_app(staged, installed, build, run)
        if bundle_hash(staged) != request['bundleHash']:
            raise DeliveryError('The staged package differs from the verified request')
        record = {**record, 'phase': PHASES['publishing'], 'app': str(staged)}
        save_record(state, record_path, record)
        pending = {'id': operation, 'app': str(staged), 'build': build, 'from': 'local-delivery'}
        try:
            publish_pending(state, pending)
        except FileExistsError as error:
            raise DeliveryError('Another update became pending; no request was replaced') from error
        record = {**record, 'phase': PHASES['submitted']}
        save_record(state, record_path, record)
        return record


def verify_completion(record, installed, observe):
    request = record['request']
    state = Path(request['stateDir'])
    receipt = updates.read_json(state / updates.RECEIPT_FILE) or {}
    if receipt.get('id') != record['id'] or receipt.get('outcome') != 'installed':
        raise DeliveryError('The delivery result is unknown; no matching installed receipt exists')
    identity = observe()
    if (updates.read_build(installed) != request['build']
            or bundle_hash(installed) != request['bundleHash']):
        raise DeliveryError('The installed bundle does not match the delivery receipt')
    if (identity.get('backendBuild') != request['backendHash']
            or identity.get('pid') == record['beforePid']):
        raise DeliveryPending('The running backend does not yet match the installed receipt')
    if check_backend(identity, installed) != state:
        raise DeliveryError('The running backend uses another state directory')
    after = read_handles(state)
    if any(after.get(key) != value for key, value in record['handles'].items()):
        raise DeliveryError('A retained provider changed its PID or generation; inspect its state before recovery')
    return {**record, 'phase': PHASES['installed'], 'afterPid': identity['pid']}


def wait_for_completion(record, installed, receipts, observe):
    deadline = time.monotonic() + INSTALL_WAIT_SECONDS
    state = Path(record['request']['stateDir'])
    while time.monotonic() < deadline:
        receipt = updates.read_json(state / updates.RECEIPT_FILE) or {}
        if receipt.get('id') == record['id'] and receipt.get('outcome') in ('failed', 'obsolete'):
            raise DeliveryError('The recovery owner reports delivery ' + str(receipt['outcome']))
        if receipt.get('id') == record['id'] and receipt.get('outcome') == 'installed':
            try:
                result = verify_completion(record, installed, observe)
            except (OSError, DeliveryPending):
                pass  # The receipt can precede backend startup; keep observing the same request.
            else:
                save_record(state, receipts / CONTRACT['receiptFile'], result)
                return result
        time.sleep(POLL_SECONDS)
    raise DeliveryError('The delivery result is unknown after the wait; retry with the same package and receipts')


def resume_saved(source, commit, installed, receipts, observe):
    """Observe an existing submission without another preflight, copy, or pending write."""
    validate_receipt_directory(receipts.resolve(), source, installed)
    saved = read_delivery_record(receipts / CONTRACT['receiptFile'])
    if not saved or saved.get('phase') not in PHASES.values():
        return None
    request = saved.get('request', {})
    validate_receipt_directory(receipts.resolve(), source, installed, Path(request['stateDir']))
    durable = read_delivery_record(journal_path(Path(request['stateDir']), saved['id']))
    if durable:
        if durable['request'] != request:
            raise DeliveryError('The saved delivery differs from its state-owned journal')
        saved = durable
    if (request.get('commit') != commit or request.get('installed') != str(installed.resolve())
            or request.get('build') != updates.read_build(source)
            or request.get('bundleHash') != bundle_hash(source)):
        raise DeliveryError('The saved delivery request names a different package or target')
    return wait_for_completion(saved, installed.resolve(), receipts.resolve(), observe)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--installed', type=Path, default=Path('/Applications') / updates.APP_NAME)
    parser.add_argument('--receipts', type=Path, required=True)
    parser.add_argument('--url', required=True)
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    observe = lambda: observe_desktop(args.url)
    record = None if args.dry_run else resume_saved(
        args.app, args.commit, args.installed, args.receipts, observe)
    if record is None:
        record = stage(args.app, args.commit, args.installed, args.receipts,
                       observe=observe, dry_run=args.dry_run)
    if not args.dry_run and record['phase'] not in (PHASES['installed'], 'already_installed'):
        record = wait_for_completion(record, args.installed.resolve(), args.receipts.resolve(), observe)
    print(json.dumps({'id': record['id'], 'commit': args.commit, 'phase': record['phase'],
                      'providers': len(record['handles']), 'receipts': str(args.receipts)}), flush=True)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, sqlite3.Error, updates.UpdateError, DeliveryError) as error:
        raise SystemExit(str(error)) from None
