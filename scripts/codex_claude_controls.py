"""Native Claude controls with durable workspace reservations and receipts."""
import json
import re
import time
import uuid

_MODES = {'default', 'acceptEdits', 'auto', 'plan', 'bypassPermissions'}


def profile(rt, body):
    options, label = body.get('options'), body.get('label')
    if body.get('account_key'):
        rt.accounts.update_claude(body['account_key'], options, label)
    else:
        rt.accounts.register_claude(options, label)
    rt.changed.set()
    return rt.accounts.snapshot()


def _actor(rt, db, key):
    agent = rt.checked_actor(db, key)
    if rt.accounts.get(agent.get('accountKey', 'default')).get('provider') != 'claude':
        raise ValueError('Select a Claude Code chat')
    return agent


def _settings(value):
    if not isinstance(value, dict) or set(value) - {'permissionMode', 'thinking', 'autoCompactWindow'}:
        raise ValueError('Invalid Claude settings')
    if value.get('permissionMode', 'default') not in _MODES:
        raise ValueError('Invalid Claude permission mode')
    if 'thinking' in value and not isinstance(value['thinking'], bool):
        raise ValueError('Claude thinking must be true or false')
    window = value.get('autoCompactWindow')
    if window is not None and (type(window) is not int or not 100000 <= window <= 1000000):
        raise ValueError('Claude auto-compact limit must be an integer from 100000 to 1000000')
    return dict(value)


def _pending(db, key):
    return db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND status IN ('pending','reserved','dispatching','uncertain') LIMIT 1", (key,)).fetchone()


def action(rt, body):
    key, kind = body.get('id'), body.get('action')
    if kind not in {'state', 'commands', 'command', 'settings', 'rollback', 'stop_task'}:
        raise ValueError('Unknown Claude action')
    with rt.lock, rt.db() as db:
        agent = _actor(rt, db, key)
        operations = [o for o in rt._workspace_operations(db, key) if o.get('kind', '').startswith('claude_')]
    if kind == 'state':
        state = {'settings': agent.get('claudeOptions', {}), 'turns': [], 'tasks': []}
        if agent.get('threadId'):
            state = rt.connect(agent.get('accountKey', 'default')).call('claude/state', {'threadId': agent['threadId']}, timeout=20)
        if operations:
            op = operations[0]
            state['controlOperation'] = {field: op.get(field) for field in ('requestId', 'turnId', 'phase', 'error')}
        return state
    if kind == 'commands':
        return rt.connect(agent.get('accountKey', 'default')).call('claude/commands', {'cwd': agent['cwd']}, timeout=25)
    if kind == 'command':
        command = body.get('command')
        if not isinstance(command, str) or not re.fullmatch(r'[/$][\w:.-]+(?:[ \t]+[^\r\n\x00]*)?', command.strip()):
            raise ValueError('Supply one explicit Claude command')
        request = body.get('request_id')
        if not isinstance(request, str) or not request:
            raise ValueError('Supply a command request identity')
        return rt.send(key, command.strip(), message_id=request, delivery='queue')
    if kind == 'stop_task':
        if not agent.get('threadId') or not isinstance(body.get('task_id'), str) or not body['task_id']:
            raise ValueError('Select an active Claude task')
        return rt.connect(agent.get('accountKey', 'default')).call('claude/stopTask', {'threadId': agent['threadId'], 'taskId': body['task_id']}, timeout=20)
    settings = _settings(body.get('settings')) if kind == 'settings' else None
    request = body.get('request_id')
    if kind == 'rollback' and (not isinstance(request, str) or not request or not isinstance(body.get('turn_id'), str) or not body['turn_id']):
        raise ValueError('Supply a rollback request identity and turn identity')
    control_source = rt._workspace_source(agent)
    control_server = rt.connect(agent.get('accountKey', 'default')) if agent.get('threadId') else None
    signature = rt._workspace_operation_signature({'agent': key, 'action': kind, 'settings': settings, 'turn': body.get('turn_id')})
    with rt.lock, rt.db() as db:
        agent = _actor(rt, db, key)
        if rt._workspace_source(agent) != control_source:
            raise ValueError('Claude session changed before the action')
        if kind == 'settings' and not request:
            match = next((o for o in rt._workspace_operations(db, key) if o.get('kind') == 'claude_settings' and o.get('signature') == signature), None)
            request = match['requestId'] if match else str(uuid.uuid4())
        if not isinstance(request, str) or not request:
            raise ValueError('Supply a Claude control request identity')
        operation_id = 'claude-control:' + request
        running = rt.__dict__.setdefault('_claude_control_inflight', set())
        if operation_id in running:
            raise ValueError('This Claude action is still in progress')
        operation = rt._workspace_operation(db, operation_id)
        if operation:
            if operation.get('signature') != signature:
                raise ValueError('This request identity has different content')
            if operation['phase'] == 'completed':
                return operation['result']
            rt._assert_workspace_source(operation, agent)
            if operation['phase'] == 'failed':
                raise ValueError(operation.get('error') or 'This Claude action failed')
        else:
            if agent.get('accountTransferId') or agent.get('inFlight') or agent.get('status') in {'running', 'starting', 'approval'}:
                raise ValueError('Pause Claude before changing its session')
            rt._assert_workspace_idle(db, agent)
            if kind == 'rollback' and _pending(db, key):
                raise ValueError('Send or remove queued messages before rollback')
            if kind == 'rollback' and not agent.get('threadId'):
                raise ValueError('This Claude chat has no history')
            operation = {'id': operation_id, 'requestId': request, 'kind': 'claude_' + kind,
                         'agent': key, 'turnId': body.get('turn_id'), 'signature': signature,
                         'source': rt._workspace_source(agent), 'phase': 'provider_pending', 'created': time.time()}
            rt._put_workspace_operation(db, operation)
            agent.update(workspaceOperation='branch', workspaceReservationId=operation_id)
            rt.put(db, 'agents', agent)
        running.add(operation_id)
    submitted = bool(operation.get('submitted'))
    try:
        result = operation.get('provider')
        if result is None:
            if not agent.get('threadId'):
                result = {'settings': settings}
            else:
                server = control_server
                if not submitted:
                    state = server.call('claude/state', {'threadId': agent['threadId']}, timeout=20)
                    if state.get('tasks') or any(t.get('status') == 'inProgress' for t in state.get('turns', [])):
                        raise ValueError('Pause Claude and its background tasks first')
                    if kind == 'rollback':
                        queue = server.call('thread/queue/list', {'threadId': agent['threadId']}, timeout=20)
                        if queue.get('data', queue.get('items')):
                            raise ValueError('Wait for queued Claude input before rollback')
                        turns = state.get('turns', [])
                        index = next((i for i, turn in enumerate(turns) if turn['id'] == body['turn_id']), None)
                        if index is None:
                            raise ValueError('The selected Claude turn does not exist')
                        operation['removedTurns'] = [t['id'] for t in turns[index:]]
                        operation['retainedTurn'] = turns[index - 1]['id'] if index else None
                with rt.lock, rt.db() as db:
                    current = _actor(rt, db, key)
                    rt._assert_workspace_source(operation, current)
                    operation.update(submitted=True, updated=time.time())
                    rt._put_workspace_operation(db, operation)
                submitted = True
                params = {'threadId': agent['threadId']}
                params.update({'settings': settings} if kind == 'settings' else {'turnId': body['turn_id'], 'requestId': request})
                result = server.call('claude/settings' if kind == 'settings' else 'thread/rollback', params, timeout=25)
            if kind == 'rollback' and (result.get('threadId') != agent['threadId'] or not result.get('nativeId')):
                raise RuntimeError('Claude rollback returned an invalid receipt')
            operation.update(provider=result, phase='provider_ready', updated=time.time())
            with rt.lock, rt.db() as db:
                rt._put_workspace_operation(db, operation)
        with rt.lock, rt.db() as db:
            current = _actor(rt, db, key)
            rt._assert_workspace_source(operation, current)
            if kind == 'settings':
                current['claudeOptions'] = settings
                if 'permissionMode' in settings:
                    current['yoloMode'] = settings['permissionMode'] == 'bypassPermissions'
                rt.loaded.discard(key)
                if current.get('workspaceOperation') == 'branch_recovery':
                    current.update(status='paused', error=None)
            else:
                removed = set(operation['removedTurns'])
                for row in db.execute('SELECT id,record FROM runtime_items WHERE agent=?', (key,)).fetchall():
                    record = json.loads(row['record'])
                    if record.get('turnId') in removed and not record.get('afterRestore'):
                        record['afterRestore'] = operation_id
                        db.execute('UPDATE runtime_items SET record=? WHERE id=?', (json.dumps(record), row['id']))
                current.update(status='paused', autoWake=False, inFlight=False, turnId=None,
                               lastCompletedTurn=operation.get('retainedTurn'), activity=None, activeTools=[], error=None)
                rt.loaded.discard(key)
            current.update(workspaceOperation=None)
            current.pop('workspaceReservationId', None)
            rt.put(db, 'agents', current)
            operation.update(phase='completed', result=result, error=None, updated=time.time())
            rt._put_workspace_operation(db, operation)
        rt.changed.set()
        return result
    except Exception as error:
        if submitted or operation.get('provider') is not None:
            rt._require_workspace_recovery(operation_id, key, error)
        else:
            rt._finish_workspace_operation(operation_id, key, error=error)
            with rt.lock, rt.db() as db:
                current = rt.agent(key, db)
                if current.get('workspaceReservationId') == operation_id:
                    current.pop('workspaceReservationId', None)
                    rt.put(db, 'agents', current)
        rt.changed.set()
        raise
    finally:
        with rt.lock:
            running.discard(operation_id)


def retire_idle_bridge(rt, key, account, server):
    """Retire only an idle native process; Runtime.connect holds start_lock."""
    version = getattr(server, 'initialize_result', {}).get('capabilities', {}).get('claudeVersion')
    if version == 6 and getattr(server, 'provider_options', {}) == account.get('claudeOptions', {}):
        return False

    def eligible(db):
        if rt.servers.get(key) is not server or getattr(server, 'pending', {}):
            return False
        for name in ('callbacks', 'clock_replies'):
            queue = getattr(server, name, None)
            if queue is not None and not queue.empty():
                return False
        agents = [a for a in rt.records(db, 'agents') if a.get('accountKey', 'default') == key]
        if any(a.get('inFlight') or a.get('status') in {'running', 'starting', 'approval'} or a.get('activeTools') or a.get('workspaceOperation') for a in agents):
            return False
        if any(p.get('accountKey', 'default') == key and not p['future'].done() for p in rt.preparations.values()):
            return False
        ids = {a['id'] for a in agents}
        from codex_workspace import active_monitors, active_task_records
        if any(m.get('agent') in ids and m.get('status') in {'approval', 'starting', 'running'}
               for m in active_monitors(db)):
            return False
        if any(t.get('agent') in ids for t in active_task_records(db)):
            return False
        if any(r.get('status') == 'pending' and (r.get('agent') in ids or r.get('accountKey') == key) for r in rt.records(db, 'requests')):
            return False
        return agents

    with rt.lock, rt.db() as db:
        agents = eligible(db)
        if agents is False:
            return False
    if version in {2, 3, 4, 5, 6}:
        try:
            for agent in agents:
                if agent.get('threadId'):
                    state = server.call('claude/state', {'threadId': agent['threadId']}, timeout=5)
                    if state.get('tasks') or any(t.get('status') == 'inProgress' for t in state.get('turns', [])):
                        return False
                    native_queue = server.call('thread/queue/list', {'threadId': agent['threadId']}, timeout=5)
                    if native_queue.get('data', native_queue.get('items')):
                        return False
        except Exception:
            return False
    with rt.lock, rt.db() as db:
        if eligible(db) is False:
            return False
        rt.connection_ids[key] = 'retired:' + str(uuid.uuid4())
        rt.servers.pop(key, None)
        for agent in agents:
            rt.loaded.discard(agent['id'])
        if key == 'default':
            rt.server = None
    server.close()
    return True
