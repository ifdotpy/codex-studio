"""Durable, single-use continuation after an exact terminal capacity failure."""
import hashlib
import json
import os
import time

from codex_native_errors import assert_native_thread_open, error_kind

DELAYS = (10, 30, 120, 300)
# A network outage (DNS, Wi-Fi, sleep) can last longer than a busy server.
CONNECTION_DELAYS = (10, 30, 120, 300, 600, 1200, 1800)
SERVER_KINDS = {'serverOverloaded', 'internalServerError'}
# Recheck a retry that waits for local work (a command or monitor) at most this often.
WAIT_MAX_SECONDS = 300
CONNECTION_KINDS = {'httpConnectionFailed', 'responseStreamConnectionFailed',
                    'responseStreamDisconnected', 'responseTooManyFailedAttempts'}



class CapacityRetryMixin:
    def capacity_save(self, db, a, retry):
        retry['updatedAt'] = time.time()
        db.execute('INSERT OR REPLACE INTO runtime_capacity_retries VALUES (?, ?, ?)',
                   (retry['id'], a['id'], json.dumps(retry)))
        if (a.get('capacityRetry') or {}).get('id') in (None, retry['id']):
            a['capacityRetry'] = retry

    def capacity_reset(self, db, a, reason='A new instruction replaces this retry.'):
        retry = a.get('capacityRetry')
        if retry:
            attempt = a.get('startAttempt') or {}
            if (attempt.get('capacityRetryId') == retry['id'] and not attempt.get('submitted')):
                # The same lock protects native submission and replacement by new input.
                a.pop('startAttempt', None)
                a.update(inFlight=False, status='failed')
            retry.update(status='finished', dueAt=None, reason=reason)
            self.capacity_save(db, a, retry)
        a.pop('capacityRetry', None)
        a.pop('capacityRetryCount', None)

    def capacity_restart(self, db, a):
        retry = a.get('capacityRetry')
        if not retry:
            return
        terminal = False
        if retry['status'] in {'starting', 'unknown'}:
            attempt = a.get('startAttempt') or {}
            if (attempt.get('capacityRetryId') == retry['id'] and not attempt.get('submitted')
                    and not retry.get('acceptedTurnId')):
                retry.update(status='cancelled', dueAt=None,
                             reason='The server restarted before turn submission. Automatic retry was cancelled.')
                terminal = True
                retry.pop('claimedAt', None)
                # Startup interrupts active records before this recovery hook.
                a.update(status='failed', autoWake=True, inFlight=False)
            else:
                retry.update(status='unknown', dueAt=None,
                             reason='The server restarted. The turn outcome is unknown.')
                terminal = True
        elif retry['status'] == 'scheduled':
            # Keep the exact task claim bound to its scheduled continuation.
            # capacity_retry validates these claims again before submission.
            if not retry.get('taskClaims') or not self.continuation_work_claims_valid(
                    db, a, retry.get('taskClaims', [])):
                retry.update(status='cancelled', dueAt=None,
                             reason='The server restarted. Automatic retry was cancelled.')
                terminal = True
        self.capacity_save(db, a, retry)
        if terminal:
            reason = retry.get('reason') or 'Automatic capacity retry was cancelled.'
            self.permanent_worker_hold(db, a, retry['id'], 'restart-terminal', reason)

    def capacity_started(self, db, a, attempt, turn_id):
        retry = a.get('capacityRetry') or {}
        if attempt.get('action') != 'capacity' or retry.get('id') != attempt.get('capacityRetryId'):
            return
        if not retry.get('acceptedTurnId'):
            retry['acceptedTurnId'] = turn_id
            a['capacityRetryCount'] = a.get('capacityRetryCount', 0) + 1
            retry.update(status='starting', reason=None, dueAt=None)
            self.capacity_save(db, a, retry)

    def capacity_completed(self, db, a, turn, known_turn):
        retry = a.get('capacityRetry')
        attempt = a.get('startAttempt') or {}
        if known_turn and attempt.get('action') == 'capacity':
            self.capacity_started(db, a, attempt, turn['id'])
        if retry and retry.get('acceptedTurnId') == turn.get('id'):
            retry.update(status='finished', dueAt=None)
            self.capacity_save(db, a, retry)
            if turn.get('status') == 'completed':
                a.pop('nativeFailureHold', None)
        error = turn.get('error') or {}
        kind = error_kind(error)
        if (not known_turn or not turn.get('id') or turn.get('status') != 'failed'
                or kind not in SERVER_KINDS | CONNECTION_KINDS
                or not a.get('autoWake') or a.get('deletedAt')
                or a.get('turnEpoch', a['epoch']) != a['epoch']):
            return
        try:
            assert_native_thread_open(a)
        except ValueError:
            return
        identity = [a['id'], a.get('accountKey', 'default'), a['threadId'], a['epoch'], turn['id']]
        retry_id = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
        if db.execute('SELECT 1 FROM runtime_capacity_retries WHERE id=?', (retry_id,)).fetchone():
            return
        count = a.get('capacityRetryCount', 0)
        delays = CONNECTION_DELAYS if kind in CONNECTION_KINDS else DELAYS
        retry = dict(id=retry_id, threadId=a['threadId'], turnId=turn['id'],
                     accountKey=a.get('accountKey', 'default'), epoch=a['epoch'], cause=kind,
                     status='scheduled' if count < len(delays) else 'exhausted',
                     dueAt=time.time() + delays[count] if count < len(delays) else None,
                     attempt=count + 1, maxAttempts=len(delays),
                     cwd=a['cwd'], settings=self.preparation_settings(a))
        retry['taskClaims'] = self.continuation_work_claims(db, a)
        a['capacityRetry'] = retry
        self.capacity_save(db, a, retry)

    def capacity_error(self, db, a, attempt, error, unknown):
        retry = a.get('capacityRetry') or {}
        if retry.get('id') != attempt.get('capacityRetryId') or not retry:
            return
        if not unknown and isinstance(getattr(error, 'contextRepairWait', None), dict):
            # A running command, monitor or receipt delays context repair. This
            # is a wait, not a rejection: nothing was submitted. Check again later.
            self.capacity_wait(db, a, retry, str(error))
            return
        retry.update(status='unknown' if unknown else 'failed', dueAt=None, reason=str(error))
        self.capacity_save(db, a, retry)

    def capacity_wait(self, db, a, retry, reason):
        waits = retry.get('waits', 0) + 1
        retry.pop('claimedAt', None)
        retry.update(status='scheduled', dueAt=time.time() + min(WAIT_MAX_SECONDS, 15 * waits),
                     waits=waits, reason=reason)
        self.capacity_save(db, a, retry)

    def capacity_check(self, db, a, retry, *, claimed=False):
        if (self.closed or a.get('deletedAt') or not a.get('autoWake')
                or retry['epoch'] != a['epoch'] or retry['threadId'] != a.get('threadId')
                or retry['accountKey'] != a.get('accountKey', 'default')
                or retry['cwd'] != a['cwd'] or retry['settings'] != self.preparation_settings(a)
                or retry['turnId'] != a.get('lastCompletedTurn')):
            raise ValueError('This retry belongs to an earlier agent state.')
        assert_native_thread_open(a)
        self.assert_workspace_available(db, a)
        if not claimed and (a.get('inFlight') or a.get('turnId')
                            or a['status'] in {'queued', 'starting', 'running', 'approval'}):
            raise ValueError('Wait for the current turn before retry.')
        if any(r.get('agent') == a['id'] and r['status'] in {'pending', 'answering', 'uncertain'}
               for r in self.records(db, 'requests')):
            raise ValueError('Resolve the pending request before retry.')
        agents = self.team_agents(db, a['rootId'], include_deleted=True)
        root = self.agent(a['rootId'], db)
        if a['id'] != root['id']:
            from codex_agent_modes import assert_delegation
            assert_delegation(root)
        if root.get('tokenBudget') and sum(t['tokensUsed'] for t in agents
                if t['rootId'] == root['id']) >= root['tokenBudget']:
            raise ValueError('Team token budget reached. Increase the budget before retry.')
        active = [slot for slot in self.dispatch_active_slots(db) if slot['id'] != a['id']]
        from codex_agent_modes import global_concurrency_limit
        limit = global_concurrency_limit()
        team_active = sum(slot['rootId'] == a['rootId'] and slot['id'] != slot['rootId'] for slot in active)
        if len(active) >= limit or (a['id'] != root['id'] and team_active >= root['concurrency']):
            raise ValueError('Wait for an available agent slot before retry.')

    def capacity_retry(self, key, retry_id, action, *, _automatic=False):
        if action not in {'retry', 'cancel'} or not isinstance(retry_id, str) or not retry_id:
            raise ValueError('Supply the retry identity and choose retry or cancel.')
        attempt = None
        with self.lock, self.db() as db:
            a = self.agent(key, db)
            row = db.execute('SELECT record FROM runtime_capacity_retries WHERE id=? AND agent=?',
                             (retry_id, key)).fetchone()
            if not row:
                raise ValueError('Unknown capacity retry.')
            retry = json.loads(row[0])
            if (a.get('capacityRetry') or {}).get('id') != retry_id:
                return retry
            if action == 'cancel':
                if retry['status'] == 'scheduled' and not retry.get('claimedAt'):
                    retry.update(status='cancelled', dueAt=None, reason=None)
                    self.capacity_save(db, a, retry)
                    self.put(db, 'agents', a)
                    self.permanent_worker_hold(db, a, retry_id, 'cancelled',
                                               'The automatic capacity retry was cancelled.')
                return retry
            if _automatic and retry['status'] != 'scheduled':
                return retry
            if retry.get('claimedAt') or retry['status'] not in {'scheduled', 'cancelled', 'exhausted'}:
                return retry
            if not self.continuation_work_claims_valid(db, a, retry.get('taskClaims', [])):
                retry.update(status='cancelled', dueAt=None,
                             reason='The assigned task changed before automatic continuation.')
                self.capacity_save(db, a, retry)
                self.put(db, 'agents', a)
                self.permanent_worker_hold(db, a, retry_id, 'task-changed', retry['reason'])
                return retry
            self.capacity_check(db, a, retry)
            if a.get('pendingSettings'):
                if a.get('pendingSettingsAccountKey', a.get('accountKey', 'default')) != a.get('accountKey', 'default'):
                    raise ValueError('The account changed. Save the next-turn settings again')
                # Verify the failed turn first, then bind the owner's queued choice
                # to this exact continuation in the same transaction as its claim.
                a.pop('pendingSettingsAccountKey', None)
                a.update(a.pop('pendingSettings'))
                retry['settings'] = self.preparation_settings(a)
                self.loaded.discard(a['id'])
            retry.update(status='starting', dueAt=None, claimedAt=time.time(), reason=None)
            attempt = dict(id='capacity:' + retry_id, epoch=a['epoch'], events=[], action='capacity',
                           submitted=False, capacityRetryId=retry_id, accountKey=retry['accountKey'])
            a.update(status='starting', inFlight=True, turnEpoch=a['epoch'], startAttempt=attempt)
            self.capacity_save(db, a, retry)
            self.put(db, 'agents', a)
        self.pool.submit(self.capacity_run, key, dict(attempt))
        self.changed.set()
        return retry

    def capacity_run(self, key, attempt):
        try:
            self.run_native_action(key, attempt)
        except Exception:
            # run_native_action persists an exact failure or uncertain outcome.
            pass

    def capacity_tick(self):
        with self.lock, self.db() as db:
            # Read scheduled retries only when due, plus failed context waits that need retirement.
            now = time.time()
            retrying = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.capacityRetry.status')='scheduled' "
                "AND json_extract(record,'$.capacityRetry.dueAt')<=? UNION ALL "
                "SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.capacityRetry.status')='failed' "
                "AND json_extract(record,'$.capacityRetry.acceptedTurnId') IS NULL "
                "AND json_extract(record,'$.capacityRetry.reason') LIKE 'Context repair waits for %'", (now,))]
            for a in retrying:
                retry = a.get('capacityRetry') or {}
                if (retry.get('status') == 'failed' and not retry.get('acceptedTurnId')
                        and str(retry.get('reason', '')).startswith('Context repair waits for ')
                        and not ((a.get('startAttempt') or {}).get('capacityRetryId') == retry.get('id')
                                 and (a.get('startAttempt') or {}).get('submitted'))):
                    self.capacity_wait(db, a, retry, retry['reason'])
                    self.put(db, 'agents', a)
            due = [(a['id'], a['capacityRetry']['id']) for a in retrying
                   if (a.get('capacityRetry') or {}).get('status') == 'scheduled'
                   and a['capacityRetry']['dueAt'] <= now]
        for key, retry_id in due:
            try:
                self.capacity_retry(key, retry_id, 'retry', _automatic=True)
            except ValueError as error:
                # Local guards can clear without changing the retry identity.
                with self.lock, self.db() as db:
                    a = self.agent(key, db)
                    retry = a.get('capacityRetry') or {}
                    if retry.get('id') == retry_id and retry.get('status') == 'scheduled':
                        if str(error) == 'This retry belongs to an earlier agent state.':
                            retry.update(status='cancelled', dueAt=None, reason=str(error))
                            self.capacity_save(db, a, retry)
                            self.permanent_worker_hold(db, a, retry_id, 'identity-changed', str(error))
                        else:
                            self.capacity_wait(db, a, retry, str(error))
                        self.put(db, 'agents', a)
