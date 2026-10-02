"""Reconcile lost terminal notifications against the owning native connection.

Silence only schedules a read. It never proves completion or authorizes replay.
"""
from concurrent.futures import Future
from datetime import datetime
import json
import time


def read_native_turn(server, thread_id, turn_id):
    """Find the exact turn across history pages within one read deadline."""
    deadline = time.monotonic() + 20
    cursor, seen = None, set()
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Native turn history read timed out')
        params = {'threadId': thread_id, 'limit': 10,
                  'sortDirection': 'desc', 'itemsView': 'full'}
        if cursor is not None:
            params['cursor'] = cursor
        page = server.call('thread/turns/list', params, timeout=min(10, remaining))
        turn = next((t for t in page.get('data', []) if t.get('id') == turn_id), None)
        if turn is not None:
            return turn
        cursor = page.get('nextCursor')
        if not cursor:
            return None
        if cursor in seen:
            raise ValueError('Native turn history repeated its page cursor')
        seen.add(cursor)


def orphan_busy(agent):
    """A busy flag with no native turn and no Studio start cannot be cleared by any notification."""
    return bool(agent.get('inFlight') and not agent.get('turnId') and not agent.get('startAttempt')
                and agent.get('threadId') and not agent.get('deletedAt'))


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
                if (not a.get('inFlight') or not (a.get('turnId') or orphan_busy(a))
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
                if a['id'] != force_id and (now - activity < 120 or now - checked.get(a['id'], 0) < 60):
                    continue
                candidates.append(a)
            if not candidates:
                return
            selected = min(candidates, key=lambda a: checked.get(a['id'], 0))
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
            if (not a.get('inFlight') or not (a.get('turnId') or orphan_busy(a)) or a.get('deletedAt')
                    or not self.connection_current(account, connection)):
                return {'status': 'skipped'}
            server = self.servers.get(account)
            if server is None:
                return {'status': 'skipped'}
            expected = {k: a.get(k) for k in ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
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
