"""Persist read state for one exact result under the runtime lock."""


def read_state(runtime, db, agent, data):
    if set(data) - {'id', 'read_state'}:
        raise ValueError('Change read state separately from other chat settings')
    if 'id' in data and data['id'] != agent['id']:
        raise ValueError('The chat identity changed')
    desired = data['read_state']
    fields = {'thread_id', 'turn_id', 'read', 'expected_revision'}
    if not isinstance(desired, dict) or set(desired) != fields:
        raise ValueError('Supply the result identity, read state, and revision')
    for field in ('thread_id', 'turn_id'):
        if not isinstance(desired[field], str) or not desired[field].strip():
            raise ValueError('Supply a nonempty result identity')
    if type(desired['read']) is not bool:
        raise ValueError('Supply a boolean read state')
    revision = desired['expected_revision']
    if type(revision) is not int or revision < 0:
        raise ValueError('Supply the current read state revision')
    if (agent.get('threadId') != desired['thread_id']
            or agent.get('lastCompletedTurn') != desired['turn_id']
            or agent.get('lastCompletedTurnStatus') != 'completed'):
        raise ValueError('The completed result changed. Reload the chat')
    previous = agent.get('readState') or {}
    current = previous.get('revision', 0)
    if type(current) is not int or current < 0:
        raise ValueError('The saved read state revision is invalid')
    target = {'threadId': desired['thread_id'], 'turnId': desired['turn_id'],
              'read': desired['read']}
    same = all(previous.get(field) == value for field, value in target.items())
    if same and revision in (current, current - 1):
        return agent
    if revision != current:
        raise ValueError('The read state changed. Reload the chat')
    agent['readState'] = {**target, 'revision': current + 1}
    runtime.put(db, 'agents', agent)
    return agent
