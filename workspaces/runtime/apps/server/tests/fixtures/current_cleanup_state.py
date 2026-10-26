"""One-time conversion for the current Studio deployment.

Call under Runtime.lock in the deployment transaction before replacing methods.
No native request or event is submitted here.
"""
import json


def convert_completed_budgets(db):
    """Preserve all charges; refuse to discard an unfinished usage import."""
    missing = db.execute('SELECT a.id FROM runtime_agents a '
                         'WHERE NOT EXISTS (SELECT 1 FROM runtime_budget b WHERE b.id=a.id) '
                         'AND EXISTS (SELECT 1 FROM analytics_usage u WHERE u.agent=a.id) LIMIT 1').fetchone()
    if missing:
        raise ValueError('Initialize the saved usage ledger before updating Studio: ' + missing[0])
    rows = db.execute('SELECT id,record FROM runtime_budget').fetchall()
    converted = []
    for row in rows:
        state = json.loads(row['record'])
        if state.get('migrationSeq', 0) < state.get('migrationEnd', 0):
            raise ValueError('Finish the budget usage import before updating Studio: ' + row['id'])
        if 'legacy' in state:
            if 'historicalNotices' in state and state['historicalNotices'] != state['legacy']:
                raise ValueError('Conflicting historical budget counters: ' + row['id'])
            state['historicalNotices'] = state.pop('legacy')
        if 'historicalNotices' not in state:
            raise ValueError('Missing historical budget counter: ' + row['id'])
        state.pop('migrationSeq', None)
        state.pop('migrationEnd', None)
        converted.append((json.dumps(state), row['id']))
    db.executemany('UPDATE runtime_budget SET record=? WHERE id=?', converted)
    db.execute('DROP INDEX IF EXISTS analytics_usage_migration')
    return len(converted)


def convert_user_task_recipients(runtime, db):
    """Move only proven unsent completion events, keeping their identities."""
    changed = 0
    for event in db.execute("SELECT * FROM runtime_events WHERE kind='user_task_completed' AND status='pending' AND turn_id IS NULL").fetchall():
        try:
            task_id = json.loads(event['text'])['task_id']
        except (ValueError, TypeError, KeyError):
            continue
        row = db.execute('SELECT record FROM runtime_user_tasks WHERE id=?', (task_id,)).fetchone()
        if not row:
            continue
        task = json.loads(row[0])
        if event['agent'] != task['agent'] or task['agent'] == task['rootId']:
            continue
        owner, lead = runtime.agent(task['agent'], db), runtime.agent(task['rootId'], db)
        if (owner.get('deletedAt') or lead.get('deletedAt') or not lead.get('isLead')
                or owner['epoch'] != event['epoch']):
            continue
        status = 'pending' if lead['autoWake'] else 'cancelled'
        db.execute('UPDATE runtime_events SET agent=?,epoch=?,status=? WHERE id=?',
                   (lead['id'], lead['epoch'], status, event['id']))
        task['delivery'] = status
        runtime.put(db, 'user_tasks', task)
        if lead['autoWake'] and not lead.get('nativeFailureHold') and lead['status'] not in {'running', 'starting', 'approval'}:
            lead['status'] = 'queued'
            runtime.put(db, 'agents', lead)
        if owner['status'] == 'queued' and not db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND status='pending'", (owner['id'],)).fetchone():
            owner['status'] = 'waiting'
            runtime.put(db, 'agents', owner)
        changed += 1
    return changed


def convert_unsent_transfer_members(runtime, db):
    """Retire unsent worker transfers without changing native mutation receipts."""
    changed = 0
    for op in runtime.records(db, 'account_transfers'):
        if op.get('status') != 'pending':
            continue
        dirty = False
        for aid, member in list(op['members'].items()):
            if aid == op['leadId'] or member['phase'] != 'waiting' or member.get('result'):
                continue
            transfers = getattr(runtime, '_account_transfers', None)
            if transfers and (aid in transfers.running or (op['id'], aid) in transfers.futures):
                raise ValueError('Wait for the transfer worker before updating Studio: ' + aid)
            agent = runtime.agent(aid, db)
            if agent.get('accountTransferId') == op['id']:
                agent.pop('accountTransferId')
                runtime.put(db, 'agents', agent)
            op.setdefault('retiredMembers', {})[aid] = member
            del op['members'][aid]
            dirty = True
            changed += 1
        if dirty:
            # Keep the stored operation timestamps and summaries as audit evidence.
            runtime.put(db, 'account_transfers', op)
    return changed
