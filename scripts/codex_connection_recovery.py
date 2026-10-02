"""Reconcile disconnected turns before restoring confirmed completion delivery."""
from concurrent.futures import Future
import json
import time

from codex_native_errors import native_thread_block

DISCONNECT_ERRORS = frozenset({
    'Codex disconnected. Review the transcript before resuming.',
    'Server restarted during a turn. Review history, then send a new instruction.',
})
RESTART_ACTIVE_HOLD_SECONDS = 30
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
    restart_pending = restart_turn_pending(agent)
    marker = agent.get('restartRecovery') or {}
    stale_restart = marker.get('stage') in {'pending', 'superseded'}
    return preparation_eligible(agent) or queued_restart_eligible(agent) or restart_pending or (agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and not agent.get('startAttempt')
            and not stale_restart
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not agent.get('deletedAt') and agent.get('threadId') and agent.get('turnId')
            and isinstance(agent.get('error'), str) and agent['error'] in DISCONNECT_ERRORS
            and not native_thread_block(agent))


def restart_turn_pending(agent):
    marker = agent.get('restartRecovery') or {}
    disconnect = agent.get('disconnectRecovery') or {}
    state_can_match = (agent.get('status') in {'interrupted', 'paused', 'completed', 'failed'}
                       and agent.get('turnId') in (None, marker.get('turnId')))
    state_can_match = state_can_match or (agent.get('status') == 'running'
                       and agent.get('inFlight') and agent.get('autoWake'))
    return bool(marker.get('stage') in {'pending', 'superseded'} and marker.get('autoWake')
            and marker.get('turnId') and disconnect.get('source') == 'restart'
            and marker.get('epoch') == agent.get('epoch')
            and marker.get('accountKey', 'default') == agent.get('accountKey', 'default')
            and marker.get('threadId') == agent.get('threadId') and state_can_match
            and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
            and not agent.get('accountTransferId')
            and not agent.get('workspaceOperation') and not native_thread_block(agent))


def queued_restart_eligible(agent):
    """A new unsent input can wait behind an older restart turn receipt."""
    wait = agent.get('contextRepairWait') or {}
    attempt = agent.get('startAttempt') or {}
    marker = agent.get('restartRecovery') or {}
    errors = {'Context repair waits for the existing native recovery receipt',
              'The native session was active at the last check. Your message remains queued.'}
    source = {field: agent.get(field) for field in ('id', 'accountKey', 'epoch', 'threadId')}
    source['attemptId'] = attempt.get('id')
    supervisor_wait = (agent.get('status') == 'failed' and not wait
                      and agent.get('error') == 'Cannot verify the existing supervisor child; native outcome remains unknown')
    context_wait = (agent.get('status') == 'queued' and isinstance(agent.get('error'), str)
                    and agent['error'] in errors
                    and wait.get('error') == agent['error']
                    and wait.get('scope') in {'local', 'native'} and wait.get('source') == source
                    and wait.get('events') == attempt.get('events') and not wait.get('action'))
    return bool((supervisor_wait or context_wait) and agent.get('autoWake')
                and not agent.get('inFlight') and not agent.get('deletedAt')
                and not agent.get('nativeFailureHold') and not native_thread_block(agent)
                and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
                and agent.get('threadId') and marker.get('turnId')
                and agent.get('turnId') in (None, marker['turnId'])
                and attempt.get('id') and attempt.get('submitted') is False
                and attempt.get('epoch') == agent.get('epoch')
                and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
                and attempt.get('events') and not attempt.get('action')
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
    server = None
    connection = None
    pending_restart = restart_turn_pending(agent)
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
        marker = agent.get('restartRecovery') or {}
        pending_restart = restart_turn_pending(agent)
        if (not pending_restart and thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}):
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        from codex_turn_recovery import read_native_turn
        recovery_turn = (marker['turnId'] if pending_restart else
                         agent['restartRecovery']['turnId'] if queued_restart_eligible(agent)
                         else agent['turnId'])
        turn = read_native_turn(server, agent['threadId'], recovery_turn)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        native_state = thread.get('status', {}).get('type')
        terminal_restart_result = False
        if pending_restart:
            if turn and turn.get('id') == recovery_turn and turn.get('status') in {'inProgress', 'running'}:
                if getattr(server, 'supervisor_mode', False):
                    return adopt_restart_turn(runtime, expected, connection, server, turn)
                return record_check(runtime, expected, connection, server, native_state,
                                    turn_status=turn.get('status'))
            if turn and turn.get('id') == recovery_turn and turn.get('status') in {'completed', 'failed'}:
                if turn.get('status') == 'failed':
                    return finish_restart_turn(runtime, expected, connection, server, native_state, turn)
                with runtime.lock, runtime.db() as db:
                    if not current(runtime, db, expected, connection, server):
                        return {'status': 'superseded'}
                    current_agent = runtime.agent(expected['id'], db)
                    completion_agent = dict(current_agent)
                    if completion_agent.get('turnId') is None:
                        completion_agent['turnId'] = turn.get('id')
                    deliver = (can_deliver_completion(db, completion_agent, turn)
                               or (current_agent.get('turnId') not in (None, turn.get('id'))
                                   and current_agent.get('inFlight') and current_agent.get('autoWake')))
                if deliver:
                    return finish_restart_turn(runtime, expected, connection, server, native_state, turn)
                pending_restart = False
                terminal_restart_result = True
            elif (turn and turn.get('id') == recovery_turn and turn.get('status') == 'interrupted'
                    and native_state in {'idle', 'notLoaded'}):
                pending_restart = False
            else:
                return record_check(runtime, expected, connection, server, native_state,
                                    turn_status=(turn or {}).get('status') or 'missing')
        if ((not terminal_restart_result and native_state not in {'idle', 'notLoaded'})
                or not turn or turn.get('status') not in {'completed', 'failed', 'interrupted'}):
            return record_check(runtime, expected, connection, server, native_state)
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
        if pending_restart:
            if server is not None:
                record_check(runtime, expected, connection, server, 'unreadable', error=str(error))
            else:
                with runtime.lock, runtime.db() as db:
                    agent = runtime.agent(expected['id'], db)
                    marker = agent.get('restartRecovery') or {}
                    if (marker.get('stage') in {'pending', 'superseded'} and marker.get('autoWake')
                            and all(agent.get(field) == expected.get(field)
                                    for field in ('id', 'epoch', 'accountKey', 'threadId'))):
                        agent['connectionCheck'] = {
                            'epoch': expected.get('epoch'), 'accountKey': expected.get('accountKey'),
                            'threadId': expected.get('threadId'), 'turnId': marker.get('turnId'),
                            'at': time.time(), 'nativeState': 'unreadable',
                            'restartTurnStatus': 'unreadable', 'readError': str(error)[:300],
                        }
                        runtime.put(db, 'agents', agent)
        return {'status': 'unconfirmed', 'error': str(error)}


def adopt_restart_turn(runtime, expected, connection, server, turn):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        marker = agent.get('restartRecovery') or {}
        if (marker.get('stage') not in {'pending', 'superseded'} or marker.get('turnId') != turn.get('id')
                or any(marker.get(field) != agent.get(field) for field in ('epoch', 'accountKey', 'threadId'))
                or agent.get('turnId') not in (None, turn.get('id'))):
            return {'status': 'superseded'}
        marker.update(stage='continued', observedAt=time.time(), outcome='active')
        agent.update(status='running', autoWake=True, inFlight=True, turnId=turn['id'], error=None)
        agent['connectionRecovery'] = {'source': 'native_thread_read', 'at': time.time(),
            'turnId': turn['id'], 'outcome': 'active', 'automatic': True,
            'previousError': expected['error']}
        runtime.loaded.add(agent['id'])
        runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return {'status': 'adopted', 'turnId': turn['id']}


def finish_restart_turn(runtime, expected, connection, server, native_state, turn):
    stale_current_turn = False
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        marker = agent.get('restartRecovery') or {}
        if (marker.get('stage') not in {'pending', 'superseded'} or marker.get('turnId') != turn.get('id')
                or any(marker.get(field) != agent.get(field) for field in ('epoch', 'accountKey', 'threadId'))
                or (agent.get('turnId') not in (None, turn.get('id'))
                    and not (agent.get('inFlight') and agent.get('autoWake')))):
            return {'status': 'unconfirmed'}
        stale_current_turn = agent.get('turnId') not in (None, turn.get('id'))
        if stale_current_turn:
            identity = None
        else:
            marker.update(stage='continued', observedAt=time.time(), outcome=turn['status'])
            agent.update(status='running', autoWake=True, inFlight=True, turnId=turn['id'], error=None)
            runtime.put(db, 'agents', agent)
            identity = {field: agent.get(field) for field in
                        ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
    if stale_current_turn:
        runtime.notification({'method': 'turn/completed', 'params': {
            'threadId': expected['threadId'],
            'turn': {k: turn.get(k) for k in ('id', 'status', 'error')},
        }}, expected.get('accountKey') or 'default', connection)
        with runtime.lock, runtime.db() as db:
            agent = runtime.agent(expected['id'], db)
            marker = agent.get('restartRecovery') or {}
            if marker.get('stage') not in {'pending', 'superseded'} or marker.get('turnId') != turn.get('id'):
                return {'status': 'superseded'}
            for item in turn.get('items', []):
                if item.get('type') == 'agentMessage':
                    runtime.item(db, agent['id'], item['id'], 'assistant', item.get('text', ''),
                                 turnId=turn['id'], turnStatus=turn['status'], streaming=False,
                                 phase=item.get('phase'))
            db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)',
                       (agent['id'] + ':' + turn['id'],))
            db.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?) "
                       "WHERE agent=? AND json_extract(record,'$.turnId')=?",
                       (turn['status'], agent['id'], turn['id']))
            marker.update(stage='finished', reconciledAt=time.time(), outcome=turn['status'])
            agent['connectionRecovery'] = {
                'source': 'native_thread_read', 'at': time.time(), 'turnId': turn['id'],
                'outcome': turn['status'], 'automatic': True,
                'previousError': expected.get('error'),
            }
            runtime.put(db, 'agents', agent)
            if agent.get('parentId') and turn.get('status') == 'completed':
                completed = dict(agent)
                completed.update(status='completed', autoWake=True, inFlight=False)
                answer = next((item.get('text', '') for item in reversed(turn.get('items', []))
                               if item.get('type') == 'agentMessage' and item.get('phase') != 'commentary'),
                              'No final text returned')
                runtime.parent_event(db, completed, turn['id'], answer, recovery=True)
            elif turn.get('status') == 'failed':
                reason = turn.get('error') or {'message': 'Codex ended this turn with an error.'}
                runtime.child_stopped_event(db, agent, 'failed', reason, 'turn:' + str(turn['id']))
        runtime.changed.set()
        return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': turn['status']}
    result = runtime.apply_turn_recovery(identity, connection, native_state, turn)
    with runtime.lock, runtime.db() as db:
        agent = runtime.agent(expected['id'], db)
        marker = agent.get('restartRecovery') or {}
        if marker.get('turnId') == turn['id'] and result.get('status') == 'reconciled':
            marker.update(stage='finished', reconciledAt=time.time(), outcome=turn['status'])
            agent['connectionRecovery'] = {
                'source': 'native_thread_read', 'at': time.time(), 'turnId': turn['id'],
                'outcome': turn['status'], 'automatic': True,
                'previousError': expected.get('error'),
            }
            runtime.put(db, 'agents', agent)
    return result


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
        if (agent.get('status') == 'failed'
                and agent['error'] == 'Cannot verify the existing supervisor child; native outcome remains unknown'
                and outcome != 'failed'):
            agent.update(status='queued', error=None)
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



def record_check(runtime, expected, connection, server, native_state, *, turn_status=None, error=None):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        # A verified read proves only that Codex answered this check. Keep the
        # unknown turn, its original error, and all delivery permissions intact.
        agent['connectionCheck'] = {field: expected[field] for field in
                                    ('epoch', 'accountKey', 'threadId', 'turnId')}
        marker = agent.get('restartRecovery') or {}
        if (queued_restart_eligible(agent)
                or marker.get('stage') in {'pending', 'superseded'} and marker.get('autoWake')):
            agent['connectionCheck']['turnId'] = agent['restartRecovery']['turnId']
        agent['connectionCheck'].update(at=time.time(), previousError=expected['error'],
                                         nativeState=native_state)
        if turn_status is not None:
            agent['connectionCheck']['restartTurnStatus'] = turn_status
        if error:
            agent['connectionCheck']['readError'] = str(error)[:300]
        runtime.put(db, 'agents', agent)
        return {'status': 'unconfirmed', 'checked': True}


def _hold_restart(runtime, db, agent, marker, reason, now):
    marker.update(stage='held', reason=reason, heldAt=now)
    turn_id = marker.get('turnId') or marker.get('at')
    runtime.item(db, agent['id'], 'restart-recovery-held:' + str(turn_id), 'system',
                 reason, 'Restart recovery', nativeNotice='warning', turnId=marker.get('turnId'))
    runtime.permanent_worker_hold(db, agent, 'restart:' + str(turn_id), 'held', reason)


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
            marker = agent.get('restartRecovery') or {}
            check = agent.get('connectionCheck') or {}
            stale_restart = (marker.get('stage') == 'pending'
                and any(marker.get(field) != agent.get(field)
                        for field in ('epoch', 'accountKey', 'threadId')))
            if stale_restart:
                with runtime.db() as db:
                    current_agent = runtime.agent(agent['id'], db)
                    current_marker = current_agent.get('restartRecovery') or {}
                    if (current_marker.get('stage') == 'pending'
                            and all(current_marker.get(field) == marker.get(field)
                                    for field in ('epoch', 'accountKey', 'threadId', 'turnId'))
                            and any(current_marker.get(field) != current_agent.get(field)
                                    for field in ('epoch', 'accountKey', 'threadId'))):
                        current_marker.update(stage='superseded', supersededAt=now,
                                              reason='The agent scope changed after restart.')
                        runtime.put(db, 'agents', current_agent)
                continue
            unreadable_restart_expired = (marker.get('stage') in {'pending', 'superseded'}
                and marker.get('autoWake')
                and check.get('turnId') == marker.get('turnId')
                and check.get('restartTurnStatus') not in {'inProgress', 'running'}
                and now - marker.get('at', now) >= RESTART_ACTIVE_HOLD_SECONDS)
            if unreadable_restart_expired:
                with runtime.db() as db:
                    current_agent = runtime.agent(agent['id'], db)
                    current_marker = current_agent.get('restartRecovery') or {}
                    current_check = current_agent.get('connectionCheck') or {}
                    same_recovery = (current_marker.get('stage') in {'pending', 'superseded'}
                            and current_marker.get('autoWake')
                            and all(current_marker.get(field) == agent.get(field)
                                    for field in ('epoch', 'accountKey', 'threadId', 'turnId')))
                    still_unreadable = (current_check.get('turnId') == current_marker.get('turnId')
                            and current_check.get('restartTurnStatus')
                                not in {'inProgress', 'running', 'completed', 'failed', 'interrupted'}
                            and now - current_marker.get('at', now) >= RESTART_ACTIVE_HOLD_SECONDS)
                    if same_recovery and still_unreadable:
                        reason = ('Studio could not confirm the exact native turn after restart. '
                                  'No input was resent. Check the native thread before you recover this worker.')
                        _hold_restart(runtime, db, current_agent, current_marker, reason, now)
                        runtime.put(db, 'agents', current_agent)
                        continue
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
