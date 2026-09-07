"""Read legacy receipts from the exact state directory of an older local server."""

import json
from pathlib import Path
import sqlite3
import urllib.error
import urllib.parse
import urllib.request

from codex_tool_requests import _spawned_ids


def _origin(url):
    try:
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme not in {'http', 'https'} or parsed.username is not None or parsed.password is not None:
            return None
        return parsed.scheme, parsed.hostname, parsed.port or (443 if parsed.scheme == 'https' else 80)
    except ValueError:
        return None


def same_origin(first, second):
    origin = _origin(first)
    return origin is not None and origin == _origin(second)


def is_loopback_url(url):
    origin = _origin(url)
    return origin is not None and origin[1] in {'localhost', '127.0.0.1', '::1'}


class _SameOriginRedirect(urllib.request.HTTPRedirectHandler):
    def __init__(self, origin):
        self.origin = origin

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not same_origin(self.origin, newurl):
            raise ValueError('Legacy recovery refuses a redirect to another origin')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _prefix(actor):
    account = actor.get('accountKey', 'default')
    return (account + ':' if account != 'default' else '') + actor['threadId'] + ':'


def _summary(actor, key, result):
    successful = result.get('success') is True
    return {'id': key, 'agent': actor['id'], 'threadId': actor['threadId'],
            'callId': key[len(_prefix(actor)):], 'legacy': True,
            'stage': 'completed' if successful else 'failed',
            'outcome': 'applied' if successful else 'unknown',
            'agentIds': _spawned_ids(result)}


def _registry(db, actor, agent_ids):
    result = []
    for agent_id in agent_ids:
        row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
        agent = json.loads(row[0]) if row else None
        if not agent or agent.get('parentId') != actor['id']:
            result.append({'id': agent_id, 'absent': True})
        else:
            result.append({**{key: agent.get(key) for key in ('id', 'name', 'status', 'turnId', 'model')},
                           'deleted': bool(agent.get('deletedAt'))})
    return result


def recover_legacy_requests(url, agent_id, request_id=None):
    """Use only after this exact local server returns 404 for the new endpoint."""
    if not is_loopback_url(url):
        raise ValueError('Legacy request recovery is available only for a local server')
    if not isinstance(agent_id, str) or not agent_id:
        raise ValueError('Supply the agent id')
    if request_id is not None and (not isinstance(request_id, str) or not 1 <= len(request_id) <= 1000):
        raise ValueError('Supply request_id with 1 to 1000 characters')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _SameOriginRedirect(url))
    with opener.open(url.rstrip('/') + '/api/desktop', timeout=5) as response:
        if not same_origin(url, response.url):
            raise ValueError('Legacy recovery refuses metadata from another origin')
        metadata = json.load(response)
    if not isinstance(metadata, dict) or metadata.get('application') != 'codex-agents':
        raise ValueError('The local server did not identify a Codex Studio state directory')
    directory = metadata.get('stateDir')
    if not isinstance(directory, str) or not Path(directory).is_absolute():
        raise ValueError('The local server did not return an absolute state directory')
    path = Path(directory) / 'canvas.sqlite3'
    try:
        # mode=ro cannot create a database or change schema. Keep WAL visibility;
        # immutable=1 would ignore current writes from the running server.
        db = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=2)
        try:
            row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
            if not row:
                raise ValueError('Unknown managed agent')
            actor = json.loads(row[0])
            if actor.get('deletedAt'):
                raise ValueError('This agent was deleted')
            if not actor.get('threadId'):
                return ({'requests': [], 'legacy': True} if request_id is None else
                        {'id': request_id, 'agent': agent_id, 'legacy': True, 'stage': 'not_found', 'outcome': 'unknown',
                         'message': 'No receipt found. This does not prove that the operation did not execute.'})
            prefix = _prefix(actor)
            if request_id is None:
                rows = db.execute('SELECT id,result FROM runtime_tool_results WHERE substr(id,1,?)=? ORDER BY rowid DESC LIMIT 50',
                                  (len(prefix), prefix)).fetchall()
                return {'legacy': True, 'requests': [_summary(actor, key, json.loads(result)) for key, result in rows]}
            key = request_id if request_id.startswith(prefix) else prefix + request_id
            row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()
            if not row:
                return {'id': request_id, 'agent': agent_id, 'legacy': True, 'stage': 'not_found', 'outcome': 'unknown',
                        'message': 'No receipt found. This does not prove that the operation did not execute.'}
            result = json.loads(row[0])
            summary = _summary(actor, key, result)
            return {**summary, 'result': result, 'agents': _registry(db, actor, summary['agentIds'])}
        finally:
            db.close()
    except sqlite3.Error as error:
        raise ValueError('Cannot read legacy request receipts: ' + str(error)) from error
