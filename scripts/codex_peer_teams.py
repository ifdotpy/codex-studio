"""Equal lead chat groups. Membership grants communication, never work access."""

import json
import time
import uuid
from pathlib import Path

from codex_work import text_field


def _path(value):
    if not isinstance(value, str) or not value.strip():
        return None
    return str(Path(value).expanduser().resolve())


def _lead(agent, path):
    return (agent.get('isLead') is True and not agent.get('deletedAt')
            and agent.get('parentId') is None and agent.get('rootId') == agent.get('id')
            and _path(agent.get('cwd')) == path)


def _agents(db):
    return {row['id']: row for row in
            (json.loads(item[0]) for item in db.execute('SELECT record FROM runtime_agents'))}


def _teams(project, agents):
    path = _path(project.get('path'))
    if path is None:
        return []
    result = []
    for team in project.get('peerTeams', []):
        members = [key for key in team['members'] if key in agents and _lead(agents[key], path)]
        if len(set(members)) >= 2:
            result.append({'id': team['id'], 'name': team['name'], 'projectPath': path,
                           'members': list(dict.fromkeys(members)),
                           'revision': project.get('peerTeamsRevision', 0)})
    return result


def snapshot(runtime, db):
    agents = _agents(db)
    return [team for row in db.execute('SELECT record FROM runtime_projects')
            for team in _teams(json.loads(row[0]), agents)]


def peer_pair_allowed(db, left_id, right_id):
    if left_id == right_id:
        return False
    rows = db.execute('SELECT record FROM runtime_agents WHERE id IN (?,?)',
                      (left_id, right_id)).fetchall()
    agents = {agent['id']: agent for agent in (json.loads(row[0]) for row in rows)}
    left, right = agents.get(left_id), agents.get(right_id)
    if not left or not right:
        return False
    path = _path(left.get('cwd'))
    if path is None or not _lead(left, path) or not _lead(right, path):
        return False
    row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (path,)).fetchone()
    if not row:
        return False
    return any(left_id in team['members'] and right_id in team['members']
               for team in _teams(json.loads(row[0]), agents))


def peers_for(runtime, db, viewer):
    viewer_id = viewer.get('id') if isinstance(viewer, dict) else viewer
    agents = _agents(db)
    ids = {key for team in snapshot(runtime, db) if viewer_id in team['members']
           for key in team['members'] if key != viewer_id}
    return [agents[key] for key in sorted(ids)]


def manage(runtime, data):
    action = data.get('action')
    if action not in ('save', 'delete', 'move'):
        raise ValueError('Unknown peer team action')
    path = runtime.project_directory(data.get('path'), require_existing=False)
    revision = data.get('expected_revision')
    if type(revision) is not int or revision < 0:
        raise ValueError('Supply the current peer team revision')
    try:
        team_id = (None if action == 'move' and data.get('team_id') is None
                   else str(uuid.UUID(data.get('team_id', ''))))
    except (ValueError, TypeError, AttributeError):
        raise ValueError('Supply a team ID') from None
    request_id = text_field(data.get('request_id'), 'a request ID', 255)
    name, members = None, None
    if action == 'save':
        name = text_field(data.get('name'), 'a team name', 80)
        members = data.get('members')
        if (not isinstance(members, list) or len(members) < 2
                or any(not isinstance(key, str) or not key for key in members)
                or len(set(members)) != len(members)):
            raise ValueError('Select at least two different lead chats')
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        signature, previous = runtime.operation_receipt(
            db, 'peer-team:' + request_id, {'operation': 'peer-team', 'body': data})
        if previous is not None:
            return previous
        project = runtime.ensure_project(path, runtime.project_account(path, db=db), db)
        current = project.get('peerTeamsRevision', 0)
        if revision != current:
            raise ValueError('Peer teams changed. Reload before saving')
        agents = _agents(db)
        teams = [{key: team[key] for key in ('id', 'name', 'members')}
                 for team in _teams(project, agents)]
        if action == 'move':
            member = text_field(data.get('member'), 'a lead chat ID', 255)
            if member not in agents or not _lead(agents[member], path):
                raise ValueError('Select a live lead chat from this project only')
            target = next((team for team in teams if team['id'] == team_id), None)
            if team_id is not None and target is None:
                raise ValueError('The destination team is no longer available')
            source = next((team for team in teams if member in team['members']), None)
            if source is target:
                raise ValueError('The chat already has this team membership')
            if source:
                source['members'].remove(member)
            if target:
                target['members'].append(member)
            teams = [team for team in teams if len(team['members']) >= 2]
        elif action == 'save':
            for key in members:
                if key not in agents or not _lead(agents[key], path):
                    raise ValueError('Select live lead chats from this project only')
            for team in teams:
                if team['id'] != team_id and set(team['members']).intersection(members):
                    raise ValueError('A chat already belongs to another peer team')
            replacement = {'id': team_id, 'name': name, 'members': list(members)}
            teams = [replacement if team['id'] == team_id else team for team in teams]
            if not any(team['id'] == team_id for team in teams):
                teams.append(replacement)
        else:
            teams = [team for team in teams if team['id'] != team_id]
        project.update(peerTeams=teams, peerTeamsRevision=current + 1, updated=time.time())
        runtime.put(db, 'projects', project)
        return runtime.save_receipt(db, 'peer-team:' + request_id, signature, project)
