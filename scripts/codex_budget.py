"""Durable token admission. Call helpers under Runtime.lock and its transaction.

Response IDs identify charges. Notice counters are provisional observations, not
additional charges. Migration preserves an existing lifetime floor; historical
imports overlap that floor. Only responses after the migration boundary add to it.
"""
import hashlib
import json
import time
from pathlib import Path


def _tokens(value):
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


def budget_init(db):
    # Do not use executescript: it commits the caller's reservation transaction.
    db.execute('CREATE TABLE IF NOT EXISTS runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_budget_usage (id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, tokens INTEGER NOT NULL, at REAL NOT NULL)')
    db.execute('CREATE INDEX IF NOT EXISTS runtime_budget_usage_agent ON runtime_budget_usage(agent,kind)')


def _accounting(state):
    provisional = (state['noticeSpent'] > state['before'] + state['after'] or state['faults']
                   or state['ambiguousNotices'] or state['migrationSeq'] < state['migrationEnd'])
    return 'provisional' if provisional else 'responseRecords'


def _save(db, a, state):
    state['spent'] = max(state['spent'], state['floor'] + state['after'],
                         state['before'] + state['after'], state['noticeSpent'], state['legacy'])
    db.execute('INSERT INTO runtime_budget VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
               (a['id'], json.dumps(state)))
    # History captures use a historical agent copy. Update only budget fields.
    db.execute("UPDATE runtime_agents SET record=json_set(record,'$.tokensUsed',?,'$.tokenUsageAccounting',?) WHERE id=?",
               (state['spent'], _accounting(state), a['id']))
    a['tokensUsed'] = state['spent']
    a['tokenUsageAccounting'] = _accounting(state)
    return state


def _state(db, a):
    budget_init(db)
    row = db.execute('SELECT record FROM runtime_budget WHERE id=?', (a['id'],)).fetchone()
    if row:
        state = json.loads(row[0])
        a['tokensUsed'] = state['spent']
        a['tokenUsageAccounting'] = _accounting(state)
        return state
    current = db.execute('SELECT record FROM runtime_agents WHERE id=?', (a['id'],)).fetchone()
    current = json.loads(current[0]) if current else a
    floor = _tokens(current.get('tokensUsed')) or 0
    state = {'cutoff': time.time(), 'floor': floor, 'spent': floor, 'before': 0, 'after': 0,
             'legacy': 0, 'noticeSpent': floor, 'counter': floor if floor else None,
             'thread': current.get('threadId'), 'noticeAt': 0, 'created': current.get('created', 0),
             'lastAmount': None, 'ambiguousNotices': [], 'faults': []}
    exists = db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_usage'").fetchone()
    state['migrationEnd'] = (db.execute('SELECT COALESCE(MAX(seq),0) FROM analytics_usage').fetchone()[0] if exists else 0)
    state['migrationSeq'] = 0
    return _save(db, a, state)


def budget_prepare_migration(runtime):
    """Prepare the scan index in the history worker, outside Runtime.lock."""
    if getattr(runtime, '_budget_index_ready', False):
        return
    with runtime.db() as db:
        if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_usage'").fetchone():
            db.execute('CREATE INDEX IF NOT EXISTS analytics_usage_migration ON analytics_usage(agent,seq)')
    runtime._budget_index_ready = True


def budget_migrate(db, a, limit=64):
    """Advance one bounded page in the existing history worker transaction."""
    state = _state(db, a)
    if state['migrationSeq'] >= state['migrationEnd']:
        return
    rows = db.execute('SELECT seq,record FROM analytics_usage WHERE agent=? AND seq>? AND seq<=? ORDER BY seq LIMIT ?',
                      (a['id'], state['migrationSeq'], state['migrationEnd'], limit)).fetchall()
    for row in rows:
        record = json.loads(row[1])
        p = {'threadId': record.get('threadId'), 'turnId': record.get('turnId'),
             'responseId': record.get('responseId'), 'rawTokenUsageRecord': record.get('rawTokenUsageRecord'),
             'requestUsage': record.get('requestUsage'), 'tokenUsage': {'total': record.get('total'), 'last': record.get('last')},
             '_analyticsTimestampSource': record.get('timestampSource')}
        _capture(db, a, state, p, record.get('at', 0), 'rollout')
        state['migrationSeq'] = row[0]
    if len(rows) < limit:
        state['migrationSeq'] = state['migrationEnd']
    _save(db, a, state)


def _capture(db, a, state, p, at, source):
    # A new branch receives old native history, not new provider charges.
    if source != 'live' and at < state['created']:
        return
    usage = p.get('tokenUsage') or {}
    last = p.get('requestUsage') or usage.get('last') or {}
    amount = _tokens(last.get('totalTokens'))
    thread = p.get('threadId') or a.get('threadId')
    response = p.get('responseId')
    exact = bool(response and p.get('rawTokenUsageRecord'))
    identity = [a['id'], a.get('accountKey', 'default')]
    if exact:
        # Provider response identity survives a native thread fork.
        identity = [a['id'], 'response', response]
    else:
        identity += ['notice', p.get('turnId') or a.get('turnId'), usage.get('total'), usage.get('last')]
    key = hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    previous = db.execute('SELECT tokens FROM runtime_budget_usage WHERE id=?', (key,)).fetchone()
    if previous:
        if not exact and response and key in state['ambiguousNotices']:
            state['ambiguousNotices'].remove(key)
        if exact and amount is not None and previous[0] != amount:
            if 'conflictingResponseUsage' not in state['faults']:
                state['faults'].append('conflictingResponseUsage')
        return
    if amount is None and not exact:
        amount = _tokens((usage.get('total') or {}).get('totalTokens'))
    if amount is None:
        if 'missingRequestUsage' not in state['faults']:
            state['faults'].append('missingRequestUsage')
        return
    db.execute('INSERT INTO runtime_budget_usage VALUES (?,?,?,?,?)', (key, a['id'], 'response' if exact else 'notice', amount, at))
    if exact:
        if p.get('_analyticsTimestampSource') == 'fileModifiedEstimate':
            if 'estimatedResponseTime' not in state['faults']:
                state['faults'].append('estimatedResponseTime')
            # Unknown timestamps cannot safely add a response above the floor.
            state['before'] += amount
        else:
            state['after' if at > state['cutoff'] else 'before'] += amount
    elif source != 'live':
        if not response:
            state['legacy'] += amount
    else:
        counter = _tokens((usage.get('total') or {}).get('totalTokens'))
        if counter is None:
            state['noticeSpent'] += amount
        elif at < state['noticeAt']:
            # Arrival order is not charge identity. Exact records settle this case.
            state['noticeSpent'] = max(state['noticeSpent'], counter)
        else:
            reset = state['counter'] is not None and counter < state['counter']
            delta = amount if state['counter'] is None or thread != state['thread'] else counter if reset else counter - state['counter']
            if reset and amount == state['lastAmount']:
                # A reset can repeat the previous response. Equal-size new
                # responses count when their distinct IDs arrive.
                delta = 0
                if not response:
                    state['ambiguousNotices'].append(key)
            state['noticeSpent'] += max(0, delta)
            state.update(counter=counter, thread=thread, noticeAt=at, lastAmount=amount)


def budget_capture(db, a, p, *, at=None, source='live'):
    state = _state(db, a)
    _capture(db, a, state, p, time.time() if at is None else at, source)
    return _save(db, a, state)['spent']


def _coverage(db, a, state):
    if state['migrationSeq'] < state['migrationEnd']:
        return 'Stored usage migration is incomplete'
    if state['faults']:
        return ', '.join(state['faults'])
    if state['ambiguousNotices']:
        return 'Native counter reset has no response identity'
    if not a.get('threadId'):
        return None if not state['spent'] else 'Native usage history is unavailable'
    if not state['spent'] and not a.get('turnId') and not a.get('lastCompletedTurn'):
        return None
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_history'").fetchone():
        return 'Native usage history is unavailable'
    key = a['id'] + ':' + a.get('accountKey', 'default') + ':' + a['threadId']
    row = db.execute('SELECT record FROM analytics_history WHERE id=?', (key,)).fetchone()
    history = json.loads(row[0]) if row else {}
    if history.get('status') != 'current' or history.get('coverage') == 'partial':
        return 'Native usage history is incomplete'
    try:
        path = Path(history['path'])
        info = path.stat()
        offset = history.get('offset', 0)
        if history.get('identity') != [info.st_dev, info.st_ino] or offset > info.st_size:
            return 'Native usage history identity changed'
        if info.st_size > offset:
            # Thread preparation can append settings after a complete import.
            # Those records do not invalidate usage coverage. New response or
            # compaction records must first pass through the history collector.
            if info.st_size - offset > 65536:
                return 'Native usage history has not caught up'
            with path.open('rb') as handle:
                handle.seek(offset)
                tail = handle.read(65537)
            if not tail.endswith(b'\n') or len(tail) > 65536:
                return 'Native usage history has not caught up'
            for line in tail.splitlines():
                record = json.loads(line)
                if record.get('type') in {'token_usage_record', 'compacted'} or (record.get('payload') or {}).get('type') == 'token_count':
                    return 'Native usage history has not caught up'
            if path.stat().st_size != info.st_size:
                return 'Native usage history has not caught up'
    except (OSError, KeyError, TypeError, ValueError, AttributeError):
        return 'Native usage history is unavailable'
    if state['spent'] and not (history.get('context') or {}).get('requestUsageAvailable'):
        return 'Native history lacks response usage records'
    if state['before'] + state['after'] < max(state['floor'], state['noticeSpent'], state['legacy']):
        return 'Response usage does not cover the observed lifetime counter'
    return None


def budget_status(runtime, db, agent, *, check_coverage=True):
    root = runtime.agent(agent.get('rootId') or agent['id'], db)
    members = [a for a in runtime.records(db, 'agents') if (a.get('rootId') or a['id']) == root['id']]
    spent, incomplete = 0, []
    for member in members:
        state = _state(db, member)
        spent += state['spent']
        if member['id'] == agent['id']:
            agent['tokensUsed'] = member['tokensUsed']
            agent['tokenUsageAccounting'] = member['tokenUsageAccounting']
        if check_coverage and root.get('tokenBudget') is not None:
            reason = _coverage(db, member, state)
            if reason:
                incomplete.append({'agentId': member['id'], 'reason': reason})
    return {'rootId': root['id'], 'tokenBudget': root.get('tokenBudget'), 'tokensUsed': spent,
            'reached': root.get('tokenBudget') is not None and spent >= root['tokenBudget'],
            'incomplete': incomplete}


def budget_admission(runtime, db, agent):
    """Check the team immediately before reservation or native submission."""
    root = runtime.agent(agent.get('rootId') or agent['id'], db)
    if root.get('tokenBudget') is None:
        return {'rootId': root['id'], 'tokenBudget': None, 'reached': False, 'incomplete': []}
    status = budget_status(runtime, db, agent)
    if status['reached']:
        raise _budget_error(status, agent, 'Team token budget reached. Increase the budget before resuming')
    if status['tokenBudget'] is not None and status['incomplete']:
        raise _budget_error(status, agent, 'Team token budget cannot be verified: ' + status['incomplete'][0]['reason'])
    return status


def _budget_error(status, agent, message):
    error = ValueError(message)
    error.budgetAdmission = {**status, 'agentId': agent['id'], 'epoch': agent.get('epoch'),
                             'accountKey': agent.get('accountKey', 'default'), 'threadId': agent.get('threadId'),
                             'attemptId': (agent.get('startAttempt') or {}).get('id')}
    return error


def _unsubmitted_budget_attempt(agent, attempt_id):
    attempt = agent.get('startAttempt') or {}
    return bool(attempt.get('id') == attempt_id and attempt.get('submitted') is False
                and not attempt.get('turnId') and not attempt.get('observedTurnId')
                and not attempt.get('notSubmittedReason')
                and (not agent.get('turnId') or agent.get('turnId') == agent.get('lastCompletedTurn')
                     and agent.get('lastCompletedTurnStatus') in {'completed', 'failed', 'interrupted'})
                and attempt.get('epoch') == agent.get('epoch') and agent.get('autoWake')
                and not agent.get('deletedAt')
                and attempt.get('accountKey', agent.get('accountKey', 'default')) == agent.get('accountKey', 'default')
                and attempt.get('threadId') in (None, agent.get('threadId')))


def defer_budget_start(runtime, agent_id, attempt_id, error, *, unknown=False):
    """Keep only a proven unsubmitted budget denial available for later dispatch."""
    denial = getattr(error, 'budgetAdmission', None)
    if (unknown or type(error) is not ValueError or not isinstance(denial, dict)
            or denial.get('tokenBudget') is None or not (denial.get('reached') or denial.get('incomplete'))):
        return False
    with runtime.lock, runtime.db() as db:
        agent = runtime.agent(agent_id, db)
        if (runtime.closed or not _unsubmitted_budget_attempt(agent, attempt_id)
                or denial.get('agentId') != agent_id or denial.get('attemptId') != attempt_id
                or any(denial.get(field) != agent.get(field, 'default' if field == 'accountKey' else None)
                       for field in ('accountKey', 'epoch', 'threadId'))):
            return False
        attempt = agent['startAttempt']
        events = attempt.get('events')
        if not isinstance(events, list) or len(events) > 32 or len(set(events)) != len(events):
            return False
        action = attempt.get('action')
        if action and (events or action not in {'compact', 'review', 'capacity'}):
            return False
        if not action and not events:
            return False
        if action:
            from codex_native_action_receipts import assert_identity
            try:
                assert_identity(agent, attempt)
            except ValueError:
                return False
        rows = []
        for event_id in events:
            row = db.execute('SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                             (event_id, agent_id, agent['epoch'])).fetchone()
            if not row or row['status'] not in {'reserved', 'dispatching', 'pending'} or row['turn_id']:
                return False
            rows.append(row)
        for row in rows:
            db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=?", (row['id'],))
        field = 'budgetActionWait' if action else 'budgetStartWait'
        wait = {'attemptId': attempt_id, 'agentId': agent_id, 'accountKey': agent.get('accountKey', 'default'),
                'epoch': agent['epoch'], 'threadId': agent.get('threadId'), 'events': list(events),
                'action': action, 'actionRequestId': attempt.get('actionRequestId'),
                'actionIdentity': attempt.get('actionIdentity'), 'error': str(error), 'at': time.time(), 'admission': denial}
        # Preserve the first wait receipt when a callback repeats the same denial.
        previous = agent.get(field) or {}
        if previous.get('attemptId') == attempt_id:
            wait['at'] = previous['at']
        agent[field] = wait
        agent.update(status='queued', inFlight=False, budgetBlocked=str(error), error=str(error))
        runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return True


def _retire_budget_wait(runtime, db, agent, field, wait):
    agent.pop(field, None)
    agent['lastBudgetWait'] = {**wait, 'status': 'superseded', 'finishedAt': time.time()}
    if agent.get('budgetBlocked') == wait.get('error'):
        agent.pop('budgetBlocked', None)
    if agent.get('error') == wait.get('error'):
        agent['error'] = None
    request_id = wait.get('actionRequestId')
    current = agent.get('startAttempt') or {}
    if (request_id and current.get('id') == wait.get('attemptId') and current.get('submitted') is False
            and not current.get('turnId') and not current.get('observedTurnId')):
        db.execute("UPDATE runtime_native_action_receipts SET outcome=? WHERE id=? "
                   "AND json_extract(receipt,'$.attemptId')=? AND json_extract(outcome,'$.status')='pending'",
                   (json.dumps({'status': 'failed', 'notSubmitted': True, 'error': 'The budget wait belongs to an earlier agent state'}),
                    request_id, wait.get('attemptId')))
    runtime.put(db, 'agents', agent)


def claim_budget_wait(runtime, db, agent):
    """Claim the same attempt after dispatch checks capacity and budget admission.

    Return a turn/action job, or None when no valid wait remains. This helper does
    not submit native work. The normal final admission guard still applies.
    """
    current = runtime.agent(agent['id'], db)
    agent.clear()
    agent.update(current)
    field = 'budgetActionWait' if agent.get('budgetActionWait') else 'budgetStartWait'
    wait = agent.get(field)
    if not isinstance(wait, dict):
        return None
    attempt = agent.get('startAttempt') or {}
    valid = (_unsubmitted_budget_attempt(agent, wait.get('attemptId'))
             and not runtime.closed and agent.get('status') == 'queued' and not agent.get('inFlight')
             and wait.get('agentId') == agent['id']
             and all(wait.get(name) == agent.get(name, 'default' if name == 'accountKey' else None)
                     for name in ('accountKey', 'epoch', 'threadId'))
             and wait.get('events') == attempt.get('events')
             and wait.get('action') == attempt.get('action')
             and wait.get('actionRequestId') == attempt.get('actionRequestId')
             and wait.get('actionIdentity') == attempt.get('actionIdentity'))
    if valid and wait.get('action'):
        from codex_native_action_receipts import assert_identity
        try:
            assert_identity(agent, attempt)
        except ValueError:
            valid = False
    rows = []
    if valid:
        for event_id in wait['events']:
            row = db.execute("SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=? AND status='pending' AND turn_id IS NULL",
                             (event_id, agent['id'], agent['epoch'])).fetchone()
            if not row:
                valid = False
                break
            rows.append(dict(row))
    if not valid:
        _retire_budget_wait(runtime, db, agent, field, wait)
        return None
    for row in rows:
        db.execute("UPDATE runtime_events SET status='reserved' WHERE id=? AND status='pending'", (row['id'],))
    agent.pop(field, None)
    agent['lastBudgetWait'] = {**wait, 'status': 'resumed', 'finishedAt': time.time()}
    if agent.get('budgetBlocked') == wait.get('error'):
        agent.pop('budgetBlocked', None)
    if agent.get('error') == wait.get('error'):
        agent['error'] = None
    agent.update(status='starting', inFlight=True, turnEpoch=agent['epoch'])
    runtime.put(db, 'agents', agent)
    return {'kind': 'action' if wait.get('action') else 'turn', 'agent': agent, 'attempt': dict(attempt), 'rows': rows}
