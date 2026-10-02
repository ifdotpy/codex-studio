"""Reconcile disconnected turns before restoring confirmed completion delivery."""
from concurrent.futures import Future
import json
import time

from codex_native_errors import native_thread_block

DISCONNECT_ERRORS = frozenset({
    'Codex disconnected. Review the transcript before resuming.',
    'Server restarted during a turn. Review history, then send a new instruction.',
})
IDENTITY = ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'status', 'error',
            'inFlight', 'autoWake', 'startAttempt', 'accountTransferId',
            'workspaceOperation', 'nativeThreadBlock', 'disconnectRecovery', 'restartRecovery')


def preparation_eligible(agent):
    attempt = agent.get('startAttempt') or {}
    return bool(agent.get('status') == 'failed' and agent.get('autoWake')
                and not agent.get('inFlight') and not agent.get('turnId')
                and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
                and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
                and not native_thread_block(agent)
                and isinstance(agent.get('error'), str)
                and agent['error'] in {'AF_UNIX path too long', '[Errno 2] No such file or directory',
                    'Cannot verify the existing supervisor child; native outcome remains unknown',
                    'Supervisor open failed: Supervisor handle exists with an incompatible or stopped child'}
                and attempt.get('id') and attempt.get('submitted') is False
                and attempt.get('epoch') == agent.get('epoch')
                and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
                and attempt.get('events') and not attempt.get('action')
                and not attempt.get('activeAtReservation')
                and not attempt.get('turnId') and not attempt.get('observedTurnId'))


def eligible(agent):
    return preparation_eligible(agent) or queued_restart_eligible(agent) or (agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and not agent.get('startAttempt')
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not agent.get('deletedAt') and agent.get('threadId') and agent.get('turnId')
            and isinstance(agent.get('error'), str) and agent['error'] in DISCONNECT_ERRORS
            and not native_thread_block(agent))


def queued_restart_eligible(agent):
    """A new unsent input can wait behind an older restart turn receipt."""
    wait = agent.get('contextRepairWait') or {}
    attempt = agent.get('startAttempt') or {}
    marker = agent.get('restartRecovery') or {}
    errors = {'Context repair waits for the existing native recovery receipt',
              'The native session was active at the last check. Your message remains queued.'}
    source = {field: agent.get(field) for field in ('id', 'accountKey', 'epoch', 'threadId')}
    source['attemptId'] = attempt.get('id')
    return bool(agent.get('status') == 'queued' and agent.get('autoWake')
                and not agent.get('inFlight') and not agent.get('deletedAt')
                and not agent.get('nativeFailureHold') and not native_thread_block(agent)
                and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
                and agent.get('threadId') and marker.get('turnId')
                and agent.get('turnId') in (None, marker['turnId'])
                and isinstance(agent.get('error'), str) and agent['error'] in errors
                and wait.get('error') == agent['error']
                and wait.get('scope') in {'local', 'native'} and wait.get('source') == source
                and attempt.get('id') and attempt.get('submitted') is False
                and attempt.get('epoch') == agent.get('epoch')
                and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
                and attempt.get('events') and wait.get('events') == attempt.get('events')
                and not attempt.get('action') and not wait.get('action')
                and not attempt.get('activeAtReservation')
                and not attempt.get('turnId') and not attempt.get('observedTurnId')
                and marker.get('stage') == 'pending' and marker.get('autoWake')
                and all(marker.get(k) == agent.get(k) for k in ('epoch', 'accountKey', 'threadId'))
                and (agent.get('contextRepair') or {}).get('phase') not in {'preparing', 'submitted', 'unknown', 'ready'})


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
        if preparation_eligible(agent):
            return restore_preparation(runtime, expected, connection, server)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        from codex_turn_recovery import read_native_turn
        recovery_turn = (agent['restartRecovery']['turnId'] if queued_restart_eligible(agent)
                         else agent['turnId'])
        turn = read_native_turn(server, agent['threadId'], recovery_turn)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if (thread.get('status', {}).get('type') not in {'idle', 'notLoaded'} or not turn
                or turn.get('status') not in {'completed', 'failed', 'interrupted'}):
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        result = Future()
        def apply():
            try:
                if queued_restart_eligible(agent):
                    value = restore_queued_restart(runtime, expected, connection, server, turn)
                else:
                    value = apply_result(runtime, expected, connection, server, turn, automatic=automatic)
                result.set_result(value)
            except Exception as error:
                result.set_exception(error)
        server.after_events(apply)
        return result.result(timeout=10)
    except Exception as error:
        return {'status': 'unconfirmed', 'error': str(error)}


def restore_queued_restart(runtime, expected, connection, server, turn):
    """Settle the exact old turn; preserve the new input and its normal checks."""
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        if (not queued_restart_eligible(agent) or turn.get('id') != agent['restartRecovery']['turnId']
                or turn.get('status') not in {'completed', 'failed', 'interrupted'}
                or runtime.accounts.get(agent.get('accountKey', 'default')).get('disconnected')):
            return {'status': 'unconfirmed'}
        for event_id in agent['startAttempt']['events']:
            row = db.execute('SELECT agent,epoch,status,turn_id FROM runtime_events WHERE id=?', (event_id,)).fetchone()
            if not row or tuple(row) != (agent['id'], agent['epoch'], 'pending', None):
                return {'status': 'unconfirmed'}
        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                      "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                      (agent['id'], agent['epoch'])).fetchone():
            return {'status': 'unconfirmed'}
        outcome = turn['status']
        for item in turn.get('items', []):
            if item.get('type') == 'agentMessage':
                runtime.item(db, agent['id'], item['id'], 'assistant', item.get('text', ''),
                             turnId=turn['id'], turnStatus=outcome, streaming=False, phase=item.get('phase'))
                if item.get('phase') != 'commentary':
                    agent['lastAnswer'] = item.get('text', '')[-16000:]
                    agent['tail'] = item.get('text', '')[-300:]
        db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)', (agent['id'] + ':' + turn['id'],))
        agent.update(turnId=None, lastCompletedTurn=turn['id'], lastCompletedTurnStatus=outcome)
        agent['restartRecovery'].update(stage='finished', reconciledAt=time.time())
        agent['connectionRecovery'] = {'source': 'native_thread_read', 'at': time.time(),
            'turnId': turn['id'], 'outcome': outcome, 'queuedInputPreserved': True,
            'previousError': agent['error']}
        if outcome == 'failed':
            agent.update(status='failed', error=turn.get('error') or {'message': 'Codex ended this turn with an error.'},
                         autoWake=False, nativeFailureHold=True)
        # claim_context_wait retains ownership of the original input reservation,
        # tool receipts, monitors, native context checks, and action permissions.
        runtime.loaded.discard(agent['id'])
        runtime.put(db, 'agents', agent)
        if outcome == 'failed':
            runtime.child_stopped_event(db, agent, 'failed', agent['error'],
                                        'turn:' + str(turn.get('id') or 'unknown'))
    runtime.changed.set()
    return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': outcome, 'queuedInputPreserved': True}


def restore_preparation(runtime, expected, connection, server):
    """Restore admitted input only after proving that none of it was submitted."""
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        if runtime.accounts.get(agent.get('accountKey', 'default')).get('disconnected'):
            return {'status': 'superseded'}
        attempt = agent['startAttempt']
        for event_id in attempt['events']:
            row = db.execute('SELECT agent,epoch,status FROM runtime_events WHERE id=?', (event_id,)).fetchone()
            if not row or tuple(row) != (agent['id'], agent['epoch'], 'pending'):
                return {'status': 'unconfirmed'}
        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                      "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                      (agent['id'], agent['epoch'])).fetchone():
            return {'status': 'unconfirmed'}
        agent['connectionRecovery'] = {'source': 'transport_attach', 'at': time.time(),
            'attemptId': attempt['id'], 'eventIds': list(attempt['events']),
            'previousError': agent['error'], 'outcome': 'input_restored'}
        agent.update(status='queued', error=None)
        agent.pop('startAttempt')
        runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return {'status': 'input_restored', 'attemptId': attempt['id']}


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
        if queued_restart_eligible(agent):
            agent['connectionCheck']['turnId'] = agent['restartRecovery']['turnId']
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
                    marker = agent.get('restartRecovery') or {}
                    if marker.get('turnId') == turn['id'] and result['status'] == 'reconciled':
                        marker.update(stage='finished', reconciledAt=time.time())
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
        from codex_restart_recovery import can_continue, continue_interrupted, settle_reconciled
        restart_continuation = automatic and can_continue(db, agent, turn)
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
        hold_operations = unresolved_completion_operations(db, agent, turn) if outcome == 'completed' else []
        if hold_operations:
            agent['connectionRecovery']['holdOperations'] = hold_operations
        runtime.loaded.discard(agent['id'])
        if not restart_continuation:
            settle_reconciled(agent)
        runtime.put(db, 'agents', agent)
        if restart_continuation:
            continue_interrupted(runtime, db, agent, turn)
        elif hold_operations:
            result_text = agent.get('lastAnswer') or 'The turn completed without final text.'
            warning = 'Studio held follow-up work because these operation receipts remain unresolved: '
            runtime.parent_event(db, agent, turn['id'], result_text + '\n\n' + warning
                                 + '; '.join(item['label'] for item in hold_operations), recovery=True)
            runtime.child_stopped_event(db, agent, 'completed', warning
                                        + '; '.join(item['label'] for item in hold_operations),
                                        'turn:' + str(turn['id']) + ':unresolved')
        elif outcome in {'failed', 'interrupted'} and not runtime.worker_continuation_pending(agent):
            marker = agent.get('restartRecovery') or {}
            reason = marker.get('reason') or error or agent.get('error')
            runtime.child_stopped_event(db, agent, outcome,
                reason or 'The interrupted turn was reconciled without continuation.',
                'turn:' + str(turn.get('id') or 'unknown'))
        return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': outcome,
                **({'continued': True} if restart_continuation else {})}


def native_operations_settled(turn):
    # Native history can contain an item whose start notification never reached
    # SQLite. A terminal model turn does not prove that such a command ended.
    for item in turn.get('items', []):
        kind = item.get('type')
        if kind == 'commandExecution':
            if item.get('status') not in {'completed', 'failed', 'declined'}:
                return False
            if item.get('status') != 'declined' and type(item.get('exitCode')) is not int:
                return False
        elif kind == 'dynamicToolCall':
            if item.get('status') != 'completed':
                return False
        elif kind in {'mcpToolCall', 'fileChange', 'computerToolCall'}:
            if item.get('status') not in {'completed', 'failed', 'declined'}:
                return False
    return True


def can_deliver_completion(db, agent, turn):
    if not native_operations_settled(turn):
        return False
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
        for row in db.execute(f"SELECT record FROM {table} WHERE json_extract(record,'$.agent')=? "
                              "AND json_extract(record,'$.status') IN ('lost','running','starting','approval')",
                              (agent['id'],)):
            operation = json.loads(row['record'])
            if operation.get('turnId') in (None, turn.get('id')):
                return False
    for row in db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "AND (json_extract(record,'$.outcome')='unknown' OR "
                          "json_extract(record,'$.stage') IN ('queued','running'))", (agent['id'],)):
        operation = json.loads(row['record'])
        if operation.get('turnId') in (None, turn.get('id')):
            return False
    return True


def unresolved_completion_operations(db, agent, turn):
    """Name only operation receipts tied to this turn, plus legacy unscoped receipts."""
    unresolved = []
    for item in turn.get('items', []):
        kind = item.get('type')
        terminal = True
        if kind == 'commandExecution':
            terminal = (item.get('status') in {'completed', 'failed', 'declined'}
                        and (item.get('status') == 'declined' or type(item.get('exitCode')) is int))
        elif kind == 'dynamicToolCall':
            terminal = item.get('status') == 'completed'
        elif kind in {'mcpToolCall', 'fileChange', 'computerToolCall'}:
            terminal = item.get('status') in {'completed', 'failed', 'declined'}
        if kind and not terminal:
            unresolved.append({'id': str(item.get('id') or kind), 'kind': kind,
                               'label': str(item.get('command') or item.get('tool')
                                            or item.get('id') or kind)})
    for table in ('runtime_tasks', 'runtime_monitors'):
        rows = db.execute(f"SELECT record FROM {table} WHERE json_extract(record,'$.agent')=? "
                          "AND json_extract(record,'$.status') IN ('lost','running','starting','approval')",
                          (agent['id'],))
        for row in rows:
            operation = json.loads(row['record'])
            if operation.get('turnId') in (None, turn.get('id')):
                unresolved.append({'id': str(operation.get('id') or table), 'kind': table,
                                   'label': str(operation.get('command') or operation.get('tool')
                                                or operation.get('id') or table)})
    rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                      "AND (json_extract(record,'$.outcome')='unknown' OR "
                      "json_extract(record,'$.stage') IN ('queued','running'))", (agent['id'],))
    for row in rows:
        operation = json.loads(row['record'])
        if operation.get('turnId') in (None, turn.get('id')):
            unresolved.append({'id': str(operation.get('id') or 'tool request'),
                               'kind': 'tool_request',
                               'label': str(operation.get('tool') or operation.get('id')
                                            or 'tool request')})
    unique = {}
    for operation in unresolved:
        unique[(operation['kind'], operation['id'])] = operation
    return list(unique.values())


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
