"""Remove retired configuration without deleting conversation history."""

import json


def remove_review_assignments(runtime, db):
    changed = {'agents': 0, 'rooms': 0, 'pending': 0}
    for table, field in (('agents', 'reviewSchedules'), ('rooms', 'reviewTargets')):
        rows = db.execute(f"SELECT record FROM runtime_{table} WHERE json_type(record,?) IS NOT NULL",
                          ('$.' + field,)).fetchall()
        for row in rows:
            record = json.loads(row[0])
            record.pop(field, None)
            runtime.put(db, table, record)
            changed[table] += 1
    # Submitted and uncertain inputs retain their exact receipt and history.
    changed['pending'] = db.execute(
        "UPDATE runtime_events SET status='cancelled',error='Scheduled chat reviews have been removed' "
        "WHERE kind='chat_review' AND status='pending'").rowcount
    return changed
