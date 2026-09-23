"""A durable, bounded speaking order for two independent peer chats."""
import json
import time
import uuid

from codex_peer_teams import _agents, _path, snapshot
from codex_work import text_field


def _rooms(db):
    return [json.loads(row[0]) for row in db.execute(
        "SELECT record FROM runtime_rooms WHERE id GLOB 'radio:*' AND json_type(record,'$.radio')='object'")]


def _save(runtime, db, room):
    room['updated'] = time.time()
    runtime.put(db, 'rooms', room)


def _valid(runtime, db, room, ready=False):
    team = next((t for t in snapshot(runtime, db) if t['id'] == room['radio']['teamId']
                 and t['projectPath'] == room['projectPath']), None)
    if not team or len(team['members']) != 2 or set(team['members']) != set(room['members']):
        return 'The shared chat membership changed. Its history stays with its original members.'
    agents = _agents(db)
    for key in room['members']:
        a = agents[key]
        if a.get('accountTransferId') or a.get('nativeThreadBlock'):
            return 'A participant is being transferred or its native chat is unavailable.'
        if ready and (not a.get('autoWake') or a.get('nativeFailureHold')):
            return 'A participant is stopped. Resume that chat before sending.'
    return None


def _message(db, room, key, sender, text, created=None):
    db.execute('INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) '
               'VALUES (?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET text=excluded.text',
               (key, room['id'], sender, text, time.time() if created is None else created, '{}'))


def _unsent_cancelled(db, active):
    event = _event(db, active)
    if not event or event['status'] != 'cancelled' or event.get('turn_id') or active.get('turnId'):
        return False
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (active['agentId'],)).fetchone()
    attempt = (json.loads(row[0]).get('startAttempt') or {}) if row else {}
    if active['eventId'] in attempt.get('events', []):
        return not (attempt.get('submitted') or attempt.get('observedTurnId') or attempt.get('turnId'))
    # Studio cancels radio events only before reservation/submission. An unknown
    # accepted input retains dispatching/uncertain/delivered status instead.
    return True


def _cancel_pending(db, active):
    db.execute("UPDATE runtime_events SET status='cancelled',error='Shared chat stopped' "
               "WHERE id=? AND status='pending'", (active['eventId'],))
    return _unsent_cancelled(db, active)


def manage(runtime, data):
    action = data.get('radio_action')
    if action not in {'open', 'send', 'pass', 'stop'}:
        raise ValueError('Unknown shared chat action')
    path = _path(data.get('path'))
    team_id = str(uuid.UUID(data.get('team_id', '')))
    request_id = text_field(data.get('request_id'), 'a request ID', 255)
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        key = 'radio:' + request_id
        signature, previous = runtime.operation_receipt(db, key, {'operation': 'radio', 'body': data})
        if previous is not None:
            return previous
        row = db.execute('SELECT record FROM runtime_rooms WHERE id=?', ('radio:' + team_id,)).fetchone()
        if row:
            room = json.loads(row[0])
            if room.get('projectPath') != path:
                raise ValueError('The shared chat belongs to another project')
        else:
            if action != 'open':
                raise ValueError('Open the shared chat first')
            team = next((t for t in snapshot(runtime, db) if t['id'] == team_id and t['projectPath'] == path), None)
            if not team or len(team['members']) != 2:
                raise ValueError('Select a team with exactly two chats in this project')
            room = {'id': 'radio:' + team_id, 'kind': 'private', 'members': list(team['members']),
                    'projectPath': path, 'customName': team['name'] + ' · Shared chat',
                    'created': time.time(), 'userHidden': False,
                    'radio': {'teamId': team_id, 'revision': 0, 'status': 'idle', 'speaker': None,
                              'next': [], 'active': None, 'error': None, 'seen': {}}}
        denial = _valid(runtime, db, room, ready=action in {'send', 'pass'})
        if denial and action != 'stop':
            raise ValueError(denial)
        radio = room['radio']
        if action != 'open':
            revision = data.get('expected_revision')
            if type(revision) is not int or revision != radio['revision']:
                raise ValueError('The shared chat changed. Reload before sending')
            radio['revision'] += 1
            if action in {'send', 'pass'}:
                target = data.get('target', 'both')
                if target != 'both' and target not in room['members']:
                    raise ValueError('Select a participant in this shared chat')
                rounds = data.get('rounds', 1)
                if type(rounds) is not int or rounds not in {1, 2}:
                    raise ValueError('Select one or two rounds')
                if action == 'pass' and target == 'both':
                    raise ValueError('Select the next speaker')
                text = (text_field(data.get('text'), 'a message', 32000) if action == 'send'
                        else 'Please give the next reply: ' + _agents(db)[target]['name'])
                _message(db, room, key, 'user', text)
                radio['next'] = list(room['members']) * rounds if target == 'both' else [target]
                # A new instruction must never replay an unresolved accepted turn.
                if radio['active'] is None:
                    radio.update(status='waiting', error=None)
                elif radio['status'] not in {'blocked', 'stopping'}:
                    radio['error'] = None
            else:
                radio['next'] = []
                if radio['active'] and _cancel_pending(db, radio['active']):
                    radio.update(active=None, speaker=None, status='idle', error=None)
                elif radio['active']:
                    radio.update(status='stopping', error=None)
                else:
                    radio.update(status='idle', speaker=None, error=None)
        _save(runtime, db, room)
        return runtime.save_receipt(db, key, signature, {'room': room})


def _identity(a):
    return [a.get('threadId'), a.get('epoch'), a.get('compactions', 0)]


def _bind_thread(agent, active):
    """Bind a newly prepared native thread only to its reserved radio input."""
    if active.get('threadId') is not None or not agent.get('threadId'):
        return False
    attempt = agent.get('startAttempt') or {}
    if (attempt.get('events') != [active['eventId']]
            or attempt.get('epoch') != active['epoch'] or agent.get('epoch') != active['epoch']
            or attempt.get('threadId') not in (None, agent['threadId'])):
        return False
    active['threadId'] = agent['threadId']
    active['identity'] = _identity(agent)
    return True


def _event(db, active):
    row = db.execute('SELECT * FROM runtime_events WHERE id=?', (active['eventId'],)).fetchone()
    return dict(row) if row else None


def _turn(a, active, event):
    if not event or event['agent'] != a['id'] or event['epoch'] != active['epoch']:
        return None
    if event.get('turn_id'):
        return event['turn_id']
    attempt = a.get('startAttempt') or {}
    if (attempt.get('events') == [active['eventId']] and attempt.get('epoch') == active['epoch']
            and attempt.get('submitted') and attempt.get('threadId') == a.get('threadId')):
        return attempt.get('turnId') or attempt.get('observedTurnId')
    return None


def _blocked(radio, error):
    radio.update(status='blocked', error=error, next=[])


def tick(runtime, db):
    rooms = _rooms(db)
    if not rooms:
        return
    agents = _agents(db)
    for room in rooms:
        radio = room['radio']
        before = json.dumps(radio, sort_keys=True)
        if radio['active'] and _unsent_cancelled(db, radio['active']):
            stopped = radio['status'] == 'stopping'
            radio.update(active=None, speaker=None, next=[],
                         status='idle' if stopped else 'blocked',
                         error=None if stopped else 'The shared input was cancelled before delivery. Send a new message to continue.')
        denial = _valid(runtime, db, room, ready=True)
        if denial and radio['status'] != 'stopping':
            _blocked(radio, denial)
            if radio['active'] and _cancel_pending(db, radio['active']):
                radio.update(active=None, speaker=None)
        elif radio['active']:
            active = radio['active']
            a = agents.get(active['agentId'])
            event = _event(db, active)
            if a:
                _bind_thread(a, active)
            if not a or a.get('epoch') != active['epoch'] or a.get('threadId') != active['threadId']:
                _blocked(radio, 'The participant context changed during its reply.')
            elif event is None:
                _blocked(radio, 'The shared input receipt is missing.')
            else:
                turn = _turn(a, active, event)
                if turn:
                    active['turnId'] = turn
                completed = turn and db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                                (a['id'] + ':' + turn,)).fetchone()
                if completed:
                    # Exact delivery and exact outcome are both required. A later native
                    # turn must not be mistaken for this shared reply.
                    if (event['status'] != 'delivered' or event.get('turn_id') != turn
                            or a.get('lastCompletedTurn') != turn):
                        _blocked(radio, 'The shared reply outcome is unconfirmed.')
                    else:
                        success = a.get('lastCompletedTurnStatus') == 'completed'
                        try:
                            if not denial:
                                _reconcile(runtime, db, room, a, turn)
                        except ValueError as error:
                            _blocked(radio, str(error))
                            _save(runtime, db, room)
                            continue
                        was_stopping = radio['status'] == 'stopping'
                        if success and not was_stopping and _pending_question(db, a['id'], turn):
                            radio.update(status='waiting', error=None)
                            if json.dumps(radio, sort_keys=True) != before:
                                radio['revision'] += 1
                                _save(runtime, db, room)
                            continue
                        if success:
                            radio['seen'][a['id']] = {'identity': active['identity'], 'seq': active['through']}
                        radio.update(active=None, speaker=None)
                        if was_stopping:
                            radio.update(status='idle', next=[], error=None)
                        elif success:
                            radio.update(status='waiting' if radio['next'] else 'idle', error=None)
                        else:
                            _blocked(radio, 'The shared reply failed or was interrupted. Send a new message to continue.')
                elif radio['status'] == 'stopping':
                    if turn and not active.get('interruptRequested'):
                        active['interruptRequested'] = True
                        runtime.pool.submit(runtime.interrupt, dict(a, turnId=turn))
                elif event['status'] in {'uncertain', 'failed', 'cancelled'}:
                    _blocked(radio, 'The shared input outcome is unknown or failed. It will not be sent again.')
                elif radio['status'] != 'blocked':
                    radio['status'] = 'speaking' if turn else 'waiting'
        if not denial and radio['active'] is None and radio['next'] and radio['status'] != 'blocked':
            a = agents[radio['next'][0]]
            # Do not take an agent away from independent work or join its input batch.
            busy = (a.get('inFlight') or a.get('startAttempt') and a.get('status') in {'starting', 'running'}
                    or a.get('status') in {'starting', 'running', 'approval'}
                    or db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND status IN "
                                  "('pending','reserved','dispatching','uncertain') LIMIT 1", (a['id'],)).fetchone())
            if not busy:
                seen = radio['seen'].get(a['id'], {})
                cursor = seen.get('seq', 0) if seen.get('identity') == _identity(a) else 0
                messages = [dict(row) for row in db.execute(
                    'SELECT seq,sender,text,created FROM runtime_chat_messages WHERE room=? AND seq>? ORDER BY seq',
                    (room['id'], cursor))]
                participants = [{'id': key, 'name': agents[key].get('name'),
                                 'model': agents[key].get('model'), 'provider': agents[key].get('provider', 'codex')}
                                for key in room['members']]
                context = {'roomId': room['id'], 'selfId': a['id'], 'participants': participants}
                payload = ('This is a shared chat with two equal agents. It is your turn to reply. '
                           'All your assistant text is visible to the user and the other participant. '
                           'Reply normally here. Do not forward this conversation through messaging tools. '
                           'Other independent work is not part of this room. Reply once, then yield.\n'
                           'Shared room (JSON):\n' + json.dumps(context, ensure_ascii=False) + '\n'
                           'Shared messages since your last successful reply (JSON):\n' + json.dumps(messages, ensure_ascii=False))
                if len(payload) > 200000:
                    _blocked(radio, 'Shared history exceeds the safe input size. No messages were omitted.')
                else:
                    event_id = 'radio-turn:' + str(uuid.uuid4())
                    runtime.enqueue(db, a, 'radio_turn', payload, event_id)
                    radio['next'].pop(0)
                    radio.update(status='waiting', speaker=a['id'], active={
                        'eventId': event_id, 'agentId': a['id'], 'epoch': a['epoch'],
                        'threadId': a.get('threadId'), 'identity': _identity(a),
                        'through': messages[-1]['seq'] if messages else cursor})
        if json.dumps(radio, sort_keys=True) != before:
            radio['revision'] += 1
            _save(runtime, db, room)


def validate_event(runtime, db, agent, event):
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent['id'],)).fetchone()
    if not row:
        return 'The shared participant no longer exists'
    agent = json.loads(row[0])
    for room in _rooms(db):
        radio = room['radio']
        active = radio.get('active') or {}
        if active.get('eventId') != event['id']:
            continue
        denial = _valid(runtime, db, room, ready=True)
        if denial:
            return denial
        if _bind_thread(agent, active):
            _save(runtime, db, room)
        if (radio['status'] in {'blocked', 'stopping'} or active.get('agentId') != agent['id']
                or active.get('epoch') != agent.get('epoch') or active.get('threadId') != agent.get('threadId')):
            return 'The shared turn is no longer authorized'
        return None
    return 'The shared turn has no active room receipt'


def select_pending(runtime, db, agent, rows):
    ordinary = [row for row in rows if row['kind'] != 'radio_turn']
    valid = [row for row in rows if row['kind'] == 'radio_turn' and validate_event(runtime, db, agent, row) is None]
    return valid[:1] if valid else ordinary


def holds_floor(runtime, db, agent):
    """Pause independent dispatch while an exact completed question holds the floor."""
    for room in _rooms(db):
        active = room['radio'].get('active') or {}
        if active.get('agentId') != agent['id']:
            continue
        turn = _turn(agent, active, _event(db, active))
        if (turn and _pending_question(db, agent['id'], turn)
                and db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                               (agent['id'] + ':' + turn,)).fetchone()):
            return True
    return False


def observe_item(runtime, db, agent_id, item_key, role, text, metadata):
    if role != 'assistant' or not isinstance(text, str) or not metadata.get('turnId'):
        return
    rooms = [room for room in _rooms(db) if (room['radio'].get('active') or {}).get('agentId') == agent_id]
    if not rooms:
        return
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
    if not row:
        return
    a = json.loads(row[0])
    for room in rooms:
        active = room['radio'].get('active') or {}
        if active.get('agentId') != agent_id or _valid(runtime, db, room):
            continue
        _bind_thread(a, active)
        if a.get('epoch') != active['epoch'] or a.get('threadId') != active['threadId']:
            continue
        turn = _turn(a, active, _event(db, active))
        if turn and turn == metadata['turnId']:
            _message(db, room, room['id'] + ':' + active['eventId'] + ':' + item_key, agent_id, text)
            _save(runtime, db, room)


def guard_message(runtime, db, sender, recipient):
    sender_id = sender['id'] if isinstance(sender, dict) else sender
    recipient_id = recipient['id'] if isinstance(recipient, dict) else recipient
    rooms = [room for room in _rooms(db) if (room['radio'].get('active') or {}).get('agentId') == sender_id]
    if not rooms:
        return
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (sender_id,)).fetchone()
    if not row:
        return
    a = json.loads(row[0])
    for room in rooms:
        active = room['radio'].get('active') or {}
        if active.get('agentId') != sender_id or recipient_id not in room['members']:
            continue
        turn = _turn(a, active, _event(db, active))
        if turn and turn == a.get('turnId') and a.get('inFlight'):
            raise ValueError('Your reply is shared automatically. Reply in this turn and let Studio pass the floor.')


def _reconcile(runtime, db, room, a, turn):
    for row in db.execute("SELECT id,record,created FROM runtime_items WHERE agent=? "
                          "AND json_extract(record,'$.turnId')=? AND json_extract(record,'$.role')='assistant' ORDER BY created,id",
                          (a['id'], turn)).fetchall():
        item = json.loads(row['record'])
        full = db.execute('SELECT body FROM runtime_search WHERE rowid=(SELECT search_rowid '
                          'FROM runtime_search_rows WHERE id=?)', (row['id'],)).fetchone()
        if full is not None:
            text = full[0]
        elif not item.get('truncated'):
            text = item.get('text', '')
        else:
            raise ValueError('The full shared reply is unavailable. No truncated reply was forwarded.')
        _message(db, room, room['id'] + ':' + room['radio']['active']['eventId'] + ':' + row['id'], a['id'], text, created=row['created'])


def guard_input(runtime, db, agent, question=None):
    """Keep independent main-chat instructions out of the current shared reply."""
    agent_id = agent['id'] if isinstance(agent, dict) else agent
    rooms = [room for room in _rooms(db)
             if (room['radio'].get('active') or {}).get('agentId') == agent_id]
    if not rooms:
        return
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
    if not row:
        return
    current = json.loads(row[0])
    if not current.get('inFlight'):
        return
    for room in rooms:
        active = room['radio']['active']
        turn = _turn(current, active, _event(db, active))
        if not turn or turn != current.get('turnId'):
            continue
        if (isinstance(question, dict) and question.get('method') == 'agent/asyncQuestion'
                and question.get('status') == 'pending' and question.get('agent') == agent_id
                and question.get('epoch') == current.get('epoch')
                and question.get('turnId') == turn):
            return
        raise ValueError('This agent is replying in the shared chat. Send there or choose queue.')


def _pending_question(db, agent_id, turn):
    return db.execute("SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
                      "AND json_extract(record,'$.turnId')=? "
                      "AND json_extract(record,'$.method')='agent/asyncQuestion' "
                      "AND json_extract(record,'$.status')='pending' LIMIT 1", (agent_id, turn)).fetchone()


def route_question_answer(runtime, db, question, text, accepted=False):
    """Return True when the answer replaces a completed shared question turn.

    Call before native send. If False, call again with accepted=True only after
    native acceptance to mirror an answer steered into the active shared turn.
    The caller marks the durable question answered in the same transaction.
    """
    if question.get('method') != 'agent/asyncQuestion' or not question.get('turnId'):
        return False
    agent_id = question.get('agent')
    for room in _rooms(db):
        radio = room['radio']
        active = radio.get('active') or {}
        if active.get('agentId') != agent_id:
            continue
        row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
        if not row:
            continue
        agent = json.loads(row[0])
        event = _event(db, active)
        turn = _turn(agent, active, event)
        if (turn != question['turnId'] or question.get('epoch') != active['epoch']
                or agent.get('epoch') != active['epoch'] or agent.get('threadId') != active['threadId']):
            continue
        denial = _valid(runtime, db, room, ready=True)
        if denial or radio['status'] in {'stopping', 'blocked'}:
            raise ValueError(denial or 'The shared reply is stopped or blocked')
        if accepted:
            _message(db, room, question['id'] + ':answer', 'user', text)
            _save(runtime, db, room)
            return False
        completed = db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?', (agent_id + ':' + turn,)).fetchone()
        if completed:
            if (event['status'] != 'delivered' or event.get('turn_id') != turn
                    or agent.get('lastCompletedTurn') != turn or agent.get('lastCompletedTurnStatus') != 'completed'):
                raise ValueError('The shared question turn outcome is unconfirmed')
            key = question['id'] + ':answer'
            exists = db.execute('SELECT text FROM runtime_chat_messages WHERE id=?', (key,)).fetchone()
            if exists and exists[0] != text:
                raise ValueError('This question already has a different answer')
            if not exists:
                _message(db, room, key, 'user', text)
                if not active.get('questionContinuationPlanned'):
                    radio['next'].insert(0, agent_id)
                    active['questionContinuationPlanned'] = True
                radio['revision'] += 1
                _save(runtime, db, room)
            return True
        return False
    return False
