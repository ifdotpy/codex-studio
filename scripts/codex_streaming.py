"""Bounded durable updates for native text and command-output streams."""
import json
import threading
import time

from codex_analytics import encoded
from codex_native_errors import advance_native_status

FLUSH_SECONDS = .75


class StreamBuffer:
    def __init__(self, runtime):
        self.runtime = runtime
        self.lock = threading.RLock()
        self.entries = {}
        self.closed = {}
        self.timer = None
        self.error = None

    def enqueue(self, message, account, connection):
        method, params = message.get('method'), message.get('params') or {}
        if (method not in {'item/agentMessage/delta', 'item/commandExecution/outputDelta'}
                or not isinstance(params, dict) or not isinstance(params.get('delta'), str)
                or not isinstance(params.get('threadId'), str) or not isinstance(params.get('itemId'), str)):
            return False
        key = (account, connection, params['threadId'], params.get('turnId'), params['itemId'], method)
        samples = message.get('_studioNotificationSamples') or [params]
        now = time.time()
        # Match analytics_event's original encoded payload accounting. This
        # work is outside Runtime.lock and one buffered batch keeps one string.
        size = sum(len(encoded(sample).encode('utf-8')) for sample in samples)
        delta = params['delta']
        metrics = (len(delta.encode('utf-8')), len(delta), delta.count('\n'))
        command = method == 'item/commandExecution/outputDelta'
        retained = delta[-12000:] if command else delta
        stored_params = {**params, 'delta': retained} if command else dict(params)
        with self.lock:
            if key in self.closed or self.runtime.closed:
                return True
            entry = self.entries.setdefault(key, {'base': None, 'batches': []})
            batches = entry['batches']
            if command and batches and int(batches[-1][3] // 3600) == int(now // 3600):
                previous = batches[-1]
                batches[-1] = ((previous[0] + retained)[-12000:], previous[1] + len(samples),
                               previous[2] + size, previous[3], previous[4],
                               previous[5] + metrics[0], previous[6] + metrics[1],
                               previous[7] + metrics[2], message.get('_studioSupervisorSequence'))
            else:
                batches.append((retained, len(samples), size, now, stored_params, *metrics,
                                message.get('_studioSupervisorSequence')))
            self._schedule_locked()
        return True

    def _schedule_locked(self):
        flushable = any(entry['batches'] and not any(
            len(batch) > 8 and batch[8] is not None for batch in entry['batches'])
            for entry in self.entries.values())
        if self.timer is not None or not flushable:
            return
        timer = threading.Timer(FLUSH_SECONDS, self._tick)
        timer.daemon = True
        self.timer = timer
        timer.start()

    def _tick(self):
        with self.lock:
            self.timer = None
        try:
            with self.runtime.lock, self.runtime.db() as db:
                self.flush_locked(db)
            self.error = None
        except Exception as error:
            # Keep every batch for a later timer or terminal flush. A failed
            # transaction cannot consume uncommitted stream text.
            self.error = str(error)[:500]
        with self.lock:
            if not self.runtime.closed:
                self._schedule_locked()

    def flush_locked(self, db, *, account=None, thread_id=None, item_id=None, turn_id=None,
                     close=False, close_commands=True, force=False,
                     supervisor_handle=None, supervisor_sequence=None, analytics_captures=None):
        """Caller holds Runtime.lock. Commit before consuming buffered batches."""
        with self.lock:
            selected = [(key, entry, list(entry['batches'])) for key, entry in self.entries.items()
                        if (account is None or key[0] == account)
                        and (thread_id is None or key[2] == thread_id)
                        and (item_id is None or key[4] == item_id)
                        and (turn_id is None or key[3] == turn_id)
                        and (supervisor_handle is not None or not any(
                            len(batch) > 8 and batch[8] is not None for batch in entry['batches']))]
            if close:
                for key, _, _ in selected:
                    if close_commands or key[5] == 'item/agentMessage/delta':
                        self.closed[key] = time.monotonic()
                while len(self.closed) > 8192:
                    self.closed.pop(next(iter(self.closed)))
        applied = []
        for key, entry, batches in selected:
            if not batches:
                continue
            account, connection, thread, turn, item_id_value, method = key
            if not force and not self.runtime.connection_current(account, connection):
                applied.append((key, entry, len(batches), None))
                continue
            row = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? "
                             "AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
                             "ELSE json_extract(record,'$.accountKey') END=? ORDER BY rowid LIMIT 1",
                             (thread, account)).fetchone()
            if row is None:
                applied.append((key, entry, len(batches), None))
                continue
            agent = json.loads(row[0])
            if agent.get('deletedAt'):
                applied.append((key, entry, len(batches), None))
                continue
            key_id = agent['id'] + ':' + item_id_value
            saved = db.execute('SELECT record FROM runtime_items WHERE id=?', (key_id,)).fetchone()
            previous = json.loads(saved[0]) if saved else {}
            task_row = (db.execute('SELECT record FROM runtime_tasks WHERE id=?', (key_id,)).fetchone()
                        if method == 'item/commandExecution/outputDelta' else None)
            task = json.loads(task_row[0]) if task_row else None
            stale = bool(turn and agent.get('turnId') != turn)
            base = entry['base']
            if base is None:
                if method == 'item/agentMessage/delta':
                    if previous.get('truncated'):
                        from codex_search_text import search_text
                        base = search_text(db, key_id)
                    else:
                        base = previous.get('text', '')
                else:
                    if not saved:
                        applied.append((key, entry, len(batches), None))
                        continue
                    try:
                        base = (json.loads(previous['text']).get('aggregatedOutput') or '')[-12000:]
                    except (ValueError, KeyError, TypeError):
                        base = (task.get('tail') or '')[-12000:] if task else ''
            delta = ''.join(batch[0] for batch in batches)
            if method == 'item/commandExecution/outputDelta':
                delta = delta[-12000:]
            target = (base + delta)[-12000:] if method == 'item/commandExecution/outputDelta' else base + delta
            count = sum(batch[1] for batch in batches)
            stream_bytes = sum(batch[5] for batch in batches)
            stream_chars = sum(batch[6] for batch in batches)
            stream_lines = sum(batch[7] for batch in batches)
            first_at = batches[0][3]
            params = {**batches[0][4], 'delta': delta}
            hour = int(first_at // 3600) * 3600
            synthetic_size = len(encoded(params).encode('utf-8'))
            def capture(db, agent=agent, method=method, params=params, first_at=first_at,
                        batches=batches, hour=hour, synthetic_size=synthetic_size,
                        count=count, thread=thread, item_id_value=item_id_value,
                        stream_bytes=stream_bytes, stream_chars=stream_chars,
                        stream_lines=stream_lines, delta=delta):
                self.runtime.analytics_event(db, agent, method, params, at=first_at)
                by_hour = {}
                for batch in batches:
                    batch_count, batch_bytes, batch_at = batch[1:4]
                    bucket = int(batch_at // 3600) * 3600
                    old_count, old_bytes = by_hour.get(bucket, (0, 0))
                    by_hour[bucket] = (old_count + batch_count, old_bytes + batch_bytes)
                first_count, first_bytes = by_hour.pop(hour)
                db.execute('UPDATE analytics_notifications SET count=count+?,bytes=bytes+? WHERE id=?',
                           (first_count - 1, first_bytes - synthetic_size,
                            encoded([agent['id'], method, hour])))
                for bucket, (bucket_count, bucket_bytes) in by_hour.items():
                    db.execute('INSERT INTO analytics_notifications VALUES (?,?,?,?,?,?,?) '
                               'ON CONFLICT(id) DO UPDATE SET count=count+excluded.count,bytes=bytes+excluded.bytes',
                               (encoded([agent['id'], method, bucket]), agent['id'],
                                agent.get('rootId') or agent['id'], method, bucket, bucket_count, bucket_bytes))
                if count > 1 or method == 'item/commandExecution/outputDelta':
                    analytics_key = ':'.join((agent['id'], str(thread), str(item_id_value)))
                    db.execute("UPDATE analytics_items SET record=json_set(record,"
                               "'$.stream.deltas',coalesce(json_extract(record,'$.stream.deltas'),0)+?,"
                               "'$.stream.bytes',coalesce(json_extract(record,'$.stream.bytes'),0)+?,"
                               "'$.stream.chars',coalesce(json_extract(record,'$.stream.chars'),0)+?,"
                               "'$.stream.lines',coalesce(json_extract(record,'$.stream.lines'),0)+?) "
                               "WHERE id=? AND json_extract(record,'$.stream') IS NOT NULL "
                               "AND json_extract(record,'$.finishedAt') IS NULL",
                               (count - 1, stream_bytes - len(delta.encode('utf-8')),
                                stream_chars - len(delta), stream_lines - delta.count('\n'), analytics_key))
            if analytics_captures is None:
                self.runtime.analytics_safe(db, capture)
            else:
                analytics_captures.append(capture)
            if stale and (method == 'item/agentMessage/delta' or not task
                          or task.get('kind') != 'command' or task.get('turnId') != turn):
                applied.append((key, entry, len(batches), None))
                continue
            if method == 'item/agentMessage/delta':
                advance_native_status(agent, method, params)
                self.runtime.item(db, agent['id'], item_id_value, 'assistant', target,
                                  streaming=True, turnId=turn or agent.get('turnId'),
                                  phase=previous.get('phase'), index_search=len(target) > 20000)
                agent['activity'] = {'phase': 'writing', 'at': time.time()}
                agent['tail'] = target[-300:]
            else:
                try:
                    item = json.loads(previous['text'])
                except (ValueError, KeyError, TypeError):
                    item = {'type': 'commandExecution', 'id': item_id_value}
                truncated = (bool(item.get('outputTruncated')) or bool(task and task.get('outputTruncated'))
                             or len(base) + stream_chars > 12000)
                item.update(aggregatedOutput=target, outputTruncated=truncated)
                serialized = json.dumps(item, ensure_ascii=False)
                self.runtime.item(db, agent['id'], item_id_value, 'output', serialized,
                                  'commandExecution', toolStatus='running', turnId=turn or agent.get('turnId'),
                                  index_search=False)
                if task:
                    task.update(tail=target, outputTruncated=truncated)
                    self.runtime.put(db, 'tasks', task)
            if not stale:
                agent['events'] += count
                agent['lastEvent'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
                self.runtime.put(db, 'agents', agent)
            applied.append((key, entry, len(batches), target))
        if supervisor_handle is not None and supervisor_sequence is not None:
            db.execute('INSERT INTO runtime_supervisor_cursor VALUES (?,?) '
                       'ON CONFLICT(handle) DO UPDATE SET sequence=max(sequence,excluded.sequence)',
                       (supervisor_handle, supervisor_sequence))
        if applied or (supervisor_handle is not None and supervisor_sequence is not None):
            db.commit()
            with self.lock:
                for key, entry, length, target in applied:
                    if self.entries.get(key) is entry:
                        del entry['batches'][:length]
                        if target is None and not entry['batches']:
                            self.entries.pop(key, None)
                        elif target is not None:
                            entry['base'] = target
        if close:
            with self.lock:
                for key, entry, _ in selected:
                    if (close_commands or key[5] == 'item/agentMessage/delta') \
                            and self.entries.get(key) is entry and not entry['batches']:
                        self.entries.pop(key, None)
        with self.lock:
            self._schedule_locked()

    def shutdown_locked(self, db):
        self.flush_locked(db, close=True)
        with self.lock:
            if self.timer is not None:
                self.timer.cancel()
                self.timer = None
