"""Continue only inputs stopped by a local Claude authentication probe timeout."""
import copy
import hashlib
import json
import math
import time

POLL_SECONDS = 180
MAX_PROFILES = 2
MAX_EVENTS = 64


class AuthProbeTimeout(ValueError):
    """The selected account probe timed out before native input submission."""

    def __init__(self, capture):
        super().__init__('Cannot read Claude Code sign-in status')
        self.capture = capture
        self.studioPreparation = True


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def _account_pin(row):
    return {key: copy.deepcopy(row.get(key)) for key in (
        'id', 'provider', 'home', 'claudeOptions', 'accountId', '_credentialIdentity',
        'deleted', 'disconnected', 'duplicateOf', 'updatedAt')}


def _source(runtime, agent):
    return {**{key: copy.deepcopy(agent.get(key)) for key in (
        'id', 'epoch', 'accountKey', 'threadId', 'rootId', 'parentId', 'provider',
        'cwd', 'branch', 'role', 'isLead', 'imageWorkspace', 'imageWorkspaceReady', 'compactions',
        'claudeOptions', 'pendingSettings', 'pendingSettingsAccountKey')},
        'settings': runtime.preparation_settings(agent)}


def _inputs(db, agent):
    rows = db.execute("SELECT * FROM runtime_events WHERE agent=? AND epoch=? "
                      "AND status IN ('pending','reserved','dispatching','uncertain') "
                      "ORDER BY created,id LIMIT ?", (agent['id'], agent['epoch'], MAX_EVENTS + 1)).fetchall()
    if len(rows) > MAX_EVENTS:
        return None
    result = []
    for row in rows:
        if row['status'] not in {'pending', 'reserved'} or row['turn_id']:
            return None
        meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (row['id'],)).fetchone()
        metadata = json.loads(meta[0]) if meta else None
        assets = []
        for asset_id in (metadata or {}).get('assets', []):
            asset = db.execute('SELECT record FROM runtime_assets WHERE id=?', (asset_id,)).fetchone()
            if not asset:
                return None
            assets.append([asset_id, _digest(json.loads(asset[0]))])
        immutable = {key: row[key] for key in ('id', 'agent', 'epoch', 'kind', 'text', 'created', 'turn_id')}
        result.append({'id': row['id'], 'digest': _digest([immutable, metadata, assets])})
    return result


def require_auth(runtime, agent):
    """Probe outside both shared locks and classify only this exact local result."""
    account = agent.get('accountKey', 'default')
    with runtime.lock, runtime.read_db() as db:
        current = runtime.agent(agent['id'], db)
        attempt = copy.deepcopy(current.get('startAttempt'))
        if (current.get('provider') != 'claude' or not isinstance(attempt, dict)
                or attempt != agent.get('startAttempt') or attempt.get('submitted') is not False):
            return
        capture = {'source': _source(runtime, current), 'attempt': attempt, 'inputs': _inputs(db, current),
                   'taskClaims': runtime.continuation_work_claims(db, current)}
    with runtime.accounts.lock:
        before = _account_pin(runtime.accounts._row(account))
    selected = runtime.accounts.get(account)
    with runtime.accounts.lock:
        row = runtime.accounts._row(account)
        after = _account_pin(row)
        timeout = row.get('status') == 'error' and row.get('_authErrorKind') == 'timeout'
    if (before != after or after.get('provider') != 'claude' or after.get('deleted')
            or after.get('disconnected') or after.get('duplicateOf')):
        error = ValueError('The Claude account changed during authentication')
    elif selected.get('status') == 'ready':
        return
    elif timeout and selected.get('status') == 'error' and capture['inputs'] is not None:
        capture['accountPin'] = after
        raise AuthProbeTimeout(capture)
    else:
        error = ValueError(selected.get('error') or 'Sign in with claude auth login first')
    error.studioPreparation = True
    raise error


def _eligible(runtime, db, agent, receipt):
    from codex_context_repair import blocked
    from codex_native_errors import native_thread_block
    attempt = agent.get('startAttempt') or {}
    if (runtime.closed or agent.get('deletedAt') or agent.get('agentArchive')
            or not agent.get('autoWake') or agent.get('status') not in {'waiting', 'failed'}
            or agent.get('inFlight') or agent.get('turnId') or agent.get('nativeFailureHold')
            or agent.get('activeTools') or agent.get('workspaceOperation') or agent.get('accountTransferId')
            or blocked(agent) or native_thread_block(agent)
            or _source(runtime, agent) != receipt['source']
            or attempt != receipt['attempt'] or attempt.get('submitted') is not False
            or attempt.get('activeAtReservation') or attempt.get('turnId') or attempt.get('observedTurnId')
            or agent.get('error') != receipt['error'] or _inputs(db, agent) != receipt['inputs']
            or not isinstance(receipt.get('taskClaims'), list)
            or not runtime.continuation_work_claims_valid(db, agent, receipt['taskClaims'])):
        return False
    for event_id in attempt.get('events', []):
        if not db.execute("SELECT 1 FROM runtime_events WHERE id=? AND agent=? AND epoch=? "
                          "AND status='pending' AND turn_id IS NULL", (event_id, agent['id'], agent['epoch'])).fetchone():
            return False
    parent, seen = agent, set()
    while parent.get('parentId'):
        if parent['id'] in seen or len(seen) >= 64:
            return False
        seen.add(parent['id'])
        parent = runtime.agent(parent['parentId'], db)
        if not parent.get('autoWake') or parent.get('deletedAt') or parent.get('status') == 'paused':
            return False
    return parent['id'] == agent['rootId']


def record_wait(runtime, db, agent, error, *, unknown=False):
    """Save a sparse receipt after the original events return to pending."""
    if unknown or not isinstance(error, AuthProbeTimeout):
        return False
    capture = error.capture
    original = capture.get('attempt') or {}
    attempt = agent.get('startAttempt') or {}
    expected = {**original, 'executionOutcome': 'unsent'}
    if (not original.get('events') or original.get('submitted') is not False
            or original.get('action') or attempt != expected):
        return False
    receipt = {**copy.deepcopy(capture), 'id': 'claude-auth-probe:' + original['id'],
               'attempt': copy.deepcopy(attempt), 'agent': agent['id'], 'error': str(error),
               'accountKey': agent.get('accountKey', 'default'),
               'status': 'auth_probe_wait', 'cause': 'claude_auth_probe',
               'createdAt': time.time(), 'dueAt': time.time() + POLL_SECONDS}
    if not _eligible(runtime, db, agent, receipt):
        return False
    with runtime.accounts.lock:
        if _account_pin(runtime.accounts._row(agent.get('accountKey', 'default'))) != receipt['accountPin']:
            return False
        db.execute('INSERT OR REPLACE INTO runtime_usage_resumes VALUES (?,?,?)',
                   (receipt['id'], agent['id'], json.dumps(receipt)))
        agent['status'] = 'waiting'
        runtime.put(db, 'agents', agent)
    return True


def retained_wait(runtime, db, agent):
    """Keep an exact unsubmitted wait through the usual startup receipt cleanup."""
    attempt = agent.get('startAttempt') or {}
    if (agent.get('provider') != 'claude' or agent.get('status') not in {'waiting', 'failed'}
            or attempt.get('submitted') is not False or not isinstance(attempt.get('id'), str)):
        return False
    row = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=? AND agent=?',
                     ('claude-auth-probe:' + attempt['id'], agent['id'])).fetchone()
    if not row:
        return False
    try:
        receipt = json.loads(row[0])
    except (TypeError, ValueError):
        return False
    if (not isinstance(receipt, dict) or receipt.get('status') != 'auth_probe_wait' or receipt.get('cause') != 'claude_auth_probe'
            or receipt.get('agent') != agent['id'] or receipt.get('error') != 'Cannot read Claude Code sign-in status'
            or type(receipt.get('createdAt')) not in (int, float)
            or type(receipt.get('dueAt')) not in (int, float)
            or not math.isfinite(receipt['createdAt']) or not math.isfinite(receipt['dueAt'])
            or receipt['createdAt'] <= 0
            or receipt['dueAt'] < receipt['createdAt']
            or not all(key in receipt for key in ('source', 'attempt', 'inputs', 'accountPin', 'taskClaims'))):
        return False
    if not _eligible(runtime, db, agent, receipt):
        return False
    with runtime.accounts.lock:
        try:
            return _account_pin(runtime.accounts._row(agent.get('accountKey', 'default'))) == receipt['accountPin']
        except ValueError:
            return False


def tick(runtime):
    """Claim at most two profile probes without waiting in the scheduler."""
    if runtime.closed:
        return
    now = time.time()
    with runtime.read_db() as db:
        rows = db.execute("SELECT agent,record FROM runtime_usage_resumes "
                          "WHERE json_extract(record,'$.status')='auth_probe_wait' "
                          "AND json_extract(record,'$.dueAt')<=? ORDER BY json_extract(record,'$.dueAt') LIMIT 16",
                          (now,)).fetchall()
    if not rows:
        return
    jobs = []
    with runtime.lock, runtime.db() as db:
        active = runtime.__dict__.setdefault('_claude_auth_wait_jobs', {})
        for row in rows:
            receipt = json.loads(row['record'])
            saved = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=?', (receipt['id'],)).fetchone()
            receipt = json.loads(saved[0]) if saved else {}
            if receipt.get('status') != 'auth_probe_wait' or receipt.get('dueAt', now + 1) > now:
                continue
            account = receipt['source'].get('accountKey', 'default')
            if account in active or len(active) >= MAX_PROFILES:
                continue
            current = runtime.agent(row['agent'], db)
            if not _eligible(runtime, db, current, receipt):
                db.execute("UPDATE runtime_usage_resumes SET record=json_set(record,'$.status','cancelled',"
                           "'$.dueAt',NULL,'$.reason','The original input or chat changed') WHERE id=? "
                           "AND json_extract(record,'$.status')='auth_probe_wait'", (receipt['id'],))
                continue
            with runtime.accounts.lock:
                profile = _account_pin(runtime.accounts._row(account))
            if profile != receipt['accountPin']:
                db.execute("UPDATE runtime_usage_resumes SET record=json_set(record,'$.status','cancelled',"
                           "'$.dueAt',NULL,'$.reason','The selected account changed') WHERE id=?", (receipt['id'],))
                continue
            # All waits for this profile share one sparse probe deadline.
            db.execute("UPDATE runtime_usage_resumes SET record=json_set(record,'$.dueAt',?) "
                       "WHERE json_extract(record,'$.status')='auth_probe_wait' "
                       "AND json_extract(record,'$.accountKey')=?", (now + POLL_SECONDS, account))
            token = object()
            active[account] = token
            jobs.append((account, profile, token))
    for account, profile, token in jobs:
        try:
            runtime.recovery_pool.submit(_probe, runtime, account, profile, token)
        except Exception:
            with runtime.lock:
                if runtime.__dict__.get('_claude_auth_wait_jobs', {}).get(account) is token:
                    runtime._claude_auth_wait_jobs.pop(account)


def _probe(runtime, account, profile, token):
    try:
        with runtime.lock:
            if runtime.closed or runtime._claude_auth_wait_jobs.get(account) is not token:
                return
        with runtime.accounts.lock:
            if _account_pin(runtime.accounts._row(account)) != profile:
                return
        from codex_claude import auth_metadata
        result = auth_metadata(copy.deepcopy(profile['claudeOptions']), force=True)
        with runtime.lock, runtime.db() as db, runtime.accounts.lock:
            if runtime.closed or runtime._claude_auth_wait_jobs.get(account) is not token:
                return
            same_profile = _account_pin(runtime.accounts._row(account)) == profile
            rows = db.execute("SELECT agent,record FROM runtime_usage_resumes "
                              "WHERE json_extract(record,'$.status')='auth_probe_wait' "
                              "AND json_extract(record,'$.accountKey')=? LIMIT 32", (account,)).fetchall()
            for row in rows:
                receipt = json.loads(row['record'])
                agent = runtime.agent(row['agent'], db)
                if not same_profile or receipt['accountPin'] != profile or not _eligible(runtime, db, agent, receipt):
                    receipt.update(status='cancelled', dueAt=None, reason='The original input or chat changed')
                elif (result.get('status') == 'ready' and isinstance(profile.get('accountId'), str)
                      and profile['accountId'] and result.get('accountId') == profile['accountId']
                      and isinstance(profile.get('_credentialIdentity'), str) and profile['_credentialIdentity']
                      and result.get('_credentialIdentity') == profile['_credentialIdentity']):
                    receipt.update(status='completed', dueAt=None, proofAt=time.time())
                    agent.update(status='queued', error=None)
                    runtime.put(db, 'agents', agent)
                    runtime.changed.set()
                db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                           (json.dumps(receipt), receipt['id']))
    finally:
        with runtime.lock:
            if runtime.__dict__.get('_claude_auth_wait_jobs', {}).get(account) is token:
                runtime._claude_auth_wait_jobs.pop(account)
