"""Durable measurements of observed protocol usage, never per-tool billing guesses."""
from __future__ import annotations

import hashlib
import json
import math
import struct
import time
from collections import defaultdict, deque
from contextlib import nullcontext
import statistics
import threading

from codex_budget import budget_capture

TOKEN_FIELDS = ('inputTokens', 'cachedInputTokens', 'cacheWriteInputTokens',
                'outputTokens', 'reasoningOutputTokens', 'totalTokens')
NON_TOOLS = {'userMessage', 'agentMessage', 'reasoning', 'plan', 'contextCompaction', 'compactionSnapshot'}


def encoded(value):
    return value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def number(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0 else None


def payload_size(value):
    """Sizes describe our UTF-8 text/compact JSON representation, not transport framing."""
    if value is None:
        return None
    text = encoded(value)
    result = {'bytes': len(text.encode('utf-8')), 'chars': len(text),
              'lines': text.count('\n') + bool(text), 'imageCount': 0,
              'imageBytes': 0, 'images': [], 'format': 'text' if isinstance(value, str) else 'json'}
    def walk(node):
        if isinstance(node, list):
            for child in node:
                walk(child)
        elif isinstance(node, dict):
            kind = node.get('type', '')
            if kind in {'image', 'inputImage', 'input_image', 'image_url', 'localImage'}:
                result['imageCount'] += 1
                url = node.get('imageUrl') or node.get('image_url') or node.get('url') or ''
                if isinstance(url, dict):
                    url = url.get('url', '')
                data = node.get('data')
                image = {'bytes': None, 'width': None, 'height': None}
                if isinstance(url, str) and url.startswith('data:') and ';base64,' in url:
                    data = url.split(';base64,', 1)[1]
                if isinstance(data, str):
                    # Do not allocate a second decoded copy of a potentially large image.
                    compact = ''.join(data.split())
                    image['bytes'] = max(0, len(compact) * 3 // 4 - (len(compact) - len(compact.rstrip('='))))
                    result['imageBytes'] += image['bytes']
                    try:
                        import base64
                        header = base64.b64decode(compact[:32])
                        if header.startswith(b'\x89PNG\r\n\x1a\n') and len(header) >= 24:
                            image['width'], image['height'] = struct.unpack('>II', header[16:24])
                    except (ValueError, struct.error):
                        pass
                result['images'].append(image)
            else:
                for child in node.values():
                    walk(child)
    walk(value)
    if any(image['bytes'] is None for image in result['images']):
        result['imageBytes'] = None
    return result


def item_payload_values(kind, item):
    input_value = item.get('arguments', item.get('command', item.get('query')))
    output_value = item.get('aggregatedOutput', item.get('contentItems', item.get('result')))
    if kind == 'fileChange':
        output_value = item.get('changes')
    elif kind == 'userMessage':
        input_value = item.get('content')
    elif kind == 'agentMessage':
        output_value = item.get('text')
    elif kind == 'reasoning':
        output_value = {k: item[k] for k in ('summary', 'content', 'text') if k in item} or None
    return input_value, output_value


def event_payload_measurements(method, p):
    """Measure rollout payloads before the importer takes the runtime lock."""
    item = p.get('item') or {}
    if not item and not p.get('itemId'):
        return {}
    if method in {'item/started', 'item/completed'}:
        if not item.get('type'):
            return None
        input_value, output_value = item_payload_values(item.get('type'), item)
        return {'input': payload_size(input_value) if input_value is not None else None,
                'output': payload_size(output_value) if output_value is not None else None}
    delta = p.get('delta')
    if isinstance(delta, str) and (method.endswith('Delta') or method.endswith('/delta')):
        return {'stream': payload_size(delta)}
    return {}


def model_payload_measurements(p):
    kind = p.get('type', 'unknown')
    if kind in {'function_call', 'custom_tool_call', 'tool_call'}:
        return {'input': payload_size(p.get('arguments', p.get('input')))}
    if kind in {'function_call_output', 'custom_tool_call_output', 'tool_result'}:
        return {'output': payload_size(p.get('output', p.get('content')))}
    direction = 'input' if p.get('role') in {'user', 'system', 'developer'} else 'output'
    return {direction: payload_size(p.get('content', p.get('summary')))}


def nullable_sum(values):
    known = [n for n in values if n is not None]
    return sum(known) if known else None


class _ItemTotals:
    def __init__(self, *, duration_samples=False, duration_total=False):
        self.count = 0
        self.failed = 0
        self.values = {}
        self.input_measurements = 0
        self.output_measurements = 0
        self.durations = [] if duration_samples else None
        self.duration_values = [] if duration_total else None

    def add(self, row):
        self.count += 1
        self.failed += row.get('status') == 'failed'
        for direction in ('input', 'output'):
            payload = row.get(direction)
            if payload is not None:
                if direction == 'input':
                    self.input_measurements += 1
                else:
                    self.output_measurements += 1
            for key in ('bytes', 'chars', 'imageCount'):
                value = (payload or {}).get(key)
                if value is not None:
                    slot = (direction, key)
                    self.values[slot] = self.values.get(slot, 0) + value
        duration = row.get('durationMs')
        if self.duration_values is not None and duration is not None:
            self.duration_values.append(duration)
        if self.durations is not None and number(duration) is not None:
            self.durations.append(duration)

    def size(self, direction, key):
        return self.values.get((direction, key))

    def duration_total(self):
        return sum(self.duration_values) if self.duration_values else None

    def duration(self):
        values = sorted(self.durations)
        return {'count': len(values), 'min': min(values) if values else None,
                'max': max(values) if values else None,
                'mean': statistics.mean(values) if values else None,
                'p50': statistics.median(values) if values else None,
                'p95': values[max(0, math.ceil(len(values) * .95) - 1)] if values else None}


class _UsageTotals:
    def __init__(self, *, metrics=True):
        self.metrics = metrics
        self.count = 0
        self.exact = 0
        self.legacy = 0
        self.baseline_missing = 0
        self.cache_pairs = 0
        self.peak_context = None
        self.peak_percent = None
        self.deltas = {field: [] for field in TOKEN_FIELDS}

    def add(self, row):
        self.count += 1
        for field in TOKEN_FIELDS:
            value = row['delta'].get(field)
            if value is not None:
                self.deltas[field].append(value)
        if not self.metrics:
            return
        self.exact += bool(row.get('responseId'))
        self.legacy += not row.get('responseId')
        self.baseline_missing += row['baselineMissing']
        delta = row['delta']
        if (number(delta.get('inputTokens')) is not None
                and number(delta.get('cachedInputTokens')) is not None
                and delta['cachedInputTokens'] <= delta['inputTokens']):
            self.cache_pairs += 1
        context = number(row['last'].get('totalTokens'))
        if context is not None:
            self.peak_context = max(self.peak_context, context) if self.peak_context is not None else context
            if row.get('modelContextWindow'):
                percent = row['last']['totalTokens'] / row['modelContextWindow'] * 100
                self.peak_percent = max(self.peak_percent, percent) if self.peak_percent is not None else percent

    def tokens(self):
        return {field: sum(values) if values else None for field, values in self.deltas.items()}


class _ContextBuckets:
    def __init__(self, limit=500):
        self.limit = limit
        self.width = 3600
        self.points = {}

    @staticmethod
    def percent(row):
        last = number(row.get('last', {}).get('totalTokens'))
        window = number(row.get('modelContextWindow'))
        return last / window if last is not None and window else None

    def add(self, row):
        percent = self.percent(row)
        at = number(row.get('at'))
        if percent is None or at is None:
            return
        bucket = int(at // self.width)
        previous = self.points.get(bucket)
        if previous is None or percent > self.percent(previous):
            self.points[bucket] = row
        while len(self.points) > self.limit:
            self.width *= 2
            compacted = {}
            for point in self.points.values():
                key = int(point['at'] // self.width)
                old = compacted.get(key)
                if old is None or self.percent(point) > self.percent(old):
                    compacted[key] = point
            self.points = compacted

    def rows(self):
        return sorted(self.points.values(), key=lambda row: row['at'])


class AnalyticsMixin:
    def analytics_connection(self, fallback=None):
        opener = getattr(self, 'analytics_db', None)
        return opener() if opener else nullcontext(fallback)

    def analytics_read_connection(self, fallback=None):
        opener = getattr(self, 'analytics_read_db', None)
        return opener() if opener else self.analytics_connection(fallback)

    def analytics_init(self, db):
        with self.analytics_connection(db) as analytics_db:
            analytics_db.executescript('''
          CREATE TABLE IF NOT EXISTS analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS analytics_turns (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, at REAL NOT NULL, record TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS analytics_turns_scope ON analytics_turns(agent,at);
          CREATE TABLE IF NOT EXISTS analytics_notifications (
            id TEXT PRIMARY KEY, agent TEXT, root TEXT, method TEXT, hour INTEGER,
            count INTEGER NOT NULL, bytes INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS analytics_limits (
            id INTEGER PRIMARY KEY AUTOINCREMENT, account TEXT NOT NULL, at REAL NOT NULL, record TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS analytics_usage (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
            agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT, at REAL NOT NULL, record TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS analytics_usage_scope ON analytics_usage(agent,at);
          CREATE INDEX IF NOT EXISTS analytics_usage_team ON analytics_usage(root,at);
          CREATE INDEX IF NOT EXISTS analytics_usage_root_seq ON analytics_usage(root,seq);
          CREATE INDEX IF NOT EXISTS analytics_usage_response ON analytics_usage(agent,thread,json_extract(record,'$.responseId'));
          CREATE INDEX IF NOT EXISTS analytics_usage_thread ON analytics_usage(agent,thread,seq);
          CREATE INDEX IF NOT EXISTS analytics_usage_migration ON analytics_usage(agent,seq);
          CREATE TABLE IF NOT EXISTS analytics_items (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT,
            at REAL NOT NULL, type TEXT, name TEXT, is_tool INTEGER NOT NULL, record TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS analytics_items_scope ON analytics_items(agent,at);
          CREATE INDEX IF NOT EXISTS analytics_items_team ON analytics_items(root,at);
          CREATE INDEX IF NOT EXISTS analytics_items_tool ON analytics_items(name,at);
            ''')
            analytics_db.execute('INSERT OR IGNORE INTO analytics_meta VALUES (?,?)', ('trackingSince', str(time.time())))
            for a in self.records(db, 'agents'):
                self.analytics_agent(analytics_db, a)

    def analytics_safe(self, db, operation, *args, **kwargs):
        """An analytics failure cannot consume a native result or lifecycle notice."""
        with self.analytics_connection(db) as analytics_db:
            if not analytics_db.in_transaction:
                analytics_db.execute('BEGIN')
            analytics_db.execute('SAVEPOINT analytics_capture')
            budget_context = self.__dict__.setdefault('_analytics_budget_context', threading.local())
            previous_budget_db = getattr(budget_context, 'connection', None)
            budget_context.connection = db
            try:
                result = operation(analytics_db, *args, **kwargs)
                budget_context.connection = previous_budget_db
                analytics_db.execute('RELEASE analytics_capture')
                return result
            except Exception as error:
                budget_context.connection = previous_budget_db
                analytics_db.execute('ROLLBACK TO analytics_capture')
                analytics_db.execute('RELEASE analytics_capture')
                detail = {'at': time.time(), 'operation': getattr(operation, '__name__', type(operation).__name__), 'error': str(error)[:1000]}
                try:
                    row = analytics_db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()
                    previous = json.loads(row[0]) if row else {'count': 0, 'last': None}
                    previous.update(count=previous['count'] + 1, last=detail)
                    analytics_db.execute("INSERT INTO analytics_meta VALUES ('captureErrors',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (json.dumps(previous),))
                except Exception:
                    import sys
                    print('Analytics capture failed: ' + json.dumps(detail), file=sys.stderr)
                return None

    def analytics_budget_capture(self, db, agent, params, *, at, source):
        # Budget admission and the visible agent fields remain runtime state;
        # keep that brief write on a runtime connection, outside analytics SQL.
        opener = getattr(self, 'db', None)
        if hasattr(self, 'analytics_db') and opener:
            context = self.__dict__.setdefault('_analytics_budget_context', threading.local())
            current = getattr(context, 'connection', None)
            if current is not None:
                return budget_capture(current, agent, params, at=at, source=source)
            with opener() as runtime_db:
                return budget_capture(runtime_db, agent, params, at=at, source=source)
        return budget_capture(db, agent, params, at=at, source=source)

    def analytics_delta_batch_safe(self, db, a, samples, *, at_values=None):
        """Capture coalesced live assistant deltas with the sequential semantics."""
        samples = list(samples)
        if not samples:
            return
        if at_values is None:
            at_values = [time.time() for _ in samples]
        if len(at_values) != len(samples):
            raise ValueError('One timestamp is required for each analytics sample')
        first = samples[0]
        identity = {key: value for key, value in first.items() if key != 'delta'} if isinstance(first, dict) else None
        if identity is None or any(not isinstance(sample, dict) or not isinstance(sample.get('delta'), str)
               or {key: value for key, value in sample.items() if key != 'delta'} != identity
               for sample in samples):
            for sample, at in zip(samples, at_values):
                self.analytics_safe(db, self.analytics_event, a, 'item/agentMessage/delta', sample, at=at)
            return

        def capture_batch(db):
            meta = self.analytics_agent(db, a)
            meta.update(threadId=first.get('threadId') or a.get('threadId'),
                        turnId=first.get('turnId') or (first.get('turn') or {}).get('id') or a.get('turnId'))
            buckets = {}
            for sample, at in zip(samples, at_values):
                hour = int(at // 3600) * 3600
                count, byte_count = buckets.get(hour, (0, 0))
                buckets[hour] = (count + 1, byte_count + len(encoded(sample).encode('utf-8')))
            for hour, (count, byte_count) in buckets.items():
                key = encoded([a['id'], 'item/agentMessage/delta', hour])
                db.execute('INSERT INTO analytics_notifications VALUES (?,?,?,?,?,?,?) '
                           'ON CONFLICT(id) DO UPDATE SET count=count+excluded.count,bytes=bytes+excluded.bytes',
                           (key, a['id'], meta['rootId'], 'item/agentMessage/delta', hour, count, byte_count))

            turn = meta['turnId']
            if turn:
                turn_key = ':'.join((a['id'], str(meta['threadId']), str(turn)))
                row = db.execute('SELECT record FROM analytics_turns WHERE id=?', (turn_key,)).fetchone()
                record = json.loads(row[0]) if row else {**meta, 'id': turn_key, 'at': at_values[0],
                    'startedAt': None, 'finishedAt': None, 'firstOutputAt': None, 'durationMs': None,
                    'firstOutputDelayMs': None, 'status': 'unknown', 'source': 'live'}
                if record['firstOutputAt'] is None:
                    for sample, at in zip(samples, at_values):
                        if sample.get('delta'):
                            record['firstOutputAt'] = at
                            break
                if record['startedAt'] is not None:
                    for event, metric in (('finishedAt', 'durationMs'), ('firstOutputAt', 'firstOutputDelayMs')):
                        if record[event] is not None:
                            record[metric] = max(0, (record[event] - record['startedAt']) * 1000)
                db.execute('INSERT INTO analytics_turns VALUES (?,?,?,?,?) '
                           'ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                           (turn_key, a['id'], meta['rootId'], record['at'], json.dumps(record)))

            item_id = first.get('itemId')
            if item_id:
                item_key = ':'.join((a['id'], str(meta['threadId']), str(item_id)))
                row = db.execute('SELECT record FROM analytics_items WHERE id=?', (item_key,)).fetchone()
                record = json.loads(row[0]) if row else None
                if record and record.get('finishedAt') is None:
                    stream = record.get('stream') or {'bytes': 0, 'chars': 0, 'lines': 0, 'deltas': 0}
                    stream['bytes'] += sum(len(sample['delta'].encode('utf-8')) for sample in samples)
                    stream['chars'] += sum(len(sample['delta']) for sample in samples)
                    stream['lines'] += sum(sample['delta'].count('\n') for sample in samples)
                    stream['deltas'] += len(samples)
                    record['stream'] = stream
                    self.analytics_store_item(db, record)

        try:
            with self.analytics_connection(db) as analytics_db:
                if not analytics_db.in_transaction:
                    analytics_db.execute('BEGIN')
                analytics_db.execute('SAVEPOINT analytics_delta_batch')
                try:
                    capture_batch(analytics_db)
                    analytics_db.execute('RELEASE analytics_delta_batch')
                except Exception:
                    analytics_db.execute('ROLLBACK TO analytics_delta_batch')
                    analytics_db.execute('RELEASE analytics_delta_batch')
                    raise
        except Exception:
            for sample, at in zip(samples, at_values):
                self.analytics_safe(db, self.analytics_event, a, 'item/agentMessage/delta', sample, at=at)

    def analytics_limit(self, db, account_key, value):
        if self.analytics_limit_changed(db, account_key, value):
            db.execute('INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)', (account_key, time.time(), json.dumps(value)))

    def analytics_limit_changed(self, db, account_key, value):
        """Skip a snapshot equal to the account's previous one; the time series keeps each change."""
        def content(record):
            if not isinstance(record, dict):
                return json.dumps(record, sort_keys=True)
            return json.dumps({k: v for k, v in record.items()
                               if k not in ('at', 'processedAt', 'checkedAt') and not k.startswith('_')},
                              sort_keys=True)
        cache = self.__dict__.setdefault('_analytics_last_limit', {})
        if account_key not in cache:
            row = db.execute('SELECT record FROM analytics_limits WHERE account=? ORDER BY at DESC LIMIT 1',
                             (account_key,)).fetchone()
            cache[account_key] = content(json.loads(row[0])) if row else None
        current = content(value)
        if cache[account_key] == current:
            return False
        cache[account_key] = current
        return True

    def analytics_agent(self, db, a):
        record = {key: a.get(key) for key in ('id', 'name', 'rootId', 'parentId', 'accountKey', 'threadId', 'model', 'effort', 'fastMode', 'daybreakEnabled', 'cyberAccessProgram', 'cwd', 'deletedAt')}
        # Skip an unchanged row to avoid a database write for every native
        # analytics notice.
        db.execute('INSERT INTO analytics_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record '
                   'WHERE record IS NOT excluded.record', (a['id'], json.dumps(record)))
        return {'agentId': a['id'], 'agentName': a.get('name'), 'rootId': a.get('rootId') or a['id'],
                'accountKey': a.get('accountKey', 'default'), 'threadId': a.get('threadId'),
                'model': a.get('model'), 'effort': a.get('effort'), 'fastMode': a.get('fastMode'),
                'daybreakEnabled': a.get('daybreakEnabled'), 'cyberAccessProgram': a.get('cyberAccessProgram')}

    def analytics_event(self, db, a, method, p, *, at=None, source='live', measurements=None):
        at = time.time() if at is None else at
        meta = self.analytics_agent(db, a)
        if source != 'live':
            # A historical record must not inherit the current chat's access mode.
            meta.update(daybreakEnabled=None, cyberAccessProgram=None)
        if source == 'live':
            hour = int(at // 3600) * 3600
            key = encoded([a['id'], method, hour])
            db.execute('INSERT INTO analytics_notifications VALUES (?,?,?,?,?,1,?) ON CONFLICT(id) DO UPDATE SET count=count+1,bytes=bytes+excluded.bytes',
                       (key, a['id'], meta['rootId'], method, hour, len(encoded(p).encode('utf-8'))))
        turn = p.get('turnId') or (p.get('turn') or {}).get('id') or a.get('turnId')
        meta.update(threadId=p.get('threadId') or a.get('threadId'), turnId=turn)
        if method == 'thread/tokenUsage/updated':
            generation = db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()
            generation = int(generation[0]) + 1 if generation else 1
            db.execute("INSERT INTO analytics_meta VALUES ('usageGeneration',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (str(generation),))
            db.execute("INSERT INTO analytics_usage_roots(root,generation) VALUES (?,1) ON CONFLICT(root) DO UPDATE SET generation=generation+1",
                       (meta['rootId'],))
            if turn:
                turn_key = ':'.join((a['id'], str(meta['threadId']), str(turn)))
                known = db.execute('SELECT record FROM analytics_turns WHERE id=?', (turn_key,)).fetchone()
                known = json.loads(known[0]) if known else {}
                if known.get('accountKey') == meta['accountKey'] and known.get('cyberAccessProgram') is not None:
                    meta.update(daybreakEnabled=known.get('daybreakEnabled'),
                                cyberAccessProgram=known['cyberAccessProgram'])
            captured_tokens = self.analytics_budget_capture(db, a, p, at=at, source=source)
            usage = p.get('tokenUsage') or {}
            current, last = usage.get('total') or {}, usage.get('last') or {}
            # Totals identify a request across native notices and rollout records.
            # The provider response id distinguishes real zero-token responses.
            fingerprint = hashlib.sha256(encoded([turn, number(current.get('totalTokens')) if number(current.get('totalTokens')) is not None else current]).encode()).hexdigest()
            key = hashlib.sha256(encoded([a['id'], meta['threadId'], fingerprint]).encode()).hexdigest()
            existing = db.execute('SELECT record FROM analytics_usage WHERE id=?', (key,)).fetchone()
            existing = json.loads(existing[0]) if existing else None
            response_id = p.get('responseId')
            if response_id:
                exact = db.execute("SELECT id,record FROM analytics_usage WHERE agent=? AND thread IS ? AND json_extract(record,'$.responseId')=? LIMIT 1", (a['id'], meta['threadId'], response_id)).fetchone()
                if exact:
                    record = json.loads(exact['record'])
                    # Reset notice counters are an observation of the same response,
                    # not a replacement for its authoritative lifetime usage.
                    if not p.get('rawTokenUsageRecord'):
                        record['noticeTotal'] = current
                        record['noticeLast'] = last
                        record['noticeAt'] = at
                        if record.get('modelContextWindow') is None:
                            record['modelContextWindow'] = number(usage.get('modelContextWindow'))
                        db.execute('UPDATE analytics_usage SET record=? WHERE id=?', (json.dumps(record), exact['id']))
                        return captured_tokens
                    existing, key = record, exact['id']
            response_model = p.get('model')
            if existing and response_id and existing.get('responseId') and response_id != existing['responseId']:
                key += ':' + response_id
                existing = db.execute('SELECT record FROM analytics_usage WHERE id=?', (key,)).fetchone()
                existing = json.loads(existing[0]) if existing else None
            if existing:
                if response_id and (not existing.get('responseId') or p.get('rawTokenUsageRecord') and not existing.get('rawTokenUsageRecord')):
                    existing.update(responseId=response_id, source=source, at=at, recordedAt=time.time(),
                                    rawTokenUsageRecord=p.get('rawTokenUsageRecord'), turnUsage=p.get('turnUsage'),
                                    requestUsage=p.get('requestUsage'), model=p.get('model') or meta['model'], last=last, total=current,
                                    delta={k: number(last.get(k)) for k in TOKEN_FIELDS},
                                    counterDomain='response' if p.get('rawTokenUsageRecord') else 'nativeNotice',
                                    cumulativeDelta={k: None for k in TOKEN_FIELDS}, reset=None, baselineMissing=True)
                    db.execute('UPDATE analytics_usage SET at=?,record=? WHERE id=?', (at, json.dumps(existing), key))
                return captured_tokens
            counter_domain = 'response' if p.get('rawTokenUsageRecord') else 'nativeNotice'
            prior = db.execute("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND at<=? AND json_extract(record,'$.counterDomain')=? ORDER BY at DESC,seq DESC LIMIT 1", (a['id'], meta['threadId'], at, counter_domain)).fetchone()
            prior = json.loads(prior[0]) if prior else None
            reset = bool(prior and any(number(current.get(k)) is not None and number(prior['total'].get(k)) is not None and current[k] < prior['total'][k] for k in TOKEN_FIELDS))
            cumulative_delta = {k: current[k] - prior['total'][k] if prior and not reset and number(current.get(k)) is not None and number(prior['total'].get(k)) is not None else None for k in TOKEN_FIELDS}
            record = {**meta, 'id': key, 'at': at, 'recordedAt': time.time(), 'source': source, 'timestampSource': p.get('_analyticsTimestampSource', 'observed' if source == 'live' else 'record'),
                      'model': response_model or meta['model'],
                      'last': last, 'total': current, 'delta': {k: number(last.get(k)) for k in TOKEN_FIELDS}, 'raw': usage,
                      'cumulativeDelta': cumulative_delta, 'counterDomain': counter_domain,
                      'modelContextWindow': number(usage.get('modelContextWindow')),
                      'reset': reset, 'baselineMissing': prior is None or reset,
                      'fingerprint': fingerprint, 'responseId': response_id,
                      'rawTokenUsageRecord': p.get('rawTokenUsageRecord'), 'turnUsage': p.get('turnUsage'),
                      'requestUsage': p.get('requestUsage')}
            db.execute('INSERT OR IGNORE INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                       (key, a['id'], meta['rootId'], meta['threadId'], turn, at, json.dumps(record)))
            return captured_tokens
        if turn and method in {'turn/started', 'turn/completed', 'item/agentMessage/delta', 'item/started', 'item/completed'}:
            key = ':'.join((a['id'], str(meta['threadId']), str(turn)))
            row = db.execute('SELECT record FROM analytics_turns WHERE id=?', (key,)).fetchone()
            record = json.loads(row[0]) if row else {**meta, 'id': key, 'at': at, 'startedAt': None, 'finishedAt': None,
                'firstOutputAt': None, 'durationMs': None, 'firstOutputDelayMs': None, 'status': 'unknown', 'source': source}
            if method == 'turn/started':
                if source == 'live':
                    record.update(daybreakEnabled=meta.get('daybreakEnabled'),
                                  cyberAccessProgram=meta.get('cyberAccessProgram'))
                record['startedAt'] = min(at, record['startedAt']) if record['startedAt'] is not None else at
                if record['finishedAt'] is None:
                    record['status'] = 'running'
            elif method == 'turn/completed':
                terminal = p.get('turn') or {}
                error = terminal.get('error')
                status = 'failed' if error else terminal.get('status', 'unknown')
                # Terminal failures belong to this exact agent/thread/turn key.
                # A history projection without an error cannot erase evidence.
                previous_failure = record.get('error') or record.get('status') == 'failed'
                incoming_failure = error or status == 'failed'
                replace = not previous_failure or (incoming_failure and
                    (not record.get('error') or source == 'live' or record.get('terminalSource', record.get('source')) != 'live'))
                if replace:
                    record.update(finishedAt=at, status=status, error=error or record.get('error') if incoming_failure else None,
                                  terminalSource=source)
                for field, value in (('nativeDurationMs', p.get('durationMs')), ('nativeTimeToFirstTokenMs', p.get('timeToFirstTokenMs'))):
                    if number(value) is not None:
                        record[field] = number(value)
            elif method == 'item/agentMessage/delta' and p.get('delta') or (p.get('item') or {}).get('type') == 'agentMessage' and (p.get('item') or {}).get('text'):
                if record['firstOutputAt'] is None:
                    record['firstOutputAt'] = at
            if record['startedAt'] is not None:
                for event, metric in (('finishedAt', 'durationMs'), ('firstOutputAt', 'firstOutputDelayMs')):
                    if record[event] is not None:
                        record[metric] = max(0, (record[event] - record['startedAt']) * 1000)
            db.execute('INSERT INTO analytics_turns VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                       (key, a['id'], meta['rootId'], record['at'], json.dumps(record)))
        if method == 'analytics/rateLimits':
            if self.analytics_limit_changed(db, meta['accountKey'], p):
                db.execute('INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)', (meta['accountKey'], at, json.dumps(p)))
            return
        if method == 'analytics/compaction':
            self.analytics_event(db, a, 'item/completed', {**p, 'item': {'id': p.get('id') or p.get('_analyticsId') or str(at), 'type': 'compactionSnapshot', 'compactionMetadata': p}}, at=at, source=source)
            return
        item = p.get('item') or {}
        item_id = item.get('id') or p.get('itemId')
        if not item_id or not method.startswith('item/'):
            return
        key = ':'.join((a['id'], str(meta['threadId']), str(item_id)))
        row = db.execute('SELECT record FROM analytics_items WHERE id=?', (key,)).fetchone()
        record = json.loads(row[0]) if row else None
        if method not in {'item/started', 'item/completed'}:
            if not record or record.get('finishedAt') is not None or not method.endswith('Delta') and not method.endswith('/delta'):
                return
            delta = p.get('delta')
            if not isinstance(delta, str):
                return
            # Increment counters only. A final authoritative payload replaces these.
            size = measurements['stream'] if measurements is not None and 'stream' in measurements else payload_size(delta)
            stream = record.get('stream') or {'bytes': 0, 'chars': 0, 'lines': 0, 'deltas': 0}
            for field in ('bytes', 'chars', 'lines'):
                stream[field] += size[field] if field != 'lines' else delta.count('\n')
            stream['deltas'] += 1
            record['stream'] = stream
        else:
            kind = item.get('type') or (record or {}).get('type') or 'unknown'
            if record and record.get('finishedAt') is not None and method == 'item/started':
                return
            # Live measurements are authoritative over reconstructed history.
            if record and record.get('source') == 'live' and source != 'live':
                for field in ('startedAt', 'completedAt'):
                    if number(p.get(field)) is not None:
                        record['finishedAt' if field == 'completedAt' else field] = p[field]
                if record.get('startedAt') is not None and record.get('finishedAt') is not None and record.get('durationSource') != 'provider':
                    record['durationMs'] = max(0, (record['finishedAt'] - record['startedAt']) * 1000)
                    record['durationSource'] = 'native_item_timestamps'
                self.analytics_store_item(db, record)
                return
            record = record or {**meta, 'id': key, 'itemId': item_id, 'startedAt': at if method == 'item/started' else None,
                                'at': at, 'source': source, 'input': None, 'output': None, 'payloadBoundary': 'protocol'}
            record.update(type=kind, name=item.get('tool') or item.get('name') or kind,
                          recordedAt=time.time(), source=source,
                          isTool=kind not in NON_TOOLS)
            if number(p.get('startedAt')) is not None:
                record['startedAt'] = p['startedAt']
            record['timestampSource'] = p.get('_analyticsTimestampSource', 'observed' if source == 'live' else 'record')
            if method == 'item/completed':
                record['finishedAt'] = p.get('completedAt') if number(p.get('completedAt')) is not None else at
            record['status'] = ('failed' if item.get('success') is False or item.get('error') or item.get('status') in {'failed', 'declined'} or item.get('exitCode') not in (None, 0) else 'completed') if method == 'item/completed' else 'running'
            for field in ('command', 'cwd', 'server', 'exitCode', 'processId', 'status'):
                if field in item and field != 'status':
                    record[field] = item[field]
            if item.get('compactionMetadata'):
                record['compactionMetadata'] = item['compactionMetadata']
            if item.get('error'):
                record['error'] = encoded(item['error'])[:2000]
            supplied = number(item.get('durationMs'))
            record['durationMs'] = supplied if supplied is not None else (max(0, (record['finishedAt'] - record['startedAt']) * 1000) if method == 'item/completed' and record.get('startedAt') is not None else None)
            record['durationSource'] = 'provider' if supplied is not None else 'observed_wall_time' if record['durationMs'] is not None else None
            input_value, output_value = item_payload_values(kind, item)
            if input_value is not None:
                record['input'] = measurements['input'] if measurements is not None else payload_size(input_value)
            if output_value is not None:
                record['output'] = measurements['output'] if measurements is not None else payload_size(output_value)
            record['coverage'] = 'protocol_payload' if source == 'live' else source
            record['payloadTruncated'] = item.get('outputTruncated')
        self.analytics_store_item(db, record)

    def analytics_store_item(self, db, record):
        # Date filters use call start when known, then completion or observation.
        # Enrichment must update both the record and its indexed filter value.
        record.setdefault('firstRecordedAt', record.get('recordedAt', time.time()))
        record.setdefault('firstSourceAt', record['at'])
        record['at'] = next((value for value in (record.get('startedAt'), record.get('finishedAt'), record['at']) if number(value) is not None))
        db.execute('INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record,at=excluded.at,type=excluded.type,name=excluded.name,is_tool=excluded.is_tool',
                   (record['id'], record['agentId'], record.get('rootId'), record.get('threadId'), record.get('turnId'), record['at'], record['type'], record['name'], int(record['isTool']), json.dumps(record)))

    def analytics_dynamic(self, db, a, p, result, *, at=None, source='live'):
        item_id = p.get('callId') or p.get('itemId')
        if not item_id:
            return
        params = {**p, 'item': {'id': item_id, 'type': 'dynamicToolCall', 'tool': p.get('tool'), 'arguments': p.get('arguments'), **result}}
        self.analytics_event(db, a, 'item/completed', params, at=at, source=source)

    def analytics_model_payload(self, db, a, p, *, at=None, turn_id=None, source='rollout', measurements=None):
        at = time.time() if at is None else at
        kind = p.get('type', 'unknown')
        outputs = {'function_call_output', 'custom_tool_call_output', 'tool_result'}
        inputs = {'function_call', 'custom_tool_call', 'tool_call'}
        call_id = p.get('call_id') or p.get('callId') or p.get('id') or p.get('_analyticsId')
        if not call_id:
            return
        category = p.get('_analyticsCategory') or ('tool' if kind in outputs | inputs else 'message')
        is_tool = category == 'tool' and kind in outputs | inputs
        identity = p.get('_analyticsId') if category in {'compactionReplacement', 'compactionGuardian'} else call_id
        key = ':'.join((a['id'], str(a.get('threadId')), 'model', str(identity or call_id), category))
        row = db.execute('SELECT record FROM analytics_items WHERE id=?', (key,)).fetchone()
        record = json.loads(row[0]) if row else {**self.analytics_agent(db, a), 'id': key, 'itemId': call_id,
            'turnId': turn_id, 'at': at, 'startedAt': at if kind in inputs else None, 'finishedAt': None,
            'type': 'modelToolCall' if is_tool else 'modelMessage', 'name': p.get('name') or kind,
            'isTool': is_tool, 'input': None, 'output': None, 'durationMs': None,
            'payloadBoundary': 'model', 'category': category, 'role': p.get('role'), 'callId': call_id, 'snapshotItemId': p.get('_analyticsId') if category in {'compactionReplacement', 'compactionGuardian'} else None}
        record.update(source=source, recordedAt=time.time(), coverage='model_protocol_payload', timestampSource=p.get('_analyticsTimestampSource', 'record'))
        if p.get('name'):
            record['name'] = (p['namespace'] + '.' if p.get('namespace') else '') + p['name']
            record['namespace'] = p.get('namespace')
        if kind in inputs:
            record['input'] = record['modelInput'] = (measurements['input'] if measurements is not None else payload_size(p.get('arguments', p.get('input'))))
            record['startedAt'] = at
            record['status'] = 'completed' if record.get('finishedAt') is not None else 'running'
        elif kind in outputs:
            record['output'] = record['modelOutput'] = (measurements['output'] if measurements is not None else payload_size(p.get('output', p.get('content'))))
            record['finishedAt'] = at
            record['status'] = 'failed' if p.get('is_error') is True else 'completed'
        else:
            direction = 'input' if p.get('role') in {'user', 'system', 'developer'} else 'output'
            # Never retain hidden reasoning text. Sizes only, if the provider exposes a payload.
            record[direction] = (measurements[direction] if measurements is not None else payload_size(p.get('content', p.get('summary'))))
            record['status'] = 'completed'
            record['finishedAt'] = at
        if record.get('startedAt') is not None and record.get('finishedAt') is not None:
            record['durationMs'] = max(0, (record['finishedAt'] - record['startedAt']) * 1000)
            record['durationSource'] = 'observed_wall_time'
        self.analytics_store_item(db, record)

    def analytics_turn_errors(self, db, agent, thread, turns):
        keys = [':'.join((agent, thread, turn)) for turn in turns]
        if not keys:
            return []
        rows = db.execute('SELECT record FROM analytics_turns WHERE id IN ('
                          + ','.join('?' for _ in keys) + ')', keys).fetchall()
        return [{key: record.get(key) for key in ('agentId', 'threadId', 'turnId', 'status', 'error')}
                for record in (json.loads(row[0]) for row in rows)]

    def analytics_detail_page(self, agent, scope, options):
        detail = options.get('detail')
        if detail not in {'rateLimits', 'turns'}:
            raise ValueError('Unknown analytics detail page')
        limit = max(1, min(100, int(options.get('limit', 100))))
        offset = max(0, int(options.get('offset', 0)))
        def timestamp(key):
            if options.get(key) in (None, ''):
                return None
            value = number(float(options[key]))
            if value is None:
                raise ValueError('Invalid analytics time')
            return value
        start, end = timestamp('from'), timestamp('to')
        with self.analytics_read_connection() as db:
            agents = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_agents')]
            selected = next((a for a in agents if a['id'] == agent), None)
            if scope != 'all' and not selected:
                raise ValueError('Select an agent for analytics')
            if detail == 'turns':
                where, args = [], []
                if scope == 'agent':
                    where.append('agent=?'); args.append(agent)
                elif scope == 'team':
                    where.append('root=?'); args.append(selected.get('rootId') or agent)
                if start is not None:
                    where.append('at>=?'); args.append(start)
                if end is not None:
                    where.append('at<=?'); args.append(end)
                clause = ' WHERE ' + ' AND '.join(where) if where else ''
                total = db.execute('SELECT COUNT(*) FROM analytics_turns' + clause, args).fetchone()[0]
                page = [json.loads(row[0]) for row in db.execute(
                    'SELECT record FROM analytics_turns' + clause + ' ORDER BY at DESC,id DESC LIMIT ? OFFSET ?',
                    args + [limit, offset])]
            else:
                runtime_agents = {a['id']: a for a in self.records(db, 'agents')}
                for entry in agents:
                    if entry['id'] in runtime_agents:
                        current = runtime_agents[entry['id']]
                        for field in ('name', 'deletedAt', 'model', 'effort', 'fastMode', 'threadId',
                                      'accountKey', 'rootId', 'parentId', 'cwd'):
                            entry[field] = current.get(field)
                relevant = [a for a in agents if scope == 'all'
                            or scope == 'agent' and a['id'] == agent
                            or scope == 'team' and (a.get('rootId') or a['id']) == (selected.get('rootId') or agent)]
                accounts = sorted({a.get('accountKey', 'default') for a in relevant})
                where, args = [], []
                if accounts:
                    where.append('account IN (' + ','.join('?' for _ in accounts) + ')')
                    args.extend(accounts)
                else:
                    where.append('0')
                if start is not None:
                    where.append('at>=?'); args.append(start)
                if end is not None:
                    where.append('at<=?'); args.append(end)
                clause = ' WHERE ' + ' AND '.join(where)
                total = db.execute('SELECT COUNT(*) FROM analytics_limits' + clause, args).fetchone()[0]
                rows = db.execute('SELECT id,account,at FROM analytics_limits' + clause
                                  + ' ORDER BY at DESC,id DESC LIMIT ? OFFSET ?', args + [limit, offset]).fetchall()
                ids = [row['id'] for row in rows]
                records = {row['id']: row['record'] for row in db.execute(
                    'SELECT id,record FROM analytics_limits WHERE id IN ('
                    + ','.join('?' for _ in ids) + ')', ids)} if ids else {}
                page = [{'accountKey': row['account'], 'at': row['at'], 'data': json.loads(records[row['id']])}
                        for row in rows]
        return {detail: page, 'pagination': {'limit': limit, 'offset': offset, 'total': total,
                                             'hasMore': offset + len(page) < total}}

    def analytics(self, agent=None, scope='agent', **options):
        shared_db = options.pop('_db', None)
        if shared_db is not None and not hasattr(shared_db, 'execute'):
            raise ValueError('Invalid analytics database')
        timing = options.get('timing') == '1'
        request_started = time.perf_counter() if timing else None
        if scope not in {'agent', 'team', 'all'}:
            raise ValueError('Unknown analytics scope')
        if options.get('view') == 'message-info':
            if scope != 'agent':
                raise ValueError('Select one agent for message metadata')
            from codex_message_info import message_info
            return message_info(self, agent, options.get('item'), options.get('turn'), options.get('thread'))
        if options.get('view') == 'turn-errors':
            self.agent(agent)
            thread = options.get('thread')
            turns = list(dict.fromkeys((options.get('turns') or '').split(',')))
            if scope != 'agent' or not thread or not all(turns) or len(turns) > 120:
                raise ValueError('Select one thread and up to 120 turns')
            with self.analytics_read_connection() as db:
                return {'turns': self.analytics_turn_errors(db, agent, thread, turns)}
        if options.get('view') == 'detail':
            return self.analytics_detail_page(agent, scope, options)
        def timestamp(key):
            if options.get(key) in (None, ''):
                return None
            value = number(float(options[key]))
            if value is None:
                raise ValueError('Invalid analytics time')
            return value
        start, end = timestamp('from'), timestamp('to')
        if start is not None and end is not None and start > end:
            raise ValueError('Analytics start is after end')
        limit = max(1, min(500, int(options.get('limit', 50))))
        offset = max(0, int(options.get('offset', 0)))
        export = str(options.get('export', '0')) == '1'
        tool = options.get('tool') or None
        read_started = time.perf_counter() if timing else None
        with (nullcontext(shared_db) if shared_db is not None else self.analytics_read_connection()) as db:
            if not db.in_transaction:
                db.execute('BEGIN')
            agents = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_agents')]
            selected = next((a for a in agents if a['id'] == agent), None)
            if scope != 'all' and not selected:
                raise ValueError('Select an agent for analytics')
            where, args = [], []
            if scope == 'agent':
                where.append('agent=?'); args.append(agent)
            elif scope == 'team':
                where.append('root=?'); args.append(selected.get('rootId') or agent)
            if start is not None:
                where.append('at>=?'); args.append(start)
            if end is not None:
                where.append('at<=?'); args.append(end)
            clause = ' WHERE ' + ' AND '.join(where) if where else ''
            call_where = [*where, 'is_tool=1']
            call_args = list(args)
            if tool is not None:
                call_where.append('name=?'); call_args.append(tool)
            call_clause = ' WHERE ' + ' AND '.join(call_where)
            calls_total = db.execute('SELECT COUNT(*) FROM analytics_items' + call_clause,
                                     call_args).fetchone()[0]
            calls_page = [json.loads(row[0]) for row in db.execute(
                'SELECT record FROM analytics_items' + call_clause + ' ORDER BY at DESC,id'
                + (' LIMIT ? OFFSET ?' if not export else ''),
                call_args + ([] if export else [limit, offset]))]
            # Provisional-vs-authoritative deduplication is local to the selected
            # agent/team. The former global JSON scan touched every usage row on
            # each agent analytics request, despite the scope indexes above.
            authoritative_where, authoritative_args = [], []
            if scope == 'agent':
                authoritative_where.append('agent=?'); authoritative_args.append(agent)
            elif scope == 'team':
                authoritative_where.append('root=?'); authoritative_args.append(selected.get('rootId') or agent)
            authoritative_where.append("json_extract(record,'$.responseId') IS NOT NULL")
            authoritative_turns = {(row['agent'], row['thread'], row['turn']) for row in db.execute(
                'SELECT DISTINCT agent,thread,turn FROM analytics_usage WHERE '
                + ' AND '.join(authoritative_where), authoritative_args)}
            usage_totals = _UsageTotals()
            chart_buckets = _ContextBuckets()
            provisional_count = 0
            timeline = [] if export else deque(maxlen=500)
            provisional_rows = [] if export else deque(maxlen=100)
            usage_agents, usage_turns = set(), set()
            usage_by_model, usage_by_account, usage_by_agent = {}, {}, {}
            for raw, in db.execute('SELECT record FROM analytics_usage' + clause + ' ORDER BY at,seq', args):
                row = json.loads(raw)
                if not row.get('responseId') and (row['agentId'], row.get('threadId'), row.get('turnId')) in authoritative_turns:
                    provisional_count += 1
                    provisional_rows.append(row)
                    continue
                usage_totals.add(row)
                chart_buckets.add(row)
                timeline.append(row)
                usage_agents.add(row['agentId'])
                if row.get('turnId'):
                    usage_turns.add((row['agentId'], row['turnId']))
                for groups, key in ((usage_by_model, row.get('model')),
                                    (usage_by_account, row.get('accountKey')),
                                    (usage_by_agent, row['agentId'])):
                    group = groups.get(key)
                    if group is None:
                        group = groups[key] = _UsageTotals(metrics=False)
                    group.add(row)
            turns_total = db.execute('SELECT COUNT(*) FROM analytics_turns' + clause, args).fetchone()[0]
            turns = [json.loads(row[0]) for row in db.execute(
                'SELECT record FROM analytics_turns' + clause + ' ORDER BY at DESC,id DESC'
                + (' LIMIT ? OFFSET ?' if not export else ''),
                args + ([] if export else [100, 0]))]
            item_groups = {}
            detailed_item_groups = {}
            by_tool = {}
            tool_boundaries = {}
            model_items = _ItemTotals(duration_total=True)
            protocol_items = _ItemTotals(duration_total=True)
            call_items = _ItemTotals()
            agent_call_items = {}
            item_agent_ids, item_turns = set(), set()
            non_tool_items, non_tool_total = [], 0
            compactions, snapshots = [], []
            for raw, in db.execute('SELECT record FROM analytics_items' + clause + ' ORDER BY at DESC,id', args):
                row = json.loads(raw)
                item_agent_ids.add(row['agentId'])
                if row.get('turnId'):
                    item_turns.add((row['agentId'], row['turnId']))
                kind = row['type']
                group = item_groups.get(kind)
                if group is None:
                    group = item_groups[kind] = _ItemTotals()
                group.add(row)
                detail_key = (kind, row.get('payloadBoundary'), row.get('category'), row.get('role'))
                group = detailed_item_groups.get(detail_key)
                if group is None:
                    group = detailed_item_groups[detail_key] = _ItemTotals()
                group.add(row)
                if kind == 'contextCompaction' and row.get('finishedAt') is not None:
                    compactions.append(row)
                elif kind == 'compactionSnapshot':
                    snapshots.append(row)
                if not row['isTool']:
                    non_tool_total += 1
                    if export or len(non_tool_items) < 100:
                        non_tool_items.append(row)
                    continue
                key = (row['name'], kind)
                group = by_tool.get(key)
                if group is None:
                    group = by_tool[key] = _ItemTotals(duration_samples=True, duration_total=True)
                group.add(row)
                tool_boundaries.setdefault(key, row.get('payloadBoundary', 'protocol'))
                if tool is not None and row['name'] != tool:
                    continue
                call_items.add(row)
                boundary = 'model' if row.get('payloadBoundary') == 'model' else 'protocol'
                (model_items if boundary == 'model' else protocol_items).add(row)
                agent_key = (row['agentId'], boundary)
                group = agent_call_items.get(agent_key)
                if group is None:
                    group = agent_call_items[agent_key] = _ItemTotals(duration_samples=True)
                group.add(row)
            relevant_agents = [a for a in agents if scope == 'all' or a['id'] == agent and scope == 'agent' or scope == 'team' and (a.get('rootId') or a['id']) == (selected.get('rootId') or agent)]
            relevant_ids = {a['id'] for a in relevant_agents}
            agent_ids = sorted(relevant_ids)
            agent_filter = (' IN (' + ','.join('?' for _ in agent_ids) + ')') if agent_ids else ' IN (NULL)'
            runtime_agents = {row['id']: json.loads(row['record']) for row in db.execute(
                'SELECT id,record FROM canvas.runtime_agents WHERE id' + agent_filter, agent_ids)}
            for entry in agents:
                if entry['id'] in runtime_agents:
                    current_agent = runtime_agents[entry['id']]
                    for field in ('name', 'deletedAt', 'model', 'effort', 'fastMode', 'threadId', 'accountKey', 'rootId', 'parentId', 'cwd'):
                        entry[field] = current_agent.get(field)
            account_keys = {a.get('accountKey', 'default') for a in relevant_agents}
            operational = {
                table: [json.loads(row[0]) for row in db.execute(
                    'SELECT record FROM runtime_' + table + ' WHERE json_extract(record,\'$.agent\')' + agent_filter,
                    agent_ids)]
                for table in ('monitors', 'requests')
            }
            queued_events = [dict(row) for row in db.execute(
                'SELECT agent,kind,status,created FROM canvas.runtime_events WHERE agent' + agent_filter,
                agent_ids)]
            notification_rows = [dict(row) for row in db.execute(
                'SELECT * FROM analytics_notifications WHERE agent' + agent_filter, agent_ids)]
            limit_where, limit_args = [], []
            if account_keys:
                limit_where.append('account IN (' + ','.join('?' for _ in account_keys) + ')')
                limit_args.extend(sorted(account_keys))
            else:
                limit_where.append('0')
            if start is not None:
                limit_where.append('at>=?'); limit_args.append(start)
            if end is not None:
                limit_where.append('at<=?'); limit_args.append(end)
            limit_clause = ' WHERE ' + ' AND '.join(limit_where)
            rate_limit_total = db.execute('SELECT COUNT(*) FROM analytics_limits' + limit_clause,
                                          limit_args).fetchone()[0]
            if export:
                limit_rows = [dict(row) for row in db.execute(
                    'SELECT account,at,record FROM analytics_limits' + limit_clause
                    + ' ORDER BY at DESC,id DESC', limit_args)]
            else:
                limit_rows = [dict(row) for row in db.execute(
                    'SELECT id,account,at FROM analytics_limits' + limit_clause
                    + ' ORDER BY at DESC,id DESC LIMIT 100', limit_args)]
                limit_ids = [row['id'] for row in limit_rows]
                limit_records = {row['id']: row['record'] for row in db.execute(
                    'SELECT id,record FROM analytics_limits WHERE id IN ('
                    + ','.join('?' for _ in limit_ids) + ')', limit_ids)} if limit_ids else {}
                for row in limit_rows:
                    row['record'] = limit_records[row['id']]
            history = ([json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_history')]
                       if db.execute("SELECT 1 FROM sqlite_master WHERE name='analytics_history'").fetchone() else [])
            capture_error = db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()
            capture_error = json.loads(capture_error[0]) if capture_error else {'count': 0, 'last': None}
            tracking = float(db.execute("SELECT value FROM analytics_meta WHERE key='trackingSince'").fetchone()[0])
        read_ms = (time.perf_counter() - read_started) * 1000 if timing else None
        build_started = time.perf_counter() if timing else None
        def size(rows, direction, key):
            return rows.size(direction, key)
        def durations(rows):
            return rows.duration()
        tools = [{'name': name, 'type': kind, 'payloadBoundary': tool_boundaries[(name, kind)], 'calls': rows.count, 'failed': rows.failed,
                  'inputBytes': size(rows, 'input', 'bytes'), 'outputBytes': size(rows, 'output', 'bytes'),
                  'modelInputBytes': size(rows, 'input', 'bytes') if kind == 'modelToolCall' else None,
                  'modelOutputBytes': size(rows, 'output', 'bytes') if kind == 'modelToolCall' else None,
                  'durationMs': rows.duration_total(),
                  'imageCount': nullable_sum([size(rows, 'input', 'imageCount'), size(rows, 'output', 'imageCount')]),
                  'inputMeasurements': rows.input_measurements, 'outputMeasurements': rows.output_measurements, 'duration': durations(rows)}
                 for (name, kind), rows in by_tool.items()]
        tools.sort(key=lambda row: row['outputBytes'] or 0, reverse=True)
        tokens = usage_totals.tokens()
        native_compaction_turns = {(r['agentId'], r.get('turnId')) for r in compactions}
        compactions += [r for r in snapshots if (r['agentId'], r.get('turnId')) not in native_compaction_turns]
        selected_ids = item_agent_ids | usage_agents
        item_turns.update(usage_turns)
        summary = {'agents': len(selected_ids), 'turns': len(item_turns),
                   'usageSamples': usage_totals.count, 'provisionalUsageSamples': provisional_count, 'exactResponseSamples': usage_totals.exact, 'legacyUsageSamples': usage_totals.legacy, 'modelToolCalls': model_items.count, 'protocolToolCalls': protocol_items.count, 'observedToolRows': call_items.count, 'toolCalls': model_items.count, 'failedToolCalls': model_items.failed, 'modelFailedToolCalls': model_items.failed, 'protocolFailedToolCalls': protocol_items.failed,
                   'compactions': len(compactions), 'inputBytes': size(model_items, 'input', 'bytes'), 'outputBytes': size(model_items, 'output', 'bytes'),
                   'modelInputBytes': size(model_items, 'input', 'bytes'), 'modelOutputBytes': size(model_items, 'output', 'bytes'),
                   'protocolInputBytes': size(protocol_items, 'input', 'bytes'), 'protocolOutputBytes': size(protocol_items, 'output', 'bytes'),
                   'durationMs': model_items.duration_total(),
                   'modelDurationMs': model_items.duration_total(),
                   'protocolDurationMs': protocol_items.duration_total(), 'tokens': tokens,
                   'tokenObservations': {field: len(usage_totals.deltas[field]) for field in TOKEN_FIELDS},
                   'cacheHitRate': tokens['cachedInputTokens'] / tokens['inputTokens'] if usage_totals.cache_pairs == usage_totals.count and tokens['cachedInputTokens'] is not None and tokens['inputTokens'] else None,
                   'cacheHitRateSamples': usage_totals.cache_pairs, 'cacheHitRateTotalSamples': usage_totals.count,
                   'peakContextTokens': usage_totals.peak_context,
                   'peakContextPercent': usage_totals.peak_percent, 'baselineMissingSamples': usage_totals.baseline_missing}
        def within(value):
            return value is not None and (start is None or value >= start) and (end is None or value <= end)
        def group_usage(field, groups):
            return [{field: key, 'samples': rows.count, 'tokens': rows.tokens()} for key, rows in groups.items()]
        agent_totals = []
        for entry in relevant_agents:
            samples = usage_by_agent.get(entry['id'], _UsageTotals(metrics=False))
            own_model = agent_call_items.get((entry['id'], 'model'), _ItemTotals(duration_samples=True))
            own_protocol = agent_call_items.get((entry['id'], 'protocol'), _ItemTotals(duration_samples=True))
            agent_totals.append({**entry, 'tokens': samples.tokens(),
                                'usageSamples': samples.count, 'toolCalls': own_model.count, 'modelToolCalls': own_model.count, 'protocolToolCalls': own_protocol.count,
                                'failedToolCalls': own_model.failed, 'protocolFailedToolCalls': own_protocol.failed,
                                'compactions': sum(r['agentId'] == entry['id'] for r in compactions), 'duration': durations(own_model), 'protocolDuration': durations(own_protocol)})
        monitors = [{k: m.get(k) for k in ('id', 'agent', 'status', 'created', 'finished', 'bytes', 'exitCode', 'error', 'timeout_ms')}
                    for m in operational['monitors'] if m.get('agent') in relevant_ids and within(m.get('created'))]
        counts = defaultdict(int)
        for event in queued_events:
            if event['agent'] in relevant_ids and within(event['created']):
                counts[event['status'] + ':' + event['kind']] += 1
        approvals = [r for r in operational['requests'] if r.get('agent') in relevant_ids]
        notifications = [r for r in notification_rows if r['agent'] in relevant_ids and (start is None or r['hour'] + 3600 > start) and (end is None or r['hour'] <= end)]
        result = {'version': 1, 'generatedAt': time.time(), 'filters': {'agent': agent, 'scope': scope, 'from': start, 'to': end, 'tool': tool},
                'coverage': {'trackingSince': tracking, 'captureErrors': capture_error, 'historyErrors': [r for r in history if r.get('status') == 'error'], 'provisionalUsageSamples': provisional_count, 'tokenAttribution': 'provider_usage_only', 'payloadMeasurement': 'observed_protocol_payload',
                             'history': 'live_and_stored_history', 'notes': [
                                 'Tool filters affect tool calls only. Provider usage is scoped to the selected agents and time.',
                                 'Item date filters use start time when known, otherwise completion time or the first observation. History can establish an earlier start.',
                                 'Input and output sizes measure UTF-8 text or compact JSON observed at the protocol boundary, not context tokens or billed tokens.',
                                 'Cached input is part of input. Reasoning output is part of output. Do not add these subsets twice.',
                                 'Token totals use response-id records when available for a turn. Other notices in that turn remain provisional and are excluded.',
                                 'Turns without response records use deduplicated last-usage notices. Current requests can remain pending until rollout import catches up. Missing requests are not inferred.',
                                 'Reasoning items expose only protocol-visible content. Hidden reasoning text and unreported context overhead are unavailable.',
                                 'Turn duration and first-output delay include tool execution and waits, not model-only latency.',
                                 'Notification histogram buckets are full UTC hours and can overlap the selected time range.',
                                 'Approval counts describe current stored requests across all time. Monitor and delivery-event filters use creation time.',
                                 'Model and native tool calls, failures, and durations are separate populations. Main aliases describe model calls only.',
                                 'Duration sums include parallel calls and are not elapsed session time. Missing measurements remain null.']},
                'summary': summary, 'agents': relevant_agents, 'agentTotals': agent_totals, 'tools': tools,
                'modelTotals': group_usage('model', usage_by_model), 'accountTotals': group_usage('accountKey', usage_by_account),
                'operations': {'monitors': monitors, 'eventCounts': dict(counts),
                               'approvalCounts': {status: sum(r.get('status') == status for r in approvals) for status in {r.get('status') for r in approvals}}},
                'notifications': notifications, 'history': [r for r in history if not r.get('agent') and not r.get('agentId') or r.get('agent') in relevant_ids or r.get('agentId') in relevant_ids],
                'rateLimits': [{'accountKey': r['account'], 'at': r['at'], 'data': json.loads(r['record'])} for r in limit_rows if r['account'] in account_keys and within(r['at'])],
                'turns': turns, 'timeline': list(timeline), 'chartBuckets': chart_buckets.rows(),
                'timelineTotal': usage_totals.count, 'provisionalUsage': list(provisional_rows),
                'calls': calls_page,
                'items': [{'type': kind, 'count': rows.count, 'bytes': nullable_sum([size(rows, 'input', 'bytes'), size(rows, 'output', 'bytes')]),
                           'chars': nullable_sum([size(rows, 'input', 'chars'), size(rows, 'output', 'chars')])} for kind, rows in item_groups.items()],
                'itemRecords': non_tool_items, 'itemRecordsTotal': non_tool_total,
                'itemBreakdown': [{'type': kind, 'payloadBoundary': boundary, 'category': category, 'role': role, 'count': rows.count, 'inputBytes': size(rows, 'input', 'bytes'), 'outputBytes': size(rows, 'output', 'bytes'), 'inputMeasurements': rows.input_measurements, 'outputMeasurements': rows.output_measurements} for (kind, boundary, category, role), rows in detailed_item_groups.items()],
                'compactions': compactions, 'compactionSnapshots': snapshots,
                'pagination': {'limit': limit, 'offset': offset, 'total': calls_total, 'hasMore': not export and offset + limit < calls_total},
                'detailPagination': {'rateLimits': {'limit': rate_limit_total if export else 100, 'offset': 0, 'total': rate_limit_total, 'hasMore': not export and 100 < rate_limit_total},
                                     'turns': {'limit': turns_total if export else 100, 'offset': 0, 'total': turns_total, 'hasMore': not export and 100 < turns_total}}}
        if timing:
            result['__serverTiming'] = {
                'analytics-read': read_ms,
                'analytics-build': (time.perf_counter() - build_started) * 1000,
                'analytics-total': (time.perf_counter() - request_started) * 1000,
            }
        return result

    def analytics_export_chunks(self, agent=None, scope='agent', **options):
        """Yield one JSON export while keeping large histories outside memory."""
        options = {key: value for key, value in options.items() if key != '_db'}
        options['export'] = '0'
        with self.analytics_read_connection() as db:
            if not db.in_transaction:
                db.execute('BEGIN')
            result = self.analytics(agent, scope, _db=db, **options)
            result.pop('__serverTiming', None)
            filters = result['filters']
            where, args = [], []
            if scope == 'agent':
                where.append('agent=?')
                args.append(agent)
            elif scope == 'team':
                selected = json.loads(db.execute(
                    'SELECT record FROM analytics_agents WHERE id=?', (agent,)).fetchone()[0])
                where.append('root=?')
                args.append(selected.get('rootId') or agent)
            if filters['from'] is not None:
                where.append('at>=?')
                args.append(filters['from'])
            if filters['to'] is not None:
                where.append('at<=?')
                args.append(filters['to'])
            clause = ' WHERE ' + ' AND '.join(where) if where else ''
            call_where = [*where, 'is_tool=1']
            call_args = list(args)
            if filters['tool'] is not None:
                call_where.append('name=?')
                call_args.append(filters['tool'])
            call_clause = ' WHERE ' + ' AND '.join(call_where)
            item_clause = ' WHERE ' + ' AND '.join([*where, 'is_tool=0'])
            authoritative_where, authoritative_args = [], []
            if scope == 'agent':
                authoritative_where.append('agent=?')
                authoritative_args.append(agent)
            elif scope == 'team':
                authoritative_where.append('root=?')
                authoritative_args.append(selected.get('rootId') or agent)
            authoritative_where.append("json_extract(record,'$.responseId') IS NOT NULL")
            authoritative_turns = None
            accounts = {a.get('accountKey', 'default') for a in result['agents']}
            limit_where, limit_args = [], []
            if accounts:
                limit_where.append('account IN (' + ','.join('?' for _ in accounts) + ')')
                limit_args.extend(sorted(accounts))
            else:
                limit_where.append('0')
            if filters['from'] is not None:
                limit_where.append('at>=?')
                limit_args.append(filters['from'])
            if filters['to'] is not None:
                limit_where.append('at<=?')
                limit_args.append(filters['to'])
            limit_clause = ' WHERE ' + ' AND '.join(limit_where)
            result['pagination']['hasMore'] = False
            for name in ('rateLimits', 'turns'):
                result['detailPagination'][name]['limit'] = result['detailPagination'][name]['total']
                result['detailPagination'][name]['hasMore'] = False

            def records(query, parameters):
                for raw, in db.execute(query, parameters):
                    yield json.loads(raw)

            def usage_rows(provisional):
                nonlocal authoritative_turns
                if authoritative_turns is None:
                    authoritative_turns = {(row['agent'], row['thread'], row['turn']) for row in db.execute(
                        'SELECT DISTINCT agent,thread,turn FROM analytics_usage WHERE '
                        + ' AND '.join(authoritative_where), authoritative_args)}
                for raw, in db.execute(
                        'SELECT record FROM analytics_usage' + clause + ' ORDER BY at,seq', args):
                    row = json.loads(raw)
                    duplicate = (not row.get('responseId')
                                 and (row['agentId'], row.get('threadId'), row.get('turnId'))
                                 in authoritative_turns)
                    if duplicate == provisional:
                        yield row

            def rate_limits():
                for account, at, raw in db.execute(
                        'SELECT account,at,record FROM analytics_limits' + limit_clause
                        + ' ORDER BY at DESC,id DESC', limit_args):
                    yield {'accountKey': account, 'at': at, 'data': json.loads(raw)}

            streams = {
                'calls': lambda: records(
                    'SELECT record FROM analytics_items' + call_clause + ' ORDER BY at DESC,id',
                    call_args),
                'turns': lambda: records(
                    'SELECT record FROM analytics_turns' + clause + ' ORDER BY at DESC,id DESC',
                    args),
                'timeline': lambda: usage_rows(False),
                'provisionalUsage': lambda: usage_rows(True),
                'itemRecords': lambda: records(
                    'SELECT record FROM analytics_items' + item_clause + ' ORDER BY at DESC,id',
                    args),
                'rateLimits': rate_limits,
            }

            def array_chunks(rows):
                yield b'['
                pending, size, first = [], 0, True
                for row in rows:
                    part = (b'' if first else b',') + json.dumps(row, ensure_ascii=False).encode()
                    first = False
                    if pending and size + len(part) > 65536:
                        yield b''.join(pending)
                        pending, size = [], 0
                    pending.append(part)
                    size += len(part)
                if pending:
                    yield b''.join(pending)
                yield b']'

            yield b'{'
            for index, (key, value) in enumerate(result.items()):
                yield (b',' if index else b'') + json.dumps(key).encode() + b':'
                if key in streams:
                    yield from array_chunks(streams[key]())
                else:
                    yield json.dumps(value, ensure_ascii=False).encode()
            yield b'}'
