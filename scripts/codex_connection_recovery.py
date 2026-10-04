"""Reconcile disconnected turns before restoring confirmed completion delivery."""
from concurrent.futures import Future
import json
import threading
import time

from codex_native_errors import native_thread_block

DISCONNECT_ERRORS = frozenset({
    'Codex disconnected. Review the transcript before resuming.',
    'Server restarted during a turn. Review history, then send a new instruction.',
})
RESTART_ACTIVE_HOLD_SECONDS = 30
MAX_ACCOUNT_RECOVERIES = 8
IDENTITY = ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'status', 'error',
            'inFlight', 'autoWake', 'startAttempt', 'accountTransferId',
            'workspaceOperation', 'nativeThreadBlock', 'nativeFailureHold',
            'contextRepair', 'contextRepairWait', 'connectionRecovery', 'disconnectRecovery', 'restartRecovery')


def supervisor_identity(server):
    """Identify the native child generation, without querying or changing it."""
    if server is None or not getattr(server, 'supervisor_mode', False):
        return None
    process = getattr(server, 'proc', None)
    root = getattr(process, 'root', None)
    handle = getattr(process, 'handle', None)
    generation = getattr(process, 'generation', None)
    if root is None or not isinstance(handle, str) or type(generation) is not int or generation < 1:
        return None
    from pathlib import Path
    return {'stateDir': str(Path(root).resolve()), 'handle': handle, 'generation': generation}


def transport_turn_eligible(agent):
    """Only a saved, exact transport-loss permission can adopt an active turn."""
    from codex_context_repair import blocked
    previous = agent.get('disconnectRecovery') or {}
    attempt = agent.get('startAttempt')
    return bool(agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and agent.get('threadId') and agent.get('turnId')
            and agent.get('error') == 'Codex disconnected. Review the transcript before resuming.'
            and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not native_thread_block(agent)
            and not blocked(agent)
            and (agent.get('restartRecovery') or {}).get('stage') not in {'pending', 'superseded'}
            and previous.get('source') != 'restart' and previous.get('autoWake')
            and all(previous.get(field) == agent.get(field)
                    for field in ('epoch', 'accountKey', 'threadId', 'turnId'))
            and agent.get('turnEpoch', agent['epoch']) == agent['epoch']
            and (not attempt or (attempt == previous.get('startAttempt')
                and attempt.get('id') and attempt.get('submitted') is True and not attempt.get('action')
                and attempt.get('epoch') == agent['epoch']
                and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
                and attempt.get('turnId') in (None, agent['turnId'])
                and attempt.get('observedTurnId') in (None, agent['turnId']))))


def preparation_eligible(agent):
    attempt = agent.get('startAttempt') or {}
    busy_claude = False
    if agent.get('provider') == 'claude' and agent.get('threadId') and isinstance(agent.get('error'), str):
        try:
            busy_claude = json.loads(agent['error']) == {'code': -32000, 'message': 'Claude is still working'}
        except (ValueError, TypeError):
            pass
    return bool(agent.get('status') == 'failed' and agent.get('autoWake')
                and not agent.get('inFlight') and not agent.get('turnId')
                and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
                and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
                and not native_thread_block(agent)
                and isinstance(agent.get('error'), str)
                and (busy_claude or agent['error'] in {'AF_UNIX path too long', '[Errno 2] No such file or directory',
                    'Cannot verify the existing supervisor child; native outcome remains unknown',
                    'Supervisor native launch settings changed; existing work was preserved',
                    'Supervisor open failed: Supervisor handle exists with an incompatible or stopped child'})
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
    return disconnected_preparation_eligible(agent) or preparation_eligible(agent) or queued_restart_eligible(agent) or queued_active_wait_eligible(agent) or restart_pending or transport_turn_eligible(agent) or (agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and not agent.get('startAttempt')
            and not stale_restart
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not agent.get('deletedAt') and agent.get('threadId') and agent.get('turnId')
            and isinstance(agent.get('error'), str) and agent['error'] in DISCONNECT_ERRORS
            and not native_thread_block(agent))


def disconnected_preparation_eligible(agent):
    """A retained pre-submit receipt proves that the original input stayed local."""
    previous = agent.get('disconnectRecovery') or {}
    attempt = previous.get('startAttempt') or {}
    events = attempt.get('events')
    return bool(agent.get('status') == 'interrupted' and not agent.get('inFlight')
        and not agent.get('autoWake') and not agent.get('startAttempt') and not agent.get('turnId')
        and agent.get('error') == 'Codex disconnected. Review the transcript before resuming.'
        and previous.get('autoWake') and not previous.get('turnId')
        and all(previous.get(field) == agent.get(field) for field in ('epoch', 'accountKey', 'threadId'))
        and attempt.get('id') and attempt.get('submitted') is False
        and attempt.get('epoch') == agent.get('epoch')
        and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
        and not attempt.get('activeAtReservation') and not attempt.get('turnId')
        and not attempt.get('observedTurnId') and not attempt.get('action')
        and isinstance(events, list) and 0 < len(events) <= 32
        and all(isinstance(event, str) and event for event in events)
        and len(events) == len(set(events))
        and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
        and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
        and not native_thread_block(agent)
        and (agent.get('restartRecovery') or {}).get('stage') not in {'pending', 'held', 'superseded'})


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


def queued_active_wait_eligible(agent):
    """An unsent local wait can obscure a previously verified surviving turn."""
    wait = agent.get('contextRepairWait') or {}
    attempt = agent.get('startAttempt') or {}
    marker = agent.get('restartRecovery') or {}
    recovery = agent.get('connectionRecovery') or {}
    repair = agent.get('contextRepair') or {}
    scope = {field: agent.get(field) for field in ('id', 'accountKey', 'epoch', 'threadId')}
    events = attempt.get('events')
    repair_source = repair.get('source') or {}
    disconnect = agent.get('disconnectRecovery') or {}
    surviving_restart = (marker.get('stage') == 'finished' and marker.get('autoWake')
        and disconnect.get('source') == 'restart' and disconnect.get('autoWake')
        and all(disconnect.get(field) == agent.get(field)
                for field in ('epoch', 'accountKey', 'threadId', 'turnId'))
        and repair.get('phase') == 'failed'
        and str(repair.get('error', '')).startswith('Context repair requires a confirmed idle native thread; native status: ')
        and repair_source.get('threadId') == agent.get('threadId'))
    continued = (marker.get('stage') == 'continued' and marker.get('outcome') == 'active'
        and recovery.get('source') == 'native_thread_read' and recovery.get('outcome') == 'active'
        and recovery.get('automatic') is True and recovery.get('turnId') == agent.get('turnId'))
    same_repair = (repair.get('agent') == agent.get('id')
        and all(repair_source.get(field) == scope[field] for field in ('id', 'accountKey', 'epoch'))
        and ((repair.get('phase') == 'unchanged' and repair_source.get('threadId') == agent.get('threadId'))
             or (repair.get('phase') == 'completed' and repair.get('newThreadId') == agent.get('threadId'))
             or surviving_restart))
    return bool(agent.get('status') == 'queued' and agent.get('autoWake') and not agent.get('inFlight')
        and agent.get('threadId') and agent.get('turnId')
        and agent.get('turnEpoch', agent['epoch']) == agent['epoch']
        and not agent.get('deletedAt') and not agent.get('nativeFailureHold')
        and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
        and not native_thread_block(agent) and same_repair
        and agent.get('error') == 'Context repair waits for the current agent operation'
        and wait.get('error') == agent['error'] and wait.get('scope') == 'local'
        and wait.get('source') == {**scope, 'attemptId': attempt.get('id')}
        and not wait.get('action') and not wait.get('actionRequestId') and not wait.get('actionIdentity')
        and attempt.get('id') and attempt.get('submitted') is False
        and attempt.get('epoch') == agent['epoch']
        and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
        and not attempt.get('activeAtReservation') and not attempt.get('action')
        and not attempt.get('actionRequestId') and not attempt.get('actionIdentity')
        and not attempt.get('turnId') and not attempt.get('observedTurnId')
        and isinstance(events, list) and 0 < len(events) <= 32
        and all(isinstance(event, str) and event for event in events)
        and len(events) == len(set(events)) and wait.get('events') == events
        and (continued or surviving_restart) and marker.get('autoWake')
        and all(marker.get(field) == agent.get(field) for field in ('epoch', 'accountKey', 'threadId', 'turnId'))
        )


def recover(runtime, key, *, automatic=False):
    from codex_tool_response_recovery import recover as recover_tool_response
    delivered = recover_tool_response(runtime, key)
    if delivered.get('status') == 'tool_response_delivered':
        return delivered
    with runtime.lock, runtime.db() as db:
        agent = runtime.checked_actor(db, key)
        if runtime.closed or not eligible(agent):
            return {'status': 'superseded'}
        expected = {field: agent.get(field) for field in IDENTITY}
        account = agent.get('accountKey', 'default')
        if automatic and runtime.accounts.get(account).get('disconnected'):
            return {'status': 'superseded'}
        queued_active_wait = queued_active_wait_eligible(agent)
        if queued_active_wait:
            server = runtime.servers.get(account)
            connection = runtime.connection_ids.get(account)
            return_args = (runtime, expected, connection, server)
        previous = agent.get('disconnectRecovery') or {}
        prior_supervisor = previous.get('supervisor')
        if (not prior_supervisor and previous.get('connectionId')
                and runtime.connection_ids.get(account) == previous['connectionId']):
            # Older receipts can still identify their retained account transport.
            prior_supervisor = supervisor_identity(runtime.servers.get(account))
    if queued_active_wait:
        return recover_queued_active_wait(*return_args)
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
        if disconnected_preparation_eligible(agent):
            return restore_disconnected_preparation(runtime, expected, connection, server)
        if preparation_eligible(agent):
            return restore_preparation(runtime, expected, connection, server)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        marker = agent.get('restartRecovery') or {}
        pending_restart = restart_turn_pending(agent)
        if (automatic and not pending_restart and transport_turn_eligible(agent)
                and thread.get('status', {}).get('type') == 'active'
                and not thread.get('status', {}).get('activeFlags')):
            if not prior_supervisor or supervisor_identity(server) != prior_supervisor:
                return record_check(runtime, expected, connection, server, 'active')
            page = server.call('thread/turns/list', {'threadId': agent['threadId'], 'limit': 1,
                               'sortDirection': 'desc', 'itemsView': 'full'}, timeout=5)
            turn = next(iter(page.get('data') or []), None)
            thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != agent['threadId']:
                raise ValueError('Native thread identity changed')
            if (thread.get('status', {}).get('type') == 'active'
                    and not thread.get('status', {}).get('activeFlags')
                    and turn and turn.get('id') == agent['turnId']
                    and turn.get('status') in {'inProgress', 'running'}):
                result = Future()
                def adopt():
                    try:
                        result.set_result(adopt_transport_turn(runtime, expected, connection,
                                                              server, turn, prior_supervisor))
                    except Exception as error:
                        result.set_exception(error)
                server.after_events(adopt)
                return result.result(timeout=10)
            return record_check(runtime, expected, connection, server,
                                thread.get('status', {}).get('type'))
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


def recover_queued_active_wait(runtime, expected, connection, server):
    """Use the current transport only; never attach, resume, or submit input."""
    from codex_native_tools import account_reserved
    account = expected.get('accountKey') or 'default'
    with runtime.lock:
        native_identity = supervisor_identity(server)
        if (runtime.closed or server is None or getattr(server, 'closed', False)
                or native_identity is None or not runtime.connection_current(account, connection)
                or account_reserved(runtime, account) or runtime.accounts.get(account).get('disconnected')):
            return {'status': 'unconfirmed'}
    try:
        observed = []
        for _ in range(2):
            thread = server.call('thread/read', {'threadId': expected['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != expected['threadId']:
                return {'status': 'unconfirmed'}
            status = thread.get('status') or {}
            if status.get('activeFlags') or status.get('type') not in {'active', 'idle', 'notLoaded'}:
                return {'status': 'unconfirmed'}
            page = server.call('thread/turns/list', {'threadId': expected['threadId'], 'limit': 1,
                               'sortDirection': 'desc', 'itemsView': 'notLoaded'}, timeout=5)
            turn = next(iter(page.get('data') or []), None)
            if not turn or turn.get('id') != expected['turnId']:
                return {'status': 'unconfirmed'}
            observed.append((status.get('type'), turn.get('status')))
        if observed[0] != observed[1]:
            return {'status': 'unconfirmed'}
        native_state, outcome = observed[-1]
        active = native_state == 'active' and outcome in {'inProgress', 'running'}
        terminal = native_state in {'idle', 'notLoaded'} and outcome in {'completed', 'failed', 'interrupted'}
        if not active and not terminal:
            return {'status': 'unconfirmed'}
        full_turn = None
        if terminal:
            from codex_turn_recovery import read_native_turn
            full_turn = read_native_turn(server, expected['threadId'], expected['turnId'])
            if not full_turn or full_turn.get('id') != expected['turnId'] or full_turn.get('status') != outcome:
                return {'status': 'unconfirmed'}
            thread = server.call('thread/read', {'threadId': expected['threadId'], 'includeTurns': False}, timeout=5)['thread']
            status = thread.get('status') or {}
            page = server.call('thread/turns/list', {'threadId': expected['threadId'], 'limit': 1,
                               'sortDirection': 'desc', 'itemsView': 'notLoaded'}, timeout=5)
            latest = next(iter(page.get('data') or []), None)
            if (thread.get('id') != expected['threadId'] or status.get('activeFlags')
                    or status.get('type') != native_state or not latest
                    or latest.get('id') != expected['turnId'] or latest.get('status') != outcome):
                return {'status': 'unconfirmed'}
        result = Future()
        def apply():
            try:
                with runtime.lock:
                    with runtime.db() as db:
                        if (not current(runtime, db, expected, connection, server)
                                or supervisor_identity(server) != native_identity or account_reserved(runtime, account)
                                or runtime.accounts.get(account).get('disconnected')):
                            result.set_result({'status': 'superseded'})
                            return
                        agent = runtime.agent(expected['id'], db)
                        if db.execute("SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
                                "AND json_extract(record,'$.status')='pending' AND "
                                "(json_extract(record,'$.turnId') IS NULL OR json_extract(record,'$.turnId')=?) LIMIT 1",
                                (agent['id'], expected['turnId'])).fetchone():
                            result.set_result({'status': 'unconfirmed'})
                            return
                        for event_id in agent['startAttempt']['events']:
                            row = db.execute('SELECT agent,epoch,status,turn_id FROM runtime_events WHERE id=?', (event_id,)).fetchone()
                            if not row or tuple(row) != (agent['id'], agent['epoch'], 'pending', None):
                                result.set_result({'status': 'unconfirmed'})
                                return
                        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                                      "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                                      (agent['id'], agent['epoch'])).fetchone():
                            result.set_result({'status': 'unconfirmed'})
                            return
                        wait = agent.pop('contextRepairWait')
                        attempt = agent.pop('startAttempt')
                        agent['lastContextRepairWait'] = {**wait, 'status': 'superseded',
                            'reason': 'The exact native turn was confirmed', 'finishedAt': time.time()}
                        agent.update(status='running', inFlight=True, error=None)
                        agent['connectionRecovery'] = {**(agent.get('connectionRecovery') or {}), 'at': time.time(),
                            'turnId': expected['turnId'], 'outcome': 'active',
                            'supervisor': native_identity, 'queuedInputPreserved': True,
                            'attemptId': attempt['id'], 'eventIds': list(attempt['events'])}
                        runtime.loaded.add(agent['id'])
                        runtime.put(db, 'agents', agent)
                        identity = {field: agent.get(field) for field in
                                    ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
                    # Commit the unsent wait receipt before terminal callbacks.
                    # Keep dispatch outside this lock until the old turn settles.
                    value = (runtime.apply_turn_recovery(identity, connection, native_state, full_turn)
                             if terminal else {'status': 'adopted', 'turnId': expected['turnId']})
                runtime.changed.set()
                result.set_result(value)
            except Exception as error:
                result.set_exception(error)
        server.after_events(apply)
        return result.result(timeout=10)
    except Exception as error:
        return {'status': 'unconfirmed', 'error': str(error)}


def adopt_transport_turn(runtime, expected, connection, server, turn, prior_supervisor):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        if (not transport_turn_eligible(agent)
                or runtime.accounts.get(agent.get('accountKey', 'default')).get('disconnected')
                or supervisor_identity(server) != prior_supervisor):
            return {'status': 'superseded'}
        if (unresolved_turn_receipts(db, agent, turn['id'])
                or db.execute("SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
                    "AND json_extract(record,'$.status')='pending' AND "
                    "(json_extract(record,'$.turnId') IS NULL OR json_extract(record,'$.turnId')=?) LIMIT 1",
                    (agent['id'], turn['id'])).fetchone()):
            return {'status': 'unconfirmed'}
        agent.update(status='running', autoWake=True, inFlight=True, error=None)
        agent['connectionRecovery'] = {'source': 'native_thread_read', 'at': time.time(),
            'turnId': turn['id'], 'outcome': 'active', 'automatic': True,
            'previousError': expected['error'], 'supervisor': prior_supervisor}
        runtime.loaded.add(agent['id'])
        runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return {'status': 'adopted', 'turnId': turn['id']}


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


def restore_disconnected_preparation(runtime, expected, connection, server):
    """Return only the exact unsubmitted batch to its original queue."""
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        if not disconnected_preparation_eligible(agent):
            return {'status': 'superseded'}
        attempt = agent['disconnectRecovery']['startAttempt']
        rows = []
        for event_id in attempt['events']:
            row = db.execute('SELECT agent,epoch,status,turn_id FROM runtime_events WHERE id=?',
                             (event_id,)).fetchone()
            if (not row or row['agent'] != agent['id'] or row['epoch'] != agent['epoch']
                    or row['status'] not in {'pending', 'reserved'} or row['turn_id'] is not None):
                return {'status': 'unconfirmed'}
            rows.append(event_id)
        for event_id in rows:
            db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=? "
                       "AND agent=? AND epoch=? AND status IN ('pending','reserved') AND turn_id IS NULL",
                       (event_id, agent['id'], agent['epoch']))
        agent['connectionRecovery'] = {'source': 'disconnected_preparation', 'at': time.time(),
            'attemptId': attempt['id'], 'eventIds': rows, 'previousError': agent['error'],
            'outcome': 'input_restored'}
        agent.update(status='queued', autoWake=True, inFlight=False, error=None)
        runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return {'status': 'input_restored', 'attemptId': attempt['id']}


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
    return not unresolved_turn_receipts(db, agent, turn.get('id'))


def unresolved_turn_receipts(db, agent, turn_id):
    # A completed model turn does not settle a lost process or tool mutation.
    # Pending input remains pending; no uncertain input is replayed.
    if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                  "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                  (agent['id'], agent['epoch'])).fetchone():
        return True
    for table in ('runtime_tasks', 'runtime_monitors'):
        for row in db.execute(f"SELECT record FROM {table} WHERE json_extract(record,'$.agent')=? "
                              "AND json_extract(record,'$.status') IN ('lost','running','starting','approval')",
                              (agent['id'],)):
            operation = json.loads(row['record'])
            if operation.get('turnId') in (None, turn_id):
                return True
    for row in db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "AND (json_extract(record,'$.outcome')='unknown' OR "
                          "json_extract(record,'$.stage') IN ('queued','running'))", (agent['id'],)):
        operation = json.loads(row['record'])
        if operation.get('turnId') in (None, turn_id):
            return True
    return False


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
    """One read job per account; a stalled account does not consume another's slot."""
    with runtime.lock:
        if runtime.closed:
            return
        jobs = runtime.__dict__.setdefault('_connection_recovery_jobs', {})
        # A live update can leave one reader on the old shared executor.
        if getattr(runtime, '_connection_recovery_busy', False) and not jobs:
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
            account = agent.get('accountKey', 'default')
            if account in jobs or runtime.accounts.get(account).get('disconnected'):
                continue
            identity = tuple(agent.get(field) for field in ('id', 'epoch', 'accountKey', 'threadId', 'turnId'))
            disconnect = agent.get('disconnectRecovery') or {}
            identity += (disconnect.get('connectionId'), disconnect.get('at'), marker.get('at'))
            previous = checks.get(identity, {})
            if previous.get('nextAt', 0) <= now:
                candidates.append((previous.get('at', 0), identity, agent['id'], account))
        if not candidates:
            return
        for _, identity, key, account in sorted(candidates, key=lambda candidate: candidate[0]):
            if len(jobs) >= MAX_ACCOUNT_RECOVERIES:
                break
            if account in jobs:
                continue
            worker = threading.Thread(target=run, args=(runtime, key, identity),
                                      name='studio-connection-recovery-' + account[:12], daemon=True)
            jobs[account] = worker
            runtime._connection_recovery_busy = True
            try:
                worker.start()
            except Exception:
                jobs.pop(account, None)
                runtime._connection_recovery_busy = bool(jobs)
                raise


def run(runtime, key, identity):
    result = {'status': 'unconfirmed'}
    try:
        result = recover(runtime, key, automatic=True)
        return result
    finally:
        with runtime.lock:
            checks = runtime.__dict__.setdefault('_connection_recovery_checks', {})
            restored = result.get('status') in {'adopted', 'reconciled', 'input_restored'}
            failures = 0 if restored else min(checks.get(identity, {}).get('failures', 0) + 1, 6)
            delay = 0 if restored else min(300, 15 * 2 ** (failures - 1))
            now = time.time()
            checks[identity] = {**result, 'at': now, 'nextAt': now + delay, 'failures': failures}
            # Keep recent diagnostics without retaining every historical epoch.
            if len(checks) > 256:
                for old in sorted(checks, key=lambda key: checks[key]['at'])[:-256]:
                    checks.pop(old)
            jobs = runtime.__dict__.setdefault('_connection_recovery_jobs', {})
            jobs.pop(identity[2] or 'default', None)
            runtime._connection_recovery_busy = bool(jobs)
            runtime.changed.set()


class ConnectionRecovery:
    """Check durable recovery work independently of the dispatch scheduler."""
    def __init__(self, runtime, interval=2):
        self.runtime = runtime
        self.interval = interval
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, name='studio-connection-recovery', daemon=True)

    def run(self):
        while not self.stop.wait(self.interval) and not self.runtime.closed:
            # A recovery timer must not join a queue behind a long runtime write.
            if not self.runtime.lock.acquire(blocking=False):
                continue
            try:
                with self.runtime.read_db() as db:
                    agents = self.runtime.scheduler_agents(db)
                tick(self.runtime, agents)
                from codex_tool_response_recovery import tick as tool_response_tick
                tool_response_tick(self.runtime, agents)
            except Exception as error:
                self.runtime._connection_recovery_error = {'at': time.time(), 'error': str(error)[:300]}
            finally:
                self.runtime.lock.release()

    def close(self):
        self.stop.set()
        if self.thread is not threading.current_thread():
            self.thread.join()


_START_LOCK = threading.Lock()


def start(runtime, *, interval=2):
    with _START_LOCK:
        manager = getattr(runtime, '_connection_recovery_service', None)
        if manager is None:
            manager = runtime._connection_recovery_service = ConnectionRecovery(runtime, interval)
            manager.thread.start()
        return manager


def close(runtime):
    manager = getattr(runtime, '_connection_recovery_service', None)
    if manager is not None:
        manager.close()
    # Runtime.close sets closed and closes native transports before releasing its
    # lease. Account readers must finish before another runtime owns that state.
    with runtime.lock:
        workers = [*getattr(runtime, '_connection_recovery_jobs', {}).values(),
                   *getattr(runtime, '_tool_response_recovery_jobs', {}).values()]
    for worker in workers:
        if worker is not threading.current_thread():
            worker.join()
