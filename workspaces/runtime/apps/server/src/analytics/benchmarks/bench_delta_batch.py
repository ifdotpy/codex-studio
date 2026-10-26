"""Count production SQLite operations for sequential and aggregated deltas."""
from pathlib import Path
import re
import sqlite3
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from codex_analytics import AnalyticsMixin

SAMPLES = [
    {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'item', 'delta': text}
    for text in ('hello', ' мир', '\nsecond', '', '🙂', ' ending', '\n', 'done')
]
TIMES = [7201] * len(SAMPLES)
AGENT = {'id': 'bench-agent', 'name': 'Synthetic', 'rootId': 'bench-root',
         'threadId': 'thread', 'accountKey': 'default', 'model': 'synthetic'}
COUNTERS = ('agent_upsert', 'notification_upsert', 'turn_select', 'turn_upsert',
            'item_select', 'item_upsert', 'savepoint', 'release')


class Store(AnalyticsMixin):
    def records(self, _db, _kind):
        return []


def run(batch):
    store = Store()
    db = sqlite3.connect(':memory:')
    with patch('codex_analytics.time.time', return_value=9000):
        store.analytics_init(db)
        store.analytics_event(db, AGENT, 'turn/started', {'threadId': 'thread',
                            'turn': {'id': 'turn'}}, at=7000)
        store.analytics_event(db, AGENT, 'item/started', {'threadId': 'thread',
                            'turnId': 'turn', 'item': {'id': 'item', 'type': 'agentMessage'}}, at=7001)
    counts = dict.fromkeys(COUNTERS, 0)

    def trace(sql):
        normalized = re.sub(r'\s+', ' ', sql.strip()).upper()
        patterns = {
            'agent_upsert': 'INSERT INTO ANALYTICS_AGENTS',
            'notification_upsert': 'INSERT INTO ANALYTICS_NOTIFICATIONS',
            'turn_select': 'SELECT RECORD FROM ANALYTICS_TURNS',
            'turn_upsert': 'INSERT INTO ANALYTICS_TURNS',
            'item_select': 'SELECT RECORD FROM ANALYTICS_ITEMS',
            'item_upsert': 'INSERT INTO ANALYTICS_ITEMS',
            'savepoint': 'SAVEPOINT ',
            'release': 'RELEASE ',
        }
        for name, marker in patterns.items():
            if normalized.startswith(marker):
                counts[name] += 1

    db.set_trace_callback(trace)
    with db:
        if batch:
            store.analytics_delta_batch_safe(db, AGENT, SAMPLES, at_values=TIMES)
        else:
            for sample, at in zip(SAMPLES, TIMES):
                store.analytics_safe(db, store.analytics_event, AGENT,
                                    'item/agentMessage/delta', sample, at=at)
    db.set_trace_callback(None)
    result = {
        'notifications': db.execute('SELECT id,agent,root,method,hour,count,bytes FROM analytics_notifications ORDER BY id').fetchall(),
        'turns': db.execute('SELECT id,agent,root,at,record FROM analytics_turns ORDER BY id').fetchall(),
        'items': db.execute('SELECT id,agent,root,thread,turn,at,type,name,is_tool,record FROM analytics_items ORDER BY id').fetchall(),
    }
    db.close()
    return counts, result


def main():
    sequential_counts, sequential_rows = run(False)
    batch_counts, batch_rows = run(True)
    if sequential_rows != batch_rows:
        raise SystemExit('Production sequential and aggregate paths produced different rows')
    sequential_total = sum(sequential_counts.values())
    batch_total = sum(batch_counts.values())
    if batch_total >= sequential_total or batch_counts['notification_upsert'] >= sequential_counts['notification_upsert']:
        raise SystemExit('Aggregate path did not reduce the expected production SQL work')
    print(f'production_sql_statements sequential={sequential_total} aggregate={batch_total}')
    for name in COUNTERS:
        print(f'{name}: {sequential_counts[name]} -> {batch_counts[name]}')
    print(f'fragments={len(SAMPLES)}; rows_equal=true; timing_claim=none')


if __name__ == '__main__':
    main()
