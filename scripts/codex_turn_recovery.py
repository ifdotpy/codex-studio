"""Reconcile lost terminal notifications against the owning native connection.

Silence only schedules a read. It never proves completion or authorizes replay.
"""
from concurrent.futures import Future
from datetime import datetime
import json
import time


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
                if not a.get('inFlight') or not a.get('turnId') or not a.get('threadId') or a.get('deletedAt'):
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
            if (not a.get('inFlight') or not a.get('turnId') or a.get('deletedAt')
                    or not self.connection_current(account, connection)):
                return {'status': 'skipped'}
            server = self.servers.get(account)
            if server is None:
                return {'status': 'skipped'}
            expected = {k: a.get(k) for k in ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'startAttempt')}
        try:
            # Most long-running turns only need this small status response.
            thread = server.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=5)['thread']
            if thread.get('id') != a['threadId']:
                raise ValueError('Native thread identity changed')
            if thread.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
                return {'status': 'active_or_unknown'}
            # The exact terminal turn is required. Never infer an outcome from silence.
            page = server.call('thread/turns/list', {'threadId': a['threadId'], 'limit': 10,
                'sortDirection': 'desc', 'itemsView': 'full'}, timeout=10)
            turn = next((t for t in page.get('data', []) if t.get('id') == a['turnId']), None)
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

    def apply_turn_recovery(self, expected, connection, native_state, turn):
        account = expected.get('accountKey') or 'default'
        with self.lock:
            with self.db() as db:
                a = self.agent(expected['id'], db)
                if (not self.connection_current(account, connection) or self.closed
                        or not a.get('inFlight') or a.get('deletedAt')
                        or any(a.get(k) != value for k, value in expected.items())):
                    return {'status': 'superseded'}
                if (native_state not in {'idle', 'notLoaded'} or turn.get('id') != a.get('turnId')
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
