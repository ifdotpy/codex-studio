"""Lead-only worker inspection and reversible, guarded team cleanup."""
import time


def management_tools(tool, text):
    return [tool('orchestration_agent_manage',
        'Manage your own descendant workers. inspect returns archive blockers. recover reconciles an existing native turn without replaying input. '
        'archive hides an inactive worker and preserves its history and files; it refuses active or uncertain work. '
        'restore returns an archived worker paused; use orchestration_send to resume. list_archived is paged. Only the lead may use this tool.',
        {'action': {'type': 'string', 'enum': ['inspect', 'recover', 'archive', 'restore', 'list_archived']},
         'agent_id': text, 'reason': {'type': 'string', 'maxLength': 1000},
         'limit': {'type': 'integer', 'minimum': 1, 'maximum': 50}, 'cursor': text}, ['action'])]


def _authorize(rt, db, actor_id, epoch, target=None):
    actor = rt.agent(actor_id, db)
    if (not actor.get('isLead') or actor.get('deletedAt') or not actor.get('autoWake')
            or (epoch is not None and actor['epoch'] != epoch)):
        raise ValueError('Only the active orchestrator can manage its workers')
    if target is not None:
        if target.get('isLead') or target['id'] == actor_id or target['rootId'] != actor['rootId']:
            raise ValueError('You can manage only your own descendant workers')
        seen = set()
        cursor = target
        while cursor.get('parentId') != actor_id:
            if not cursor.get('parentId') or cursor['id'] in seen:
                raise ValueError('This worker is not your descendant')
            seen.add(cursor['id'])
            cursor = rt.agent(cursor['parentId'], db)
    return actor


def _brief(a):
    return {k: a.get(k) for k in ('id', 'name', 'parentId', 'status', 'inFlight', 'threadId',
                                 'turnId', 'error', 'lastEvent', 'agentArchive')}


def _blockers(rt, db, a):
    key = a['id']
    result = []
    def add(kind, ids):
        if ids: result.append({'kind': kind, 'count': len(ids), 'ids': ids[:20]})
    if a.get('inFlight') or a['status'] in {'starting', 'running', 'approval', 'queued'}:
        add('active_turn', [key])
    if a.get('workspaceOperation'): add('workspace_operation', [key])
    preparation = getattr(rt, 'preparations', {}).get(key)
    if preparation and not preparation['future'].done(): add('thread_preparation', [key])
    add('descendants', [x['id'] for x in rt.records(db, 'agents') if x.get('parentId') == key and not x.get('deletedAt')])
    add('input_delivery', [r[0] for r in db.execute(
        "SELECT id FROM runtime_events WHERE agent=? AND epoch=? AND status IN ('pending','reserved','dispatching','uncertain')", (key, a['epoch']))])
    add('monitors', [x['id'] for x in rt.records(db, 'monitors') if x.get('agent') == key and x.get('status') in {'starting','running','approval'}])
    add('background_tasks', [x['id'] for x in rt.records(db, 'tasks') if x.get('agent') == key and x.get('status') in {'running','starting','pending','unknown'}])
    add('questions', [x['id'] for x in rt.records(db, 'requests') if x.get('agent') == key and x.get('status') == 'pending'])
    add('assigned_work', [x['id'] for x in rt.records(db, 'work') if x.get('owner') == key and x.get('status') not in {'accepted','cancelled'}])
    # Older live servers can lack the request ledger. Their input/turn guards still apply.
    if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_tool_requests'").fetchone():
        if hasattr(rt, 'reconcile_tool_requests'):
            rt.reconcile_tool_requests(db, key)
        add('tool_requests', [row[0] for row in db.execute(
            "SELECT id FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
            "AND (json_extract(record,'$.stage') IN ('queued','running') OR json_extract(record,'$.outcome')='unknown')",
            (key,))])
    board = rt.resource_action().get('state', {})
    add('resource_claims', [name for name, claim in board.get('claims', {}).items() if claim.get('worker') == key])
    add('resource_queue', [name for name, waiters in board.get('queue', {}).items() if any(w.get('worker') == key for w in waiters)])
    return result


def manage_agent(rt, actor_id, args, epoch=None):
    action = args.get('action')
    if action not in {'inspect','recover','archive','restore','list_archived'}:
        raise ValueError('Choose inspect, recover, archive, restore, or list_archived')
    observed = None
    if action == 'archive':
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)
            _authorize(rt, db, actor_id, epoch, target)
            if not target.get('deletedAt'):
                blockers = _blockers(rt, db, target)
                if blockers: return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
                observed = (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                connection = getattr(rt, 'connection_ids', {}).get(observed[2])
                server = getattr(rt, 'servers', {}).get(observed[2])
        if observed and observed[1]:
            try:
                if server is None: raise ValueError('Owning account is offline')
                native = server.call('thread/read', {'threadId': observed[1], 'includeTurns': False}, timeout=5)['thread']
                if native.get('id') != observed[1] or native.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
                    raise ValueError('Native thread is active or its status is unknown')
            except Exception:
                return {'status': 'blocked', 'agent': _brief(target),
                        'blockers': [{'kind': 'native_state_unconfirmed', 'count': 1, 'ids': [target['id']]}]}
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        if action == 'list_archived':
            rows = [_brief(a) for a in rt.records(db, 'agents')
                    if a['rootId'] == actor['rootId'] and a.get('agentArchive') and a.get('deletedAt')]
            return rt.model_page(sorted(rows, key=lambda x: x['id']), args, [actor['rootId'], 'archived_workers'])
        target = rt.agent(args.get('agent_id'), db)
        _authorize(rt, db, actor_id, epoch, target)
        archived = target.get('agentArchive')
        if target.get('deletedAt') and not archived:
            raise ValueError('This worker was deleted, not archived')
        if action == 'archive' and archived:
            return {'status': 'archived', 'agent': _brief(target), 'replayed': True}
        if action == 'restore':
            if not archived:
                return {'status': 'not_archived', 'agent': _brief(target)}
            if target.get('deletedAt') != archived['at'] or target['epoch'] != archived['epoch']:
                raise ValueError('The archived worker changed; restoration requires inspection')
            parent = rt.agent(target['parentId'], db)
            if parent.get('deletedAt'):
                raise ValueError('Restore the parent first')
            root = rt.agent(target['rootId'], db)
            count = sum(a['rootId'] == root['id'] and not a.get('deletedAt') for a in rt.records(db, 'agents'))
            if count >= root.get('maxAgents', 64):
                raise ValueError('The team is at its agent limit')
            target.pop('deletedAt', None)
            target.pop('agentArchive', None)
            target.update(status='paused', autoWake=False, epoch=target['epoch'] + 1)
            rt.put(db, 'agents', target)
            return {'status': 'restored', 'agent': _brief(target), 'next': 'Use orchestration_send to resume with an instruction'}
        if action == 'inspect':
            blockers = [] if archived else _blockers(rt, db, target)
            return {'agent': _brief(target), 'canArchive': not archived and not blockers,
                    'blockers': blockers, 'schedulerAlive': getattr(rt, 'scheduler', None).is_alive() if getattr(rt, 'scheduler', None) else None}
        if archived: raise ValueError('Restore this worker before recovery')
        request_recovery = (rt.reconcile_tool_requests(db, target['id'])
                            if action == 'recover' and hasattr(rt, 'reconcile_tool_requests') else None)
        if action == 'archive':
            if (observed != (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                    or connection != getattr(rt, 'connection_ids', {}).get(observed[2])):
                raise ValueError('Worker changed during native inspection; inspect it again')
            reason = args.get('reason', '')
            if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
                raise ValueError('Give an archive reason with 1 to 1000 characters')
            blockers = _blockers(rt, db, target)
            if blockers:
                return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
            at = time.time()
            target.update(deletedAt=at, autoWake=False, status='paused', epoch=target['epoch'] + 1)
            target['agentArchive'] = {'at': at, 'by': actor_id, 'reason': reason.strip(), 'epoch': target['epoch']}
            rt.put(db, 'agents', target)
            return {'status': 'archived', 'agent': _brief(target)}
    # Native reads must not hold the runtime lock or a SQLite transaction.
    result = rt.reconcile_turn(target['id'])
    return {'status': 'checked', 'recovery': result, 'requests': request_recovery,
            'next': 'Inspect the worker. Use orchestration_send for an explicit continuation; never replay unknown mutations'}
