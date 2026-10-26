"""Reconcile execution identities in the caller's existing SQLite transaction.

These records explain native runs. Receipts and recovery markers still own
submission evidence and continuation permission. No helper commits or retries.
"""
import json
import logging
import time
import uuid
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import sqlite3
    from typing import Any
    from codex_records import AgentRecord
    from codex_runtime import Runtime


SCHEMA = (
    "CREATE TABLE IF NOT EXISTS runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_run_agent ON runtime_execution_runs(agent,created DESC)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_run_created ON runtime_execution_runs(created DESC)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_run_message ON runtime_execution_runs(agent,thread,turn)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_run_native ON runtime_execution_runs(agent,account,thread,turn)",
    "CREATE TABLE IF NOT EXISTS runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_attempt_run ON runtime_execution_attempts(run)",
    "CREATE TABLE IF NOT EXISTS runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event))",
    "CREATE INDEX IF NOT EXISTS runtime_execution_input_event ON runtime_execution_inputs(event)",
    "CREATE TABLE IF NOT EXISTS runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_effect_run ON runtime_execution_effects(run)",
    "CREATE TABLE IF NOT EXISTS runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS runtime_execution_node_run ON runtime_execution_nodes(run)",
)
TERMINAL = {'completed', 'failed', 'interrupted', 'cancelled'}
AGENT_FIELDS = ('startAttempt', 'turnId', 'threadId', 'accountKey', 'epoch',
                'lastCompletedTurn', 'lastCompletedTurnStatus', 'lastCompletedTurnError',
                'restartRecovery', 'disconnectRecovery', 'connectionRecovery',
                'status', 'inFlight', 'prepareAttempt')


RETENTION_SECONDS = 30 * 86400
PRUNE_LIMIT = 200


def _log_failure(callback: "Any", error: BaseException) -> None:
    # Do not log provider input, results, or request arguments.
    logging.getLogger(__name__).error("Execution record failed: %s (%s)",
                                     getattr(callback, '__name__', type(callback).__name__), type(error).__name__)


def safe_record(db: "sqlite3.Connection", callback: "Any", *args: "Any",
                **kwargs: "Any") -> "Any":
    """Rollback partial side records without discarding the caller's state."""
    opened = False
    try:
        # RELEASE must never commit side records before the caller's writes.
        # Take the writer before callback reads to avoid a snapshot upgrade.
        # This uses the same commit already owned by Runtime.db.
        if not db.in_transaction:
            db.execute('BEGIN IMMEDIATE')
        db.execute('SAVEPOINT exec_record')
        opened = True
        result = callback(*args, **kwargs)
        db.execute('RELEASE exec_record')
        return result
    except Exception as error:
        if opened:
            try:
                db.execute('ROLLBACK TO exec_record')
                db.execute('RELEASE exec_record')
            except Exception as cleanup_error:
                _log_failure(safe_record, cleanup_error)
        _log_failure(callback, error)
        return None


def needs_record(table: str, record: dict[str, "Any"], previous: dict[str, "Any"] | None) -> bool:
    if table == 'agents' and previous:
        return any(previous.get(field) != record.get(field) for field in AGENT_FIELDS)
    return True


def prune(db: "sqlite3.Connection", *, now: float | None = None,
          limit: int = PRUNE_LIMIT) -> int:
    """Delete at most 200 expired finished runs in server maintenance."""
    cutoff = (time.time() if now is None else now) - RETENTION_SECONDS
    ids = [row[0] for row in db.execute(
        "SELECT id FROM runtime_execution_runs WHERE created<? AND "
        "json_extract(record,'$.status') IN ('completed','failed','interrupted','cancelled','rejected') "
        "ORDER BY created LIMIT ?", (cutoff, min(PRUNE_LIMIT, max(1, limit))))]
    if ids:
        slots = ','.join('?' for _ in ids)
        # Delete dependents first. Existing receipts and input events remain.
        for table in ('attempts', 'inputs', 'effects', 'nodes'):
            db.execute('DELETE FROM runtime_execution_' + table + ' WHERE run IN (' + slots + ')', ids)
        db.execute('DELETE FROM runtime_execution_runs WHERE id IN (' + slots + ')', ids)
    return len(ids)


def maintenance(runtime: "Runtime") -> int:
    # The HTTP server already performs hourly maintenance. Never run in put().
    deadline = time.monotonic() + 2
    total = 0
    while time.monotonic() < deadline:
        try:
            remaining_ms = max(1, int((deadline - time.monotonic()) * 1000))
            with runtime.db(busy_timeout=remaining_ms) as db:
                deleted = safe_record(db, prune, db)
        except Exception as error:
            _log_failure(maintenance, error)
            break
        if deleted is None:
            break
        total += deleted
        if deleted < PRUNE_LIMIT:
            break
    return total


def observe_child_thread(db: "sqlite3.Connection", account: str, method: str,
                         params: dict[str, "Any"]) -> None:
    parent = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? AND COALESCE(json_extract(record,'$.accountKey'),'default')=? LIMIT 1", (params['parentThreadId'], account)).fetchone()
    if parent:
        observe_native(db, json.loads(parent[0]), method, params)


def record_spawn(db: "sqlite3.Connection", actor: "AgentRecord", child: "AgentRecord",
                 request_id: str | None) -> None:
    effect(db, actor_run(db, actor), 'spawn', child['id'], requestId=request_id, status=child.get('status'))


def submission_identity(db: "sqlite3.Connection", actor: "AgentRecord") -> dict[str, "Any"]:
    run = actor_run(db, actor)
    return {'runId': run['id'], 'attemptId': run.get('latestAttemptId')} if run else {}


def ensure_tables(db: "sqlite3.Connection") -> None:
    # New, empty tables only. Never scan or rewrite the existing history.
    for statement in SCHEMA:
        db.execute(statement)


def _load(db: "sqlite3.Connection", table: str, key: str | None) -> "Any":
    row = db.execute('SELECT record FROM runtime_execution_' + table + ' WHERE id=?', (key,)).fetchone()
    return json.loads(row[0]) if row else None


def native_run(db: "sqlite3.Connection", agent: str, account: str,
               thread: str | None, turn: str | None) -> "Any":
    if not turn:
        return None
    rows = db.execute('SELECT record FROM runtime_execution_runs WHERE agent=? AND account=? AND thread IS ? AND turn=? LIMIT 2',
                      (agent, account, thread, turn)).fetchall()
    return json.loads(rows[0][0]) if len(rows) == 1 else None


def actor_run(db: "sqlite3.Connection", agent: "AgentRecord", *, turn: str | None = None) -> "Any":
    turn = turn or agent.get('turnId')
    if turn:
        return native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), turn)
    row = db.execute("SELECT record FROM runtime_execution_runs WHERE agent=? AND account=? AND epoch=? AND turn IS NULL ORDER BY created DESC LIMIT 1", (agent['id'], agent.get('accountKey', 'default'), agent.get('epoch'))).fetchone()
    run = json.loads(row[0]) if row else None
    return run if run and run['status'] not in TERMINAL else None


def _save_run(db: "sqlite3.Connection", run: dict[str, "Any"]) -> None:
    if run['status'] in TERMINAL:
        db.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.turnStatus',?,'$.resultRunId',?) WHERE run=? AND json_extract(record,'$.turnStatus') IS NOT ?", (run['status'], run['id'], run['id'], run['status']))
    db.execute('INSERT INTO runtime_execution_runs VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET thread=excluded.thread,turn=excluded.turn,record=excluded.record',
               (run['id'], run['agent'], run['accountKey'], run['epoch'], run.get('threadId'), run.get('turnId'), run['created'], json.dumps(run)))


def _save(db: "sqlite3.Connection", table: str, key: str, run: str,
          record: dict[str, "Any"]) -> None:
    db.execute('INSERT INTO runtime_execution_' + table + ' VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET run=excluded.run,record=excluded.record',
               (key, run, json.dumps(record)))


def effect(db: "sqlite3.Connection", run: dict[str, "Any"] | None, kind: str,
           reference: str, **fields: "Any") -> None:
    if not run:
        return
    key = kind + ':' + reference
    old = _load(db, 'effects', key)
    # A later turn cannot adopt an earlier effect.
    run_id = old['runId'] if old else run['id']
    if fields.get('requestId') is not None:
        fields['requestId'] = str(fields['requestId'])
    record = {'id': key, 'runId': run_id, 'kind': kind, 'referenceId': reference,
              'attemptId': old.get('attemptId') if old else run.get('latestAttemptId'), **fields}
    if old != record:
        db.execute('INSERT INTO runtime_execution_effects VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   (key, run_id, kind, reference, json.dumps(record)))


def _new_run(agent: "AgentRecord", identity: str,
             attempt: "Any" = None) -> dict[str, "Any"]:
    attempt = attempt or {}
    return {'id': 'run:' + identity, 'agent': agent['id'],
            'accountKey': attempt.get('accountKey', agent.get('accountKey', 'default')),
            'epoch': attempt.get('epoch', agent['epoch']),
            'threadId': attempt.get('threadId', agent.get('threadId')),
            'turnId': None, 'created': attempt.get('created', time.time()),
            'status': 'pending', 'firstAttemptId': None, 'rootAttemptId': None, 'latestAttemptId': None}


def reconcile_agent(db: "sqlite3.Connection", agent: "AgentRecord",
                    previous: "AgentRecord | None") -> None:
    if previous and all(previous.get(field) == agent.get(field) for field in AGENT_FIELDS):
        return
    attempt = agent.get('startAttempt') or {}
    historical: "Any" = (agent.get('restartRecovery') or {}).get('startAttempt') or (previous or {}).get('startAttempt') or {}  # type: ignore[call-overload]  # typed-narrowing: Absent previous yields empty fallback
    historical_saved = _load(db, 'attempts', historical['id']) if historical.get('id') else None
    if not attempt and historical.get('id') and not historical_saved:
        attempt = historical
    run = _load(db, 'runs', historical_saved['runId']) if not attempt and historical_saved else None
    if attempt.get('id'):
        stored = historical_saved if historical.get('id') == attempt['id'] else _load(db, 'attempts', attempt['id'])
        if stored:
            run = _load(db, 'runs', stored['runId'])
        if not run and attempt.get('activeAtReservation'):
            run = actor_run(db, agent)
        if not run and attempt.get('events'):
            # Reuse the logical run only for an exact retry of unaccepted input.
            row = db.execute('SELECT r.record FROM runtime_execution_inputs i JOIN runtime_execution_runs r ON r.id=i.run WHERE i.event=? AND r.agent=? AND r.account=? AND r.epoch=? AND r.turn IS NULL ORDER BY r.created DESC LIMIT 1',
                             (attempt['events'][0], agent['id'], attempt.get('accountKey', agent.get('accountKey', 'default')), attempt['epoch'])).fetchone()
            run = json.loads(row[0]) if row else None
        if not run:
            run = _new_run(agent, attempt['id'], attempt)
        turn = attempt.get('turnId') or attempt.get('observedTurnId')
        if turn and run.get('turnId') and run['turnId'] != turn:
            previous_run = run
            run = _new_run(agent, attempt['id'], attempt)
            for event in attempt.get('events', []):
                db.execute('DELETE FROM runtime_execution_inputs WHERE run=? AND event=?', (previous_run['id'], event))
            if previous_run.get('latestAttemptId') == attempt['id']:
                previous_run['latestAttemptId'] = previous_run.get('firstAttemptId')
            _save_run(db, previous_run)
        if turn:
            accepted_run = native_run(db, agent['id'], run['accountKey'], attempt.get('threadId', agent.get('threadId')), turn)
            if accepted_run and accepted_run['id'] != run['id']:
                # Busy input belongs to the containing native run.
                run = accepted_run
            if not run.get('turnId') or not attempt.get('activeAtReservation'):
                run['rootAttemptId'] = run.get('rootAttemptId') or attempt['id']
            run['turnId'] = turn
        run['threadId'] = attempt.get('threadId', agent.get('threadId'))
        run['firstAttemptId'] = run.get('firstAttemptId') or attempt['id']
        run['latestAttemptId'] = attempt['id']
        evidence = ('accepted' if attempt.get('turnId') else
                    'observed' if attempt.get('observedTurnId') else
                    'unknown' if attempt.get('submitted') else 'unsent')
        if evidence in {'unknown', 'unsent'}:
            evidence = attempt.get('executionOutcome', evidence)
        saved = {**attempt, 'id': attempt['id'], 'runId': run['id'],
                 'submission': evidence, 'prepareAttemptId': agent.get('prepareAttempt'),
                 'turnStatus': run['status'] if run['status'] in TERMINAL else None,
                 'resultRunId': run['id'] if run['status'] in TERMINAL else None}
        _save(db, 'attempts', attempt['id'], run['id'], saved)
        for event in attempt.get('events', []):
            db.execute('INSERT OR IGNORE INTO runtime_execution_inputs VALUES (?,?)', (run['id'], event))
        if run['status'] not in TERMINAL:
            run['status'] = 'running' if turn else evidence
    turn = agent.get('turnId')
    if turn and (not run or run.get('turnId') not in (None, turn)):
        run = native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), turn)
        if not run:
            identity = str(uuid.uuid5(uuid.NAMESPACE_URL, json.dumps([agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), turn])))
            if _load(db, 'nodes', 'native:' + identity):
                # Native child activity can change the agent's visible turn.
                # Its node still has no authority to become a root run.
                return
            run = _new_run(agent, identity)
            run.update(turnId=turn, status='running', reconciled=True)
    elif turn and run and not run.get('turnId'):
        run['turnId'] = turn
    if not run:
        root_turn = agent.get('lastCompletedTurn') or (agent.get('disconnectRecovery') or {}).get('turnId') or (agent.get('restartRecovery') or {}).get('turnId')
        run = native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), root_turn)
    if not run:
        return
    # Provider terminal evidence only. Agent interruption on disconnect is not
    # proof that the native run ended.
    recovery = agent.get('connectionRecovery') or {}
    completed, status = recovery.get('turnId'), recovery.get('outcome')
    same_native_scope = run['accountKey'] == agent.get('accountKey', 'default') and run.get('threadId') == agent.get('threadId')
    if same_native_scope and run.get('turnId') and completed == run['turnId'] and status in TERMINAL:
        if run['status'] not in TERMINAL:
            run.update(status=status, finished=time.time(), result=agent.get('lastAnswer', '')[:16000],
                       error=agent.get('lastCompletedTurnError'))
    for field in ('restartRecovery', 'disconnectRecovery', 'connectionRecovery'):
        marker: "Any" = agent.get(field)
        if (marker and marker.get('turnId') in (None, run.get('turnId'))
                and marker.get('accountKey', run['accountKey']) == run['accountKey']
                and marker.get('threadId', run.get('threadId')) == run.get('threadId')):
            run[field] = marker
    _save_run(db, run)


def reject_attempt(db: "sqlite3.Connection", attempt: dict[str, "Any"], error: BaseException) -> None:
    saved = _load(db, 'attempts', attempt.get('id'))
    if saved and saved.get('submission') not in {'accepted', 'observed'}:
        saved.update(submission='rejected', error=str(error))
        _save(db, 'attempts', saved['id'], saved['runId'], saved)


def reconcile_effect(runtime: "Runtime", db: "sqlite3.Connection", table: str,
                     record: "Any", previous: "Any" = None) -> None:
    if table == 'agents':
        if not previous and record.get('parentId'):
            parent = runtime.agent(record['parentId'], db)
            run = actor_run(db, parent)
            if run:
                effect(db, run, 'spawn', record['id'], status=record.get('status'))
                _save(db, 'nodes', 'worker:' + record['id'], run['id'],
                      {'id': 'worker:' + record['id'], 'runId': run['id'], 'kind': 'managed_worker', 'agentId': record['id'], 'status': record.get('status')})
        elif previous and record.get('parentId') and previous.get('status') != record.get('status'):
            node = _load(db, 'nodes', 'worker:' + record['id'])
            if node:
                node['status'] = record.get('status')
                if record.get('lastCompletedTurn'):
                    node.update(result=record.get('lastAnswer', '')[:16000], resultTurnId=record['lastCompletedTurn'])
                _save(db, 'nodes', node['id'], node['runId'], node)
        reconcile_agent(db, record, previous)
    elif table in {'tool_requests', 'monitors', 'requests'}:
        old = _load(db, 'effects', table + ':' + record['id'])
        run = _load(db, 'runs', old['runId']) if old else None
        if not run and record.get('agent'):
            actor = runtime.agent(record['agent'], db)
            if actor:
                run = actor_run(db, actor, turn=record.get('turnId'))
        effect(db, run, table, record['id'], requestId=record.get('request_id') or record.get('callId') or record.get('requestId'),
               status=record.get('stage', record.get('status')), outcome=record.get('outcome'),
               resultReference=record['id'] if record.get('finished') else None)
    elif table == 'work':
        for result in record.get('results', [])[-1:]:
            if result.get('runId'):
                effect(db, {'id': result['runId'], 'latestAttemptId': result.get('attemptId')}, 'task_submit', result['id'], taskId=record['id'],
                       status='submitted', resultReference=result.get('resultFile'), revision=result.get('revision'))


def child_event(params: dict[str, "Any"]) -> bool:
    turn = params.get('turn') or {}
    return bool(params.get('parentTurnId') or params.get('parentThreadId')
                or params.get('background') or params.get('isBackground')
                or turn.get('parentTurnId') or turn.get('parentId') or turn.get('background'))


def observe_native(db: "sqlite3.Connection", agent: "AgentRecord", method: str,
                   params: dict[str, "Any"]) -> None:
    """Keep child and background nodes without assigning root authority."""
    turn = params.get('turn') or {}
    if method == 'turn/completed' and turn.get('status') == 'interrupted':
        from codex_native_errors import error_kind
        if error_kind(turn.get('error')) == 'tooManyDenials':
            turn = {**turn, 'status': 'failed'}
    turn_id = turn.get('id') or params.get('turnId')
    if not turn_id or method not in {'turn/started', 'turn/completed'}:
        return
    run = native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), turn_id)
    related = child_event(params)
    if not related and run:
        if (method == 'turn/completed' and turn.get('status') in TERMINAL
                and run['status'] not in TERMINAL):
            row = db.execute("SELECT record FROM runtime_items WHERE agent=? AND created>=? AND json_extract(record,'$.turnId')=? AND json_extract(record,'$.role')='assistant' AND COALESCE(json_extract(record,'$.phase'),'')!='commentary' ORDER BY created DESC LIMIT 1", (agent['id'], run['created'] - 60, turn_id)).fetchone()
            item = json.loads(row[0]) if row else {}
            run.update(status=turn['status'], finished=time.time(), result=item.get('text', '')[:16000], resultItemId=item.get('id'), error=turn.get('error'))
            _save_run(db, run)
        return
    thread = params.get('threadId') or agent.get('threadId')
    key = 'native:' + str(uuid.uuid5(uuid.NAMESPACE_URL, json.dumps([agent['id'], agent.get('accountKey', 'default'), thread, turn_id])))
    old = _load(db, 'nodes', key)
    parent_turn = params.get('parentTurnId') or turn.get('parentTurnId') or turn.get('parentId')
    root = _load(db, 'runs', old['runId']) if old else native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), parent_turn)
    if not root:
        root = actor_run(db, agent)
    if not root:
        root = native_run(db, agent['id'], agent.get('accountKey', 'default'), agent.get('threadId'), agent.get('lastCompletedTurn'))
    if (related or old) and root:
        node = {'id': key, 'runId': root['id'], 'kind': old['kind'] if old else 'background' if params.get('background') or params.get('isBackground') or turn.get('background') else 'native_child',
                'threadId': thread, 'turnId': turn_id, 'status': turn.get('status', 'running'),
                'error': turn.get('error'), 'result': str(turn.get('result') or '')[:16000], 'parentTurnId': params.get('parentTurnId') or root.get('turnId')}
        if old and old.get('status') in TERMINAL:
            return
        _save(db, 'nodes', key, root['id'], node)


def chain(db: "sqlite3.Connection", *, agent: str | None = None,
          limit: int = 25) -> dict[str, "Any"]:
    rows = db.execute('SELECT record FROM runtime_execution_runs ' + ('WHERE agent=? ' if agent else '') + 'ORDER BY created DESC LIMIT ?',
                      (agent, limit) if agent else (limit,)).fetchall()
    runs = [json.loads(row[0]) for row in rows]
    for run in runs:
        for table in ('attempts', 'nodes', 'effects'):
            run[table] = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_execution_' + table + ' WHERE run=? LIMIT 100', (run['id'],))]
        run['attemptIds'] = [attempt['id'] for attempt in run['attempts']]
        run['requestIds'] = sorted({effect['requestId'] for effect in run['effects'] if effect.get('requestId')} | {attempt[field] for attempt in run['attempts'] for field in ('actionRequestId', 'nativeOperationId') if attempt.get(field)})
        run['inputEventIds'] = [row[0] for row in db.execute('SELECT event FROM runtime_execution_inputs WHERE run=? LIMIT 100', (run['id'],))]
    return {'runs': runs, 'limit': limit, 'relatedLimit': 100}


def message_identity(runtime: "Runtime", agent: str, thread: str | None,
                    turn: str) -> dict[str, "Any"]:
    with runtime.db() as db:
        rows = db.execute('SELECT record FROM runtime_execution_runs WHERE agent=? AND thread IS ? AND turn=? LIMIT 2', (agent, thread, turn)).fetchall()
        if len(rows) != 1:
            return {}
        run = json.loads(rows[0][0])
        return {'runId': run['id'], 'attemptId': run.get('rootAttemptId')}
