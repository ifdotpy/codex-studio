"""Reconcile lost terminal notifications against the owning native connection.

Silence only schedules a read. It never proves completion or authorizes replay.
"""
from concurrent.futures import Future
from datetime import datetime
import json
import time

START_PACE_WINDOW_SECONDS = 90
START_PACE_LIMIT = 4
UNKNOWN_START_HOLD_SECONDS = 600


def recent_account_starts(agents, now=None):
    """Count recent Codex starts that still lack a native turn identity."""
    now = time.time() if now is None else now
    counts = {}
    for agent in agents:
        attempt = agent.get('startAttempt') or {}
        created = attempt.get('created')
        if (agent.get('provider', 'codex') != 'codex' or not agent.get('autoWake')
                or agent.get('deletedAt') or not agent.get('inFlight')
                or not attempt.get('id') or attempt.get('activeAtReservation')
                or attempt.get('turnId') or attempt.get('observedTurnId')
                or type(created) not in (float, int)
                or not 0 <= now - created < START_PACE_WINDOW_SECONDS):
            continue
        account = agent.get('accountKey', 'default')
        counts[account] = counts.get(account, 0) + 1
    return counts


def read_native_turn(server, thread_id, turn_id):
    """Read all items of one exact turn without loading other turns' bodies."""
    from codex_native_errors import NativeRpcError

    deadline = time.monotonic() + 20

    def read(method, params):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Native turn history read timed out')
        return server.call(method, params, timeout=min(10, remaining))

    def turn_page(params):
        page = read('thread/turns/list', params)
        if not isinstance(page, dict) or not isinstance(page.get('data'), list):
            raise ValueError('Native turn history returned an invalid page')
        ids = set()
        for turn in page['data']:
            if (not isinstance(turn, dict) or not isinstance(turn.get('id'), str)
                    or not turn['id'] or turn['id'] in ids
                    or turn.get('threadId', thread_id) != thread_id
                    or not isinstance(turn.get('items'), list)
                    or turn.get('itemsView', 'full') not in {'full', 'summary', 'notLoaded'}):
                raise ValueError('Native turn history changed a turn identity or items view')
            ids.add(turn['id'])
        return page

    def full_turn(turn):
        if turn.get('itemsView', 'full') != 'full':
            raise ValueError('Native turn history did not return all target items')
        items, known = [], {}
        for item in turn['items']:
            add_item(items, known, item)
        return {**turn, 'items': items}

    def add_item(items, known, item):
        if (not isinstance(item, dict) or not isinstance(item.get('id'), str)
                or not item['id'] or not isinstance(item.get('type'), str) or not item['type']
                or item.get('turnId', turn_id) != turn_id
                or item.get('threadId', thread_id) != thread_id):
            raise ValueError('Native turn history changed an item identity')
        previous = known.get(item['id'])
        if previous is not None:
            if previous != item:
                raise ValueError('Native turn history returned conflicting item receipts')
            return
        known[item['id']] = item
        items.append(item)

    cursor, seen = None, set()
    while True:
        params = {'threadId': thread_id, 'limit': 10,
                  'sortDirection': 'desc', 'itemsView': 'notLoaded'}
        if cursor is not None:
            params['cursor'] = cursor
        page = turn_page(params)
        turn = next((t for t in page['data'] if t['id'] == turn_id), None)
        if turn is not None:
            if turn.get('itemsView', 'full') == 'full':
                return full_turn(turn)
            items, known = [], {}
            item_cursor, item_seen = None, set()
            while True:
                item_params = {'threadId': thread_id, 'turnId': turn_id,
                               'limit': 100, 'sortDirection': 'asc'}
                if item_cursor is not None:
                    item_params['cursor'] = item_cursor
                try:
                    item_page = read('thread/items/list', item_params)
                except NativeRpcError as error:
                    if error.code != -32601:
                        raise
                    fallback = turn_page({**params, 'itemsView': 'full'})
                    exact = next((t for t in fallback['data'] if t['id'] == turn_id), None)
                    if exact is None:
                        raise ValueError('Native target turn changed during its history read')
                    return full_turn(exact)
                if not isinstance(item_page, dict) or not isinstance(item_page.get('data'), list):
                    raise ValueError('Native turn history returned an invalid item page')
                for entry in item_page['data']:
                    if not isinstance(entry, dict):
                        raise ValueError('Native turn history returned an invalid item entry')
                    if 'item' in entry:
                        if (entry.get('turnId') != turn_id
                                or entry.get('threadId', thread_id) != thread_id):
                            raise ValueError('Native turn history changed the item source identity')
                        item = entry['item']
                    else:
                        item = entry
                    add_item(items, known, item)
                item_cursor = item_page.get('nextCursor')
                if item_cursor is None:
                    break
                if not isinstance(item_cursor, str) or not item_cursor or item_cursor in item_seen:
                    raise ValueError('Native turn history repeated or changed its item page cursor')
                item_seen.add(item_cursor)
            # Item pages can span native updates. Keep terminal metadata tied to
            # the same exact turn after the complete item read.
            confirmed = next((t for t in turn_page(params)['data'] if t['id'] == turn_id), None)
            if (confirmed is None or confirmed.get('status') != turn.get('status')
                    or confirmed.get('error') != turn.get('error')):
                raise ValueError('Native target turn changed during its history read')
            return {**confirmed, 'items': items, 'itemsView': 'full'}
        cursor = page.get('nextCursor')
        if cursor is None:
            return None
        if not isinstance(cursor, str) or not cursor or cursor in seen:
            raise ValueError('Native turn history repeated or changed its page cursor')
        seen.add(cursor)


def orphan_busy(agent):
    """A busy flag with no native turn and no Studio start cannot be cleared by any notification."""
    return bool(agent.get('inFlight') and not agent.get('turnId') and not agent.get('startAttempt')
                and agent.get('threadId') and not agent.get('deletedAt'))


def unconfirmed_start(agent):
    """A submitted start needs its exact input receipt before another dispatch."""
    attempt = agent.get('startAttempt') or {}
    held = (agent.get('startOutcomeHold') or {}).get('stage') == 'held'
    return bool((agent.get('inFlight') or held) and agent.get('autoWake') and not agent.get('turnId')
                and agent.get('threadId') and not agent.get('deletedAt')
                and not agent.get('nativeFailureHold') and not agent.get('accountTransferId')
                and not agent.get('workspaceOperation') and not attempt.get('action')
                and attempt.get('id') and attempt.get('submitted') is True
                and (attempt.get('executionOutcome') == 'unknown'
                     or agent.get('error') == 'turn/start response timed out; outcome unknown')
                and attempt.get('epoch') == agent.get('epoch')
                and attempt.get('accountKey', 'default') == agent.get('accountKey', 'default')
                and attempt.get('threadId') == agent.get('threadId')
                and attempt.get('events') and not attempt.get('turnId')
                and not attempt.get('observedTurnId'))


class TurnRecoveryMixin:
    def queue_turn_recovery(self, agents, *, force_id=None):
        """At most one native probe occupies the existing recovery executor."""
        now = time.time()
        with self.lock:
            if self.closed or getattr(self, '_turn_recovery_busy', False):
                return
            checked = getattr(self, '_turn_recovery_checked', {})
            self._turn_recovery_checked = checked
            candidates = []
            for a in agents:
                if (not a.get('inFlight') or not (a.get('turnId') or orphan_busy(a) or unconfirmed_start(a))
                        or not a.get('threadId') or a.get('deletedAt')):
                    continue
                account = a.get('accountKey', 'default')
                if account not in self.servers or account in self.offline_accounts:
                    continue
                activity = (a.get('activity') or {}).get('at') or a.get('created') or now
                try:
                    activity = max(activity, datetime.fromisoformat((a.get('lastEvent') or '').replace('Z', '+00:00')).timestamp())
                except (ValueError, TypeError):
                    pass
                if unconfirmed_start(a):
                    activity = (a.get('startAttempt') or {}).get('created') or activity
                if a['id'] != force_id and (now - activity < (10 if unconfirmed_start(a) else 120)
                        or now - checked.get(a['id'], 0) < 60):
                    continue
                candidates.append(a)
            if not candidates:
                return
            selected = min(candidates, key=lambda a: (not unconfirmed_start(a), checked.get(a['id'], 0)))
            checked[selected['id']] = now
            self._turn_recovery_busy = True
            try:
                self.recovery_pool.submit(self.run_turn_recovery, selected['id'])
            except Exception:
                self._turn_recovery_busy = False
                raise

    def run_turn_recovery(self, key):
        try:
            result = self.reconcile_turn(key)
            with self.lock:
                notes = getattr(self, '_turn_recovery_results', {})
                notes[key] = {**result, 'at': time.time()}
                self._turn_recovery_results = notes
            return result
        finally:
            with self.lock:
                self._turn_recovery_busy = False

    def reconcile_turn(self, key):
        """Read native state without loading a thread, starting a turn, or stopping work."""
        with self.lock, self.db() as db:
            a = self.agent(key, db)
            account = a.get('accountKey', 'default')
            connection = self.connection_ids.get(account)
            if (not (a.get('inFlight') or unconfirmed_start(a))
                    or not (a.get('turnId') or orphan_busy(a) or unconfirmed_start(a)) or a.get('deletedAt')
                    or not self.connection_current(account, connection)):
                return {'status': 'skipped'}
            server = self.servers.get(account)
            if server is None:
                return {'status': 'skipped'}
            expected = {k: a.get(k) for k in ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
        if unconfirmed_start(a):
            return self.reconcile_start_receipt(server, a, expected, connection)
        if not a.get('turnId'):
            return self.reconcile_orphan_busy(server, a, expected, connection)
        try:
            # Most long-running turns only need this small status response.
            thread = server.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != a['threadId']:
                raise ValueError('Native thread identity changed')
            if thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
                return {'status': 'active_or_unknown'}
            # The exact terminal turn is required. Never infer an outcome from silence.
            turn = read_native_turn(server, a['threadId'], a['turnId'])
            thread = server.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != a['threadId']:
                raise ValueError('Native thread identity changed')
            state = thread.get('status', {}).get('type')
            if state not in {'idle', 'notLoaded'} or not turn or turn.get('status') not in {'completed', 'failed', 'interrupted'}:
                return {'status': 'unconfirmed'}
            result = Future()
            def apply():
                try:
                    result.set_result(self.apply_turn_recovery(expected, connection, state, turn))
                except Exception as error:
                    result.set_exception(error)
            # Process earlier notifications first, then recheck every identity under the lock.
            server.after_events(apply)
            return result.result(timeout=10)
        except Exception as error:
            # A failed read is not a failed turn. Retain the recorded state and receipt.
            return {'status': 'unconfirmed', 'error': str(error)}

    def reconcile_start_receipt(self, server, a, expected, connection):
        """Read an exact start receipt without replay or an idle-state inference."""
        attempt = a['startAttempt']
        from codex_connection_recovery import supervisor_identity
        previous_child = attempt.get('supervisorIdentity')
        current_child = supervisor_identity(server)
        replaced_child = bool(previous_child and current_child
            and previous_child.get('stateDir') == current_child.get('stateDir')
            and previous_child.get('handle') == current_child.get('handle')
            and type(previous_child.get('generation')) is int
            and current_child['generation'] > previous_child['generation'])
        if attempt.get('connectionId') != connection and not replaced_child:
            return {'status': 'unconfirmed'}
        message = attempt['events'][0]
        cursor, seen = None, set()
        deadline = time.monotonic() + 20
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('Native start receipt read timed out')
                params = {'threadId': a['threadId'], 'limit': 10,
                          'sortDirection': 'desc', 'itemsView': 'full'}
                if cursor is not None:
                    params['cursor'] = cursor
                page = server.call('thread/turns/list', params, timeout=min(5, remaining))
                matches = [turn for turn in page.get('data', [])
                           if turn.get('startOutcome') not in {'preparing', 'not_applied'} and (
                               turn.get('clientUserMessageId') == message or any(
                               item.get('type') == 'userMessage' and item.get('clientId') == message
                               and item.get('deliveryStatus') not in {'preparing', 'not_applied'}
                               for item in turn.get('items', [])))]
                if len(matches) > 1:
                    raise ValueError('Native history has conflicting start receipts')
                if matches:
                    turn = matches[0]
                    if not isinstance(turn.get('id'), str) or not turn['id']:
                        raise ValueError('The native start receipt has no turn identity')
                    break
                cursor = page.get('nextCursor')
                if not cursor:
                    # A replaced native child cannot finish an old request. A
                    # live child can still accept its request after this read.
                    if replaced_child:
                        return self.restore_absent_start(server, expected, connection)
                    if not (a.get('startOutcomeHold') or {}).get('stage') and (
                            time.time() - attempt.get('created', time.time()) >= UNKNOWN_START_HOLD_SECONDS):
                        return self.hold_unknown_start(server, expected, connection)
                    return {'status': 'unconfirmed'}
                if cursor in seen:
                    raise ValueError('Native start history repeated its page cursor')
                seen.add(cursor)
            result = Future()
            def apply():
                try:
                    with self.lock:
                        current = self.agent(a['id'])
                        if (not self.connection_current(a.get('accountKey', 'default'), connection)
                                or self.closed or not unconfirmed_start(current)
                                or any(current.get(k) != value for k, value in expected.items())):
                            result.set_result({'status': 'superseded'})
                            return
                        self.start_accepted(a['id'], {**attempt, 'connectionId': connection}, {'turn': turn})
                        current = self.agent(a['id'])
                        if (current.get('inFlight') and current.get('turnId') == turn['id']
                                and turn.get('status') in {'completed', 'failed', 'interrupted'}):
                            terminal_source = {k: current.get(k) for k in expected}
                            # An exact terminal turn remains terminal while other native work runs.
                            value = self.apply_turn_recovery(terminal_source, connection, 'active', turn)
                        else:
                            value = {'status': 'reconciled', 'turnId': turn['id'],
                                     'outcome': turn.get('status')}
                    result.set_result(value)
                except Exception as error:
                    result.set_exception(error)
            server.after_events(apply)
            return result.result(timeout=10)
        except Exception as error:
            return {'status': 'unconfirmed', 'error': str(error)}

    def restore_absent_start(self, server, expected, connection):
        """Return an exact batch only after the old native child is gone and idle history is complete."""
        a = expected
        try:
            for _ in range(2):
                thread = server.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=5)['thread']
                status = thread.get('status') or {}
                if (thread.get('id') != a['threadId'] or status.get('type') not in {'idle', 'notLoaded'}
                        or status.get('activeFlags')):
                    return {'status': 'unconfirmed'}
            result = Future()
            def apply():
                try:
                    with self.lock, self.db() as db:
                        current = self.agent(a['id'], db)
                        if (self.closed or not self.connection_current(a.get('accountKey', 'default'), connection)
                                or not unconfirmed_start(current)
                                or any(current.get(k) != value for k, value in expected.items())):
                            result.set_result({'status': 'superseded'})
                            return
                        attempt = current['startAttempt']
                        for event_id in attempt['events']:
                            row = db.execute('SELECT agent,epoch,status,turn_id FROM runtime_events WHERE id=?',
                                             (event_id,)).fetchone()
                            if not row or tuple(row) != (current['id'], current['epoch'], 'uncertain', None):
                                result.set_result({'status': 'unconfirmed'})
                                return
                        for event_id in attempt['events']:
                            db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=?",
                                       (event_id,))
                        current.pop('startAttempt')
                        current.pop('startOutcomeHold', None)
                        current.update(status='queued', inFlight=False, error=None,
                                       activity=None, activeTools=[])
                        current['turnRecovery'] = {'at': time.time(), 'turnId': None,
                            'outcome': 'input_absent', 'source': 'replaced_native_child',
                            'attemptId': attempt['id']}
                        self.put(db, 'agents', current)
                        self.changed.set()
                        result.set_result({'status': 'input_restored'})
                except Exception as error:
                    result.set_exception(error)
            server.after_events(apply)
            return result.result(timeout=10)
        except Exception as error:
            return {'status': 'unconfirmed', 'error': str(error)}

    def hold_unknown_start(self, server, expected, connection):
        """Stop showing a perpetual start while preserving the unresolved native input."""
        try:
            callbacks = getattr(server, 'callbacks', None)
            if callbacks is not None and not callbacks.empty():
                return {'status': 'unconfirmed'}
            process = getattr(server, 'proc', None)
            if getattr(server, 'supervisor_mode', False):
                if process is None or not hasattr(process, 'call'):
                    return {'status': 'unconfirmed'}
                journal = process.call('status')
                if (journal.get('sequence') != journal.get('acknowledged')
                        or journal.get('backpressure')):
                    return {'status': 'unconfirmed'}
            for _ in range(2):
                thread = server.call('thread/read', {'threadId': expected['threadId'],
                                                     'includeTurns': False}, timeout=5)['thread']
                status = thread.get('status') or {}
                if (thread.get('id') != expected['threadId']
                        or status.get('type') not in {'idle', 'notLoaded'} or status.get('activeFlags')):
                    return {'status': 'unconfirmed'}
            result = Future()
            def apply():
                try:
                    with self.lock, self.db() as db:
                        current = self.agent(expected['id'], db)
                        if (self.closed or not self.connection_current(current.get('accountKey', 'default'), connection)
                                or not unconfirmed_start(current)
                                or any(current.get(k) != value for k, value in expected.items())):
                            result.set_result({'status': 'superseded'})
                            return
                        at = time.time()
                        reason = ('Start outcome unknown. Studio checked native history at '
                                  + time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime(at))
                                  + '. The original request can still finish. Recover this worker before new input.')
                        current.update(status='interrupted', inFlight=False, error=reason,
                                       activity=None, activeTools=[])
                        current['startAttempt']['executionOutcome'] = 'unknown'
                        current['startOutcomeHold'] = {'stage': 'held', 'at': at,
                            'attemptId': current['startAttempt']['id'], 'threadId': current['threadId'],
                            'connectionId': connection, 'evidence': 'complete_history_absent_idle_twice_journal_drained'}
                        self.child_stopped_event(db, current, 'interrupted', reason,
                                                 'start-unknown:' + current['startAttempt']['id'])
                        self.put(db, 'agents', current)
                        self.changed.set()
                        result.set_result({'status': 'held', 'at': at})
                except Exception as error:
                    result.set_exception(error)
            server.after_events(apply)
            return result.result(timeout=10)
        except Exception as error:
            return {'status': 'unconfirmed', 'error': str(error)}

    def reconcile_orphan_busy(self, server, a, expected, connection):
        """Clear a busy flag only when native is idle and its last turn already completed here."""
        def idle():
            thread = server.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != a['threadId']:
                raise ValueError('Native thread identity changed')
            return thread.get('status', {}).get('type') in {'idle', 'notLoaded'}
        try:
            if not idle():
                return {'status': 'active_or_unknown'}
            page = server.call('thread/turns/list', {'threadId': a['threadId'], 'limit': 1,
                                                     'sortDirection': 'desc'}, timeout=5)
            latest = (page.get('data') or [None])[0]
            if not idle():
                return {'status': 'active_or_unknown'}
            if latest is not None and latest.get('status') not in {'completed', 'failed', 'interrupted'}:
                return {'status': 'unconfirmed'}
            result = Future()
            def apply():
                try:
                    result.set_result(self.apply_orphan_recovery(expected, connection, latest))
                except Exception as error:
                    result.set_exception(error)
            server.after_events(apply)
            return result.result(timeout=10)
        except Exception as error:
            return {'status': 'unconfirmed', 'error': str(error)}

    def apply_orphan_recovery(self, expected, connection, latest):
        account = expected.get('accountKey') or 'default'
        with self.lock, self.db() as db:
            a = self.agent(expected['id'], db)
            if (not self.connection_current(account, connection) or self.closed or not orphan_busy(a)
                    or any(a.get(k) != value for k, value in expected.items())):
                return {'status': 'superseded'}
            # An unprocessed terminal turn still owns its completion effects.
            if latest is not None and not db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                                     (a['id'] + ':' + str(latest.get('id')),)).fetchone():
                return {'status': 'unconfirmed'}
            if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                          "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                          (a['id'], a['epoch'])).fetchone():
                return {'status': 'unconfirmed'}
            a.update(inFlight=False, activity=None, activeTools=[],
                     status='queued' if a.get('autoWake') else 'paused')
            a.pop('steerRejectedTurnId', None)
            a['turnRecovery'] = {'at': time.time(), 'turnId': None, 'latestTurnId': (latest or {}).get('id'),
                                 'outcome': 'idle', 'source': 'native_thread_read'}
            self.put(db, 'agents', a)
            self.changed.set()
            return {'status': 'reconciled', 'turnId': None, 'outcome': 'idle'}

    def apply_turn_recovery(self, expected, connection, native_state, turn):
        account = expected.get('accountKey') or 'default'
        with self._token_rate_observation_batch():
            with self.lock:
                with self.db() as db:
                    a = self.agent(expected['id'], db)
                    if (not self.connection_current(account, connection) or self.closed
                            or not a.get('inFlight') or a.get('deletedAt')
                            or any(a.get(k) != value for k, value in expected.items())):
                        return {'status': 'superseded'}
                    if (native_state not in {'idle', 'notLoaded', 'active'} or turn.get('id') != a.get('turnId')
                            or turn.get('status') not in {'completed', 'failed', 'interrupted'}):
                        return {'status': 'unconfirmed'}
                    # Do not manufacture user delivery receipts or re-execute tool calls.
                    messages = []
                    for item in turn.get('items', []):
                        if item.get('type') != 'agentMessage':
                            continue
                        row = db.execute('SELECT record FROM runtime_items WHERE id=?', (a['id'] + ':' + item['id'],)).fetchone()
                        stored = json.loads(row[0]) if row else {}
                        if not stored or stored.get('streaming') or stored.get('text') != item.get('text', ''):
                            messages.append(item)
                    if len(messages) + 1 > self._token_rate_observation_limit():
                        return {'status': 'unconfirmed'}
                if native_state == 'notLoaded':
                    self.loaded.discard(a['id'])
                for item in messages:
                    self.notification({'method': 'item/completed', 'params': {
                        'threadId': a['threadId'], 'turnId': a['turnId'], 'item': item,
                    }}, account, connection)
                self.notification({'method': 'turn/completed', 'params': {
                    'threadId': a['threadId'], 'turn': {k: turn.get(k) for k in ('id', 'status', 'error')},
                }}, account, connection)
                with self.db() as db:
                    current = self.agent(a['id'], db)
                    current['turnRecovery'] = {'at': time.time(), 'turnId': turn['id'],
                                               'outcome': turn['status'], 'source': 'native_thread_read'}
                    self.put(db, 'agents', current)
                return {'status': 'reconciled', 'turnId': turn['id'], 'outcome': turn['status']}
