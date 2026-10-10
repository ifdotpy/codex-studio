"""Retry only Claude input that an exact native receipt proves was not applied."""
import copy
import hashlib
import json
from typing import TYPE_CHECKING, Any, Literal, cast

from codex_native_errors import NativeRpcError, native_thread_block

if TYPE_CHECKING:
    import sqlite3
    from codex_records import AgentRecord, StartAttemptRecord
    from codex_runtime import Runtime


class ClaudeRetryChanged(ValueError):
    """The original input is safe to retry, but its current scope changed."""


def _attempt_receipt(db: "sqlite3.Connection", agent_id: str, attempt: Any) -> Any:
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' "
                      "AND name='runtime_execution_attempts'").fetchone():
        return None
    row = db.execute(
        "SELECT t.record FROM runtime_execution_attempts t "
        "JOIN runtime_execution_runs r ON r.id=t.run "
        "WHERE t.id=? AND r.agent=?", (attempt.get('id'), agent_id)).fetchone()
    if not row:
        return None
    saved = json.loads(row[0])
    fields = ('id', 'epoch', 'accountKey', 'threadId', 'connectionId',
              'nativeOperationId', 'events', 'submitted', 'supervisorIdentity', 'claudeInputRequest')
    return saved if all(saved.get(key) == attempt.get(key) for key in fields) else None


def rejected_start_saved(db: "sqlite3.Connection", agent_id: str, attempt: Any) -> bool:
    """A late RPC success cannot replace the same request's durable rejection."""
    saved = _attempt_receipt(db, agent_id, attempt)
    receipt = (saved or {}).get('claudeInputRejection') or {}
    return (receipt.get('outcome') == 'not_applied' and receipt.get('agent') == agent_id
            and all(receipt.get(key) == attempt.get(key) for key in
                    ('id', 'epoch', 'accountKey', 'threadId', 'connectionId', 'nativeOperationId', 'events')))


def _supervisor_matches(server: Any, expected: Any) -> bool:
    if server is None or getattr(server, 'closed', False):
        return False
    if not getattr(server, 'supervisor_mode', False):
        return expected is None
    process = getattr(server, 'proc', None)
    # No path resolution under the caller's runtime lock or SQLite transaction.
    return (isinstance(expected, dict) and expected.get('stateDir') == str(getattr(process, 'root', None))
            and expected.get('handle') == getattr(process, 'handle', None)
            and expected.get('generation') == getattr(process, 'generation', None))


def _source(agent: "AgentRecord") -> dict[str, Any]:
    return {key: agent.get(key) for key in (
        'cwd', 'rootId', 'parentId', 'role', 'isLead', 'model', 'effort', 'nativeEffort',
        'fastMode', 'yoloMode', 'imageWorkspace', 'imageWorkspaceReady', 'compactions')}


def _event_snapshot(db: "sqlite3.Connection", row: Any) -> dict[str, Any] | None:
    saved = db.execute('SELECT kind,text,created FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                       (row['id'], row['agent'], row['epoch'])).fetchone()
    meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (row['id'],)).fetchone()
    assets = json.loads(meta[0]).get('assets', []) if meta else []
    return ({'id': row['id'], 'kind': saved['kind'], 'created': saved['created'], 'assets': assets,
             'textHash': hashlib.sha256(saved['text'].encode()).hexdigest()} if saved else None)


def capture_input(
    db: "sqlite3.Connection", agent: "AgentRecord", rows: list[Any], params: Any, text: str
) -> dict[str, Any]:
    """Keep the exact input beside its submitted attempt, before native I/O."""
    return {'input': copy.deepcopy(params['input']), 'source': _source(agent),
            'clientUserMessageId': params['clientUserMessageId'],
            'configuration': {key: copy.deepcopy(value) for key, value in params.items()
                              if key not in {'input', 'dynamicTools', 'clientUserMessageId'}},
            'events': [_event_snapshot(db, row) for row in rows],
            'transcriptItemId': agent['id'] + ':' + rows[0]['id'],
            'textHash': hashlib.sha256(text.encode()).hexdigest()}


def _later_user_input(db: "sqlite3.Connection", agent: "AgentRecord", attempt: Any) -> bool:
    """A newer pending user instruction can resume a proven rejected input."""
    created = attempt.get('created')
    if type(created) not in (int, float):
        return False
    return bool(db.execute(
        "SELECT 1 FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id "
        "WHERE e.agent=? AND e.epoch=? AND e.kind='user' AND e.status='pending' "
        "AND e.turn_id IS NULL AND e.created>? "
        "AND json_type(m.record,'$.acceptedAt') IN ('integer','real') "
        "AND json_extract(m.record,'$.acceptedAt')>? LIMIT 1",
        (agent['id'], agent['epoch'], created, created)).fetchone())


def _saved_retry(runtime: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord",
                 *, allow_held: bool = False) -> Any:
    marker = agent.get('claudePreInputRetry')
    if (not isinstance(marker, dict) or not agent.get('autoWake') or agent.get('status') == 'paused'
            or runtime.closed or agent.get('provider') != 'claude' or agent.get('deletedAt')
            or agent.get('agentArchive') or (agent.get('nativeFailureHold') and not allow_held)
            or agent.get('accountTransferId') or agent.get('workspaceOperation') or native_thread_block(agent)
            or marker.get('epoch') != agent.get('epoch')
            or marker.get('accountKey') != agent.get('accountKey', 'default')
            or marker.get('threadId') != agent.get('threadId')):
        return None
    row = db.execute('SELECT t.record FROM runtime_execution_attempts t '
                     'JOIN runtime_execution_runs r ON r.id=t.run WHERE t.id=? AND r.agent=?',
                     (marker.get('id'), agent['id'])).fetchone()
    saved = json.loads(row[0]) if row else None
    if not saved or any(saved.get(key) != value for key, value in marker.items()):
        return None
    request = saved.get('claudeInputRequest')
    if (not rejected_start_saved(db, agent['id'], saved) or not isinstance(request, dict)
            or not isinstance(request.get('input'), list) or not isinstance(request.get('configuration'), dict)
            or request.get('transcriptItemId') != agent['id'] + ':' + saved['events'][0]
            or request.get('clientUserMessageId') != saved['events'][0]):
        return None
    source = _source(agent)
    captured = request.get('source')
    later_user = _later_user_input(db, agent, saved)
    if captured != source:
        # A background Claude turn can compact history while its rejected input
        # stays local. A newer instruction permits the identical input after that
        # compaction, with every workspace, account and permission field unchanged.
        if (not later_user or not isinstance(captured, dict)
                or type(captured.get('compactions')) is not int
                or type(source.get('compactions')) is not int
                or source['compactions'] <= captured['compactions']
                or {k: v for k, v in captured.items() if k != 'compactions'}
                    != {k: v for k, v in source.items() if k != 'compactions'}):
            return None
    same_transport = (runtime.connection_current(cast(str, marker.get('accountKey')), marker.get('connectionId'))
                      and _supervisor_matches(runtime.server_for(marker.get('accountKey'), runtime.agent_connection(agent)),
                                              marker.get('supervisorIdentity')))
    return saved if same_transport or later_user else None


def _retry_rows(db: "sqlite3.Connection", agent: "AgentRecord", saved: Any) -> list[Any] | None:
    rows = [db.execute('SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                       (key, agent['id'], agent['epoch'])).fetchone() for key in saved['events']]
    if (any(row is None or row['status'] != 'pending' or row['turn_id'] for row in rows)
            or [_event_snapshot(db, row) for row in rows] != saved['claudeInputRequest']['events']):
        return None
    return [dict(row) for row in rows]


def _input_text(db: "sqlite3.Connection", agent: "AgentRecord", request: Any) -> str | None:
    item = db.execute('SELECT record FROM runtime_items WHERE id=? AND agent=?',
                      (request['transcriptItemId'], agent['id'])).fetchone()
    fulltext = db.execute('SELECT body FROM runtime_item_fulltext WHERE id=?',
                          (request['transcriptItemId'],)).fetchone()
    item = json.loads(item[0]) if item else None
    text = fulltext[0] if fulltext else (item or {}).get('text')
    if (not item or item.get('role') != 'user' or not isinstance(text, str)
            or hashlib.sha256(text.encode()).hexdigest() != request['textHash']):
        return None
    return text


def _resume_held_retry(runtime: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord") -> None:
    if (not agent.get('nativeFailureHold') or agent.get('inFlight')
            or agent.get('turnId') or agent.get('startAttempt')):
        return
    saved = _saved_retry(runtime, db, agent, allow_held=True)
    if (not saved or not _later_user_input(db, agent, saved)
            or _retry_rows(db, agent, saved) is None
            or _input_text(db, agent, saved['claudeInputRequest']) is None):
        return
    agent.pop('nativeFailureHold', None)
    agent.update(status='queued', error=None)  # type: ignore[call-arg]  # typed-update
    runtime.put(db, 'agents', agent)


def retire_stopped_retry(runtime: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord") -> bool:
    """Resume a proven held retry or retire its exact stopped reservation."""
    _resume_held_retry(runtime, db, agent)
    marker = agent.get('claudePreInputRetry') or {}
    epoch = marker.get('epoch')
    if (runtime.closed or agent.get('provider') != 'claude' or not isinstance(marker, dict)
            or type(epoch) is not int or epoch >= agent['epoch']):
        return False
    row = db.execute('SELECT t.record FROM runtime_execution_attempts t '
                     'JOIN runtime_execution_runs r ON r.id=t.run WHERE t.id=? AND r.agent=?',
                     (marker.get('id'), agent['id'])).fetchone()
    saved = json.loads(row[0]) if row else None
    if (not saved or any(saved.get(key) != value for key, value in marker.items())
            or not rejected_start_saved(db, agent['id'], saved)):
        return False
    events = []
    for key in saved['events']:
        event = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                           (key, agent['id'], epoch)).fetchone()
        if (not event or event['status'] not in {'cancelled', 'reserved', 'dispatching'}
                or event['turn_id'] is not None):
            return False
        events.append(event)
    attempt = agent.get('startAttempt') or {}
    if any(event['status'] != 'cancelled' for event in events):
        if (agent.get('inFlight') or agent.get('turnId') or agent.get('deletedAt')
                or agent.get('agentArchive') or agent.get('accountTransferId')
                or agent.get('workspaceOperation') or native_thread_block(agent)
                or marker.get('accountKey') != agent.get('accountKey', 'default')
                or marker.get('threadId') != agent.get('threadId')
                or not runtime.connection_current(cast(str, marker.get('accountKey')), marker.get('connectionId'))
                or not _supervisor_matches(runtime.server_for(marker.get('accountKey'), runtime.agent_connection(agent)),
                                           marker.get('supervisorIdentity'))
                or saved.get('claudeInputRequest', {}).get('source') != _source(agent)
                or attempt.get('claudeRetryOf') != marker.get('id')
                or attempt.get('epoch') != epoch or attempt.get('events') != saved['events']
                or attempt.get('accountKey') != marker.get('accountKey')
                or attempt.get('submitted') is not False or attempt.get('action')
                or attempt.get('activeAtReservation') or attempt.get('nativeOperationId')
                or attempt.get('turnId') or attempt.get('observedTurnId')
                or attempt.get('threadId') not in (None, marker.get('threadId'))
                or attempt.get('connectionId') not in (None, marker.get('connectionId'))):
            return False
        reservation = _attempt_receipt(db, agent['id'], attempt)
        if (not reservation or reservation.get('submission') != 'unsent'
                or reservation.get('claudeRetryOf') != marker.get('id')):
            return False
        rows = [db.execute('SELECT * FROM runtime_events WHERE id=?', (key,)).fetchone()
                for key in saved['events']]
        if [_event_snapshot(db, row) for row in rows] != saved['claudeInputRequest']['events']:
            return False
        reason = 'Cancelled by Stop before Claude retry input submission'
        reservation.update(executionOutcome='unsent', notSubmittedReason=reason)
        db.execute('UPDATE runtime_execution_attempts SET record=? WHERE id=?',
                   (json.dumps(reservation), attempt['id']))
        for key in saved['events']:
            db.execute("UPDATE runtime_events SET status='cancelled',error=? WHERE id=? "
                       "AND agent=? AND epoch=? AND status IN ('reserved','dispatching')",
                       (reason, key, agent['id'], epoch))
        stage = getattr(runtime, '_stage_event_resources', None)
        if stage is not None:
            stage(db, str(agent['id']))
        agent.pop('startAttempt', None)
    agent.pop('claudePreInputRetry')
    runtime.put(db, 'agents', agent)
    return True


def retry_batch(
    runtime: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord"
) -> list[Any] | None:
    """A rejected batch keeps its original order and cannot absorb new messages."""
    saved = _saved_retry(runtime, db, agent)
    if not saved:
        return None
    return _retry_rows(db, agent, saved)


def retry_input(
    runtime: "Runtime", db: "sqlite3.Connection", agent: "AgentRecord", rows: list[Any]
) -> Any:
    """Revalidate the original receipt immediately before this new submission."""
    attempt = agent.get('startAttempt') or {}
    if not attempt.get('claudeRetryOf'):
        if agent.get('claudePreInputRetry'):
            raise ClaudeRetryChanged('The Claude retry was reserved by an older dispatcher. Review the saved input before continuing')
        return None
    saved = _saved_retry(runtime, db, agent)
    if (not saved or saved['id'] != attempt['claudeRetryOf']
            or saved['events'] != attempt.get('events')
            or saved['events'] != [row['id'] for row in rows]
            or [_event_snapshot(db, row) for row in rows] != saved['claudeInputRequest']['events']):
        raise ClaudeRetryChanged('The proven Claude retry changed. Review the saved input before continuing')
    request = copy.deepcopy(saved['claudeInputRequest'])
    text = _input_text(db, agent, request)
    if text is None:
        raise ClaudeRetryChanged('The original Claude transcript changed. Review the saved input before continuing')
    request['text'] = text
    return request


def recover_rejected_start(
    runtime: "Runtime",
    db: "sqlite3.Connection",
    agent: "AgentRecord",
    attempt: "StartAttemptRecord",
    *,
    turn: Any = None,
    error: BaseException | None = None,
    account_key: str | None = None,
    connection_id: str | None = None,
) -> Literal["retry", "held"] | None:
    """Settle one proven rejection in the caller's transaction, without native I/O."""
    events = attempt.get('events')
    account = agent.get('accountKey', 'default')
    if (runtime.closed or agent.get('provider') != 'claude' or not agent.get('autoWake')
            or agent.get('status') == 'paused' or agent.get('deletedAt') or agent.get('agentArchive')
            or agent.get('nativeFailureHold') or agent.get('accountTransferId')
            or agent.get('workspaceOperation') or native_thread_block(agent)
            or attempt is not agent.get('startAttempt') or attempt.get('submitted') is not True
            or attempt.get('action')
            or not isinstance(attempt.get('id'), str) or not attempt['id']
            or type(attempt.get('epoch')) is not int or attempt['epoch'] != agent.get('epoch')
            or agent.get('turnEpoch', agent['epoch']) != agent['epoch']
            or attempt.get('accountKey') != account or account_key != account
            or attempt.get('threadId') != agent.get('threadId') or not attempt.get('threadId')
            or not connection_id or attempt.get('connectionId') != connection_id
            or not runtime.connection_current(account, connection_id)
            or not isinstance(events, list) or not 0 < len(events) <= 32
            or any(not isinstance(key, str) or not key for key in events)
            or len(set(events)) != len(events)):
        return None
    expected_operation = 'turn:' + agent['id'] + ':' + events[0] + ':attempt:' + attempt['id']
    if attempt.get('nativeOperationId') != expected_operation:
        return None
    if not _supervisor_matches(runtime.server_for(account, runtime.agent_connection(agent)), attempt.get('supervisorIdentity')):
        return None
    if turn is not None:
        if (not isinstance(turn, dict) or turn.get('status') != 'failed'
                or turn.get('startOutcome') != 'not_applied'
                or not isinstance(turn.get('id'), str) or not turn['id']
                or not isinstance(turn.get('error'), dict)
                or not isinstance(turn['error'].get('data'), dict)
                or turn['error']['data'].get('turnStartOutcome') != 'not_applied'
                or ('inputSubmitted' in turn and turn['inputSubmitted'] is not False)
                or (attempt.get('activeAtReservation') and turn.get('clientUserMessageId') != events[0])
                or turn.get('clientUserMessageId', events[0]) != events[0]):
            return None
        turn_id = turn['id']
        observed = attempt.get('turnId') or attempt.get('observedTurnId')
        if (observed != turn_id or any(attempt.get(key) not in (None, turn_id)
                for key in ('turnId', 'observedTurnId'))
                or agent.get('turnId') not in (None, turn_id)
                or (agent.get('turnId') is None and agent.get('lastCompletedTurn') != turn_id)):
            return None
    else:
        if (attempt.get('activeAtReservation') or not isinstance(error, NativeRpcError)
                or type(error.code) is not int or error.code != -32000
                or not isinstance(error.data, dict)
                or error.data.get('turnStartOutcome') != 'not_applied'
                or ('inputSubmitted' in error.data and error.data['inputSubmitted'] is not False)
                or attempt.get('turnId') or attempt.get('observedTurnId') or agent.get('turnId')):
            return None
        turn_id = None
    saved = _attempt_receipt(db, agent['id'], attempt)
    if (not saved or saved.get('activeAtReservation') != attempt.get('activeAtReservation')
            or saved.get('claudeInputRejection') or not isinstance(saved.get('claudeInputRequest'), dict)):
        return None
    # The positive receipt must also agree with any recorded work from this turn.
    if turn_id:
        since = attempt.get('created')
        if type(since) not in (int, float):
            return None
        if db.execute("SELECT 1 FROM runtime_items WHERE agent=? AND created>=? "
                "AND json_extract(record,'$.turnId')=? "
                "AND json_extract(record,'$.role') IN ('assistant','tool') LIMIT 1",
                (agent['id'], since - 60, turn_id)).fetchone():  # type: ignore[operator]  # typed-narrowing: Timestamp guard proves numeric values
            return None
        if db.execute("SELECT 1 FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
                "AND json_extract(record,'$.created')>=? AND json_extract(record,'$.turnId')=? LIMIT 1",
                (agent['id'], since - 60, turn_id)).fetchone():  # type: ignore[operator]  # typed-narrowing: Timestamp guard proves numeric values
            return None
    rows, metadata = [], []
    for key in events:
        row = db.execute("SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=?",
                         (key, agent['id'], agent['epoch'])).fetchone()
        if (not row or row['status'] not in {'reserved', 'dispatching', 'uncertain', 'delivered'}
                or (row['status'] == 'delivered' and (not turn_id or row['turn_id'] != turn_id))
                or row['turn_id'] not in (None, turn_id)):
            return None
        meta_row = db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()
        meta = json.loads(meta_row[0]) if meta_row else {}
        native = meta.get('native') or {}
        if (not isinstance(native, dict) or native and any(native.get(field) != value for field, value in
                (('agent', agent['id']), ('epoch', agent['epoch']),
                 ('threadId', agent['threadId']), ('accountKey', account)))):
            return None
        rows.append(row)
        metadata.append(meta)
    request = saved['claudeInputRequest']
    if (request.get('source') != _source(agent) or request.get('clientUserMessageId') != events[0]
            or [_event_snapshot(db, row) for row in rows] != request.get('events')):
        return None
    rejection_data: dict[str, Any] = (
        cast(dict[str, Any], turn['error']['data']) if turn is not None
        else cast(dict[str, Any], cast(NativeRpcError, error).data)
    )
    account_rejected = rejection_data.get('claudePreparationFailure') == 'account_validation'
    retry = not account_rejected and not any(meta.get('claudePreInputRetryUsed') for meta in metadata)
    receipt = {key: attempt[key] for key in ('id', 'epoch', 'accountKey', 'threadId',
                                           'connectionId', 'nativeOperationId', 'events')}
    receipt.update(agent=agent['id'], turnId=turn_id, outcome='not_applied', retry=retry)
    saved['claudeInputRejection'] = receipt
    saved['executionOutcome'] = 'not_applied'
    db.execute("UPDATE runtime_execution_attempts SET record=? WHERE id=?",
               (json.dumps(saved), attempt['id']))
    for row, meta in zip(rows, metadata):
        if retry:
            meta['claudePreInputRetryUsed'] = True
        db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)",
                   (row['id'], json.dumps(meta)))
        db.execute("UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL WHERE id=?",
                   (row['id'],))
    stage = getattr(runtime, '_stage_event_resources', None)
    if stage is not None:
        stage(db, str(agent['id']))
    agent['claudePreInputRetry'] = {key: copy.deepcopy(attempt.get(key)) for key in (  # type: ignore[typeddict-item]  # typed-narrowing: Checked attempt supplies all fields
        'id', 'epoch', 'accountKey', 'threadId', 'connectionId', 'nativeOperationId',
        'events', 'submitted', 'supervisorIdentity')}
    agent.pop('startAttempt', None)
    agent.pop('startOutcomeHold', None)
    agent.update(inFlight=False, turnId=None)  # type: ignore[call-arg]  # typed-update
    if retry:
        agent.pop('nativeFailureHold', None)
        agent.update(status='queued', error=None)  # type: ignore[call-arg]  # typed-update
    else:
        agent.update(status='failed', nativeFailureHold=True)  # type: ignore[call-arg]  # typed-update
    return 'retry' if retry else 'held'
