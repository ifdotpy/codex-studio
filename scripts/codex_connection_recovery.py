"""Reconcile disconnected turns before restoring confirmed completion delivery."""
from concurrent.futures import Future
import time

from codex_native_errors import native_thread_block

DISCONNECT_ERRORS = frozenset({
    'Codex disconnected. Review the transcript before resuming.',
    'Server restarted during a turn. Review history, then send a new instruction.',
})
IDENTITY = ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'status', 'error',
            'inFlight', 'autoWake', 'startAttempt', 'accountTransferId',
            'workspaceOperation', 'nativeThreadBlock', 'disconnectRecovery')


def eligible(agent):
    return (agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and not agent.get('startAttempt')
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not agent.get('deletedAt') and agent.get('threadId') and agent.get('turnId')
            and isinstance(agent.get('error'), str) and agent['error'] in DISCONNECT_ERRORS
            and not native_thread_block(agent))


def recover(runtime, key, *, automatic=False):
    with runtime.lock, runtime.db() as db:
        agent = runtime.checked_actor(db, key)
        if runtime.closed or not eligible(agent):
            return {'status': 'superseded'}
        expected = {field: agent.get(field) for field in IDENTITY}
        account = agent.get('accountKey', 'default')
        if automatic and runtime.accounts.get(account).get('disconnected'):
            return {'status': 'superseded'}
    try:
        # This can start the account transport. It never loads or resumes a thread.
        server = runtime.connect(account)
        connection = runtime.connection_ids.get(account)
        with runtime.lock, runtime.db() as db:
            if not current(runtime, db, expected, connection, server):
                return {'status': 'superseded'}
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        page = server.call('thread/turns/list', {'threadId': agent['threadId'], 'limit': 10,
            'sortDirection': 'desc', 'itemsView': 'full'}, timeout=10)
        turn = next((t for t in page.get('data', []) if t.get('id') == agent['turnId']), None)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if (thread.get('status', {}).get('type') not in {'idle', 'notLoaded'} or not turn
                or turn.get('status') not in {'completed', 'failed', 'interrupted'}):
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        result = Future()
        def apply():
            try:
                result.set_result(apply_result(runtime, expected, connection, server, turn, automatic=automatic))
            except Exception as error:
                result.set_exception(error)
        server.after_events(apply)
        return result.result(timeout=10)
    except Exception as error:
        return {'status': 'unconfirmed', 'error': str(error)}


def current(runtime, db, expected, connection, server):
    agent = runtime.agent(expected['id'], db)
    account = expected.get('accountKey') or 'default'
    return (not runtime.closed and runtime.servers.get(account) is server
            and runtime.connection_current(account, connection)
            and eligible(agent) and all(agent.get(k) == v for k, v in expected.items()))



def record_check(runtime, expected, connection, server, native_state):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        # A verified read proves only that Codex answered this check. Keep the
        # unknown turn, its original error, and all delivery permissions intact.
        agent['connectionCheck'] = {field: expected[field] for field in
                                    ('epoch', 'accountKey', 'threadId', 'turnId')}
        agent['connectionCheck'].update(at=time.time(), previousError=expected['error'],
                                         nativeState=native_state)
        runtime.put(db, 'agents', agent)
        return {'status': 'unconfirmed', 'checked': True}


def apply_result(runtime, expected, connection, server, turn, *, automatic=False):
    if automatic:
        with runtime.lock:
            with runtime.db() as db:
                if not current(runtime, db, expected, connection, server):
                    return {'status': 'superseded'}
                agent = runtime.agent(expected['id'], db)
                if runtime.accounts.get(agent.get('accountKey', 'default')).get('disconnected'):
                    return {'status': 'superseded'}
                resume_completion = can_deliver_completion(db, agent, turn)
                if resume_completion:
                    # The exact turn is terminal. Restore only the permission that
                    # this transport loss removed, then use normal completion delivery.
                    agent.update(autoWake=True, inFlight=True)
                    runtime.put(db, 'agents', agent)
                    identity = {field: agent.get(field) for field in
                                ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
            if resume_completion:
                result = runtime.apply_turn_recovery(identity, connection, 'notLoaded', turn)
                with runtime.db() as db:
                    agent = runtime.agent(expected['id'], db)
                    agent['connectionRecovery'] = {
                        'turnId': turn['id'], 'outcome': turn['status'], 'at': time.time(),
                        'source': 'native_thread_read', 'previousError': expected['error'],
                        'automatic': True, 'completionDelivered': result['status'] == 'reconciled',
                    }
                    runtime.put(db, 'agents', agent)
                return result
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        outcome = turn['status']
        # Restore final text directly. Notification handlers can wake parents,
        # create questions, or schedule checkpoints; this read must do none of those.
        for item in turn.get('items', []):
            if item.get('type') == 'agentMessage':
                runtime.item(db, agent['id'], item['id'], 'assistant', item.get('text', ''),
                             turnId=turn['id'], turnStatus=outcome, streaming=False,
                             phase=item.get('phase'))
                if item.get('phase') != 'commentary':
                    agent['lastAnswer'] = item.get('text', '')[-16000:]
                    agent['tail'] = item.get('text', '')[-300:]
        runtime.item(db, agent['id'], 'connection-recovery:' + turn['id'], 'system',
                     'Connection checked. Codex reports the previous turn as ' + outcome + '.',
                     'Connection recovery', nativeNotice='info', turnId=turn['id'],
                     previousError=agent['error'], details=agent['error'], turnStatus=outcome)
        db.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?) WHERE agent=? AND json_extract(record,'$.turnId')=?",
                   (outcome, agent['id'], turn['id']))
        db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)', (agent['id'] + ':' + turn['id'],))
        error = turn.get('error')
        if outcome == 'failed':
            error = error or {'message': 'Codex ended this turn with an error.'}
            agent['nativeFailureHold'] = True
        agent.update(status=outcome, error=error, inFlight=False, autoWake=False,
                     turnId=None, activity=None, activeTools=[],
                     lastCompletedTurn=turn['id'], lastCompletedTurnStatus=outcome,
                     connectionRecovery={'turnId': turn['id'], 'outcome': outcome,
                         'at': time.time(), 'source': 'native_thread_read',
                         'previousError': expected['error']})
        runtime.loaded.discard(agent['id'])
        runtime.put(db, 'agents', agent)
        return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': outcome}


def can_deliver_completion(db, agent, turn):
    previous = agent.get('disconnectRecovery') or {}
    if (turn.get('status') != 'completed' or not previous.get('autoWake')
            or agent.get('nativeFailureHold')
            or any(previous.get(field) != agent.get(field) for field in
                   ('epoch', 'accountKey', 'threadId', 'turnId'))
            or agent.get('turnEpoch', agent['epoch']) != agent['epoch']):
        return False
    # A completed model turn does not settle a lost process or tool mutation.
    # Pending input remains pending; no uncertain input is replayed.
    if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                  "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                  (agent['id'], agent['epoch'])).fetchone():
        return False
    for table in ('runtime_tasks', 'runtime_monitors'):
        if db.execute(f"SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=? "
                      "AND json_extract(record,'$.status') IN ('lost','running','starting','approval') LIMIT 1",
                      (agent['id'],)).fetchone():
            return False
    if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                  "AND (json_extract(record,'$.outcome')='unknown' OR "
                  "json_extract(record,'$.stage') IN ('queued','running')) LIMIT 1",
                  (agent['id'],)).fetchone():
        return False
    return True


def tick(runtime, agents):
    """One bounded read job; failed checks back off without blocking the scheduler."""
    with runtime.lock:
        if runtime.closed or getattr(runtime, '_connection_recovery_busy', False):
            return
        checks = runtime.__dict__.setdefault('_connection_recovery_checks', {})
        now = time.time()
        candidates = []
        for agent in agents:
            if not eligible(agent):
                continue
            if runtime.accounts.get(agent.get('accountKey', 'default')).get('disconnected'):
                continue
            identity = tuple(agent.get(field) for field in ('id', 'epoch', 'accountKey', 'threadId', 'turnId'))
            previous = checks.get(identity, {})
            if previous.get('nextAt', 0) <= now:
                candidates.append((previous.get('at', 0), identity, agent['id']))
        if not candidates:
            return
        _, identity, key = min(candidates, key=lambda candidate: candidate[0])
        runtime._connection_recovery_busy = True
        try:
            runtime.recovery_pool.submit(run, runtime, key, identity)
        except Exception:
            runtime._connection_recovery_busy = False
            raise


def run(runtime, key, identity):
    result = {'status': 'unconfirmed'}
    try:
        result = recover(runtime, key, automatic=True)
        return result
    finally:
        with runtime.lock:
            checks = runtime.__dict__.setdefault('_connection_recovery_checks', {})
            failures = min(checks.get(identity, {}).get('failures', 0) + 1, 6)
            delay = min(300, 15 * 2 ** (failures - 1))
            now = time.time()
            checks[identity] = {**result, 'at': now, 'nextAt': now + delay, 'failures': failures}
            # Keep recent diagnostics without retaining every historical epoch.
            if len(checks) > 256:
                for old in sorted(checks, key=lambda key: checks[key]['at'])[:-256]:
                    checks.pop(old)
            runtime._connection_recovery_busy = False
            runtime.changed.set()
