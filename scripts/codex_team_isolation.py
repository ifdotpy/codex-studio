"""Check durable agent deliveries against the current team boundary."""

import json


SENSITIVE_KINDS = ('agent_message', 'chat_review', 'complaint_response', 'followup')
DENIED = 'Agent communication is allowed only within one team'


def _object(text):
    try:
        value = json.loads(text)
    except (TypeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _agent(db, agent_id):
    if not isinstance(agent_id, str) or not agent_id:
        return {}
    row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent_id,)).fetchone()
    return _object(row[0]) if row else {}


def _same_team(db, recipient, sender_id):
    sender = _agent(db, sender_id)
    return bool(recipient.get('rootId') and sender.get('rootId') == recipient['rootId'])


def validate_event(runtime, db, recipient, event):
    """Return a denial reason, or None. Do not mutate records or call a model."""
    from codex_chat_reviews import review_pair_allowed
    kind = event['kind']
    if kind not in SENSITIVE_KINDS:
        return None
    recipient = _agent(db, recipient['id'])
    if not recipient.get('rootId'):
        return DENIED + ': the recipient team is unavailable'
    if kind == 'followup':
        row = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (event['id'],)).fetchone()
        # Restart, account-transfer, and saved-task continuations are system
        # followups without a sender. Agent sends record senderId.
        if row is None:
            return None
        metadata = _object(row[0])
        if 'senderId' not in metadata:
            return None
        sender_id = metadata['senderId']
    elif kind == 'chat_review':
        # The scheduler owns this durable key and its participant identities.
        parts = str(event['id']).split(':')
        if len(parts) != 5 or parts[0] != 'review' or parts[2] != recipient['id']:
            return DENIED + ': the review participants are unavailable'
        sender_id = parts[1]
    else:
        payload = _object(event['text'])
        if kind == 'agent_message':
            sender_id = payload.get('sender')
            room_id = payload.get('room')
            if not isinstance(room_id, str):
                return DENIED + ': the message room is unavailable'
            row = db.execute('SELECT record FROM runtime_rooms WHERE id=?', (room_id,)).fetchone()
            room = _object(row[0]) if row else {}
            if room.get('kind') == 'broadcast':
                if room.get('rootId') != recipient['rootId'] or room.get('rootId') == 'all':
                    return DENIED + ': global and foreign broadcasts are disabled'
            elif room.get('kind') == 'private':
                members = room.get('members')
                if (not isinstance(members, list) or sender_id not in members
                        or recipient['id'] not in members
                        or (not all(_same_team(db, recipient, member) for member in members)
                            and not (len(members) == 2 and review_pair_allowed(db, *members)))):
                    return DENIED + ': the message room crosses team boundaries'
            else:
                return DENIED + ': the message room is unavailable'
        else:
            if not isinstance(payload.get('complaint_id'), str):
                return DENIED + ': the complaint is unavailable'
            row = db.execute('SELECT record FROM runtime_complaints WHERE id=?',
                             (payload.get('complaint_id'),)).fetchone()
            complaint = _object(row[0]) if row else {}
            if complaint.get('author') != recipient['id']:
                return DENIED + ': the complaint recipient does not match'
            sender_id = payload.get('responder', payload.get('lead'))
            response = payload.get('response')
            if not isinstance(response, dict) or response.get('author') != sender_id:
                return DENIED + ': the complaint responder is unavailable'
            if sender_id == 'user':
                return None
            if complaint.get('leadId') != sender_id:
                return DENIED + ': the complaint responder does not match'
    if not _same_team(db, recipient, sender_id):
        if kind not in {'agent_message', 'chat_review'} or not review_pair_allowed(db, recipient['id'], sender_id):
            return DENIED
        if kind == 'chat_review':
            target = _agent(db, sender_id)
            entry = next((s for s in target.get('reviewSchedules', []) if s.get('reviewerId') == recipient['id']), None)
            if (not entry or not entry.get('enabled') or entry.get('removed')
                    or str(entry.get('revision')) != parts[3]):
                return DENIED + ': the review assignment changed'
    return None


def assert_events(runtime, db, recipient, rows):
    """Refuse the complete batch before any native submission."""
    for event in rows:
        reason = validate_event(runtime, db, recipient, event)
        if reason:
            raise ValueError(reason)


def cancel_pending(runtime, db):
    """Cancel only unsent forbidden events. Retain messages and delivery history."""
    rows = db.execute("SELECT id,agent,kind,text FROM runtime_events WHERE status='pending' "
                      "AND kind IN ('agent_message','chat_review','complaint_response','followup')").fetchall()
    cancelled = 0
    for event in rows:
        reason = validate_event(runtime, db, {'id': event['agent']}, event)
        if reason is None:
            continue
        result = db.execute("UPDATE runtime_events SET status='cancelled',error=? WHERE id=? AND status='pending'",
                            (reason, event['id']))
        if not result.rowcount:
            continue
        cancelled += 1
        if event['kind'] == 'agent_message':
            payload = _object(event['text'])
            if not all(isinstance(payload.get(field), str) for field in ('message_id', 'room', 'sender')):
                continue
            message = db.execute('SELECT id,deliveries FROM runtime_chat_messages WHERE id=? AND room=? AND sender=?',
                                 (payload.get('message_id'), payload.get('room'), payload.get('sender'))).fetchone()
            if message:
                deliveries = _object(message['deliveries'])
                if deliveries.get(event['agent']) in {'queued', 'pending'}:
                    deliveries[event['agent']] = 'cancelled'
                    db.execute('UPDATE runtime_chat_messages SET deliveries=? WHERE id=?',
                               (json.dumps(deliveries), message['id']))
    if cancelled:
        runtime.changed.set()
    return cancelled
