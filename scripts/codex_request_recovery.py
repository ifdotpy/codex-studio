"""Read legacy receipts from the exact state directory of an older local server."""

import json
from pathlib import Path
import sqlite3
import urllib.error
import urllib.parse
import urllib.request

from codex_tool_requests import _spawned_ids, operation_receipt_evidence, request_prefixes, request_result_outcome


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


def _summary(actor, key, result):
    successful = result.get('success') is True
    return {'id': key, 'agent': actor['id'], 'threadId': actor['threadId'],
            'callId': key[len(next(prefix for prefix in request_prefixes(actor) if key.startswith(prefix))):], 'legacy': True,
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
            if not request_prefixes(actor):
                return ({'requests': [], 'legacy': True} if request_id is None else
                        {'id': request_id, 'agent': agent_id, 'legacy': True, 'stage': 'not_found', 'outcome': 'unknown',
                         'message': 'No receipt found. This does not prove that the operation did not execute.'})
            prefixes = request_prefixes(actor)
            has_ledger = db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_tool_requests'").fetchone()
            if request_id is None:
                conditions = ' OR '.join('substr(id,1,?)=?' for _ in prefixes)
                params = [part for prefix in prefixes for part in (len(prefix), prefix)]
                rows = db.execute('SELECT id,result FROM runtime_tool_results WHERE ' + conditions + ' ORDER BY rowid DESC LIMIT 50', params).fetchall()
                return {'legacy': True, 'requests': [_summary(actor, key, json.loads(result)) for key, result in rows]}
            candidates = ([request_id] if any(request_id.startswith(prefix) for prefix in prefixes)
                          else [prefix + request_id for prefix in prefixes])
            # The immutable ledger owner remains authoritative after an account or
            # native thread change. Historical prefixes cover pre-ledger receipts.
            if has_ledger:
                owned = db.execute("SELECT id FROM runtime_tool_requests WHERE id=? AND json_extract(record,'$.agent')=?",
                                   (request_id, agent_id)).fetchone()
                if owned:
                    candidates = [owned[0]]
                elif db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_tool_request_aliases'").fetchone():
                    aliases = db.execute("SELECT a.request FROM runtime_tool_request_aliases a JOIN runtime_tool_requests r "
                                         "ON r.id=a.request WHERE a.agent=? AND a.alias=? AND json_extract(r.record,'$.agent')=?",
                                         (agent_id, request_id, agent_id)).fetchall()
                    if aliases:
                        candidates = list(dict.fromkeys(row[0] for row in aliases))
            found = []
            for key in candidates:
                record = None
                if has_ledger:
                    saved = db.execute('SELECT record FROM runtime_tool_requests WHERE id=?', (key,)).fetchone()
                    record = json.loads(saved[0]) if saved else None
                    if record and record.get('agent') != agent_id:
                        continue
                row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()
                evidence = operation_receipt_evidence(db, key)
                if record or row or evidence:
                    found.append((key, record, row, evidence))
            if len(found) > 1:
                return {'id': request_id, 'agent': agent_id, 'legacy': True, 'stage': 'ambiguous', 'outcome': 'unknown',
                        'message': 'Use the full request id; this call id exists in several account histories.'}
            if not found:
                return {'id': request_id, 'agent': agent_id, 'legacy': True, 'stage': 'not_found', 'outcome': 'unknown',
                        'message': 'No receipt found. This does not prove that the operation did not execute.'}
            key, record, row, evidence = found[0]
            if record:
                result = json.loads(row[0]) if row else record.get('result')
                record = {k: v for k, v in record.items() if k != 'signature'}
                if isinstance(result, dict):
                    outcome = request_result_outcome(record, result)
                    if outcome == 'not_applied' and evidence:
                        outcome = 'unknown'
                    if record.get('outcome') not in {'applied', 'not_applied'}:
                        record.update(outcome=outcome, stage='completed' if outcome == 'applied' else 'failed')
                    record['result'] = result
                if record.get('outcome') not in {'applied', 'not_applied'} and evidence:
                    record.update(evidence)
                return {**record, 'legacy': True, 'agents': _registry(db, actor, record.get('agentIds', []))}
            prefix = next(prefix for prefix in prefixes if key.startswith(prefix))
            if not row:
                return {'id': key, 'agent': agent_id, 'threadId': actor['threadId'],
                        'callId': key[len(prefix):], 'legacy': True, 'stage': 'unknown', 'outcome': 'unknown',
                        **evidence,
                        'message': 'The operation receipt is committed. The final tool result is unavailable; do not repeat the operation.'}
            result = json.loads(row[0])
            summary = _summary(actor, key, result)
            return {**summary, 'result': result, 'agents': _registry(db, actor, summary['agentIds'])}
        finally:
            db.close()
    except sqlite3.Error as error:
        raise ValueError('Cannot read legacy request receipts: ' + str(error)) from error
