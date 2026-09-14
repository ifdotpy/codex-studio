"""Retire proven obsolete notifications before native turn reservation."""

import json


def complaint_records(db, agent, text):
    try:
        value = json.loads(text)
        entries = value['complaints']
        if not isinstance(entries, list) or not entries:
            return None
        complaints = []
        for entry in entries:
            cid = entry['complaint_id']
            if not isinstance(cid, str) or not cid:
                return None
            saved = db.execute('SELECT record FROM runtime_complaints WHERE id=?', (cid,)).fetchone()
            complaint = json.loads(saved[0]) if saved else None
            if not complaint or complaint['leadId'] != agent['id']:
                return None
            complaints.append(complaint)
        return complaints
    except (ValueError, TypeError, KeyError):
        return None


def reconcile_complaints(runtime, db, agent):
    for row in db.execute("SELECT id,text FROM runtime_events WHERE agent=? AND epoch=? "
                          "AND kind='complaint' AND status='pending'",
                          (agent['id'], agent['epoch'])).fetchall():
        complaints = complaint_records(db, agent, row['text'])
        if complaints and all(not runtime.complaint_needs_response(c) for c in complaints):
            db.execute("UPDATE runtime_events SET status='stored_only',error=? "
                       "WHERE id=? AND status='pending'",
                       ('Every referenced complaint already has a response', row['id']))


def pending_batch(runtime, db, agent):
    reconcile_complaints(runtime, db, agent)
    rows = [dict(row) for row in db.execute(
        "SELECT * FROM runtime_events WHERE agent=? AND status='pending' AND epoch=? ORDER BY created,rowid",
        (agent['id'], agent['epoch']))]
    groups = {}
    metadata_by_id = {}
    for row in rows:
        if row['kind'] == 'complaint' and complaint_records(db, agent, row['text']) is None:
            row['preserveComplaint'] = True
        meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (row['id'],)).fetchone()
        metadata_by_id[row['id']] = json.loads(meta[0]) if meta else {}
        if metadata_by_id[row['id']].get('preserveProgress'):
            row['preserveProgress'] = True
        if row['kind'] != 'agent_message':
            continue
        row['preserveProgress'] = True
        try:
            value = json.loads(row['text'])
            if (not isinstance(value, dict) or value.get('importance') != 'progress'
                    or any(not isinstance(value.get(k), str) or not value[k]
                           for k in ('sender', 'room', 'progress_key'))
                    or type(value.get('progress_version')) is not int or value['progress_version'] < 0):
                continue
            if not metadata_by_id[row['id']].get('preserveProgress'):
                row.pop('preserveProgress', None)
            key = tuple(value[k] for k in ('sender', 'room', 'progress_key'))
            groups.setdefault(key, []).append((row, value))
        except (ValueError, TypeError, KeyError):
            continue
    obsolete = set()
    for group in groups.values():
        versions = [value['progress_version'] for _, value in group]
        if len(set(versions)) != len(versions) or any(row.get('preserveProgress') for row, _ in group):
            for row, _ in group:
                row['preserveProgress'] = True
                metadata_by_id[row['id']]['preserveProgress'] = True
                db.execute('INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)',
                           (row['id'], json.dumps(metadata_by_id[row['id']])))
            continue
        if len(versions) < 2:
            continue
        latest, payload = max(group, key=lambda entry: entry[1]['progress_version'])
        for row, _ in group:
            if row['id'] == latest['id']:
                continue
            obsolete.add(row['id'])
            db.execute("UPDATE runtime_events SET status='stored_only',error=? WHERE id=? AND status='pending'",
                       ('Superseded by progress event ' + latest['id'], row['id']))
        # Only the model projection changes. Original payloads remain in events
        # and room history, including their replay signatures.
        metadata = metadata_by_id[latest['id']]
        metadata['earlierProgressUpdates'] = sum(metadata_by_id[row['id']].get('earlierProgressUpdates', 0)
                                                 for row, _ in group) + len(group) - 1
        db.execute('INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)', (latest['id'], json.dumps(metadata)))
    for row in rows:
        count = metadata_by_id[row['id']].get('earlierProgressUpdates', 0)
        if count and row['id'] not in obsolete:
            row['text'] = json.dumps({**json.loads(row['text']), 'earlierProgressUpdates': count,
                                    'history': 'Read earlier progress with orchestration_chat_read in this room.'})
    return [row for row in rows if row['id'] not in obsolete]


def _obsolete_reason(runtime, db, agent, row):
    if row['kind'] == 'complaint':
        complaints = complaint_records(db, agent, row['text'])
        if complaints and all(not runtime.complaint_needs_response(c) for c in complaints):
            return 'Every referenced complaint already has a response'
    if row['kind'] == 'work_review':
        try:
            payload = json.loads(row['text'])
            task_id, result_id = payload['task'], payload['result']['id']
            if not isinstance(task_id, str) or not isinstance(result_id, str):
                return None
            stored = db.execute('SELECT record FROM runtime_work WHERE id=?', (task_id,)).fetchone()
            work = json.loads(stored[0]) if stored else None
            if (work and work['rootId'] == agent['id']
                    and any(result['id'] == result_id for result in work['results'])
                    and any(decision['resultId'] == result_id for decision in work['decisions'])):
                return 'The exact result already has a decision'
        except (ValueError, TypeError, KeyError):
            pass
    return None


def reconcile_start(runtime, db, agent, rows):
    """Finish an obsolete, proven unsubmitted batch and retain its other input.

    The caller holds the same lock used by native submission. A delivered,
    uncertain, mismatched, or submitted batch never enters this path.
    """
    attempt = agent.get('startAttempt') or {}
    if (attempt.get('submitted') is not False or attempt.get('turnId') or attempt.get('observedTurnId')
            or attempt.get('epoch') != agent['epoch']
            or attempt.get('events') != [row['id'] for row in rows]):
        return False
    saved = [db.execute('SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                        (row['id'], agent['id'], agent['epoch'])).fetchone() for row in rows]
    if not saved or any(row is None or row['status'] not in {'reserved', 'dispatching'} for row in saved):
        return False
    obsolete = {row['id']: reason for row in saved if (reason := _obsolete_reason(runtime, db, agent, row))}
    if not obsolete:
        return False
    for row in saved:
        db.execute('UPDATE runtime_events SET status=?,error=? WHERE id=?',
                   ('stored_only' if row['id'] in obsolete else 'pending', obsolete.get(row['id']), row['id']))
    attempt.update(notSubmittedReason='Notifications resolved before native submission',
                   retiredEvents=list(obsolete))
    pending = db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND status='pending' LIMIT 1",
                         (agent['id'], agent['epoch'])).fetchone()
    agent.update(inFlight=False, status='queued' if pending else 'waiting')
    runtime.put(db, 'agents', agent)
    runtime.changed.set()
    return True
