"""User-confirmed conversion of an idle peer lead and its tree into workers."""
import json
import time

from codex_agent_modes import assert_delegation
from codex_peer_teams import _lead, _members, _teams
from codex_work import text_field


# Root ownership fields follow the schemas in WorkMixin and RulesMixin.
MOVE_FIELDS = {
    'work': ('rootId',), 'plans': ('rootId',), 'annotations': ('rootId',),
    'complaints': ('leadId',), 'event_meta': ('rootId', 'leadId'), 'rules': ('rootId',),
}
INDEXES = {
    'requests': [('runtime_request_agent_status', "json_extract(record,'$.agent'),json_extract(record,'$.status')", None)],
    'rules': [('runtime_rule_agent_inflight', "json_extract(record,'$.agent'),json_extract(record,'$.inFlight')", None)],
    'workspace_operations': [('runtime_workspace_operation_agent_phase', "json_extract(record,'$.agent'),json_extract(record,'$.phase')", None)],
    'account_transfers': [('runtime_account_transfer_status', "json_extract(record,'$.status')", None)],
    'rooms': [('runtime_room_root', "json_extract(record,'$.rootId')", None),
              ('runtime_room_member_first', "json_extract(record,'$.members[0]')", None),
              ('runtime_room_member_second', "json_extract(record,'$.members[1]')", None),
              ('runtime_room_many_members', "json_array_length(record,'$.members')",
               "json_array_length(record,'$.members')>2")],
}
for _table, _fields in MOVE_FIELDS.items():
    for _field in _fields:
        if _table == 'work' and _field == 'rootId':
            continue  # runtime_work_root_status already has this prefix.
        _expression = "json_extract(record,'$." + _field + "')"
        INDEXES.setdefault(_table, []).append((
            'runtime_conversion_' + _table + '_' + _field, _expression,
            _expression + ' IS NOT NULL'))


def setup_indexes(db):
    """Run once at startup, before user actions and the scheduler."""
    tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    for table, indexes in INDEXES.items():
        if 'runtime_' + table not in tables:
            continue
        for name, expression, predicate in indexes:
            db.execute('CREATE INDEX IF NOT EXISTS ' + name + ' ON runtime_' + table + '(' + expression + ')'
                       + (' WHERE ' + predicate if predicate else ''))


def _marks(ids):
    return ','.join('?' * len(ids))


def busy_query(table, ids):
    predicate = "json_extract(record,'$.agent') IN (" + _marks(ids) + ')'
    predicates = {
        'tasks': "json_extract(record,'$.status') IN ('running','starting','approval')",
        'monitors': "json_extract(record,'$.status') IN ('running','starting','approval')",
        'requests': "json_extract(record,'$.status')='pending'",
        'tool_requests': "COALESCE(json_extract(record,'$.readOnly'),0)=0 AND json_extract(record,'$.outcome') IN ('pending','unknown')",
        'rules': "json_extract(record,'$.inFlight')=1",
    }
    index = {'tasks': 'runtime_task_agent_created_id', 'monitors': 'runtime_monitor_agent_status'}.get(table)
    return ('SELECT 1 FROM runtime_' + table + (' INDEXED BY ' + index if index else '')
            + ' WHERE ' + predicate + ' AND ' + predicates[table] + ' LIMIT 1', tuple(ids))


def move_query(table, source_id):
    return ('SELECT record FROM runtime_' + table + ' WHERE ' + ' OR '.join(
        "json_extract(record,'$." + field + "')=?" for field in MOVE_FIELDS[table]),
        (source_id,) * len(MOVE_FIELDS[table]))


def rooms_query(roots, ids, radio=False):
    # Pair rooms use member indexes. Longer saved rooms use a narrow partial
    # index, then check every member so no historical membership is omitted.
    predicates, params = [], []
    if roots:
        predicates.append("json_extract(record,'$.rootId') IN (" + _marks(roots) + ')')
        params.extend(roots)
    if ids:
        for field in ('members[0]', 'members[1]'):
            predicates.append("json_extract(record,'$." + field + "') IN (" + _marks(ids) + ')')
            params.extend(ids)
    radio_filter = " AND json_type(record,'$.radio')='object'" if radio else ''
    query = 'SELECT record FROM runtime_rooms WHERE (' + ' OR '.join(predicates) + ')' + radio_filter
    if ids:
        query += (" UNION SELECT record FROM runtime_rooms WHERE json_array_length(record,'$.members')>2"
                  " AND EXISTS(SELECT 1 FROM json_each(record,'$.members') WHERE value IN ("
                  + _marks(ids) + '))' + radio_filter)
        params.extend(ids)
    return query, tuple(params)


def _query_records(db, query):
    return [json.loads(row[0]) for row in db.execute(*query)]


def _idle(runtime, db, agents, tables):
    ids = {agent['id'] for agent in agents}
    for agent in agents:
        name = agent.get('name') or agent['id']
        if (agent.get('inFlight') or agent.get('turnId')
                or agent.get('status') in {'running', 'starting', 'approval'}
                or (agent.get('status') == 'queued' and agent.get('autoWake'))
                or agent.get('startAttempt') or agent.get('prepareAttempt')):
            raise ValueError(name + ': finish the active or queued turn before moving')
        if agent.get('workspaceOperation') or runtime._workspace_operations(db, agent['id']):
            raise ValueError(name + ': finish the pending workspace operation before moving')
        if (agent.get('accountTransferId') or agent.get('lazyAccountTransfer')
                or (agent.get('accountTransfer') or {}).get('status') == 'pending'):
            raise ValueError(name + ': finish the account transfer before moving')
        release = agent.get('nativeRelease') or {}
        if release.get('phase') in {'pending', 'checking', 'releasing'}:
            raise ValueError(name + ': finish the native release before moving')
    marks = ','.join('?' * len(ids))
    if db.execute('SELECT 1 FROM runtime_events WHERE agent IN (' + marks + ') '
                  "AND status IN ('pending','reserved','dispatching','uncertain') LIMIT 1", tuple(ids)).fetchone():
        raise ValueError('Finish pending input before moving these chats')
    for table, reason in (
            ('tasks', 'Finish active commands and monitors before moving these chats'),
            ('monitors', 'Finish active commands and monitors before moving these chats'),
            ('requests', 'Answer pending permissions and questions before moving these chats'),
            ('tool_requests', 'Resolve the pending or unknown tool request before moving these chats'),
            ('rules', 'Finish the active watch check before moving these chats')):
        if table in tables and db.execute(*busy_query(table, ids)).fetchone():
            raise ValueError(reason)
    if 'account_transfers' in tables:
        # Only pending transfers can reserve a chat. Completed transfer history
        # never crosses the Python/SQLite boundary here.
        for row in db.execute("SELECT record FROM runtime_account_transfers WHERE json_extract(record,'$.status')='pending'"):
            op = json.loads(row[0])
            if op.get('leadId') in ids or ids.intersection(op.get('members', {})):
                raise ValueError('Finish the account transfer before moving these chats')
    for room in _query_records(db, rooms_query((), ids, radio=True)):
        radio = room['radio']
        if radio.get('active') or radio.get('next'):
            raise ValueError('Finish or stop the shared chat exchange before moving')


def convert(runtime, data):
    # This action is exposed only by the token/origin-checked user HTTP route.
    # Model tools cannot change another root's ownership.
    allowed = {'action', 'path', 'member', 'target', 'expected_revision', 'request_id'}
    if set(data) - allowed:
        raise ValueError('Unknown chat conversion field')
    path = runtime.project_directory(data.get('path'), require_existing=False)
    source_id = text_field(data.get('member'), 'a source chat ID', 255)
    target_id = text_field(data.get('target'), 'a destination lead chat ID', 255)
    request_id = text_field(data.get('request_id'), 'a request ID', 255)
    revision = data.get('expected_revision')
    if type(revision) is not int or revision < 0:
        raise ValueError('Supply the current peer team revision')
    if source_id == target_id:
        raise ValueError('Select two different chats')
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        receipt = 'peer-convert:' + request_id
        signature, previous = runtime.operation_receipt(db, receipt, {'operation': 'peer-convert', 'body': data})
        if previous is not None:
            return previous
        source = runtime.checked_actor(db, source_id)
        target = runtime.checked_actor(db, target_id)
        if any(not _lead(a, path) or a.get('sharedRoomId') or a.get('archived') for a in (source, target)):
            raise ValueError('Select two live lead chats from this project only')
        assert_delegation(target)
        project = runtime.ensure_project(path, runtime.project_account(path, db=db), db)
        if project.get('peerTeamsRevision', 0) != revision:
            raise ValueError('Peer teams changed. Reload before moving')
        peers = _members(db, (key for team in project.get('peerTeams', []) for key in team['members']))
        teams = _teams(project, peers)
        if not any(source_id in team['members'] for team in teams):
            raise ValueError('The source chat must belong to a peer team')
        agents = _query_records(db, ("SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId') IN (?,?)", (source_id, target_id)))
        moving = [a for a in agents if a.get('rootId') == source_id]
        receiving = [a for a in agents if a.get('rootId') == target_id]
        tables = {row[0].removeprefix('runtime_') for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'runtime_%'")}
        _idle(runtime, db, moving + receiving, tables)
        affected_rooms = _query_records(db, rooms_query((source_id, target_id),
            {a['id'] for a in moving} | {key for team in teams for key in team['members']}))
        at = time.time()
        project['peerTeams'] = [dict(id=t['id'], name=t['name'], members=[m for m in t['members'] if m != source_id])
                                for t in teams if len([m for m in t['members'] if m != source_id]) >= 2]
        project.update(peerTeamsRevision=revision + 1, updated=at)
        runtime.put(db, 'projects', project)
        for agent in moving:
            agent.update(rootId=target_id, updated=at)
            agent.pop('concurrency', None)
            for field in ('maxAgents', 'tokenBudget'):
                if field in target:
                    agent[field] = target[field]
            if agent['id'] == source_id:
                agent.update(isLead=False, parentId=target_id, role='implementer', status='paused', autoWake=False,
                             epoch=agent.get('epoch', 0) + 1, worktree=False)
                for field in ('subagentConcurrencyVersion', 'maxAgentsExplicit'):
                    agent.pop(field, None)
                agent['convertedFromLead'] = {'requestId': request_id, 'by': 'user', 'at': at,
                                              'oldRootId': source_id, 'rootId': target_id}
                # Existing account, native thread, model and execution settings stay intact.
                for field in ('agentMode', 'agentModeRevision', 'agentModeSupported', 'projectFolder', 'pinned'):
                    agent.pop(field, None)
            runtime.put(db, 'agents', agent, sync_rooms=False)
        # Move the complete task board, dependencies, plans and requests for the user.
        # Stable agent IDs retain command/monitor ownership and progress file paths.
        for table in MOVE_FIELDS:
            if table not in tables:
                continue
            for record in _query_records(db, move_query(table, source_id)):
                changed = False
                for field in MOVE_FIELDS[table]:
                    if record.get(field) == source_id:
                        record[field] = target_id
                        changed = True
                if changed:
                    if table == 'complaints' and 'leadName' in record:
                        record['leadName'] = target.get('name') or target_id
                    if table == 'rules' and record.get('agent') == source_id:
                        record.update(status='paused', epoch=source.get('epoch', 0) + 1,
                                      error='The lead became a subagent. Resume this watch explicitly.')
                    runtime.put(db, table, record)
        for room in affected_rooms:
            if room.get('rootId') != source_id:
                continue
            room.update(rootId=target_id, updated=at)
            if room.get('kind') == 'broadcast':
                # Keep the old broadcast transcript private to the original tree.
                # New broadcasts use the destination's canonical room.
                room.update(kind='private', members=[a['id'] for a in moving if not a.get('deletedAt')],
                            customName=(source.get('name') or source_id) + ' (previous broadcast)')
            runtime.put(db, 'rooms', room)
        # Project membership and dynamic room projections need explicit tombstones.
        from codex_sync_entities import put as sync_put
        for team in teams:
            updated = next((t for t in project['peerTeams'] if t['id'] == team['id']), None)
            sync_put(db, 'peerTeam', team['id'], dict(updated, projectPath=path) if updated else {}, updated is None)
        for room in affected_rooms:
            visible = next(iter(runtime.chat_rooms(db, room_id=room['id'])), None)
            sync_put(db, 'room', room['id'], visible or {}, visible is None)
        result = {'id': source_id, 'parentId': target_id, 'rootId': target_id,
                  'movedAgents': [a['id'] for a in moving], 'peerTeamsRevision': revision + 1}
        runtime.save_receipt(db, receipt, signature, result)
    runtime.changed.set()
    return result
