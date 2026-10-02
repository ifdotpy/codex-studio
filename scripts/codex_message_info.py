"""Read exact saved message metadata without changing analytics or transcript state."""

import json


def message_info(runtime, agent, item_id, turn_id=None, thread=None):
    runtime.agent(agent)
    if not isinstance(item_id, str) or not item_id or len(item_id) > 512:
        raise ValueError('Select a message item')
    result = {}
    with runtime.analytics_read_connection() as db:
        db.execute('BEGIN')
        where, args = 'agent=? AND json_extract(record,\'$.itemId\')=?', [agent, item_id]
        if thread:
            where += ' AND thread=?'
            args.append(thread)
        rows = db.execute('SELECT record FROM analytics_items WHERE ' + where + ' LIMIT 2', args).fetchall()
        item = json.loads(rows[0][0]) if len(rows) == 1 else None
        if item and item.get('type') == 'agentMessage' and (not turn_id or item.get('turnId') == turn_id):
            thread, turn_id = item.get('threadId'), item.get('turnId')
            result['at'] = item.get('finishedAt') or item.get('startedAt') or item.get('at')
        else:
            item = None
        if not turn_id:
            return result
        where, args = 'agent=? AND json_extract(record,\'$.turnId\')=?', [agent, turn_id]
        if thread:
            where += ' AND json_extract(record,\'$.threadId\')=?'
            args.append(thread)
        rows = db.execute('SELECT record FROM analytics_turns WHERE ' + where + ' LIMIT 2', args).fetchall()
        if len(rows) != 1:
            return result
        turn = json.loads(rows[0][0])
        thread = turn.get('threadId')
        for field in ('model', 'effort', 'accountKey'):
            if turn.get(field):
                result[field] = turn[field]
        if result.get('accountKey'):
            # AccountStore.get reads credentials and changes profile metadata.
            # Read only the saved profile's display fields under its lock.
            with runtime.accounts.lock:
                account = runtime.accounts.data['accounts'].get(result['accountKey'], {})
                result['accountLabel'] = account.get('label')
                if account:
                    result['provider'] = account.get('provider', 'codex')
        result['turnDurationMs'] = turn.get('nativeDurationMs', turn.get('durationMs'))
        # A turn can contain several provider responses and several messages.
        # Only a direct identity, or one exact response and one assistant item,
        # establishes this message's usage. Do not copy turn totals to each item.
        usage_rows = db.execute("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND turn=? "
                                "AND json_extract(record,'$.responseId') IS NOT NULL LIMIT 2",
                                (agent, thread, turn_id)).fetchall()
        usage = [json.loads(row[0]) for row in usage_rows]
        exact = db.execute("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND turn=? "
                           "AND json_extract(record,'$.responseId')=? LIMIT 2",
                           (agent, thread, turn_id, item_id)).fetchall()
        response = json.loads(exact[0][0]) if len(exact) == 1 else None
        if not response and item and len(usage) == 1 and turn.get('status') in {'completed', 'failed', 'interrupted'}:
            messages = db.execute("SELECT record FROM analytics_items WHERE agent=? AND thread IS ? AND turn=? "
                                  "AND type='agentMessage' LIMIT 2", (agent, thread, turn_id)).fetchall()
            all_usage = db.execute('SELECT id FROM analytics_usage WHERE agent=? AND thread IS ? AND turn=? LIMIT 2',
                                   (agent, thread, turn_id)).fetchall()
            tool = db.execute('SELECT 1 FROM analytics_items WHERE agent=? AND thread IS ? AND turn=? AND is_tool=1 LIMIT 1',
                              (agent, thread, turn_id)).fetchone()
            if len(all_usage) == 1 and not tool and len(messages) == 1 and json.loads(messages[0][0]).get('itemId') == item_id:
                response = usage[0]
        if response:
            result['model'] = response.get('model') or result.get('model')
            result['tokens'] = response.get('requestUsage') or response.get('last')
            response_id = response.get('responseId')
            rates = runtime.__dict__.get('_token_rates')
            if response_id and rates:
                sample = rates.response_rate(agent, thread, turn_id, response_id)
                if sample:
                    result['responseRate'] = sample['rate']
    return {key: value for key, value in result.items() if value is not None}
