"""Read-only pages of managed conversation history."""
import json


def resolve_item(db, agent, identity):
    row = db.execute("SELECT id,created,record FROM runtime_items WHERE agent=? AND id=? AND json_extract(record,'$.afterRestore') IS NULL", (agent, identity)).fetchone()
    if row:
        return row
    # Displayed user inputs have identities distinct from their batch container.
    for row in db.execute("SELECT id,created,record FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL", (agent,)):
        item = json.loads(row['record'])
        if any(identity == (agent + ':' + entry['id'] if entry.get('id') else item['id'] + ':' + str(index)) for index, entry in enumerate(item.get('inputs', []))):
            return row
    raise ValueError('Unknown message in this conversation')


def history_rows(db, agent, before=None, around=None, limit=120, after=None):
    limit = max(1, min(500, int(limit)))
    if sum(bool(value) for value in (before, around, after)) > 1:
        raise ValueError('Use one history cursor')
    base = "SELECT id,created,record FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL"
    args = [agent]
    if after:
        anchor = resolve_item(db, agent, after)
        newer = db.execute(base + " AND (created,id)>(?,?) ORDER BY created,id LIMIT ?", (agent, anchor['created'], anchor['id'], limit)).fetchall()
        return list(reversed(newer)), limit
    if before or around:
        anchor = resolve_item(db, agent, around or before)
        if around:
            # Include newer context, then fill the page toward older history.
            newer = db.execute(base + " AND (created,id)>(?,?) ORDER BY created,id LIMIT ?", (agent, anchor['created'], anchor['id'], limit // 2)).fetchall()
            if newer:
                anchor = newer[-1]
        base += " AND (created,id)" + ('<=' if around else '<') + "(?,?)"
        args += [anchor['created'], anchor['id']]
    rows = db.execute(base + " ORDER BY created DESC,id DESC LIMIT ?", (*args, limit + 1)).fetchall()
    return rows, limit


def search_history(runtime, agent, query, limit=100):
    query = str(query or '').strip()
    if not query or len(query) > 500:
        raise ValueError('Enter a search query of 1 to 500 characters')
    limit = max(1, min(500, int(limit)))
    results = []
    with runtime.lock, runtime.db() as db:
        runtime.checked_actor(db, agent)
        for row in db.execute("SELECT i.record,s.body FROM runtime_items i LEFT JOIN runtime_search_rows address ON address.id=i.id LEFT JOIN runtime_search s ON s.rowid=address.search_rowid WHERE i.agent=? AND json_extract(i.record,'$.afterRestore') IS NULL ORDER BY i.created DESC,i.id DESC", (agent,)):
            item = json.loads(row['record'])
            entries = item.get('inputs')
            candidates = []
            if entries is not None:
                for index, entry in enumerate(entries):
                    event = db.execute('SELECT text FROM runtime_events WHERE id=? AND agent=?', (entry.get('id'), agent)).fetchone()
                    candidates.append({**item, **entry, 'id': agent + ':' + entry['id'] if entry.get('id') else item['id'] + ':' + str(index), 'sourceId': item['id'], 'clientMessageId': entry.get('id'), 'role': 'user' if entry.get('kind') == 'user' else item['role'], 'text': event[0] if event else entry.get('text', '')})
            else:
                candidates.append({**item, 'sourceId': item['id'], 'text': row['body'] or item.get('text', '')})
            for candidate in candidates:
                text = candidate['text']
                position = text.casefold().find(query.casefold())
                if position < 0:
                    continue
                results.append({k: candidate.get(k) for k in ('id','sourceId','clientMessageId','role','turnId','text')} | {'agent': agent, 'excerpt': text[max(0, position-80):position+len(query)+160]})
                if len(results) > limit:
                    return {'results': results[:limit], 'truncated': True}
    return {'results': results, 'truncated': False}


def history_item(runtime, agent, identity):
    with runtime.lock, runtime.db() as db:
        runtime.checked_actor(db, agent)
        row = resolve_item(db, agent, identity)
        item = json.loads(row['record'])
        if item.get('inputs'):
            for index, entry in enumerate(item['inputs']):
                display_id = agent + ':' + entry['id'] if entry.get('id') else item['id'] + ':' + str(index)
                if identity != display_id:
                    continue
                event = db.execute('SELECT text FROM runtime_events WHERE id=? AND agent=?', (entry.get('id'), agent)).fetchone()
                return {**item, **entry, 'id': display_id, 'sourceId': item['id'], 'agent': agent,
                    'role': 'user' if entry.get('kind') == 'user' else item['role'],
                    'text': event[0] if event else entry.get('text', ''),
                    'truncated': False if event else entry.get('truncated', False)}
        full = db.execute('SELECT body FROM runtime_search WHERE rowid=(SELECT search_rowid FROM runtime_search_rows WHERE id=?)', (item['id'],)).fetchone()
        return {**item, 'agent': agent, 'sourceId': item['id'],
            'text': full[0] if full else item.get('text', ''),
            'truncated': False if full else item.get('truncated', False)}
