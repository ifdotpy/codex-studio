"""Preserve continuation authority across process exit and host restart."""
import copy
import time

RESTART_ERROR = 'Server restarted during a turn. Review history, then send a new instruction.'
SCOPE = ('epoch', 'accountKey', 'threadId')


def capture(agent):
    if (agent.get('deletedAt') or not agent.get('autoWake')
            or agent.get('nativeFailureHold') or agent.get('status') == 'paused'
            or not (agent.get('inFlight') or agent.get('status') in {'running', 'starting', 'approval'})):
        return
    agent['restartRecovery'] = {
        **{key: agent.get(key) for key in (*SCOPE, 'turnId')},
        'autoWake': True, 'at': time.time(), 'stage': 'pending',
        'startAttempt': copy.deepcopy(agent.get('startAttempt')),
    }


def restore(db, agent):
    old = agent.get('restartRecovery') or {}
    if old.get('stage') == 'pending' and any(old.get(key) != agent.get(key) for key in SCOPE):
        old['stage'] = 'superseded'
        return False
    # An active persisted turn is newer authority than an old same-scope receipt.
    capture(agent)
    marker = agent.get('restartRecovery')
    if not isinstance(marker, dict) or marker.get('stage') != 'pending':
        return False
    if (agent.get('deletedAt') or not marker.get('autoWake')
            or any(marker.get(key) != agent.get(key) for key in SCOPE)
            or agent.get('status') == 'paused' or agent.get('nativeFailureHold')):
        marker['stage'] = 'superseded'
        return False
    turn = marker.get('turnId')
    latest_attempt = agent.get('startAttempt') or {}
    saved_attempt = marker.get('startAttempt') or {}
    if latest_attempt.get('id') and latest_attempt.get('id') == saved_attempt.get('id'):
        marker['startAttempt'] = copy.deepcopy(latest_attempt)
        turn = latest_attempt.get('turnId') or latest_attempt.get('observedTurnId') or turn
        marker['turnId'] = turn
    # A completion delivered during graceful shutdown already owns the outcome.
    if (turn and agent.get('lastCompletedTurn') == turn
            and agent.get('lastCompletedTurnStatus') in {'completed', 'failed'}):
        marker['stage'] = 'finished'
        return False
    attempt = marker.get('startAttempt') or {}
    if (attempt.get('submitted') is False and attempt.get('epoch') == agent['epoch']
            and attempt.get('accountKey') == agent.get('accountKey')
            and not attempt.get('turnId') and not attempt.get('observedTurnId')):
        for key in attempt.get('events', []):
            db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=? "
                       "AND agent=? AND epoch=? AND status IN ('reserved','dispatching','uncertain')",
                       (key, agent['id'], agent['epoch']))
        agent.update(status='queued', autoWake=True, inFlight=False, error=None)
        marker['stage'] = 'input_restored'
        return True
    if turn:
        agent.update(turnId=turn, status='interrupted', autoWake=False,
                     inFlight=False, error=RESTART_ERROR)
        agent['disconnectRecovery'] = {**{key: marker.get(key) for key in (*SCOPE, 'turnId')},
                                       'autoWake': True, 'at': marker['at'], 'source': 'restart'}
        return True
    # No native turn identity means an accepted input cannot be reconciled yet.
    marker['stage'] = 'held'
    marker['reason'] = 'Native input submission has no confirmed turn identity.'
    return False


def can_continue(db, agent, turn):
    from codex_connection_recovery import can_deliver_completion
    marker = agent.get('restartRecovery') or {}
    return (marker.get('stage') == 'pending' and marker.get('autoWake')
            and turn.get('status') == 'interrupted'
            and marker.get('turnId') == turn.get('id')
            and all(marker.get(key) == agent.get(key) for key in SCOPE)
            and can_deliver_completion(db, agent, {**turn, 'status': 'completed'}))


def continue_interrupted(runtime, db, agent, turn):
    marker = agent['restartRecovery']
    key = 'restart:' + agent['id'] + ':' + str(agent['epoch']) + ':' + turn['id']
    agent.update(autoWake=True, error=None, inFlight=False, turnId=None,
                 activity=None, activeTools=[])
    marker.update(stage='continued', eventId=key, reconciledAt=time.time())
    runtime.put(db, 'agents', agent)
    runtime.enqueue(db, agent, 'followup',
        'Studio restarted. The previous native turn is confirmed interrupted. '
        'Continue the existing authorized task from its saved history. '
        'Check existing results before repeating any operation.', key)
    return key
