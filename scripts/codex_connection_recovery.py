"""Confirm a disconnected turn without resuming or replaying its work."""
from concurrent.futures import Future
import time

from codex_native_errors import native_thread_block

DISCONNECT_ERRORS = frozenset({
    'Codex disconnected. Review the transcript before resuming.',
    'Server restarted during a turn. Review history, then send a new instruction.',
})
IDENTITY = ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'status', 'error',
            'inFlight', 'autoWake', 'startAttempt', 'accountTransferId',
            'workspaceOperation', 'nativeThreadBlock')


def eligible(agent):
    return (agent.get('status') == 'interrupted' and not agent.get('inFlight')
            and not agent.get('autoWake') and not agent.get('startAttempt')
            and not agent.get('accountTransferId') and not agent.get('workspaceOperation')
            and not agent.get('deletedAt') and agent.get('threadId') and agent.get('turnId')
            and isinstance(agent.get('error'), str) and agent['error'] in DISCONNECT_ERRORS
            and not native_thread_block(agent))


def recover(runtime, key):
    with runtime.lock, runtime.db() as db:
        agent = runtime.checked_actor(db, key)
        if runtime.closed or not eligible(agent):
            return {'status': 'superseded'}
        expected = {field: agent.get(field) for field in IDENTITY}
        account = agent.get('accountKey', 'default')
    try:
        # This can start the account transport. It never loads or resumes a thread.
        server = runtime.connect(account)
        connection = runtime.connection_ids.get(account)
        with runtime.lock, runtime.db() as db:
            if not current(runtime, db, expected, connection, server):
                return {'status': 'superseded'}
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        page = server.call('thread/turns/list', {'threadId': agent['threadId'], 'limit': 10,
            'sortDirection': 'desc', 'itemsView': 'full'}, timeout=10)
        turn = next((t for t in page.get('data', []) if t.get('id') == agent['turnId']), None)
        thread = server.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=5)['thread']
        if thread.get('id') != agent['threadId']:
            raise ValueError('Native thread identity changed')
        if (thread.get('status', {}).get('type') not in {'idle', 'notLoaded'} or not turn
                or turn.get('status') not in {'completed', 'failed', 'interrupted'}):
            return record_check(runtime, expected, connection, server, thread.get('status', {}).get('type'))
        result = Future()
        def apply():
            try:
                result.set_result(apply_result(runtime, expected, connection, server, turn))
            except Exception as error:
                result.set_exception(error)
        server.after_events(apply)
        return result.result(timeout=10)
    except Exception as error:
        return {'status': 'unconfirmed', 'error': str(error)}


def current(runtime, db, expected, connection, server):
    agent = runtime.agent(expected['id'], db)
    account = expected.get('accountKey') or 'default'
    return (not runtime.closed and runtime.servers.get(account) is server
            and runtime.connection_current(account, connection)
            and eligible(agent) and all(agent.get(k) == v for k, v in expected.items()))



def record_check(runtime, expected, connection, server, native_state):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        # A verified read proves only that Codex answered this check. Keep the
        # unknown turn, its original error, and all delivery permissions intact.
        agent['connectionCheck'] = {field: expected[field] for field in
                                    ('epoch', 'accountKey', 'threadId', 'turnId')}
        agent['connectionCheck'].update(at=time.time(), previousError=expected['error'],
                                         nativeState=native_state)
        runtime.put(db, 'agents', agent)
        return {'status': 'unconfirmed', 'checked': True}


def apply_result(runtime, expected, connection, server, turn):
    with runtime.lock, runtime.db() as db:
        if not current(runtime, db, expected, connection, server):
            return {'status': 'superseded'}
        agent = runtime.agent(expected['id'], db)
        outcome = turn['status']
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
        runtime.loaded.discard(agent['id'])
        runtime.put(db, 'agents', agent)
        return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': outcome}
