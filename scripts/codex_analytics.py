"""Durable measurements of observed protocol usage, never per-tool billing guesses."""
from __future__ import annotations

import hashlib
import json
import math
import struct
import time
from collections import defaultdict
import statistics

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


def nullable_sum(values):
    known = [n for n in values if n is not None]
    return sum(known) if known else None


class AnalyticsMixin:
    def analytics_init(self, db):
        db.executescript('''
          CREATE TABLE IF NOT EXISTS analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
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
          CREATE INDEX IF NOT EXISTS analytics_usage_response ON analytics_usage(agent,thread,json_extract(record,'$.responseId'));
          CREATE INDEX IF NOT EXISTS analytics_usage_thread ON analytics_usage(agent,thread,seq);
          CREATE TABLE IF NOT EXISTS analytics_items (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT,
            at REAL NOT NULL, type TEXT, name TEXT, is_tool INTEGER NOT NULL, record TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS analytics_items_scope ON analytics_items(agent,at);
          CREATE INDEX IF NOT EXISTS analytics_items_team ON analytics_items(root,at);
          CREATE INDEX IF NOT EXISTS analytics_items_tool ON analytics_items(name,at);
        ''')
        db.execute('INSERT OR IGNORE INTO analytics_meta VALUES (?,?)', ('trackingSince', str(time.time())))
        for a in self.records(db, 'agents'):
            self.analytics_agent(db, a)

    def analytics_safe(self, db, operation, *args, **kwargs):
        """An analytics failure cannot consume a native result or lifecycle notice."""
        db.execute('SAVEPOINT analytics_capture')
        try:
            result = operation(db, *args, **kwargs)
            db.execute('RELEASE analytics_capture')
            return result
        except Exception as error:
            db.execute('ROLLBACK TO analytics_capture')
            db.execute('RELEASE analytics_capture')
            detail = {'at': time.time(), 'operation': getattr(operation, '__name__', type(operation).__name__), 'error': str(error)[:1000]}
            try:
                row = db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()
                previous = json.loads(row[0]) if row else {'count': 0, 'last': None}
                previous.update(count=previous['count'] + 1, last=detail)
                db.execute("INSERT INTO analytics_meta VALUES ('captureErrors',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (json.dumps(previous),))
            except Exception:
                import sys
                print('Analytics capture failed: ' + json.dumps(detail), file=sys.stderr)
            return None

    def analytics_limit(self, db, account_key, value):
        db.execute('INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)', (account_key, time.time(), json.dumps(value)))

    def analytics_agent(self, db, a):
        record = {key: a.get(key) for key in ('id', 'name', 'rootId', 'parentId', 'accountKey', 'threadId', 'model', 'effort', 'fastMode', 'cwd', 'deletedAt')}
        db.execute('INSERT INTO analytics_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record', (a['id'], json.dumps(record)))
        return {'agentId': a['id'], 'agentName': a.get('name'), 'rootId': a.get('rootId') or a['id'],
                'accountKey': a.get('accountKey', 'default'), 'threadId': a.get('threadId'),
                'model': a.get('model'), 'effort': a.get('effort'), 'fastMode': a.get('fastMode')}

    def analytics_event(self, db, a, method, p, *, at=None, source='live'):
        at = time.time() if at is None else at
        meta = self.analytics_agent(db, a)
        if source == 'live':
            hour = int(at // 3600) * 3600
            key = encoded([a['id'], method, hour])
            db.execute('INSERT INTO analytics_notifications VALUES (?,?,?,?,?,1,?) ON CONFLICT(id) DO UPDATE SET count=count+1,bytes=bytes+excluded.bytes',
                       (key, a['id'], meta['rootId'], method, hour, len(encoded(p).encode('utf-8'))))
        turn = p.get('turnId') or (p.get('turn') or {}).get('id') or a.get('turnId')
        meta.update(threadId=p.get('threadId') or a.get('threadId'), turnId=turn)
        if method == 'thread/tokenUsage/updated':
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
                        return
                    existing, key = record, exact['id']
            if existing and response_id and existing.get('responseId') and response_id != existing['responseId']:
                key += ':' + response_id
                existing = db.execute('SELECT record FROM analytics_usage WHERE id=?', (key,)).fetchone()
                existing = json.loads(existing[0]) if existing else None
            if existing:
                if response_id and (not existing.get('responseId') or p.get('rawTokenUsageRecord') and not existing.get('rawTokenUsageRecord')):
                    existing.update(responseId=response_id, source=source, at=at, recordedAt=time.time(),
                                    rawTokenUsageRecord=p.get('rawTokenUsageRecord'), turnUsage=p.get('turnUsage'),
                                    requestUsage=p.get('requestUsage'), model=meta['model'], last=last, total=current,
                                    delta={k: number(last.get(k)) for k in TOKEN_FIELDS},
                                    counterDomain='response' if p.get('rawTokenUsageRecord') else 'nativeNotice',
                                    cumulativeDelta={k: None for k in TOKEN_FIELDS}, reset=None, baselineMissing=True)
                    db.execute('UPDATE analytics_usage SET at=?,record=? WHERE id=?', (at, json.dumps(existing), key))
                return
            counter_domain = 'response' if p.get('rawTokenUsageRecord') else 'nativeNotice'
            prior = db.execute("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND at<=? AND json_extract(record,'$.counterDomain')=? ORDER BY at DESC,seq DESC LIMIT 1", (a['id'], meta['threadId'], at, counter_domain)).fetchone()
            prior = json.loads(prior[0]) if prior else None
            reset = bool(prior and any(number(current.get(k)) is not None and number(prior['total'].get(k)) is not None and current[k] < prior['total'][k] for k in TOKEN_FIELDS))
            cumulative_delta = {k: current[k] - prior['total'][k] if prior and not reset and number(current.get(k)) is not None and number(prior['total'].get(k)) is not None else None for k in TOKEN_FIELDS}
            record = {**meta, 'id': key, 'at': at, 'recordedAt': time.time(), 'source': source, 'timestampSource': p.get('_analyticsTimestampSource', 'observed' if source == 'live' else 'record'),
                      'last': last, 'total': current, 'delta': {k: number(last.get(k)) for k in TOKEN_FIELDS}, 'raw': usage,
                      'cumulativeDelta': cumulative_delta, 'counterDomain': counter_domain,
                      'modelContextWindow': number(usage.get('modelContextWindow')),
                      'reset': reset, 'baselineMissing': prior is None or reset,
                      'fingerprint': fingerprint, 'responseId': response_id,
                      'rawTokenUsageRecord': p.get('rawTokenUsageRecord'), 'turnUsage': p.get('turnUsage'),
                      'requestUsage': p.get('requestUsage')}
            db.execute('INSERT OR IGNORE INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                       (key, a['id'], meta['rootId'], meta['threadId'], turn, at, json.dumps(record)))
            return
        if turn and method in {'turn/started', 'turn/completed', 'item/agentMessage/delta', 'item/started', 'item/completed'}:
            key = ':'.join((a['id'], str(meta['threadId']), str(turn)))
            row = db.execute('SELECT record FROM analytics_turns WHERE id=?', (key,)).fetchone()
            record = json.loads(row[0]) if row else {**meta, 'id': key, 'at': at, 'startedAt': None, 'finishedAt': None,
                'firstOutputAt': None, 'durationMs': None, 'firstOutputDelayMs': None, 'status': 'unknown', 'source': source}
            if method == 'turn/started':
                record['startedAt'] = min(at, record['startedAt']) if record['startedAt'] is not None else at
                if record['finishedAt'] is None:
                    record['status'] = 'running'
            elif method == 'turn/completed':
                record.update(finishedAt=at, status=(p.get('turn') or {}).get('status', 'unknown'),
                              error=(p.get('turn') or {}).get('error'),
                              nativeDurationMs=number(p.get('durationMs')), nativeTimeToFirstTokenMs=number(p.get('timeToFirstTokenMs')))
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
            size = payload_size(delta)
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
            if input_value is not None:
                record['input'] = payload_size(input_value)
            if output_value is not None:
                record['output'] = payload_size(output_value)
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

    def analytics_model_payload(self, db, a, p, *, at=None, turn_id=None, source='rollout'):
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
            record['input'] = record['modelInput'] = payload_size(p.get('arguments', p.get('input')))
            record['startedAt'] = at
            record['status'] = 'completed' if record.get('finishedAt') is not None else 'running'
        elif kind in outputs:
            record['output'] = record['modelOutput'] = payload_size(p.get('output', p.get('content')))
            record['finishedAt'] = at
            record['status'] = 'failed' if p.get('is_error') is True else 'completed'
        else:
            direction = 'input' if p.get('role') in {'user', 'system', 'developer'} else 'output'
            # Never retain hidden reasoning text. Sizes only, if the provider exposes a payload.
            record[direction] = payload_size(p.get('content', p.get('summary')))
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

    def analytics(self, agent=None, scope='agent', **options):
        if scope not in {'agent', 'team', 'all'}:
            raise ValueError('Unknown analytics scope')
        if options.get('view') == 'turn-errors':
            self.agent(agent)
            thread = options.get('thread')
            turns = list(dict.fromkeys((options.get('turns') or '').split(',')))
            if scope != 'agent' or not thread or not all(turns) or len(turns) > 120:
                raise ValueError('Select one thread and up to 120 turns')
            with self.db() as db:
                return {'turns': self.analytics_turn_errors(db, agent, thread, turns)}
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
        with self.db() as db:
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
            usage = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_usage' + clause + ' ORDER BY at,seq', args)]
            authoritative_turns = {(row['agent'], row['thread'], row['turn']) for row in db.execute("SELECT DISTINCT agent,thread,turn FROM analytics_usage WHERE json_extract(record,'$.responseId') IS NOT NULL")}
            provisional = [r for r in usage if not r.get('responseId') and (r['agentId'], r.get('threadId'), r.get('turnId')) in authoritative_turns]
            usage = [r for r in usage if r.get('responseId') or (r['agentId'], r.get('threadId'), r.get('turnId')) not in authoritative_turns]
            turns = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_turns' + clause + ' ORDER BY at', args)]
            records = [json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_items' + clause + ' ORDER BY at DESC,id', args)]
            runtime_agents = {a['id']: a for a in self.records(db, 'agents')}
            for entry in agents:
                if entry['id'] in runtime_agents:
                    current_agent = runtime_agents[entry['id']]
                    for field in ('name', 'deletedAt', 'model', 'effort', 'fastMode', 'threadId', 'accountKey', 'rootId', 'parentId', 'cwd'):
                        entry[field] = current_agent.get(field)
            operational = {table: self.records(db, table) for table in ('monitors', 'requests')}
            queued_events = [dict(row) for row in db.execute('SELECT agent,kind,status,created FROM runtime_events')]
            notification_rows = [dict(row) for row in db.execute('SELECT * FROM analytics_notifications')]
            limit_rows = [dict(row) for row in db.execute('SELECT account,at,record FROM analytics_limits')]
            history = ([json.loads(row[0]) for row in db.execute('SELECT record FROM analytics_history')]
                       if db.execute("SELECT 1 FROM sqlite_master WHERE name='analytics_history'").fetchone() else [])
            capture_error = db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()
            capture_error = json.loads(capture_error[0]) if capture_error else {'count': 0, 'last': None}
            tracking = float(db.execute("SELECT value FROM analytics_meta WHERE key='trackingSince'").fetchone()[0])
        all_calls = [r for r in records if r['isTool']]
        model_calls = [r for r in all_calls if r.get('payloadBoundary') == 'model' and (tool is None or r['name'] == tool)]
        protocol_calls = [r for r in all_calls if r.get('payloadBoundary') != 'model' and (tool is None or r['name'] == tool)]
        calls = [r for r in all_calls if tool is None or r['name'] == tool]
        by_tool = defaultdict(list)
        for call in all_calls:
            by_tool[(call['name'], call['type'])].append(call)
        def size(rows, direction, key):
            return nullable_sum((r.get(direction) or {}).get(key) for r in rows)
        def durations(rows):
            values = sorted(r['durationMs'] for r in rows if number(r.get('durationMs')) is not None)
            return {'count': len(values), 'min': min(values) if values else None, 'max': max(values) if values else None,
                    'mean': statistics.mean(values) if values else None,
                    'p50': statistics.median(values) if values else None,
                    'p95': values[max(0, math.ceil(len(values) * .95) - 1)] if values else None}
        tools = [{'name': name, 'type': kind, 'payloadBoundary': rows[0].get('payloadBoundary', 'protocol'), 'calls': len(rows), 'failed': sum(r['status'] == 'failed' for r in rows),
                  'inputBytes': size(rows, 'input', 'bytes'), 'outputBytes': size(rows, 'output', 'bytes'),
                  'modelInputBytes': size(rows, 'input', 'bytes') if kind == 'modelToolCall' else None,
                  'modelOutputBytes': size(rows, 'output', 'bytes') if kind == 'modelToolCall' else None,
                  'durationMs': nullable_sum(r.get('durationMs') for r in rows),
                  'imageCount': nullable_sum([size(rows, 'input', 'imageCount'), size(rows, 'output', 'imageCount')]),
                  'inputMeasurements': sum(r.get('input') is not None for r in rows), 'outputMeasurements': sum(r.get('output') is not None for r in rows), 'duration': durations(rows)}
                 for (name, kind), rows in by_tool.items()]
        tools.sort(key=lambda row: row['outputBytes'] or 0, reverse=True)
        tokens = {field: nullable_sum(r['delta'].get(field) for r in usage) for field in TOKEN_FIELDS}
        cache_pairs = [r for r in usage if number(r['delta'].get('inputTokens')) is not None and number(r['delta'].get('cachedInputTokens')) is not None and r['delta']['cachedInputTokens'] <= r['delta']['inputTokens']]
        contexts = [number(r['last'].get('totalTokens')) for r in usage]
        percents = [r['last']['totalTokens'] / r['modelContextWindow'] * 100 for r in usage if number(r['last'].get('totalTokens')) is not None and r.get('modelContextWindow')]
        compactions = [r for r in records if r['type'] == 'contextCompaction' and r.get('finishedAt') is not None]
        snapshots = [r for r in records if r['type'] == 'compactionSnapshot']
        native_compaction_turns = {(r['agentId'], r.get('turnId')) for r in compactions}
        compactions += [r for r in snapshots if (r['agentId'], r.get('turnId')) not in native_compaction_turns]
        item_groups = defaultdict(list)
        for r in records:
            item_groups[r['type']].append(r)
        detailed_item_groups = defaultdict(list)
        for r in records:
            detailed_item_groups[(r['type'], r.get('payloadBoundary'), r.get('category'), r.get('role'))].append(r)
        non_tool_items = [r for r in records if not r['isTool']]
        selected_ids = {r['agentId'] for r in records + usage}
        relevant_agents = [a for a in agents if scope == 'all' or a['id'] == agent and scope == 'agent' or scope == 'team' and (a.get('rootId') or a['id']) == (selected.get('rootId') or agent)]
        summary = {'agents': len(selected_ids), 'turns': len({(r['agentId'], r.get('turnId')) for r in records + usage if r.get('turnId')}),
                   'usageSamples': len(usage), 'provisionalUsageSamples': len(provisional), 'exactResponseSamples': sum(bool(r.get('responseId')) for r in usage), 'legacyUsageSamples': sum(not r.get('responseId') for r in usage), 'modelToolCalls': len(model_calls), 'protocolToolCalls': len(protocol_calls), 'observedToolRows': len(calls), 'toolCalls': len(model_calls), 'failedToolCalls': sum(r['status'] == 'failed' for r in model_calls), 'modelFailedToolCalls': sum(r['status'] == 'failed' for r in model_calls), 'protocolFailedToolCalls': sum(r['status'] == 'failed' for r in protocol_calls),
                   'compactions': len(compactions), 'inputBytes': size(model_calls, 'input', 'bytes'), 'outputBytes': size(model_calls, 'output', 'bytes'),
                   'modelInputBytes': size(model_calls, 'input', 'bytes'), 'modelOutputBytes': size(model_calls, 'output', 'bytes'),
                   'protocolInputBytes': size(protocol_calls, 'input', 'bytes'), 'protocolOutputBytes': size(protocol_calls, 'output', 'bytes'),
                   'durationMs': nullable_sum(r.get('durationMs') for r in model_calls),
                   'modelDurationMs': nullable_sum(r.get('durationMs') for r in model_calls),
                   'protocolDurationMs': nullable_sum(r.get('durationMs') for r in protocol_calls), 'tokens': tokens,
                   'tokenObservations': {field: sum(r['delta'].get(field) is not None for r in usage) for field in TOKEN_FIELDS},
                   'cacheHitRate': tokens['cachedInputTokens'] / tokens['inputTokens'] if len(cache_pairs) == len(usage) and tokens['cachedInputTokens'] is not None and tokens['inputTokens'] else None,
                   'cacheHitRateSamples': len(cache_pairs), 'cacheHitRateTotalSamples': len(usage),
                   'peakContextTokens': max((v for v in contexts if v is not None), default=None),
                   'peakContextPercent': max(percents, default=None), 'baselineMissingSamples': sum(r['baselineMissing'] for r in usage)}
        relevant_ids = {a['id'] for a in relevant_agents}
        def within(value):
            return value is not None and (start is None or value >= start) and (end is None or value <= end)
        def group_usage(field):
            groups = defaultdict(list)
            for sample in usage:
                groups[sample.get(field)].append(sample)
            return [{field: key, 'samples': len(rows), 'tokens': {f: nullable_sum(r['delta'].get(f) for r in rows) for f in TOKEN_FIELDS}} for key, rows in groups.items()]
        agent_totals = []
        for entry in relevant_agents:
            samples = [r for r in usage if r['agentId'] == entry['id']]
            own_calls = [r for r in calls if r['agentId'] == entry['id']]
            own_model = [r for r in own_calls if r.get('payloadBoundary') == 'model']
            own_protocol = [r for r in own_calls if r.get('payloadBoundary') != 'model']
            agent_totals.append({**entry, 'tokens': {field: nullable_sum(r['delta'].get(field) for r in samples) for field in TOKEN_FIELDS},
                                'usageSamples': len(samples), 'toolCalls': len(own_model), 'modelToolCalls': len(own_model), 'protocolToolCalls': len(own_protocol),
                                'failedToolCalls': sum(r['status'] == 'failed' for r in own_model), 'protocolFailedToolCalls': sum(r['status'] == 'failed' for r in own_protocol),
                                'compactions': sum(r['agentId'] == entry['id'] for r in compactions), 'duration': durations(own_model), 'protocolDuration': durations(own_protocol)})
        monitors = [{k: m.get(k) for k in ('id', 'agent', 'status', 'created', 'finished', 'bytes', 'exitCode', 'error', 'timeout_ms')}
                    for m in operational['monitors'] if m.get('agent') in relevant_ids and within(m.get('created'))]
        counts = defaultdict(int)
        for event in queued_events:
            if event['agent'] in relevant_ids and within(event['created']):
                counts[event['status'] + ':' + event['kind']] += 1
        approvals = [r for r in operational['requests'] if r.get('agent') in relevant_ids]
        account_keys = {a.get('accountKey', 'default') for a in relevant_agents}
        notifications = [r for r in notification_rows if r['agent'] in relevant_ids and (start is None or r['hour'] + 3600 > start) and (end is None or r['hour'] <= end)]
        return {'version': 1, 'generatedAt': time.time(), 'filters': {'agent': agent, 'scope': scope, 'from': start, 'to': end, 'tool': tool},
                'coverage': {'trackingSince': tracking, 'captureErrors': capture_error, 'historyErrors': [r for r in history if r.get('status') == 'error'], 'provisionalUsageSamples': len(provisional), 'tokenAttribution': 'provider_usage_only', 'payloadMeasurement': 'observed_protocol_payload',
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
                'modelTotals': group_usage('model'), 'accountTotals': group_usage('accountKey'),
                'operations': {'monitors': monitors, 'eventCounts': dict(counts),
                               'approvalCounts': {status: sum(r.get('status') == status for r in approvals) for status in {r.get('status') for r in approvals}}},
                'notifications': notifications, 'history': [r for r in history if not r.get('agent') and not r.get('agentId') or r.get('agent') in relevant_ids or r.get('agentId') in relevant_ids],
                'rateLimits': [{'accountKey': r['account'], 'at': r['at'], 'data': json.loads(r['record'])} for r in limit_rows if r['account'] in account_keys and within(r['at'])],
                'turns': turns, 'timeline': usage if export else usage[-500:], 'timelineTotal': len(usage), 'provisionalUsage': provisional if export else provisional[-100:],
                'calls': calls if export else calls[offset:offset + limit],
                'items': [{'type': kind, 'count': len(rows), 'bytes': nullable_sum([size(rows, 'input', 'bytes'), size(rows, 'output', 'bytes')]),
                           'chars': nullable_sum([size(rows, 'input', 'chars'), size(rows, 'output', 'chars')])} for kind, rows in item_groups.items()],
                'itemRecords': non_tool_items if export else non_tool_items[:100], 'itemRecordsTotal': len(non_tool_items),
                'itemBreakdown': [{'type': kind, 'payloadBoundary': boundary, 'category': category, 'role': role, 'count': len(rows), 'inputBytes': size(rows, 'input', 'bytes'), 'outputBytes': size(rows, 'output', 'bytes'), 'inputMeasurements': sum(r.get('input') is not None for r in rows), 'outputMeasurements': sum(r.get('output') is not None for r in rows)} for (kind, boundary, category, role), rows in detailed_item_groups.items()],
                'compactions': compactions, 'compactionSnapshots': snapshots, 'pagination': {'limit': limit, 'offset': offset, 'total': len(calls), 'hasMore': not export and offset + limit < len(calls)}}
