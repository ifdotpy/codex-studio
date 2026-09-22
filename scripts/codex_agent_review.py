"""Reserve native reviews as ordinary Studio children, without a user turn."""
import copy
import json
import time
import uuid

from codex_agent_modes import assert_delegation
from codex_time import stamp_tool_result


def review_tools(tool, text):
    variants = []
    for kind, properties, required in (
        ('uncommittedChanges', {}, []),
        ('baseBranch', {'branch': text}, ['branch']),
        ('commit', {'sha': text, 'title': text}, ['sha']),
        ('custom', {'instructions': text}, ['instructions']),
    ):
        variants.append({'type': 'object', 'properties': {'type': {'type': 'string', 'enum': [kind]}, **properties},
                         'required': ['type', *required], 'additionalProperties': False})
    return [tool('orchestration_review',
        'Request native Codex review in a separate read-only Studio child. Defaults to all uncommitted changes. '
        'Choose a base branch, commit, or custom instructions through target. The reviewer uses your model and effort, '
        'subject to the account review_model setting. It shares your current directory and does not receive your chat history. '
        'The result wakes you automatically. Finish your turn while waiting. Use a stable request_id for retries; '
        'read orchestration_request after a lost response. Available in Multi agent mode.',
        {'target': {'anyOf': variants}, 'request_id': {'type': 'string', 'minLength': 1, 'maxLength': 200}})]


def validate(args):
    if not isinstance(args, dict) or set(args) - {'target', 'request_id'}:
        raise ValueError('Supply only target and optional request_id')
    if 'request_id' in args:
        value = args['request_id']
        if (not isinstance(value, str) or not 1 <= len(value) <= 200
                or value != value.strip() or any(ord(c) < 32 for c in value)):
            raise ValueError('Supply request_id with 1 to 200 characters and no surrounding whitespace')
    target = args.get('target', {'type': 'uncommittedChanges'})
    if not isinstance(target, dict) or not isinstance(target.get('type'), str):
        raise ValueError('Supply a native review target')
    fields = {'uncommittedChanges': set(), 'baseBranch': {'branch'},
              'commit': {'sha', 'title'}, 'custom': {'instructions'}}
    kind = target['type']
    if kind not in fields or set(target) - ({'type'} | fields[kind]):
        raise ValueError('Invalid or mixed review target fields')
    required = {'baseBranch': 'branch', 'commit': 'sha', 'custom': 'instructions'}.get(kind)
    if required:
        value = target.get(required)
        limit = 32000 if required == 'instructions' else 1000
        if not isinstance(value, str) or not 1 <= len(value.strip()) <= limit or '\x00' in value:
            raise ValueError(f'Supply a nonempty {required} with at most {limit} characters')
    if 'title' in target and target['title'] is not None:
        if not isinstance(target['title'], str) or len(target['title']) > 1000 or '\x00' in target['title']:
            raise ValueError('Review title must have at most 1000 characters')
    return copy.deepcopy(target)


def _existing(rt, db, key, actor, target):
    child_id = str(uuid.uuid5(uuid.NAMESPACE_URL, key))
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (child_id,)).fetchone()
    if not row:
        return None
    child = json.loads(row[0])
    review = child.get('nativeReview') or {}
    if (review.get('requestId') != key or review.get('actorId') != actor['id']
            or review.get('target') != target):
        raise ValueError('This review request id has different content')
    return review['response']


def request(rt, actor, args, key):
    target = validate(args)
    if actor.get('provider', 'codex') != 'codex':
        raise ValueError('Native review is available only for Codex agents')
    if actor.get('nativeReview'):
        raise ValueError('A native reviewer cannot create another review')
    with rt.lock, rt.db() as db:
        previous = _existing(rt, db, key, actor, target)
        if previous is not None:
            return previous
        assert_delegation(rt.agent(actor['rootId'], db))
    catalog = rt.catalog(actor.get('accountKey', 'default'))
    with rt.lock, rt.db() as db:
        previous = _existing(rt, db, key, actor, target)
        if previous is not None:
            return previous
        receipt = rt.tool_request(key, db)
        if receipt and receipt.get('cancelRequested'):
            raise ValueError('Request cancelled before review creation')
        current = rt.checked_actor(db, actor['id'], actor['id'])
        if (rt.closed or current['epoch'] != actor['epoch']
                or current.get('accountKey', 'default') != actor.get('accountKey', 'default')
                or current.get('provider', 'codex') != 'codex'
                or (receipt and not rt.connection_current(receipt['accountKey'], receipt.get('connectionId')))):
            raise ValueError('The parent or its account connection changed before review creation')
        assert_delegation(rt.agent(current['rootId'], db))
        spec = {'id': str(uuid.uuid5(uuid.NAMESPACE_URL, key)), 'name': 'Review',
                'role': 'reviewer', 'prompt': 'Run a native code review: ' + json.dumps(target, ensure_ascii=False),
                'model': current['model'], 'effort': current.get('effort'),
                'fast_mode': current.get('fastMode', False)}
        # The native target stores the complete instructions. This display field
        # must stay inside create()'s task size limit.
        spec['prompt'] = spec['prompt'][:32000]
        child = rt.create(spec, current['id'], parent_epoch=current['epoch'],
                          _catalog=catalog, _validate_only=True)
        child.update(yoloMode=False, worktree=False)
        value = {'requestId': key, 'agentId': child['id'], 'status': 'queued',
                 'agents': [{name: child[name] for name in ('id', 'name', 'status', 'model', 'effort', 'fastMode')}],
                 'delivery': 'The review result wakes you automatically. Finish your turn while waiting.'}
        child['nativeReview'] = {'target': target, 'requestId': key, 'actorId': current['id'],
                                 'status': 'pending', 'response': copy.deepcopy(value)}
        rt.put(db, 'agents', child)
        result = stamp_tool_result({'success': True, 'contentItems': [
            {'type': 'inputText', 'text': json.dumps(value, ensure_ascii=False)}]}, time.time())
        db.execute('INSERT OR IGNORE INTO runtime_tool_results VALUES (?,?)', (key, json.dumps(result)))
        if receipt:
            rt.finish_tool_request(key, result, outcome='applied', db=db)
        return value


def claim(rt, db, agent):
    """Called under the dispatch lock after normal concurrency/budget admission."""
    current = rt.agent(agent['id'], db)
    review = current.get('nativeReview') or {}
    if (rt.closed or review.get('status') != 'pending' or current.get('status') != 'queued'
            or current.get('deletedAt') or not current.get('autoWake') or current.get('inFlight')
            or current.get('startAttempt') or current['epoch'] != agent['epoch']
            or current.get('accountKey', 'default') != agent.get('accountKey', 'default')):
        return None
    rt.capacity_reset(db, current)
    attempt = {'id': str(uuid.uuid4()), 'epoch': current['epoch'],
               'accountKey': current.get('accountKey', 'default'), 'events': [],
               'action': 'review', 'reviewTarget': copy.deepcopy(review['target']), 'submitted': False}
    review.update(status='started', startedAt=time.time())
    current.update(nativeReview=review, status='starting', inFlight=True,
                   turnEpoch=current['epoch'], startAttempt=attempt)
    rt.put(db, 'agents', current)
    agent.update(current)
    return copy.deepcopy(attempt)
