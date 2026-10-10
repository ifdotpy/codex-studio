"""Preserve continuation authority across process exit and host restart."""
import copy
import json
import time
from typing import TYPE_CHECKING, Literal, NotRequired, TypedDict

if TYPE_CHECKING:
    import sqlite3
    from codex_records import AgentRecord
    from codex_runtime import Runtime


class NativeTurnItem(TypedDict, total=False):
    type: str
    status: str
    command: str
    id: str


class NativeTurn(TypedDict):
    id: str
    status: NotRequired[str]
    items: NotRequired[list[NativeTurnItem]]

RESTART_ERROR = 'Server restarted during a turn. Review history, then send a new instruction.'
SCOPE: tuple[Literal['epoch'], Literal['accountKey'], Literal['threadId']] = ('epoch', 'accountKey', 'threadId')


def capture(agent: "AgentRecord") -> None:
    if (agent.get('deletedAt') or not agent.get('autoWake')
            or agent.get('nativeFailureHold') or agent.get('status') == 'paused'
            or not (agent.get('inFlight') or agent.get('status') in {'running', 'starting', 'approval'})):
        return
    agent['restartRecovery'] = {
        **{key: agent.get(key) for key in (*SCOPE, 'turnId')},  # type: ignore[typeddict-item]  # typed-narrowing: scope copied from agent record
        'autoWake': True, 'at': time.time(), 'stage': 'pending',
        'startAttempt': copy.deepcopy(agent.get('startAttempt')),
    }


def restore(db: "sqlite3.Connection", agent: "AgentRecord") -> bool:
    settle_reconciled(agent)
    old = agent.get('restartRecovery') or {}
    # A native connection can die before graceful Runtime.close() captures the
    # active turn. disconnected() records the permission and exact native scope
    # before clearing autoWake; on the next process, restore that receipt rather
    # than treating the agent as an explicit stop.
    disconnected = agent.get('disconnectRecovery') or {}
    if (old.get('stage') != 'pending' and disconnected.get('autoWake')
            and disconnected.get('epoch') == agent.get('epoch')
            and disconnected.get('accountKey', 'default') == agent.get('accountKey', 'default')
            and disconnected.get('threadId') == agent.get('threadId')
            and agent.get('status') == 'interrupted'):
        agent['restartRecovery'] = {
            **{key: disconnected.get(key) for key in (*SCOPE, 'turnId')},  # type: ignore[typeddict-item]  # typed-narrowing: scope copied from receipt fields
            'autoWake': True, 'at': disconnected.get('at', time.time()),
            'stage': 'pending', 'startAttempt': copy.deepcopy(
                agent.get('startAttempt') or disconnected.get('startAttempt')),
        }
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
        agent.update(status='queued', autoWake=True, inFlight=False, error=None)  # type: ignore[call-arg]  # typed-update
        marker['stage'] = 'input_restored'
        agent['connectionRecovery'] = {'source': 'restart_unsent_attempt', 'at': time.time(),
            'attemptId': attempt.get('id'), 'eventIds': list(attempt.get('events', [])),
            'outcome': 'input_restored'}
        review = agent.get('nativeReview') or {}
        if attempt.get('action') == 'review' and review.get('status') == 'started':
            review.pop('startedAt', None)
            review['status'] = 'pending'
            agent['nativeReview'] = review
        return True
    if turn:
        agent.update(turnId=turn, status='interrupted', autoWake=False,
                     inFlight=False, error=RESTART_ERROR)  # type: ignore[call-arg]  # typed-update
        agent['disconnectRecovery'] = {**{key: marker.get(key) for key in (*SCOPE, 'turnId')},  # type: ignore[typeddict-item]  # typed-narrowing: scope copied from restart marker
                                       'autoWake': True, 'at': marker['at'], 'source': 'restart'}
        return True
    # No native turn identity means an accepted input cannot be reconciled yet.
    marker['stage'] = 'held'
    marker['reason'] = 'Native input submission has no confirmed turn identity.'
    valid_events = attempt.get('events') and all(db.execute(
        "SELECT 1 FROM runtime_events WHERE id=? AND agent=? AND epoch=? "
        "AND status IN ('reserved','dispatching','uncertain')",
        (event_id, agent['id'], agent['epoch'])).fetchone() for event_id in attempt.get('events', []))
    if (attempt.get('submitted') is True and valid_events and agent.get('threadId')):
        event_ids = list(attempt['events'])
        error = 'Context repair waits for a confirmed input receipt: ' + event_ids[0]
        agent.update(status='queued', autoWake=True, inFlight=False, error=error,
                     contextRepairWait={'error': error, 'scope': 'native',
                         'source': {'id': agent['id'], 'accountKey': agent.get('accountKey', 'default'),
                                    'epoch': agent['epoch'], 'threadId': agent['threadId'],
                                    'attemptId': None}, 'events': event_ids, 'nextCheckAt': 0})  # type: ignore[call-arg]  # typed-update
    return False


def can_continue(db: "sqlite3.Connection", agent: "AgentRecord", turn: NativeTurn) -> bool:
    # A restart interrupts running commands by design. The agent continues and
    # checks them itself; only an input with an unknown delivery blocks it.
    marker = agent.get('restartRecovery') or {}
    return bool(marker.get('stage') in {'pending', 'superseded'} and marker.get('autoWake')
                and turn.get('status') == 'interrupted'
                and marker.get('turnId') == turn.get('id')
                and all(marker.get(key) == agent.get(key) for key in SCOPE)
                and not agent.get('nativeFailureHold')
                and not db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                                   "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                                   (agent['id'], agent['epoch'])).fetchone())


def _interrupted_work(db: "sqlite3.Connection", agent: "AgentRecord", turn: NativeTurn) -> list[str]:
    names = []
    for row in db.execute("SELECT record FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
                          "AND json_extract(record,'$.status') IN ('lost','running','starting','approval') "
                          "ORDER BY json_extract(record,'$.created') DESC LIMIT 5", (agent['id'],)):
        task = json.loads(row[0])
        names.append(str(task.get('command') or task.get('tool') or task.get('kind') or task['id'])[:160])
    for item in turn.get('items', []):
        if (item.get('type') == 'commandExecution' and item.get('status') not in {'completed', 'failed', 'declined'}
                and len(names) < 5):
            names.append(str(item.get('command') or item.get('id'))[:160])
    return names


def settle_reconciled(agent: "AgentRecord") -> bool:
    """Close a verified restart receipt without granting continuation authority."""
    marker = agent.get('restartRecovery') or {}
    receipt = agent.get('connectionRecovery') or {}
    disconnected = agent.get('disconnectRecovery') or {}
    turn = marker.get('turnId')
    if (marker.get('stage') != 'pending' or not turn
            or agent.get('inFlight') or agent.get('turnId')
            or any(marker.get(key) != agent.get(key) for key in SCOPE)
            or any(disconnected.get(key) != marker.get(key) for key in (*SCOPE, 'turnId'))
            or receipt.get('source') != 'native_thread_read'
            or receipt.get('turnId') != turn
            or receipt.get('outcome') not in {'completed', 'failed', 'interrupted'}
            or not isinstance(receipt.get('at'), (int, float))
            or not isinstance(marker.get('at'), (int, float))
            or receipt['at'] < marker['at']):
        return False
    # Later turns can replace lastCompletedTurn. The exact recovery receipt
    # still proves that this older restart turn has been reconciled.
    marker.update(stage='finished', reconciledAt=receipt['at'])  # type: ignore[call-arg]  # typed-update
    return True


def continue_interrupted(runtime: "Runtime", db: "sqlite3.Connection",
                          agent: "AgentRecord", turn: NativeTurn) -> str:
    marker = agent['restartRecovery']
    key = 'restart:' + agent['id'] + ':' + str(agent['epoch']) + ':' + turn['id']
    agent.update(autoWake=True, error=None, inFlight=False, turnId=None,
                 activity=None, activeTools=[])  # type: ignore[call-arg]  # typed-update
    marker.update(stage='continued', eventId=key, reconciledAt=time.time())  # type: ignore[call-arg]  # typed-update
    runtime.put(db, 'agents', agent)
    work = _interrupted_work(db, agent, turn)
    runtime.enqueue(db, agent, 'followup',
        'Studio restarted. The previous native turn is confirmed interrupted. '
        'Continue the existing authorized task from its saved history. '
        'Check existing results before repeating any operation.'
        + (' The restart stopped these commands; check their effects first: ' + '; '.join(work) if work else ''),
        key)
    return key
