"""User-confirmed conversion of an idle peer lead and its tree into workers."""
import json
import time

from codex_agent_modes import assert_delegation
from codex_peer_teams import _agents, _lead, _teams
from codex_work import text_field


def _records(db, table):
    return [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_' + table)]


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
    for table in ('tasks', 'monitors'):
        if any(row.get('agent') in ids and row.get('status') in {'running', 'starting', 'approval'}
               for row in _records(db, table)):
            raise ValueError('Finish active commands and monitors before moving these chats')
    if any(row.get('agent') in ids and row.get('status') == 'pending'
           for row in _records(db, 'requests')):
        raise ValueError('Answer pending permissions and questions before moving these chats')
    if 'tool_requests' in tables and any(
            row.get('agent') in ids and not row.get('readOnly') and row.get('outcome') in {'pending', 'unknown'}
            for row in _records(db, 'tool_requests')):
        raise ValueError('Resolve the pending or unknown tool request before moving these chats')
    if 'account_transfers' in tables and any(
            row.get('status') == 'pending' and (row.get('leadId') in ids or ids.intersection(row.get('members', {})))
            for row in _records(db, 'account_transfers')):
        raise ValueError('Finish the account transfer before moving these chats')
    if 'rules' in tables and any(row.get('agent') in ids and row.get('inFlight')
                                for row in _records(db, 'rules')):
        raise ValueError('Finish the active watch check before moving these chats')
    for room in _records(db, 'rooms'):
        radio = room.get('radio') or {}
        if ids.intersection(room.get('members', [])) and (radio.get('active') or radio.get('next')):
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
        agents = _agents(db)
        teams = _teams(project, agents)
        if not any(source_id in team['members'] for team in teams):
            raise ValueError('The source chat must belong to a peer team')
        moving = [a for a in agents.values() if a.get('rootId') == source_id]
        receiving = [a for a in agents.values() if a.get('rootId') == target_id]
        tables = {row[0].removeprefix('runtime_') for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'runtime_%'")}
        _idle(runtime, db, moving + receiving, tables)
        at = time.time()
        project['peerTeams'] = [dict(id=t['id'], name=t['name'], members=[m for m in t['members'] if m != source_id])
                                for t in teams if len([m for m in t['members'] if m != source_id]) >= 2]
        project.update(peerTeamsRevision=revision + 1, updated=at)
        runtime.put(db, 'projects', project)
        for agent in moving:
            agent.update(rootId=target_id, updated=at)
            for field in ('concurrency', 'maxAgents', 'tokenBudget'):
                if field in target:
                    agent[field] = target[field]
            if agent['id'] == source_id:
                agent.update(isLead=False, parentId=target_id, role='implementer', status='paused', autoWake=False,
                             epoch=agent.get('epoch', 0) + 1, worktree=False)
                agent['convertedFromLead'] = {'requestId': request_id, 'by': 'user', 'at': at,
                                              'oldRootId': source_id, 'rootId': target_id}
                # Existing account, native thread, model and execution settings stay intact.
                for field in ('agentMode', 'agentModeRevision', 'agentModeSupported', 'projectFolder', 'pinned'):
                    agent.pop(field, None)
            runtime.put(db, 'agents', agent)
        # Move the complete task board, dependencies, plans and requests for the user.
        # Stable agent IDs retain command/monitor ownership and progress file paths.
        for table in ('work', 'plans', 'annotations', 'complaints', 'event_meta', 'rules'):
            if table not in tables:
                continue
            for record in _records(db, table):
                changed = False
                for field in ('rootId', 'leadId'):
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
        for room in _records(db, 'rooms'):
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
        visible_rooms = {room['id']: room for room in runtime.chat_rooms(db)}
        for room in _records(db, 'rooms'):
            sync_put(db, 'room', room['id'], visible_rooms.get(room['id'], {}), room['id'] not in visible_rooms)
        result = {'id': source_id, 'parentId': target_id, 'rootId': target_id,
                  'movedAgents': [a['id'] for a in moving], 'peerTeamsRevision': revision + 1}
        runtime.save_receipt(db, receipt, signature, result)
    runtime.changed.set()
    return result
