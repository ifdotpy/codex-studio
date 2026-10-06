"""Equal lead chat groups. Membership grants communication, never work access."""

import json
import time
import uuid
from pathlib import Path
import sqlite3
from typing import TYPE_CHECKING, Any
from collections.abc import Iterable, Mapping

from codex_records import AgentRecord, PeerTeamRecord, ProjectRecord, RecordStore
from codex_work import text_field

if TYPE_CHECKING:
    from codex_runtime import Runtime


def _path(value: object) -> str | None:
    if not isinstance(value, str) or not value.strip():
        return None
    return str(Path(value).expanduser().resolve())


def _lead(agent: AgentRecord, path: str | Path) -> bool:
    return (agent.get('isLead') is True and not agent.get('deletedAt')
            and agent.get('parentId') is None and agent.get('rootId') == agent.get('id')
            and _path(agent.get('cwd')) == path)


def _agents(db: sqlite3.Connection) -> dict[str, AgentRecord]:
    return {row['id']: row for row in
            (json.loads(item[0]) for item in db.execute('SELECT record FROM runtime_agents'))}


def _members(db: sqlite3.Connection, ids: Iterable[str]) -> dict[str, AgentRecord]:
    # Snapshots run under the shared runtime lock. Decode only team members,
    # not every agent record.
    ids = sorted(set(ids))
    if not ids:
        return {}
    rows = db.execute('SELECT record FROM runtime_agents WHERE id IN (' + ','.join('?' * len(ids)) + ')', ids)
    return {row['id']: row for row in (json.loads(item[0]) for item in rows)}


def _teams(project: ProjectRecord, agents: dict[str, AgentRecord]) -> list[PeerTeamRecord]:
    path = _path(project.get('path'))
    if path is None:
        return []
    result: list[PeerTeamRecord] = []
    for team in project.get('peerTeams', []):
        members = [key for key in team['members'] if key in agents and _lead(agents[key], path)]
        if len(set(members)) >= 2:
            result.append({'id': team['id'], 'name': team['name'], 'projectPath': path,
                           'members': list(dict.fromkeys(members)),
                           'revision': project.get('peerTeamsRevision', 0)})
    return result


def snapshot(runtime: RecordStore, db: sqlite3.Connection) -> list[PeerTeamRecord]:
    projects = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_projects')]
    agents = _members(db, (key for project in projects for team in project.get('peerTeams', [])
                           for key in team['members']))
    return [team for project in projects for team in _teams(project, agents)]


def sync_entities(runtime: RecordStore, db: sqlite3.Connection,
                  project_paths: set[str] | None = None) -> int:
    """Keep durable peer-team rows equal to the live snapshot projection."""
    from codex_sync_entities import put as sync_entity_put

    paths = {_path(value) for value in project_paths or ()} if project_paths is not None else None
    if paths is None:
        current = {team['id']: team for team in snapshot(runtime, db)}
        rows = db.execute("SELECT id,deleted,payload FROM sync_entities WHERE collection='peerTeam'").fetchall()
    else:
        current = {}
        for path in paths - {None}:
            row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
            if not row:
                continue
            project = json.loads(row[0])
            agents = _members(db, (member for team in project.get('peerTeams', [])
                                   for member in team.get('members', [])))
            for team in _teams(project, agents):
                current[team['id']] = team
        rows = db.execute("SELECT id,deleted,payload FROM sync_entities WHERE collection='peerTeam' "
                          "AND json_extract(payload,'$.value.projectPath') IN (" +
                          (','.join('?' for _ in paths - {None}) or "NULL") + ")",
                          tuple(sorted(paths - {None}))).fetchall() if paths - {None} else []  # type: ignore[type-var]  # typed-narrowing: only non-None normalized paths reach SQL parameters
    existing = {row[0]: (bool(row[1]), json.loads(row[2]).get('value', {}).get('projectPath')
                          if row[2] else None) for row in rows}
    for team_id, team in current.items():
        sync_entity_put(db, 'peerTeam', team_id, team)
    target_paths = paths if paths is not None else None
    for team_id, (deleted, project_path) in existing.items():
        if team_id not in current and not deleted and (
                target_paths is None or _path(project_path) in target_paths):
            sync_entity_put(db, 'peerTeam', team_id, {}, deleted=True)
    return len(current)


def peer_pair_allowed(db: sqlite3.Connection, left_id: str, right_id: str) -> bool:
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


def peers_for(runtime: RecordStore, db: sqlite3.Connection,
              viewer: Mapping[str, object] | str) -> list[AgentRecord]:
    viewer_id = viewer.get('id') if isinstance(viewer, dict) else viewer
    ids = {key for team in snapshot(runtime, db) if viewer_id in team['members']
           for key in team['members'] if key != viewer_id}
    agents = _members(db, ids)
    return [agents[key] for key in sorted(ids)]


def manage(runtime: "Runtime", data: Any) -> Any:
    action = data.get('action')
    if action == 'convert':
        from codex_peer_conversion import convert
        return convert(runtime, data)
    if action == 'radio':
        from codex_radio import manage as manage_radio
        return manage_radio(runtime, data)
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
    name, members = None, None  # type: tuple[str | None, list[str] | None]
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
        previous_team_ids = {team.get('id') for team in project.get('peerTeams', []) if team.get('id')}
        if revision != current:
            raise ValueError('Peer teams changed. Reload before saving')
        agents = _agents(db)
        teams: list[PeerTeamRecord] = [{key: team[key] for key in ('id', 'name', 'members')} for team in _teams(project, agents)]  # type: ignore[misc]  # typed-narrowing: these selected keys form the persisted peer team record
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
            for key in members:  # type: ignore[union-attr]  # typed-narrowing: the save branch validates members as a list
                if key not in agents or not _lead(agents[key], path):
                    raise ValueError('Select live lead chats from this project only')
            for team in teams:
                if team['id'] != team_id and set(team['members']).intersection(members):  # type: ignore[arg-type]  # typed-narrowing: the save branch validates members as a list
                    raise ValueError('A chat already belongs to another peer team')
            replacement = {'id': team_id, 'name': name, 'members': list(members)}  # type: ignore[arg-type]  # typed-narrowing: validated save fields are strings
            teams = [replacement if team['id'] == team_id else team for team in teams]  # type: ignore[misc]  # typed-narrowing: validated save fields form a peer team record
            if not any(team['id'] == team_id for team in teams):
                teams.append(replacement)  # type: ignore[arg-type]  # typed-narrowing: validated save fields form a peer team record
        else:
            teams = [team for team in teams if team['id'] != team_id]
        project.update(peerTeams=teams, peerTeamsRevision=current + 1, updated=time.time())
        runtime.put(db, 'projects', project)
        sync_entities(runtime, db, {path})
        return runtime.save_receipt(db, 'peer-team:' + request_id, signature, project)
