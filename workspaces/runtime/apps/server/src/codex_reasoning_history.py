"""Project measured reasoning intervals into the visible transcript, without text."""
import bisect
import json
import math
import time


def finite(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def reasoning_history(db, actor, rows, items, now=None):
    if not rows:
        return items
    now = time.time() if now is None else now
    positions = sorted((row['created'], row['id']) for row in rows)
    lower, upper = positions[0][0], now
    following = db.execute(
        "SELECT created FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL "
        "AND (created,id)>(?,?) ORDER BY created,id LIMIT 1",
        (actor['id'], *positions[-1]),
    ).fetchone()
    if following:
        upper = following[0]
    turns = {item.get('turnId') for item in items if item.get('turnId')}
    live = actor.get('inFlight') and actor.get('status') == 'running'
    groups = {}
    for row in db.execute(
        "SELECT record FROM analytics_items WHERE agent=? AND at>=? AND at<? AND type='reasoning' ORDER BY at,id",
        (actor['id'], lower, upper),
    ):
        record = json.loads(row[0])
        start, end = record.get('startedAt'), record.get('finishedAt')
        turn = record.get('turnId')
        if turn not in turns or not finite(start):
            continue
        observed_wait = False
        if end is None:
            next_row = db.execute(
                "SELECT at FROM analytics_items WHERE agent=? AND at>? AND at<? "
                "AND json_extract(record,'$.turnId')=? ORDER BY at LIMIT 1",
                (actor['id'], start, upper, turn),
            ).fetchone()
            next_message = next((at for at, key in positions if at > start
                                 and any(m['id'] == key and m.get('turnId') == turn for m in items)), None)
            bounds = [v for v in [next_row[0] if next_row else None, next_message] if v is not None]
            if bounds:
                # A missing completion is not proof of continuous reasoning.
                end, observed_wait = min(bounds), True
        running = (end is None and live and actor.get('turnId') == turn
                   and (actor.get('activity') or {}).get('phase') == 'thinking')
        if end is None and not running:
            continue  # No measured end: do not invent duration after a disconnect.
        if not running and (not finite(end) or end < start):
            continue
        slot = bisect.bisect_right(positions, (start, '\uffff')) - 1
        if slot < 0:
            continue
        key = (slot, turn)
        group = groups.setdefault(key, {'id': 'reasoning:' + record['id'], 'start': start,
                                       'intervals': [], 'runningSince': None, 'observedWait': False})
        group['observedWait'] |= observed_wait
        if running:
            group['runningSince'] = start
        else:
            group['intervals'].append((start, end))
    additions = {}
    for (slot, turn), group in groups.items():
        duration, edge = 0, 0
        for start, end in sorted(group['intervals']):
            duration += max(0, end - max(start, edge))
            edge = max(edge, end)
        since = group['runningSince']
        if since is not None:
            since = max(since, edge)
        if duration < 1 and since is None:
            continue
        additions.setdefault(positions[slot][1], []).append({
            'id': group['id'], 'role': 'reasoning', 'text': '', 'turnId': turn,
            'at': group['start'], 'reasoningMs': duration * 1000,
            'reasoningSince': since, 'reasoningObservedAt': now,
            'observedWait': group['observedWait'],
        })
    result = []
    for item in items:
        result.append(item)
        result.extend(additions.get(item['id'], []))
    return result
