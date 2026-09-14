"""Team-local delegation mode. Existing accepted work keeps its identity and state."""
import time


def mode_fields(agent):
    if agent.get('isLead'):
        agent.setdefault('agentMode', 'multi')
        agent.setdefault('agentModeRevision', 0)
        agent['agentModeSupported'] = True
    return agent


def assert_delegation(root):
    if root.get('agentMode', 'multi') != 'multi':
        raise ValueError('Single agent mode disables new delegation. Complete the task in the lead chat or select Multi agent.')


def assert_worker_input(runtime, db, target):
    if target['id'] != target['rootId']:
        assert_delegation(runtime.agent(target['rootId'], db))


def change_mode(runtime, key, data):
    allowed = {'id', 'agent_mode', 'expected_mode_revision', 'request_id', 'expected_account_key'}
    if set(data) - allowed:
        raise ValueError('Change agent mode separately from execution settings')
    mode, revision, request = data.get('agent_mode'), data.get('expected_mode_revision'), data.get('request_id')
    if not isinstance(mode, str) or mode not in {'multi', 'single'}:
        raise ValueError('Choose multi or single agent mode')
    if type(revision) is not int or revision < 0:
        raise ValueError('A nonnegative expected mode revision is required')
    if not isinstance(request, str) or not 1 <= len(request) <= 200:
        raise ValueError('An agent mode request id is required')
    with runtime.lock, runtime.db() as db:
        agent = runtime.checked_actor(db, key)
        if not agent.get('isLead') or agent['rootId'] != key:
            raise ValueError('Change agent mode on the lead chat')
        body = {'operation': 'agent_mode', 'agent': key, 'mode': mode, 'expectedRevision': revision}
        signature, previous = runtime.operation_receipt(db, request, body)
        if previous is not None:
            return mode_fields(agent)
        if agent.get('agentModeRevision', 0) != revision:
            raise ValueError('Agent mode changed. Read the current mode before saving')
        if agent.get('agentMode', 'multi') != mode:
            agent.update(agentMode=mode, agentModeRevision=revision + 1,
                         agentModeChangedAt=time.time(), agentModeChangedBy='user')
            runtime.put(db, 'agents', mode_fields(agent))
        runtime.save_receipt(db, request, signature,
                             {'applied': True, 'agentMode': mode, 'agentModeRevision': agent.get('agentModeRevision', 0)})
        return mode_fields(agent)


def guidance(root):
    revision = root.get('agentModeRevision', 0)
    if root.get('agentMode', 'multi') == 'single':
        text = ('Single agent mode: the lead completes new work itself. Do not spawn agents or delegate new work. '
                'Existing workers finish accepted work and report to the lead. '
                'This user setting overrides earlier delegation guidance. Only the user can change this mode.')
    else:
        text = 'Multi agent mode: delegation within this team is available under the user instructions.'
    return f'[Studio agent mode, revision {revision}] {text}'


def tool_mode_context(runtime, actor_id, result, key=None):
    # Keep a stable policy attachment for each result, revision and compaction
    # epoch. Suppress later results only after confirmed response delivery.
    with runtime.lock, runtime.db() as db:
        import json
        from codex_efficiency import digest, packed
        actor = runtime.agent(actor_id, db)
        root = runtime.agent(actor['rootId'], db)
        if not root.get('agentModeRevision'):
            return result
        epoch, _, known = runtime.model_known_context(db, actor)
        text = guidance(root)
        version = digest(text)
        db.execute('CREATE TABLE IF NOT EXISTS runtime_model_modes '
                   '(agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version))')
        identity = digest([epoch, version])
        row = db.execute('SELECT record FROM runtime_model_modes WHERE agent=? AND request=? AND version=?',
                         (actor_id, key, identity)).fetchone() if key else None
        if row:
            record = json.loads(row[0])
        else:
            record = {'epoch': epoch, 'version': version, 'revision': root['agentModeRevision'],
                      'text': text if known.get('agentMode') != version else None}
            if key and record['text']:
                db.execute('INSERT INTO runtime_model_modes VALUES (?,?,?,?)',
                           (actor_id, key, identity, packed(record)))
        if not record['text']:
            return result
        return {**result, 'contentItems': [*result.get('contentItems', []),
                {'type': 'inputText', 'text': record['text']}]}
