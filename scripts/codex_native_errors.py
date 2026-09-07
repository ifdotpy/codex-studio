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
NOTICE_METHODS = frozenset({
    'warning', 'guardianWarning', 'configWarning', 'deprecationNotice',
    'mcpServer/startupStatus/updated', 'modelProvider/authRecoveryStarted',
    'modelProvider/authRecoveryCompleted', 'mcpServer/oauthLogin/completed',
    'autoApprovalReview/strictReviewRequired',
})


def error_message(error):
    if isinstance(error, dict):
        return str(error.get('message') or 'Codex reported an error.')
    return str(error or 'Codex reported an error.')


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


def consume_native_notification(runtime, message, account_key, connection_id):
    method, p = message.get('method'), message.get('params') or {}
    if method not in NOTICE_METHODS | {'error', 'serverRequest/resolved'}:
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
                    if (r.get('agent') == a['id'] and r.get('rpcId') == p.get('requestId')
                            and r.get('accountKey', 'default') == account_key
                            and r.get('connectionId') == connection_id
                            and r.get('params', {}).get('threadId') == tid
                            and r.get('status') in {'pending', 'answering', 'uncertain'}):
                        r.update(status='resolved', resolvedAt=time.time())
                        runtime.put(db, 'requests', r)
                pending = any(r.get('agent') == a['id'] and r.get('status') == 'pending'
                              for r in runtime.records(db, 'requests'))
                if a['status'] == 'approval' and not pending and a.get('inFlight') and a.get('autoWake'):
                    a['status'] = 'running'
                    runtime.put(db, 'agents', a)
            return True
        for a in agents:
            turn = p.get('turnId')
            if turn and (turn != a.get('turnId') or not a.get('inFlight')):
                continue
            now = time.time()
            runtime.analytics_safe(db, runtime.analytics_event, a, method, p)
            a['events'] = a.get('events', 0) + 1
            if method == 'error':
                # No identity means no authority to change a turn's error state.
                if not tid or not turn:
                    continue
                error = p.get('error') or {'message': 'Codex reported an error.'}
                if p.get('willRetry') is True:
                    a['nativeStatus'] = {'phase': 'retrying', 'error': error, 'turnId': turn, 'at': now}
                    a['activity'] = {'phase': 'retrying', 'at': now}
                elif isinstance(error, dict) and isinstance(error.get('codexErrorInfo'), dict) and 'activeTurnNotSteerable' in error['codexErrorInfo']:
                    notice(runtime, db, a, 'steer:' + turn, error_message(error), 'warning', turnId=turn, nativeError=error)
                else:
                    a.pop('nativeStatus', None)
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
    if method == 'turn/started':
        agent.pop('nativeStatus', None)
        agent.pop('nativeTurnError', None)
    elif method == 'turn/completed':
        turn = params.get('turn') or {}
        previous = agent.get('nativeTurnError') or {}
        if turn.get('status') == 'failed' and not turn.get('error') and previous.get('turnId') == turn.get('id'):
            turn['error'] = previous['error']
        agent.pop('nativeStatus', None)
        agent.pop('nativeTurnError', None)
    elif method in {'item/agentMessage/delta', 'item/reasoning/textDelta',
                    'item/reasoning/summaryTextDelta', 'item/started', 'item/completed'}:
        previous = agent.get('nativeStatus') or {}
        if previous.get('phase') == 'retrying' and params.get('turnId') == previous.get('turnId'):
            agent.pop('nativeStatus', None)
            agent['activity'] = {'phase': 'thinking', 'at': time.time()}
