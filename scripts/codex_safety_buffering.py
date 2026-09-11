"""Explicit server-offered safety retry. Each native mutation is submitted once.

The durable receipt survives UI retries and server restarts. Late RPC replies
continue the same operation. An unknown result never authorizes a second fork.
"""
import json
import time
import uuid
from threading import Timer

from codex_native_errors import NativeRpcError, assert_native_thread_open

ACTIVE = {'turns', 'items', 'interrupt', 'verify_turns', 'verify_items', 'fork', 'start', 'unknown'}


def active(agent):
    receipt = agent.get('nativeSafetyRetry') or {}
    return (receipt.get('stage') in ACTIVE and receipt.get('epoch') == agent.get('epoch')
            and receipt.get('accountKey') == agent.get('accountKey', 'default'))


def action(runtime, key, data):
    choice = data.get('safety')
    if choice not in {'wait', 'retry', 'cancel'}:
        raise ValueError('Choose wait or retry')
    with runtime.lock, runtime.db() as db:
        a = runtime.checked_actor(db, key)
        db.execute('CREATE TABLE IF NOT EXISTS runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
        identity = key + ':' + str(data.get('turnId'))
        old = db.execute('SELECT record FROM runtime_safety_retries WHERE id=?', (identity,)).fetchone()
        if old:
            receipt = json.loads(old[0])
            if receipt['accountKey'] != a.get('accountKey', 'default') or receipt['epoch'] != a['epoch']:
                raise ValueError('This retry belongs to an earlier account or agent state')
            if choice == 'cancel' and receipt.get('stage') in ACTIVE:
                if receipt.get('rpcMethod') == 'turn/start':
                    raise ValueError('The retry was already submitted. Wait for its receipt or stop the turn.')
                receipt.update(stage='cancelled', error=None)
                buffering = a.get('nativeSafetyBuffering') or {}
                buffering['dismissed'] = True
                if not a.get('inFlight'):
                    a['status'] = 'interrupted'
                save(runtime, db, a, receipt)
            return public(receipt)
        if choice == 'cancel':
            raise ValueError('No model change exists for this turn')
        b = a.get('nativeSafetyBuffering') or {}
        if (not a.get('inFlight') or not a.get('autoWake') or a.get('accountTransferId')
                or b.get('turnId') != data.get('turnId') or b.get('turnId') != a.get('turnId')
                or b.get('threadId') != a.get('threadId') or not b.get('showBufferingUi')
                or b.get('responseStarted') or a.get('nativeResponseTurn') == a.get('turnId')
                or not runtime.connection_current(a.get('accountKey', 'default'), b.get('connectionId'))):
            raise ValueError('This safety check is no longer active')
        assert_native_thread_open(a)
        if choice == 'wait':
            b['dismissed'] = True
            runtime.put(db, 'agents', a)
            runtime.touch_ui(key)
            return {'stage': 'waiting'}
        runtime.assert_workspace_available(db, a)
        model = b.get('fasterModel')
        if not isinstance(model, str) or not model:
            raise ValueError('Codex did not offer another model')
        op = {'id': identity, 'agent': key, 'accountKey': a.get('accountKey', 'default'),
              'connectionId': b['connectionId'], 'epoch': a['epoch'], 'threadId': a['threadId'],
              'turnId': a['turnId'], 'model': model, 'stage': 'turns', 'created': time.time(),
              'attemptId': str(uuid.uuid4()), 'sourceThreadId': a['threadId']}
        save(runtime, db, a, op)
        db.commit()
    issue(runtime, op, 'turns')
    return public(op)


def public(op):
    return {k: op.get(k) for k in ('id', 'stage', 'model', 'turnId', 'created', 'updated', 'epoch', 'accountKey',
                                  'error', 'newThreadId', 'acceptedTurnId', 'requestId', 'rpcMethod')}


def save(runtime, db, a, op):
    op['updated'] = time.time()
    runtime.put(db, 'safety_retries', op)
    a['nativeSafetyRetry'] = public(op)
    runtime.put(db, 'agents', a)
    runtime.touch_ui(a['id'])


def current(runtime, db, op):
    a = runtime.agent(op['agent'], db)
    if (not runtime.operation_current(a, op) or not a.get('autoWake') or a.get('accountTransferId')
            or a.get('threadId') != op.get('newThreadId', op['threadId'])
            or (a.get('nativeSafetyRetry') or {}).get('id') != op['id']):
        raise ValueError('Safety retry stopped because the account or agent state changed')
    if (a.get('nativeSafetyRetry') or {}).get('stage') == 'cancelled':
        raise ValueError('The model change was cancelled')
    assert_native_thread_open(a)
    return a


def issue(runtime, op, stage):
    """Submit without a wait in the coordination pool. Preserve late replies."""
    try:
        with runtime.lock, runtime.db() as db:
            a = current(runtime, db, op)
            server = runtime.servers[op['accountKey']]
            params = {'threadId': op['threadId']}
            if stage in {'turns', 'verify_turns'}:
                method = 'thread/turns/list'
                params.update(limit=1, sortDirection='desc', itemsView='notLoaded')
            elif stage in {'items', 'verify_items'}:
                method = 'thread/items/list'
                params.update(turnId=op['turnId'], limit=1000, sortDirection='asc')
            elif stage == 'interrupt':
                b = a.get('nativeSafetyBuffering') or {}
                if (not b.get('showBufferingUi') or b.get('responseStarted') or
                        a.get('nativeResponseTurn') == op['turnId'] or a.get('turnId') != op['turnId']):
                    raise ValueError('The response started. Keep the current turn.')
                method = 'turn/interrupt'
                params['turnId'] = op['turnId']
            elif stage == 'fork':
                method = 'thread/fork'
                params.update(beforeTurnId=op['turnId'], deferGoalContinuation=True,
                              excludeTurns=True, model=op['model'], cwd=a['cwd'])
                # Preserve the native permission profile. Never increase access.
                base = runtime.new_thread_params(a)
                params.update({k: v for k, v in base.items() if k in {
                    'approvalPolicy', 'approvalsReviewer', 'sandbox', 'permissions',
                    'config', 'developerInstructions', 'runtimeWorkspaceRoots'}})
            else:
                method = 'turn/start'
                params.update(threadId=op['newThreadId'], input=op['input'],
                              clientUserMessageId=op['attemptId'], model=op['model'], effort='low',
                              serviceTier='priority' if a.get('fastMode') else 'default',
                              **runtime.turn_permissions(a))
                a.update(status='starting', inFlight=True, turnEpoch=a['epoch'], error=None,
                         startAttempt={'id': op['attemptId'], 'epoch': a['epoch'], 'events': [],
                             'action': 'safety', 'submitted': True, 'accountKey': op['accountKey'],
                             'connectionId': op['connectionId'], 'threadId': op['newThreadId']})
            op.update(stage=stage, error=None, rpcMethod=method)
            save(runtime, db, a, op)
            db.commit()
            submitted = runtime.submit_reserved(server, method, params)
            # AppServer returns (request ID, method, future). Test transports can return a future.
            if isinstance(submitted, tuple):
                op['requestId'] = submitted[0]
                save(runtime, db, a, op)
        ticket = dict(op)
        timer = Timer(15, lambda: fail(runtime, ticket, TimeoutError('Waiting for Codex to confirm ' + method), unknown=True))
        timer.daemon = True
        def receive(future):
            timer.cancel()
            if not runtime.closed:
                runtime.recovery_pool.submit(complete, runtime, op, stage, future)
        timer.start()
        server.on_result(submitted, receive)
    except Exception as error:
        fail(runtime, op, error, unknown=not isinstance(error, (ValueError, NativeRpcError)))


def complete(runtime, op, stage, future):
    try:
        result = future.result()
        with runtime.lock, runtime.db() as db:
            if stage in {'interrupt', 'fork', 'start'}:
                # Retain successful mutation receipts even after owner cancellation.
                row = db.execute('SELECT record FROM runtime_safety_retries WHERE id=?', (op['id'],)).fetchone()
                stored = json.loads(row[0])
                stored.setdefault('responses', {})[stage] = result
                runtime.put(db, 'safety_retries', stored)
                db.commit()
                op['responses'] = stored['responses']
            a = current(runtime, db, op)
            if stage in {'turns', 'verify_turns'}:
                turns = result.get('data') or []
                if not turns or turns[0].get('id') != op['turnId']:
                    raise ValueError('The selected turn is no longer the latest turn')
                if stage == 'verify_turns' and turns[0].get('status') != 'interrupted':
                    raise ValueError('Codex did not confirm an interrupted turn. No retry was sent.')
                next_stage = 'items' if stage == 'turns' else 'verify_items'
            elif stage in {'items', 'verify_items'}:
                entries = result.get('data') or []
                if result.get('nextCursor'):
                    raise ValueError('Turn history is incomplete. No retry was sent.')
                if any(entry.get('turnId') != op['turnId'] or
                       entry.get('item', {}).get('type') not in {'userMessage', 'reasoning'} for entry in entries):
                    raise ValueError('This turn already produced a response or used tools. No retry was sent.')
                inputs = [part for entry in entries if entry['item']['type'] == 'userMessage'
                          for part in entry['item'].get('content', [])]
                if not inputs:
                    raise ValueError('The original user input is unavailable. No retry was sent.')
                op['input'] = inputs
                next_stage = 'interrupt' if stage == 'items' else 'fork'
            elif stage == 'interrupt':
                next_stage = 'verify_turns'
            elif stage == 'fork':
                tid = (result.get('thread') or {}).get('id')
                if not isinstance(tid, str) or not tid or tid == op['threadId']:
                    raise RuntimeError('The fork response has no new thread identity; outcome unknown')
                op['newThreadId'] = tid
                # Keep the Studio team, queued messages and history. Native history
                # continues at the fork, excluding the interrupted turn.
                a.update(threadId=tid, model=op['model'], effort='low', nativeEffort='low',
                         turnId=None, inFlight=False, status='starting', nativeFailureHold=False,
                         activeTools=[], error=None)
                a.pop('startAttempt', None)
                a.pop('nativeSafetyBuffering', None)
                runtime.loaded.add(a['id'])
                runtime.preparations.pop(a['id'], None)
                next_stage = 'start'
            else:
                turn = (result.get('turn') or {}).get('id')
                if not isinstance(turn, str) or not turn:
                    raise RuntimeError('The retry response has no turn identity; outcome unknown')
                attempt = a.get('startAttempt') or {}
                op.update(stage='running', acceptedTurnId=turn, error=None)
                save(runtime, db, a, op)
                db.commit()
                runtime.start_accepted(a['id'], attempt, result)
                return
            save(runtime, db, a, op)
        issue(runtime, op, next_stage)
    except Exception as error:
        fail(runtime, op, error, unknown=not isinstance(error, (ValueError, NativeRpcError)))


def fail(runtime, op, error, *, unknown):
    if runtime.closed:
        return
    with runtime.lock, runtime.db() as db:
        if runtime.closed:
            return
        row = db.execute('SELECT record FROM runtime_safety_retries WHERE id=?', (op['id'],)).fetchone()
        stored = json.loads(row[0]) if row else op
        if stored.get('stage') in {'running', 'failed', 'cancelled'} or stored.get('rpcMethod') != op.get('rpcMethod'):
            return
        a = runtime.agent(op['agent'], db)
        op.update(stage='unknown' if unknown else 'failed', error=str(error))
        # Keep the receipt even if the owner stopped or transferred the agent.
        runtime.put(db, 'safety_retries', op)
        if (a.get('nativeSafetyRetry') or {}).get('id') == op['id']:
            if (op.get('rpcMethod') == 'turn/start' and not unknown
                    and runtime.operation_current(a, op)
                    and a.get('threadId') == op.get('newThreadId')
                    and (a.get('startAttempt') or {}).get('id') == op['attemptId']):
                a.update(inFlight=False, status='failed', nativeFailureHold=True, error=str(error))
            save(runtime, db, a, op)
