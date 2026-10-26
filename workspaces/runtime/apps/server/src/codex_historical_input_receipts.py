"""Prove historical batch inputs from saved content and native acceptance.

This module reads receipts. It sends no request and changes no saved state.
The caller must compare a fresh capture before it changes an event receipt.
"""
import copy
import hashlib
import json
import math
from pathlib import Path
import sqlite3
import time
import uuid

from codex_daybreak import turn_params
from codex_time import append_message_clocks, message_clock

TEXT_LIMIT = 64 * 1024
CAPTURE_LIMIT = 1024 * 1024


def _params(agent, primary, text, members):
    if (agent.get('provider', 'codex') != 'codex' or not agent.get('model')
            or any(member['metadata'].get('assets') for member in members)):
        return None
    clocks = []
    for member in members:
        at = member['metadata'].get('acceptedAt')
        if member['event']['kind'] in {'user', 'followup'} and at is not None:
            if type(at) not in {int, float} or not math.isfinite(at):
                return None
            clocks.append(message_clock(member['event']['id'], at))
    params = {'threadId': agent['threadId'], 'model': agent['model'],
              'clientUserMessageId': primary,
              'input': [{'type': 'text', 'text': append_message_clocks(text, clocks)}]}
    if agent.get('yoloMode') is True:
        params.update(approvalPolicy='never', sandboxPolicy={'type': 'dangerFullAccess'})
    elif agent.get('yoloMode') is False:
        sandbox = {'type': 'readOnly'}
        if agent.get('role') != 'reviewer':
            # A lead can add a progress directory. Do not guess its saved roots.
            if agent.get('isLead') or not isinstance(agent.get('cwd'), str):
                return None
            sandbox = {'type': 'workspaceWrite', 'writableRoots': [agent['cwd']],
                       'networkAccess': False}
        params.update(approvalPolicy='on-request', sandboxPolicy=sandbox)
    params['serviceTier'] = 'priority' if agent.get('fastMode', False) else 'default'
    params.update(turn_params(agent))
    effort = agent.get('nativeEffort', agent.get('effort'))
    if effort is not None:
        params['effort'] = effort
    return params


def _capture(db, agent, event):
    metadata = json.loads(event['metadata']) if event.get('metadata') else {}
    key = metadata.get('transcriptItemId')
    if not isinstance(key, str) or not key.startswith(agent['id'] + ':'):
        return None
    row = db.execute('SELECT * FROM runtime_items WHERE id=? AND agent=? '
                     'AND length(CAST(record AS BLOB))<=?',
                     (key, agent['id'], TEXT_LIMIT)).fetchone()
    if not row:
        return None
    item = json.loads(row['record'])
    inputs = item.get('inputs')
    if (item.get('id') != key or item.get('role') != 'user' or item.get('assets')
            or not isinstance(inputs, list) or not 1 <= len(inputs) <= 32
            or any(not isinstance(value, dict) or not isinstance(value.get('id'), str)
                   or not value['id'] or value.get('truncated') or value.get('assets')
                   for value in inputs)):
        return None
    ids = [value['id'] for value in inputs]
    if len(set(ids)) != len(ids) or event['id'] not in ids:
        return None
    primary = key[len(agent['id']) + 1:]
    if ids[0] != primary:
        return None
    text = item.get('text')
    full = db.execute('SELECT body FROM runtime_item_fulltext WHERE id=? '
                      'AND length(CAST(body AS BLOB))<=?', (key, TEXT_LIMIT)).fetchone()
    if item.get('truncated'):
        if not full:
            return None
        text = full[0]
    elif full and full[0] != text:
        return None
    if not isinstance(text, str) or len(text.encode()) > TEXT_LIMIT:
        return None
    members = []
    for value in inputs:
        saved = db.execute('SELECT e.*,m.record AS metadata FROM runtime_events e '
            'JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=? AND e.agent=? AND e.epoch=? '
            'AND length(CAST(e.text AS BLOB))<=? AND length(CAST(m.record AS BLOB))<=?',
            (value['id'], agent['id'], agent['epoch'], TEXT_LIMIT, TEXT_LIMIT)).fetchone()
        if not saved:
            return None
        saved = dict(saved)
        meta = json.loads(saved.pop('metadata'))
        manifest = meta.get('contextManifest') or {}
        scope = manifest.get('epoch')
        native = meta.get('native') or {}
        if (saved['status'] not in {'reserved', 'dispatching', 'uncertain', 'delivered'}
                or saved['text'] != value.get('text') or saved['created'] != value.get('at')
                or saved['kind'] != value.get('kind') or meta.get('assets')
                or meta.get('transcriptItemId') != key or meta.get('modelEventProjection') != 1
                or (scope and (not isinstance(scope, list) or len(scope) != 2
                               or scope[0] != agent['threadId']))
                or (native and any(native.get(field) != expected for field, expected in
                    (('agent', agent['id']), ('epoch', agent['epoch']),
                     ('accountKey', agent.get('accountKey', 'default')),
                     ('threadId', agent['threadId']))))):
            return None
        members.append({'event': saved, 'metadata': meta})
    # A saved primary manifest ties the local text to this native thread.
    primary_scope = (members[0]['metadata'].get('contextManifest') or {}).get('epoch')
    if not primary_scope or primary_scope[0] != agent['threadId']:
        return None
    params = _params(agent, primary, text, members)
    if params is None:
        return None
    canonical = {'method': 'turn/start', 'params': params}
    digest = hashlib.sha256(json.dumps(canonical, sort_keys=True,
                                      separators=(',', ':')).encode()).hexdigest()
    capture = {'agent': agent['id'], 'epoch': agent['epoch'], 'threadId': agent['threadId'],
               'accountKey': agent.get('accountKey', 'default'), 'primary': primary,
               'item': dict(row), 'fulltext': full[0] if full else None,
               'members': members, 'params': params, 'digest': digest}
    return capture if len(json.dumps(capture).encode()) <= CAPTURE_LIMIT else None


def capture_batches(db, agent, events):
    """Freeze local batch proofs under the caller's existing state lock."""
    if (not isinstance(events, list) or not 1 <= len(events) <= 32
            or not isinstance(agent.get('threadId'), str) or not agent['threadId']):
        return []
    captures = []
    for event in events:
        try:
            event = dict(event)
            if (event.get('agent') != agent['id'] or event.get('epoch') != agent['epoch']
                    or event.get('status') not in {'reserved', 'dispatching', 'uncertain'}):
                continue
            capture = _capture(db, agent, event)
            if capture and capture not in captures:
                captures.append(capture)
        except (ValueError, TypeError, KeyError, AttributeError, OverflowError):
            continue
    return copy.deepcopy(captures)


def _input_content(content):
    if not isinstance(content, list):
        return None
    result = []
    for part in content:
        if (not isinstance(part, dict) or part.get('type') != 'text'
                or not isinstance(part.get('text'), str)
                or set(part) - {'type', 'text', 'text_elements', 'textElements'}
                or any(part[key] != [] for key in ('text_elements', 'textElements') if key in part)):
            return None
        result.append({'type': 'text', 'text': part['text']})
    return result


def _native_turn(capture, turns):
    found = []
    for turn in turns:
        if (not isinstance(turn, dict) or not isinstance(turn.get('id'), str) or not turn['id']
                or turn.get('startOutcome') not in {None, 'accepted'}
                or turn.get('clientUserMessageId') not in {None, capture['primary']}):
            continue
        matches = [item for item in turn.get('items') or [] if isinstance(item, dict)
                   and item.get('type') == 'userMessage' and item.get('clientId') == capture['primary']
                   and isinstance(item.get('id'), str) and item['id']
                   and _input_content(item.get('content')) == capture['params']['input']]
        if len(matches) == 1:
            found.append(turn['id'])
    return found[0] if len(found) == 1 else None


def accepted_batches(supervisor, captures, turns):
    """Return positive event receipts. Missing records never prove rejection."""
    if (not isinstance(supervisor, dict) or not captures or len(captures) > 32
            or not isinstance(turns, list) or len(turns) > 1600):
        return {}
    try:
        root = Path(supervisor['stateDir']).resolve(strict=True)
        path = (root / 'supervisor.sqlite3').resolve(strict=True)
        path.relative_to(root)
        deadline = time.monotonic() + 2
        db = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=0.1)
        db.row_factory = sqlite3.Row
        db.set_progress_handler(lambda: time.monotonic() >= deadline, 1000)
        try:
            accepted = {}
            for capture in captures:
                if time.monotonic() >= deadline:
                    break
                handle = 'account:' + capture['accountKey']
                if supervisor.get('handle') != handle:
                    continue
                turn_id = _native_turn(capture, turns)
                if turn_id is None:
                    continue
                prefix = 'turn:' + capture['agent'] + ':' + capture['primary'] + ':attempt:'
                rows = db.execute('SELECT operation_id,digest,native_id,accepted FROM operations '
                    'WHERE handle=? AND operation_id>=? AND operation_id<? LIMIT 65',
                    (handle, prefix, prefix + '\uffff')).fetchall()
                matches = []
                for row in rows:
                    suffix = row['operation_id'][len(prefix):]
                    try:
                        exact_attempt = str(uuid.UUID(suffix)) == suffix
                    except (ValueError, AttributeError):
                        exact_attempt = False
                    if (exact_attempt and row['digest'] == capture['digest']
                            and type(row['native_id']) is int and row['native_id'] > 0
                            and type(row['accepted']) in {int, float}
                            and math.isfinite(row['accepted']) and row['accepted'] > 0):
                        matches.append(row)
                if len(rows) > 64 or len(matches) != 1:
                    continue
                for member in capture['members']:
                    if member['event']['status'] in {'reserved', 'dispatching', 'uncertain'}:
                        accepted[member['event']['id']] = turn_id
            return accepted
        finally:
            db.close()
    except (OSError, sqlite3.Error, ValueError, TypeError, KeyError, AttributeError):
        return {}
