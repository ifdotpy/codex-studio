"""Client-side error events for the Codex 0.153.4 app-server boundary.

Native Codex owns retry, authentication, compaction, and tool execution.
These handlers record its decisions; they never resubmit a model or tool call.
"""
import hashlib
import json
import time


class NativeRpcError(RuntimeError):
    """An explicit JSON-RPC rejection, distinct from a lost acknowledgement."""
    def __init__(self, error):
        self.error = error
        self.code = error.get('code') if isinstance(error, dict) else None
        self.data = error.get('data') if isinstance(error, dict) else None
        super().__init__(json.dumps(error, ensure_ascii=False))


SUPPORTED_REQUESTS = frozenset({
    'item/commandExecution/requestApproval', 'item/fileChange/requestApproval',
    'item/tool/requestUserInput', 'item/permissions/requestApproval',
    'mcpServer/elicitation/request', 'execCommandApproval', 'applyPatchApproval',
})
LEGACY_APPROVAL_REQUESTS = frozenset({'execCommandApproval', 'applyPatchApproval'})


def native_request_thread(method, params):
    field = 'conversationId' if method in LEGACY_APPROVAL_REQUESTS else 'threadId'
    return params.get(field)


NOTICE_METHODS = frozenset({
    'warning', 'guardianWarning', 'configWarning', 'deprecationNotice',
    'mcpServer/startupStatus/updated', 'modelProvider/authRecoveryStarted',
    'modelProvider/authRecoveryCompleted', 'mcpServer/oauthLogin/completed',
    'autoApprovalReview/strictReviewRequired',
})
HOOK_METHODS = frozenset({'hook/started', 'hook/completed'})
TURN_NOTICE_METHODS = frozenset({
    'model/safetyBuffering/updated', 'model/verification',
    'item/autoApprovalReview/started', 'item/autoApprovalReview/completed',
})


def error_kind(error):
    if not isinstance(error, dict):
        return ''
    info = error.get('codexErrorInfo')
    return info if isinstance(info, str) else next(iter(info), '') if isinstance(info, dict) else ''



def error_message(error):
    if isinstance(error, dict):
        return str(error.get('message') or 'Codex reported an error.')
    return str(error or 'Codex reported an error.')


THREAD_BLOCK_MESSAGE = 'This chat is stopped as a precaution. Start or resume another chat.'


def preserve_thread_block(agent, error):
    """Keep the native precaution separate from temporary transport errors."""
    if (agent.get('threadId') and isinstance(error, dict)
            and error.get('codexErrorInfo') == 'misalignmentPolicyViolation'):
        agent['nativeThreadBlock'] = {'threadId': agent['threadId'], 'error': error}


def native_thread_block(agent):
    block = agent.get('nativeThreadBlock') or {}
    if block.get('threadId') and block['threadId'] == agent.get('threadId'):
        return block
    # Existing records can predate the separate persistent precaution field.
    error = agent.get('error')
    if (agent.get('threadId') and isinstance(error, dict)
            and error.get('codexErrorInfo') == 'misalignmentPolicyViolation'):
        return {'threadId': agent['threadId'], 'error': error}
    previous = agent.get('nativeTurnError') or {}
    if agent.get('threadId') and previous.get('turnId') and previous['turnId'] == agent.get('turnId'):
        error = previous.get('error')
        if isinstance(error, dict) and error.get('codexErrorInfo') == 'misalignmentPolicyViolation':
            return {'threadId': agent.get('threadId'), 'error': error}
    return None


def assert_native_thread_open(agent):
    if native_thread_block(agent):
        raise ValueError(THREAD_BLOCK_MESSAGE)


def refresh_native_limits(runtime, db, agent, error, turn_id, account_key, connection_id):
    """Read native limits once after this account's exact failed turn."""
    if (not isinstance(error, dict) or error.get('codexErrorInfo') not in
            ('usageLimitExceeded', 'rateLimitExceeded') or not turn_id or not agent.get('threadId')
            or runtime.closed):
        return
    connection_id = connection_id or runtime.connection_ids.get(account_key)
    if not connection_id or not runtime.connection_current(account_key, connection_id):
        return
    identity = [account_key, agent['threadId'], turn_id]
    key = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_limit_refreshes '
               '(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    inserted = db.execute('INSERT OR IGNORE INTO runtime_native_limit_refreshes VALUES (?,?)',
                         (key, json.dumps({'accountKey': account_key, 'threadId': agent['threadId'],
                                           'turnId': turn_id, 'connectionId': connection_id})))
    if not inserted.rowcount:
        return
    agent['nativeLimitErrorAt'] = time.time()
    def read_limits():
        try:
            runtime.limits(account_key, force=True, connection_id=connection_id)
        except Exception:
            # A failed read does not change the turn or authorize another request.
            return
    runtime.recovery_pool.submit(read_limits)


def notice(runtime, db, agent, key, text, kind='warning', **metadata):
    runtime.item(db, agent['id'], 'native-notice:' + key, 'system', text,
                 'Codex', nativeNotice=kind, **metadata)


def warning_text(method, p):
    text = p.get('message') or p.get('summary') or ''
    if method == 'autoApprovalReview/strictReviewRequired':
        return 'Codex requires additional safety checks. Some tools may take longer.'
    if method == 'mcpServer/oauthLogin/completed':
        return (str(p.get('name') or 'Connected tool') + ': ' + str(p.get('error') or 'Sign-in failed.')
                if p.get('success') is False else '')
    if method == 'mcpServer/startupStatus/updated':
        return (str(p.get('name') or 'Connected tool') + ': ' + str(p.get('error') or p['status'])
                if p.get('status') in {'failed', 'cancelled'} else '')
    if method == 'guardianWarning' and text.startswith('Automatic approval review approved ('):
        return ''
    return text


def account_notice(runtime, db, method, p, account_key, connection_id):
    # These events can arrive during startup, before any thread exists.
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_notices (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    text = warning_text(method, p)
    identity = [account_key, connection_id, method, p.get('name') if method.startswith('mcpServer/') else text]
    key = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
    if not text:
        db.execute('DELETE FROM runtime_native_notices WHERE id=?', (key,))
        return
    runtime.put(db, 'native_notices', {'id': key, 'accountKey': account_key, 'connectionId': connection_id,
                'message': text, 'details': p.get('details'), 'at': time.time()})
    # Bound diagnostics without allowing one account to evict another account's warnings.
    db.execute("DELETE FROM runtime_native_notices WHERE json_extract(record,'$.accountKey')=? AND id NOT IN "
               "(SELECT id FROM runtime_native_notices WHERE json_extract(record,'$.accountKey')=? "
               "ORDER BY json_extract(record,'$.at') DESC LIMIT 50)", (account_key, account_key))


def account_notices(runtime, db):
    if not db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_native_notices'").fetchone():
        return []
    return [r for r in runtime.records(db, 'native_notices')
            if runtime.connection_ids.get(r['accountKey']) == r.get('connectionId')]


def hook_notice(runtime, db, agent, method, params):
    run = params.get('run') or {}
    if not isinstance(run.get('id'), str) or not run['id']:
        return
    key = 'hook:' + run['id']
    item_id = agent['id'] + ':native-notice:' + key
    previous = db.execute('SELECT record FROM runtime_items WHERE id=?', (item_id,)).fetchone()
    if method == 'hook/started' and previous:
        status = (json.loads(previous[0]).get('nativeHook') or {}).get('status')
        if status and status != 'running':
            return
    # Native TUI hides context entries and uses only the first warning entry.
    entries = []
    warning_seen = False
    for entry in run.get('entries') or []:
        if entry.get('kind') == 'context':
            continue
        if entry.get('kind') == 'warning':
            if warning_seen:
                continue
            warning_seen = True
        entries.append(entry)
    status = run.get('status') or ('running' if method == 'hook/started' else 'completed')
    if method == 'hook/completed' and status == 'completed' and not entries:
        # Publish a tombstone so transcript caches also clear the running notice.
        # Quiet success has no visible history cell in the native TUI.
        notice(runtime, db, agent, key, '', 'info', turnId=params.get('turnId'),
               nativeHook={**run, 'status': status, 'entries': []}, nativeHookQuiet=True)
        runtime.touch_ui(agent['id'])
        return
    labels = {'running': 'Hook running', 'completed': 'Hook completed', 'failed': 'Hook failed',
              'blocked': 'Blocked by hook', 'stopped': 'Hook stopped'}
    text = labels.get(status, 'Hook ' + str(status))
    if run.get('eventName'):
        text += ': ' + str(run['eventName'])
    details = '\n\n'.join(str(value) for value in
                           [run.get('statusMessage'), *(entry.get('text') for entry in entries)] if value)
    notice(runtime, db, agent, key, text, 'error' if status in {'failed', 'blocked', 'stopped'} else 'info',
           turnId=params.get('turnId'), details=details or None,
           nativeHook={**run, 'status': status, 'entries': entries})
    runtime.touch_ui(agent['id'])


def consume_native_notification(runtime, message, account_key, connection_id):
    method, p = message.get('method'), message.get('params') or {}
    if method not in NOTICE_METHODS | HOOK_METHODS | TURN_NOTICE_METHODS | {'error', 'serverRequest/resolved'}:
        return False
    tid = p.get('threadId')
    with runtime.lock, runtime.db() as db:
        if not runtime.connection_current(account_key, connection_id):
            return True
        if not tid and method in NOTICE_METHODS:
            account_notice(runtime, db, method, p, account_key, connection_id)
            return True
        agents = [a for a in runtime.records(db, 'agents')
                  if not a.get('deletedAt') and a.get('accountKey', 'default') == account_key
                  and (a.get('threadId') == tid if tid else a.get('inFlight'))]
        if method == 'serverRequest/resolved':
            # A resolved request is not evidence that permission was granted.
            for a in agents:
                for r in runtime.records(db, 'requests'):
                    if ((r.get('agent') == a['id'] or
                         (r.get('agent') is None and r.get('method') in LEGACY_APPROVAL_REQUESTS))
                            and r.get('rpcId') == p.get('requestId')
                            and r.get('accountKey', 'default') == account_key
                            and r.get('connectionId') == connection_id
                            and native_request_thread(r.get('method'), r.get('params', {})) == tid
                            and r.get('status') in {'pending', 'answering', 'uncertain', 'blocked'}):
                        r.update(status='resolved', resolvedAt=time.time())
                        runtime.put(db, 'requests', r)
                pending = any(r.get('agent') == a['id'] and r.get('status') == 'pending'
                              for r in runtime.records(db, 'requests'))
                if a['status'] == 'approval' and not pending and a.get('inFlight') and a.get('autoWake'):
                    a['status'] = 'running'
                    runtime.put(db, 'agents', a)
            return True
        for a in agents:
            if method in HOOK_METHODS:
                if tid:
                    hook_notice(runtime, db, a, method, p)
                continue
            turn = p.get('turnId')
            if turn and (turn != a.get('turnId') or not a.get('inFlight')):
                continue
            if method in TURN_NOTICE_METHODS and (not tid or not turn):
                continue
            now = time.time()
            runtime.analytics_safe(db, runtime.analytics_event, a, method, p)
            a['events'] = a.get('events', 0) + 1
            if method == 'model/safetyBuffering/updated':
                previous = a.get('nativeSafetyBuffering') or {}
                same = previous.get('turnId') == turn
                a['nativeSafetyBuffering'] = {
                    **p, 'accountKey': account_key, 'connectionId': connection_id,
                    'at': previous.get('at', now) if same else now,
                    'dismissed': bool(same and previous.get('dismissed')),
                    'responseStarted': bool(same and previous.get('responseStarted')) or
                        (a.get('nativeResponseTurn') == turn),
                }
                if p.get('showBufferingUi') and not a['nativeSafetyBuffering']['responseStarted']:
                    a['nativeStatus'] = {'phase': 'safety', 'turnId': turn, 'at': now,
                                         'message': 'Codex is checking this request'}
                    a['activity'] = {'phase': 'safety', 'at': now}
                elif (a.get('nativeStatus') or {}).get('phase') == 'safety':
                    a.pop('nativeStatus', None)
                    a['activity'] = {'phase': 'thinking', 'at': now}
            elif method == 'model/verification':
                if 'trustedAccessForCyber' in p.get('verifications', []):
                    notice(runtime, db, a, 'model-verification',
                           'Codex reports additional cybersecurity checks. Responses may take longer.',
                           'warning', turnId=turn, details='Trusted Access: https://chatgpt.com/cyber')
            elif method.startswith('item/autoApprovalReview/'):
                review = p.get('review') or {}
                status = review.get('status', '')
                label = {'inProgress': 'in progress', 'timedOut': 'timed out'}.get(status, status)
                complete = method.endswith('/completed')
                approved = complete and status == 'approved'
                notice(runtime, db, a, 'review:' + str(p.get('reviewId')),
                       '' if approved else ('Tool safety review: ' + label if complete else 'Checking tool permissions'),
                       'warning' if complete and not approved else 'info', turnId=turn,
                       details=review.get('rationale') or review.get('reason'),
                       nativeReview=p, nativeHookQuiet=approved)
            elif method == 'error':
                # No identity means no authority to change a turn's error state.
                if not tid or not turn:
                    continue
                error = p.get('error') or {'message': 'Codex reported an error.'}
                if p.get('willRetry') is True:
                    a['nativeStatus'] = {'phase': 'retrying', 'error': error, 'turnId': turn, 'at': now}
                    a['activity'] = {'phase': 'retrying', 'at': now}
                elif error_kind(error) == 'activeTurnNotSteerable':
                    notice(runtime, db, a, 'steer:' + turn, error_message(error), 'warning', turnId=turn, nativeError=error)
                else:
                    a.pop('nativeStatus', None)
                    a.pop('nativeSafetyBuffering', None)
                    preserve_thread_block(a, error)
                    refresh_native_limits(runtime, db, a, error, turn, account_key, connection_id)
                    a['nativeTurnError'] = {'turnId': turn, 'error': error}
                    a['error'] = error
                    a['activity'] = {'phase': 'error', 'at': now}
                    notice(runtime, db, a, 'error:' + turn, error_message(error), 'error',
                           turnId=turn, nativeError=error)
                # turn/completed remains the authority to release the turn.
            elif method.startswith('modelProvider/authRecovery'):
                started = method.endswith('Started')
                if started:
                    a['nativeStatus'] = {'phase': 'auth', 'message': p.get('message'), 'turnId': turn, 'at': now}
                    a['activity'] = {'phase': 'auth', 'at': now}
                elif (a.get('nativeStatus') or {}).get('phase') == 'auth':
                    a.pop('nativeStatus', None)
                    a['activity'] = {'phase': 'thinking', 'at': now}
                notice(runtime, db, a, 'auth:' + str(turn), p.get('message') or
                       ('Restoring sign-in.' if started else 'Sign-in restored.'), 'info', turnId=turn)
            else:
                text = warning_text(method, p)
                if not text:
                    continue
                key = hashlib.sha256(json.dumps([method, text, p.get('details')], ensure_ascii=False).encode()).hexdigest()[:24]
                notice(runtime, db, a, key, text, details=p.get('details'), accountWide=not bool(tid))
            a['lastEvent'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
            runtime.put(db, 'agents', a)
            runtime.touch_ui(a['id'])
    return True


def advance_native_status(agent, method, params):
    """Restore activity only on progress for this turn, not token telemetry."""
    if method in {'turn/started', 'turn/completed'}:
        agent.pop('nativeSafetyBuffering', None)
        agent.pop('nativeResponseTurn', None)
    elif (method == 'item/agentMessage/delta' or
          (method in {'item/started', 'item/completed'} and
           (params.get('item') or {}).get('type') not in {'userMessage', 'reasoning'})):
        turn = params.get('turnId') or agent.get('turnId')
        if turn == agent.get('turnId'):
            agent['nativeResponseTurn'] = turn
            buffering = agent.get('nativeSafetyBuffering') or {}
            if buffering.get('turnId') == turn:
                buffering['responseStarted'] = True
                buffering['showBufferingUi'] = False
                if (agent.get('nativeStatus') or {}).get('phase') == 'safety':
                    agent.pop('nativeStatus', None)
    if method == 'turn/started':
        agent.pop('nativeStatus', None)
        agent.pop('nativeTurnError', None)
    elif method == 'turn/completed':
        turn = params.get('turn') or {}
        previous = agent.get('nativeTurnError') or {}
        if turn.get('status') == 'failed' and not turn.get('error') and previous.get('turnId') == turn.get('id'):
            turn['error'] = previous['error']
        if turn.get('status') == 'failed':
            preserve_thread_block(agent, turn.get('error'))
        agent.pop('nativeStatus', None)
        agent.pop('nativeTurnError', None)
    elif method in {'item/agentMessage/delta', 'item/reasoning/textDelta',
                    'item/reasoning/summaryTextDelta', 'item/started', 'item/completed'}:
        previous = agent.get('nativeStatus') or {}
        if previous.get('phase') == 'retrying' and params.get('turnId') == previous.get('turnId'):
            agent.pop('nativeStatus', None)
            agent['activity'] = {'phase': 'thinking', 'at': time.time()}
