"""Durable native monitor results, independent of the runtime database commit."""

import hashlib
import json
import math
import os
from pathlib import Path
import tempfile
import time
import uuid

VERSION = 1
TERMINAL = {'completed', 'failed', 'cancelled'}


def _directory(root):
    return Path(root) / 'monitor-results'


def _path(root, key):
    return _directory(root) / (hashlib.sha256(key.encode()).hexdigest() + '.json')


def _sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _validate(value):
    if not isinstance(value, dict) or value.get('version') != VERSION:
        raise ValueError('Unsupported monitor result version')
    if not isinstance(value.get('key'), str) or not 1 <= len(value['key']) <= 200:
        raise ValueError('Invalid monitor result identity')
    code, error, operation = value.get('code'), value.get('error'), value.get('operation')
    if code is not None and type(code) is not int:
        raise ValueError('Invalid monitor exit code')
    if error is not None and not isinstance(error, str):
        raise ValueError('Invalid monitor result error')
    if code is None and not error:
        raise ValueError('Monitor result has no definitive outcome')
    if operation is not None and (not isinstance(operation, dict)
            or any(not isinstance(operation.get(k), str) or not operation[k]
                   for k in ('agent', 'accountKey', 'connectionId'))
            or type(operation.get('epoch')) is not int):
        raise ValueError('Invalid monitor result operation')
    finished = value.get('finished')
    if type(finished) not in (int, float) or not math.isfinite(finished) or finished <= 0:
        raise ValueError('Invalid monitor result time')
    return value


def persist_monitor_result(root, key, receipt):
    """Save the first observed result before any SQLite read or write."""
    value = _validate({'version': VERSION, 'key': key, **{
        name: receipt.get(name) for name in ('code', 'error', 'operation', 'finished')}})
    directory = _directory(root)
    created = not directory.exists()
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    if created:
        _sync_directory(directory.parent)
    target = _path(root, key)
    if target.exists():
        original = _validate(json.loads(target.read_text()))
        if original['key'] != key:
            raise ValueError('Monitor result identity mismatch')
        _sync_directory(directory)
        return original
    descriptor, temporary = tempfile.mkstemp(prefix='.result-', dir=directory)
    try:
        with os.fdopen(descriptor, 'w') as output:
            json.dump(value, output, ensure_ascii=False, separators=(',', ':'))
            output.flush()
            os.fsync(output.fileno())
        try:
            # Linking publishes a complete file without replacing an older result.
            os.link(temporary, target)
        except FileExistsError:
            original = _validate(json.loads(target.read_text()))
            if original['key'] != key:
                raise ValueError('Monitor result identity mismatch')
            _sync_directory(directory)
            return original
        _sync_directory(directory)
        return value
    finally:
        Path(temporary).unlink(missing_ok=True)


def acknowledge_monitor_result(root, key):
    """Call only after the monitor and its event commit successfully."""
    target = _path(root, key)
    if target.exists():
        target.unlink()
        _sync_directory(target.parent)


def recover_monitor_results(runtime, db):
    """Restore exact native outcomes. Never repeat a command or infer an exit."""
    result = {'restored': [], 'acknowledge': [], 'warnings': []}
    directory = _directory(runtime.root)
    if not directory.exists():
        return result
    for path in sorted(directory.glob('*.json')):
        try:
            receipt = _validate(json.loads(path.read_text()))
            key = receipt['key']
            if path != _path(runtime.root, key):
                raise ValueError('Monitor result filename does not match its identity')
            row = db.execute('SELECT record FROM runtime_monitors WHERE id=?', (key,)).fetchone()
            if not row:
                raise ValueError('Monitor result has no saved command')
            monitor = json.loads(row[0])
            if monitor.get('operation') != receipt['operation']:
                raise ValueError('Monitor result belongs to a different command operation')
            operation = receipt['operation']
            if operation and (operation['agent'] != monitor['agent'] or operation['epoch'] != monitor['epoch']):
                raise ValueError('Monitor result owner does not match its command')
            if monitor['status'] in TERMINAL:
                # A committed terminal record always wins over duplicate delivery.
                result['acknowledge'].append(key)
                continue
            if monitor['status'] not in {'running', 'starting', 'approval', 'lost'}:
                raise ValueError('Monitor result cannot replace this command state')
            if operation is None and monitor['status'] == 'running':
                raise ValueError('A submitted command requires a native operation identity')
        except (ValueError, TypeError, KeyError, OSError) as error:
            # Preserve the original bytes for diagnosis. A corrupt receipt cannot
            # block other recovery, and cannot invent command success.
            result['warnings'].append({'file': str(path), 'error': str(error)})
            continue
        cancelled = bool(monitor.get('cancelRequested'))
        status = 'cancelled' if cancelled else 'failed' if receipt['error'] or receipt['code'] != 0 else 'completed'
        monitor.update(status=status, exitCode=receipt['code'], error=receipt['error'],
                       finished=receipt['finished'], configurationPending=False,
                       resultRecoveredAt=time.time())
        runtime.put(db, 'monitors', monitor)
        if monitor.get('ruleId'):
            if _restore_rule_check(runtime, db, monitor):
                runtime.rule_finished(monitor['ruleId'], receipt['code'],
                                      'Monitor cancelled' if cancelled else receipt['error'], monitor['tail'], db)
        else:
            runtime._monitor_exit_event(db, runtime.agent(monitor['agent'], db), monitor)
        result['restored'].append(key)
        result['acknowledge'].append(key)
    return result


def _restore_rule_check(runtime, db, monitor):
    row = db.execute('SELECT record FROM runtime_rules WHERE id=?', (monitor['ruleId'],)).fetchone()
    if not row:
        return
    rule = json.loads(row[0])
    expected = str(uuid.uuid5(uuid.NAMESPACE_URL, 'rule:' + rule['id'] + ':' + str(rule['checks'])))
    if (monitor['id'] != expected or rule['agent'] != monitor['agent']
            or rule['epoch'] != monitor['epoch']):
        return False
    marker = rule.get('restartCheck')
    actor = runtime.agent(monitor['agent'], db)
    if (marker == {'epoch': rule['epoch'], 'checks': rule['checks'], 'monitorId': monitor['id']}
            and rule['status'] == 'paused' and not rule.get('inFlight')
            and rule['agent'] == monitor['agent'] and rule['epoch'] == monitor['epoch']
            and actor['epoch'] == monitor['epoch'] and not actor.get('deletedAt')):
        rule.update(status='active', error=None)
        rule.pop('restartCheck', None)
        runtime.put(db, 'rules', rule)
    return True
