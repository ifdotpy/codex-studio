"""Immutable admission receipts for user-requested native maintenance actions."""
import json
import time


def signature(key, action, context):
    return json.dumps({'agent': key, 'action': action, 'context': context}, sort_keys=True, separators=(',', ':'))


def find(db, request_id, key, action, context):
    if not isinstance(request_id, str) or not 1 <= len(request_id) <= 200:
        raise ValueError('A native action request ID is required. Reload Studio before this action.')
    db.execute('''CREATE TABLE IF NOT EXISTS runtime_native_action_receipts (
        id TEXT PRIMARY KEY, signature TEXT NOT NULL, receipt TEXT NOT NULL, outcome TEXT NOT NULL)''')
    row = db.execute('SELECT signature, receipt FROM runtime_native_action_receipts WHERE id=?', (request_id,)).fetchone()
    if row:
        if row[0] != signature(key, action, context):
            raise ValueError('This action request ID belongs to different input')
        return json.loads(row[1])
    return None


def reserve(db, request_id, a, action, context, attempt_id):
    if not a.get('threadId'):
        raise ValueError('Send an instruction before Review or Compact.')
    identity = {'accountKey': a.get('accountKey', 'default'), 'threadId': a.get('threadId'), 'epoch': a['epoch']}
    if not isinstance(context, dict) or any(name not in identity for name in context):
        raise ValueError('Invalid native action context')
    if any(value != identity[name] for name, value in context.items()):
        raise ValueError('The conversation changed. Check its account and thread before a new action.')
    receipt = {'requestId': request_id, 'agentId': a['id'], 'action': action, **identity,
               'attemptId': attempt_id, 'acceptedAt': time.time(), 'status': 'accepted'}
    db.execute('INSERT INTO runtime_native_action_receipts VALUES (?, ?, ?, ?)',
               (request_id, signature(a['id'], action, context), json.dumps(receipt), json.dumps({'status': 'pending'})))
    return receipt


def outcome(db, request_id):
    row = db.execute('SELECT outcome FROM runtime_native_action_receipts WHERE id=?', (request_id,)).fetchone()
    return json.loads(row[0])


def record(runtime, attempt, status, error=None):
    request_id = attempt.get('actionRequestId')
    if not request_id:
        return
    with runtime.lock, runtime.db() as db:
        db.execute('UPDATE runtime_native_action_receipts SET outcome=? WHERE id=?',
                   (json.dumps({'status': status, **({'error': str(error)} if error else {})}), request_id))


def late_result(runtime, key, attempt, future):
    try:
        runtime.native_action_accepted(key, attempt, future.result())
        record(runtime, attempt, 'acknowledged')
    except Exception as error:
        record(runtime, attempt, 'unknown' if 'outcome unknown' in str(error) else 'failed', error)
        runtime.start_error(key, attempt['id'], error, unknown='outcome unknown' in str(error))


def assert_identity(a, attempt):
    identity = attempt.get('actionIdentity')
    if not identity or all(a.get(name, 'default' if name == 'accountKey' else None) == value
                           for name, value in identity.items()):
        return
    # A completed repair may replace only the thread for this reserved action.
    # The admission receipt and actionIdentity retain their original values.
    repair = a.get('contextRepair') or {}
    source = repair.get('source') or {}
    current = a.get('startAttempt') or {}
    if (repair.get('phase') == 'completed' and repair.get('agent') == a.get('id')
            and source.get('id') == a.get('id')
            and all(source.get(name) == identity.get(name) for name in ('accountKey', 'threadId', 'epoch'))
            and a.get('accountKey', 'default') == identity.get('accountKey')
            and a.get('epoch') == identity.get('epoch')
            and attempt.get('id') and source.get('attemptId') == attempt['id'] == current.get('id')
            and current.get('actionRequestId') == attempt.get('actionRequestId')
            and current.get('actionIdentity') == identity
            and isinstance(repair.get('newThreadId'), str) and repair['newThreadId']
            and repair.get('newThreadId') == a.get('threadId') == current.get('threadId')):
        return
    raise ValueError('The native action belongs to an earlier account, thread or epoch.')
