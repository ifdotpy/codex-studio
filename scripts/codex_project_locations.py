"""Project locations and destination registrations over the paired server channel."""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import sqlite3
import time
import uuid
from typing import Any, cast
from urllib.parse import urlsplit

from codex_work import text_field


def local_server(runtime: Any) -> str:
    service = runtime.__dict__.get('_cross_server_service')
    return str(service.server_id if service else runtime._project_server_id)


def normalize(runtime: Any, project: dict[str, Any]) -> None:
    """Keep legacy IDs, account choices, folders and chat paths unchanged."""
    if 'locations' not in project:
        server = local_server(runtime)
        project['homeServerId'] = server
        project['locations'] = [{'serverId': server, 'path': project['path'], 'projectId': project['id']}]
        project['locationsRevision'] = 0


def migrate(runtime: Any) -> None:
    runtime._project_server_id = runtime.paired_access().local_server_id
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        for row in db.execute("SELECT record FROM runtime_projects WHERE json_type(record,'$.locations') IS NULL").fetchall():
            project = json.loads(row[0])
            normalize(runtime, project)
            runtime.put(db, 'projects', project)
        from codex_sync_entities import put as sync_put
        for row in db.execute("SELECT record FROM runtime_agents WHERE id NOT IN (SELECT id FROM sync_entities WHERE collection='agent' AND json_extract(payload,'$.value.serverId') IS NOT NULL)").fetchall():
            agent = json.loads(row[0])
            sync_put(db, 'agent', agent['id'], runtime.agent_entity_view(db, agent), bool(agent.get('deletedAt')))


def git_origin(path: str) -> str | None:
    """Read a comparison key without returning URL passwords or query tokens."""
    from codex_multi_server_orchestration import git_read
    try:
        raw, code = git_read(path, 'config', '--get', 'remote.origin.url')
    except (ValueError, OSError):
        return None
    if code:
        return None
    value = raw.decode('utf-8', errors='replace').strip()
    if not value or len(value) > 4096:
        return None
    if '://' in value:
        parsed = urlsplit(value)
        if parsed.scheme not in {'http', 'https', 'ssh', 'git'} or not parsed.hostname:
            return None
        host, repository = parsed.hostname.lower(), parsed.path
        try:
            port = parsed.port
        except ValueError:
            return None
        if port and port not in {22, 80, 443, 9418}:
            host += ':' + str(port)
    else:
        match = re.fullmatch(r'(?:[^@/:]+@)?([^/:]+):(.+)', value)
        if not match:
            return None
        host, repository = match.group(1).lower(), match.group(2)
    repository = repository.strip('/').removesuffix('.git')
    return host + '/' + repository if repository else None


def folders(path: object = None) -> dict[str, Any]:
    directory = Path(text_field(path, 'a folder path', 4096) if path else str(Path.home())).expanduser().resolve()
    if not directory.is_dir():
        raise ValueError('This directory is unavailable')
    rows = sorted((entry for entry in directory.iterdir() if entry.is_dir() and not entry.name.startswith('.')),
                  key=lambda entry: entry.name.casefold())
    return {'path': str(directory), 'cwd': str(directory),
            'parent': str(directory.parent) if directory != directory.parent else None,
            'directories': [{'name': entry.name, 'path': str(entry)} for entry in rows[:500]],
            'folders': [entry.name for entry in rows[:200]], 'truncated': len(rows) > 200}


def request(runtime: Any, data: dict[str, Any], *, actor: dict[str, Any] | None = None) -> dict[str, Any]:
    from codex_multi_server_orchestration import identity
    service = runtime.multi_server()
    action = data['action']
    project_id = text_field(data.get('project'), 'a project ID', 4096)
    server = text_field(data.get('server'), 'a server ID', 128)
    server = service.server_id if server == 'local' else server
    if server != service.server_id and server not in {row['id'] for row in service.transport.servers()}:
        raise PermissionError('The server is not paired or is revoked')
    request_id = text_field(data.get('request_id'), 'a request ID', 128)
    key = identity('project-location', actor['id'] if actor else 'ui', request_id)
    path = text_field(data.get('path'), 'a location path', 4096) if action == 'add_location' else None
    signature_data = {'action': action, 'project': project_id, 'server': server, 'path': path}
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        if actor:
            current = runtime.checked_actor(db, actor['id'], actor['id'])
            if not current.get('isLead') or current['epoch'] != actor['epoch']:
                raise PermissionError('Only the active lead can change project locations')
        signature, previous = runtime.operation_receipt(db, key, signature_data)
        if previous is not None:
            return cast(dict[str, Any], previous)
        row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_id,)).fetchone()
        if not row:
            raise ValueError('Select an existing project')
        project = json.loads(row[0])
        normalize(runtime, project)
        # Immutable caller content survives a rename or another server's update.
        old = db.execute('SELECT server,body FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
        if old:
            payload = json.loads(old['body'])['payload']
            if old['server'] != server or payload['request'] != signature_data:
                raise ValueError('This request ID has different content')
        else:
            busy = db.execute("""SELECT 1 FROM runtime_server_outbox WHERE state!='complete'
                AND json_extract(body,'$.action') IN ('add_location','remove_location')
                AND json_extract(body,'$.payload.project')=? LIMIT 1""", (project_id,)).fetchone()
            if busy:
                raise ValueError('A location change is pending. Read its existing receipt')
            location = next((row for row in project['locations'] if row['serverId'] == server), None)
            if action == 'add_location' and location and location.get('requestedPath', location['path']) != path:
                raise ValueError('This project already has a location on that server')
            if action == 'remove_location' and location and len(project['locations']) == 1:
                raise ValueError('Keep at least one project location')
            payload = {'project': project_id, 'name': project['name'], 'path': path,
                       'request': signature_data, 'signature': signature}
            if location:
                payload['location'] = location
            service.queue(db, server, action, payload, key)
    result = service.deliver(key)
    if result['outcome'] == 'not_applied':
        raise ValueError(result['error'])
    return cast(dict[str, Any], result)


def receive(runtime: Any, principal: str, action: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
    project_id = text_field(payload.get('project'), 'a project ID', 4096)
    name = text_field(payload.get('name'), 'a project name', 255)
    from codex_multi_server_orchestration import identity
    request_key = key
    key = identity('location-effect', key)
    signature_data = {'principal': principal, 'action': action, 'payload': payload}
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        signature, previous = runtime.operation_receipt(db, key, signature_data)
        if previous is not None:
            return cast(dict[str, Any], previous)
        if action == 'add_location':
            path = runtime.project_directory(payload.get('path'), require_existing=True)
            # Account selection stays on this server. No account ID or credential arrives.
            project = runtime.ensure_project(path, runtime.project_account(path, db=db), db)
            aliases = project.get('projectAliases', [])
            alias = {'serverId': principal, 'projectId': project_id, 'name': name}
            aliases = [row for row in aliases if (row['serverId'], row['projectId']) != (principal, project_id)]
            if principal != local_server(runtime) or project_id != project['id']:
                aliases.append(alias)
            project['projectAliases'] = aliases
            runtime.put(db, 'projects', project)
            value = {'serverId': local_server(runtime), 'path': payload['path'], 'canonicalPath': path,
                     'projectId': project['id'], 'gitOrigin': git_origin(path),
                     'homeProjectId': project_id, 'requestId': request_key}
        else:
            location = payload.get('location')
            if location:
                row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (location['projectId'],)).fetchone()
                if row:
                    project = json.loads(row[0])
                    project['projectAliases'] = [alias for alias in project.get('projectAliases', [])
                        if (alias['serverId'], alias['projectId']) != (principal, project_id)]
                    runtime.put(db, 'projects', project)
            value = {'removed': True, 'serverId': local_server(runtime)}
        runtime.save_receipt(db, key, signature, value)
        return value


def applied(runtime: Any, db: sqlite3.Connection, envelope: dict[str, Any], server: str,
            result: dict[str, Any]) -> None:
    """Commit the home location and its receipt with the outbound completion."""
    payload = envelope['payload']
    if envelope['action'] == 'add_location':
        location = result.get('value')
        canonical = location.get('canonicalPath') if isinstance(location, dict) else None
        if (not isinstance(location, dict) or location.get('serverId') != server
                or location.get('homeProjectId') != payload['project']
                or location.get('path') != payload['path'] or location.get('requestId') != envelope['requestId']
                or not isinstance(canonical, str) or not os.path.isabs(canonical)
                or os.path.normpath(canonical) != canonical or location.get('projectId') != canonical):
            raise PermissionError('The server returned a different project location')
    runtime.save_receipt(db, envelope['requestId'], payload['signature'], result)
    creation_key = 'project-registration:' + payload['project']
    db.execute("UPDATE runtime_operation_receipts SET result=? WHERE id=? AND json_extract(result,'$.requestId')=?",
               (json.dumps({**result, 'projectId': payload['project']}), creation_key, envelope['requestId']))
    row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (payload['project'],)).fetchone()
    if not row:
        return  # Deleting a project while a request is pending cannot recreate it.
    project = json.loads(row[0])
    normalize(runtime, project)
    locations = [row for row in project['locations'] if row['serverId'] != server]
    if envelope['action'] == 'add_location':
        location = result['value']
        registered = {key: location[key] for key in ('serverId', 'projectId', 'gitOrigin') if key in location}
        registered['path'] = location['canonicalPath']
        if location['path'] != location['canonicalPath']:
            registered['requestedPath'] = location['path']
        locations.append(registered)
    project.update(locations=locations, locationsRevision=project.get('locationsRevision', 0) + 1,
                   updated=time.time())
    runtime.put(db, 'projects', project)


def suggestions(runtime: Any, project_id: str, server: str, key: str) -> dict[str, Any]:
    """Suggest registered folders with the same origin. Never add a location."""
    service = runtime.multi_server()
    with runtime.read_db() as db:
        row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_id,)).fetchone()
    if not row:
        raise ValueError('Select an existing project')
    project = json.loads(row[0])
    if server not in {row['id'] for row in service.transport.servers()}:
        raise PermissionError('The server is not paired or is revoked')
    origin = next((row.get('gitOrigin') for row in project.get('locations', []) if row.get('gitOrigin')), None)
    origin = origin or (git_origin(project['path']) if not project['path'].startswith('project:') else None)
    if not origin:
        return {'matches': []}
    with runtime.db() as db:
        service.queue(db, server, 'location_matches', {'origin': origin}, key)
    return cast(dict[str, Any], service.deliver(key))


def matches(runtime: Any, origin: str, cwd: str | None = None) -> dict[str, Any]:
    candidates = {project['path']: {'projectId': project['id'], 'path': project['path'], 'name': project['name']}
                  for project in runtime.projects()['items'] if not project['path'].startswith('project:')}
    if cwd:
        directory = Path(cwd).expanduser().resolve()
        if not directory.is_dir():
            raise ValueError('This directory is unavailable')
        # Only the visible level is checked. No recursive search or file content crosses servers.
        children = sorted((entry for entry in directory.iterdir() if entry.is_dir() and (entry / '.git').exists()),
                          key=lambda entry: entry.name.casefold())[:50]
        if (directory / '.git').exists():
            children.insert(0, directory)
        for entry in children:
            path = str(entry)
            candidates.setdefault(path, {'projectId': path, 'path': path, 'name': entry.name})
    return {'matches': [row for path, row in candidates.items() if git_origin(path) == origin]}


def project_key(runtime: Any, value: object, *, require_existing: bool = False) -> str:
    """Accept stable abstract IDs and legacy directory IDs at metadata APIs."""
    text = text_field(value, 'a project ID or path', 4096)
    if text.startswith('project:'):
        try:
            uuid.UUID(text.removeprefix('project:'))
        except ValueError:
            raise ValueError('Invalid project ID') from None
        with runtime.read_db() as db:
            if not db.execute('SELECT 1 FROM runtime_projects WHERE id=?', (text,)).fetchone():
                raise ValueError('Select an existing project')
        return text
    return str(runtime.project_directory(text, require_existing=require_existing))


def create(runtime: Any, data: dict[str, Any]) -> dict[str, Any]:
    """Create a stable logical project with its first location on a paired server."""
    from codex_multi_server_orchestration import identity
    service = runtime.multi_server()
    request_id = text_field(data.get('request_id'), 'a request ID', 128)
    server = text_field(data.get('server'), 'a server ID', 128)
    path = text_field(data.get('path'), 'a location path', 4096)
    name = text_field(data.get('name') or Path(path).name or 'Project', 'a project name', 255)
    project_id = 'project:' + identity('logical-project', service.server_id, request_id)
    signature_data = {'server': server, 'path': path, 'name': name}
    if server != service.server_id and server != 'local' and server not in {row['id'] for row in service.transport.servers()}:
        raise PermissionError('The server is not paired or is revoked')
    account = runtime.accounts.default()
    creation_key = 'project-registration:' + project_id
    location_key = identity('project-location', 'ui', request_id)
    deleted = False
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        signature, previous = runtime.operation_receipt(db, creation_key, signature_data)
        if previous is not None and previous.get('outcome') == 'applied':
            return cast(dict[str, Any], previous)
        row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_id,)).fetchone()
        deleted = previous is not None and row is None
        if previous is None:
            runtime.save_receipt(db, creation_key, signature,
                                 {'outcome': 'unknown', 'projectId': project_id, 'requestId': location_key})
        if row:
            project = json.loads(row[0])
            if project.get('registration') != signature_data:
                raise ValueError('This request ID has different content')
        elif not deleted:
            project = {'id': project_id, 'path': project_id, 'name': name, 'created': time.time(),
                       'homeServerId': service.server_id, 'locations': [], 'locationsRevision': 0,
                       'accountKey': account, 'accountRevision': 1, 'registration': signature_data}
            runtime.put(db, 'projects', project)
    if deleted:
        with runtime.read_db() as db:
            queued = db.execute('SELECT 1 FROM runtime_server_outbox WHERE id=?', (location_key,)).fetchone()
        result = service.deliver(location_key) if queued else previous
        return {**cast(dict[str, Any], result), 'projectId': project_id}
    result = request(runtime, {'action': 'add_location', 'project': project_id, 'server': server,
                              'path': path, 'request_id': request_id})
    return {**result, 'projectId': project_id}


def info(path: str) -> dict[str, Any]:
    from codex_multi_server_orchestration import git_read, commit_hash
    directory = str(Path(path).expanduser().resolve())
    head = None
    try:
        raw, code = git_read(directory, 'rev-parse', '--verify', 'HEAD')
        value = raw.decode().strip()
        if code == 0 and commit_hash(value):
            head = value[:8]
    except (ValueError, OSError):
        pass
    return {'gitHead': head, 'gitOrigin': git_origin(directory), 'matches': []}


def query(runtime: Any, data: dict[str, Any]) -> dict[str, Any]:
    from codex_multi_server_orchestration import identity
    service = runtime.multi_server()
    project_id = text_field(data.get('project'), 'a project ID', 4096)
    server = text_field(data.get('server'), 'a server ID', 128)
    server = service.server_id if server == 'local' else server
    if server != service.server_id and server not in {row['id'] for row in service.transport.servers()}:
        raise PermissionError('The server is not paired or is revoked')
    with runtime.read_db() as db:
        row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_id,)).fetchone()
    if not row:
        raise ValueError('Select an existing project')
    project = json.loads(row[0])
    key = identity('project-location-read', data.get('request_id') or uuid.uuid4().hex)
    if data.get('action') == 'matches':
        origin = next((row.get('gitOrigin') for row in project.get('locations', []) if row.get('gitOrigin')), None)
        origin = origin or (git_origin(project['path']) if not project['path'].startswith('project:') else None)
        if not origin:
            return {'matches': []}
        action, payload = 'location_matches', {'origin': origin, **({'cwd': text_field(data['path'], 'a folder path', 4096)} if data.get('path') else {})}
    else:
        location = next((row for row in project.get('locations', []) if row['serverId'] == server), None)
        path = data.get('path') or (location['path'] if location else None)
        if not path:
            raise ValueError('This server has no folder for the project')
        action, payload = 'location_info', {'cwd': text_field(path, 'a folder path', 4096)}
    with runtime.db() as db:
        service.queue(db, server, action, payload, key)
    result = service.deliver(key)
    if result['outcome'] != 'applied':
        raise RuntimeError(result.get('error') or 'The server is unavailable')
    return cast(dict[str, Any], result['value'])


def chat_binding(runtime: Any, db: sqlite3.Connection, cwd: str, project_id: object,
                 home_server: object) -> dict[str, str]:
    if project_id is None and home_server is None:
        return {}
    project_id = text_field(project_id, 'a project ID', 4096)
    home_server = text_field(home_server, 'a project server ID', 128)
    local = local_server(runtime)
    if home_server != local:
        from codex_server_transport import PairedServerTransport
        transport = runtime.multi_server().transport
        if isinstance(transport, PairedServerTransport):
            paired = db.execute("SELECT 1 FROM runtime_access_clients WHERE id=? AND json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired'", (home_server,)).fetchone()
        else:
            paired = home_server in {row['id'] for row in transport.servers()}
        if not paired:
            raise PermissionError('The project server is not paired or is revoked')
    for row in db.execute('SELECT record FROM runtime_projects'):
        project = json.loads(row[0])
        if project['id'] == project_id and project.get('homeServerId') == home_server:
            if any(location['serverId'] == local and location['path'] == cwd for location in project.get('locations', [])):
                return {'projectId': project_id, 'projectServerId': home_server}
        if project['path'] == cwd and any(alias['serverId'] == home_server and alias['projectId'] == project_id
                                        for alias in project.get('projectAliases', [])):
            return {'projectId': project_id, 'projectServerId': home_server}
    raise ValueError('The selected folder is not a location of this project')


def settings_key(runtime: Any, db: sqlite3.Connection, cwd: str,
                 binding: dict[str, Any]) -> str:
    """Use settings of the local logical project, or its destination registration."""
    project_id, home = binding.get('projectId'), binding.get('projectServerId')
    if not project_id or not home:
        return cwd
    row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_id,)).fetchone()
    if row:
        project = json.loads(row[0])
        if project.get('homeServerId') == home and any(
                location['serverId'] == local_server(runtime) and location['path'] == cwd
                for location in project.get('locations', [])):
            return str(project['id'])
    for row in db.execute("SELECT record FROM runtime_projects WHERE json_extract(record,'$.path')=?", (cwd,)):
        project = json.loads(row[0])
        if any(alias['serverId'] == home and alias['projectId'] == project_id
               for alias in project.get('projectAliases', [])):
            return str(project['id'])
    return cwd
