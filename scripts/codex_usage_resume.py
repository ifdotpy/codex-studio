"""Durable continuation after account usage, rate, or authentication failures."""
import hashlib
import json
import re
import time
from pathlib import Path


POLL_SECONDS = 180
AUTH_WAIT_NOTICE_SECONDS = 1800
AUTH_BACKOFF_SECONDS = (180, 600, 1800)
# Check once just after a known reset. Poll only without a reset time, or when
# the account is still blocked after it. Early relief (for example a reset
# credit) arrives through limit updates and is handled at once.
RESET_GRACE_SECONDS = 5


def _next_check(reset, now):
    return reset + RESET_GRACE_SECONDS if reset and reset > now else now + POLL_SECONDS


def _auth_backoff(attempt):
    index = max(0, int(attempt or 0))
    return AUTH_BACKOFF_SECONDS[min(index, len(AUTH_BACKOFF_SECONDS) - 1)]
CONTINUATION = (
    "The previous turn stopped because this account reached a usage or rate limit. "
    "Check the current task and conversation state, then continue the same task. "
    "Do not repeat completed work."
)
AUTH_CONTINUATION = (
    "The previous turn stopped because this account sign-in failed. "
    "Check the current task and conversation state, then continue the same task. "
    "Do not repeat completed work."
)


def _identity(agent, account_key, turn_id):
    return hashlib.sha256(json.dumps([
        agent['id'], account_key, agent.get('threadId'), agent.get('epoch'), turn_id,
    ]).encode()).hexdigest()


def _limit_error(error):
    return isinstance(error, dict) and error.get('codexErrorInfo') in {
        'usageLimitExceeded', 'rateLimitExceeded',
    }


def _auth_error(error, provider=None):
    if not isinstance(error, dict):
        return False
    info = error.get('codexErrorInfo')
    if isinstance(info, dict):
        info = next(iter(info), '')
    if isinstance(info, str) and 'auth' in info.lower():
        return True
    message = str(error.get('message') or '')
    auth_evidence = re.search(
        r'\b(?:invalid|expired|revoked).{0,32}\b(?:token|key)\b'
        r'|\b(?:token|key)\b.{0,32}\b(?:invalid|expired|revoked)\b'
        r'|\bsign[ -]?in\b|\bauthentication required\b|\bunauthorized\b',
        message, re.I)
    if re.search(r'\b401\b', message):
        return True
    if re.search(r'\b403\b', message) and auth_evidence:
        return True
    return provider == 'claude' and bool(re.search(
        r'oauth session expired|failed to authenticate', message, re.I))


def _reset_at(data):
    """When the exhausted windows reset. Windows with room left do not delay resume.

    The latest reset among exhausted windows is when every blocking window has
    room again. Without an exhausted window the snapshot names no reset time.
    """
    buckets = [data.get('rateLimits') or {}, *(data.get('rateLimitsByLimitId') or {}).values()]
    resets = []
    for bucket in buckets:
        if not isinstance(bucket, dict):
            continue
        windows = [bucket.get(key) for key in ('primary', 'secondary')]
        windows = [window for window in windows if isinstance(window, dict)]
        full = [window for window in windows
                if isinstance(window.get('usedPercent'), (int, float)) and window['usedPercent'] >= 100]
        # A reached bucket without a full window blocks until its windows reset.
        blocking = full or (windows if bucket.get('rateLimitReachedType') is not None else [])
        resets.extend(window['resetsAt'] for window in blocking
                      if isinstance(window.get('resetsAt'), (int, float)))
    return max(resets) if resets else None


def _allowed(data, now):
    if not isinstance(data, dict) or not data.get('rateLimits'):
        return False
    if 'ordinaryUsageAllowed' in data and data['ordinaryUsageAllowed'] is not True:
        return False
    buckets = [data['rateLimits'], *(data.get('rateLimitsByLimitId') or {}).values()]
    measured = False
    for bucket in buckets:
        if not isinstance(bucket, dict) or bucket.get('rateLimitReachedType') is not None or bucket.get('spendControlReached'):
            return False
        individual = bucket.get('individualLimit')
        if isinstance(individual, dict) and isinstance(individual.get('remainingPercent'), (int, float)) and individual['remainingPercent'] <= 0:
            return False
        for name in ('primary', 'secondary'):
            window = bucket.get(name)
            if not window:
                continue
            if not isinstance(window, dict):
                return False
            used = window.get('usedPercent')
            if not isinstance(used, (int, float)):
                return False
            measured = True
            if used >= 100:
                return False
            if isinstance(window.get('resetsAt'), (int, float)) and window['resetsAt'] <= now:
                return False
    return measured


class UsageResumeMixin:
    def accounts_snapshot(self):
        snapshot = self.accounts.snapshot()
        now = time.time()
        with self.lock, self.db() as db:
            rows = db.execute("SELECT record FROM runtime_usage_resumes "
                              "WHERE json_extract(record,'$.status')='scheduled' "
                              "AND json_extract(record,'$.cause')='auth' "
                              "AND json_extract(record,'$.failedAt')<=?",
                              (now - AUTH_WAIT_NOTICE_SECONDS,)).fetchall()
        waiting = {json.loads(row['record']).get('accountKey') for row in rows}
        for account in snapshot.get('accounts', []):
            if account.get('id') in waiting:
                account['authenticationRecovery'] = 'Sign in again to this account. Chats will continue when sign-in works.'
        return snapshot

    def usage_resume_save(self, db, agent, resume):
        resume['updatedAt'] = time.time()
        db.execute('INSERT OR REPLACE INTO runtime_usage_resumes VALUES (?,?,?)',
                   (resume['id'], agent['id'], json.dumps(resume)))
        agent['usageResume'] = resume

    def usage_resume_limits_changed(self, account_key, value):
        data = value.get('data') or {}
        if value.get('error') or not data:
            return
        now = time.time()
        reset = _reset_at(data)
        allowed = _allowed(data, now)
        with self.lock, self.db() as db:
            rows = db.execute("SELECT id,agent,record FROM runtime_usage_resumes "
                              "WHERE json_extract(record,'$.status')='scheduled' "
                              "AND json_extract(record,'$.accountKey')=?", (account_key,)).fetchall()
            for row in rows:
                resume = json.loads(row['record'])
                resume['resetAt'] = reset
                resume['plannedAt'] = reset if reset and reset > now else None
                if allowed:
                    resume['dueAt'] = now
                elif reset and reset > now:
                    resume['dueAt'] = _next_check(reset, now)
                resume['updatedAt'] = now
                db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                           (json.dumps(resume), resume['id']))
                agent = self.agent(row['agent'], db)
                if (agent.get('usageResume') or {}).get('id') == resume['id']:
                    agent['usageResume'] = resume
                    self.put(db, 'agents', agent)

    def usage_resume_record(self, agent, turn_id, error, *, auth_attempt=0):
        account_key = agent.get('accountKey', 'default')
        now = time.time()
        cause = ('usage_limit' if error.get('codexErrorInfo') == 'usageLimitExceeded' else
                 'rate_limit' if error.get('codexErrorInfo') == 'rateLimitExceeded' else 'auth')
        snapshot = self.rate_limits_for(account_key) if cause != 'auth' else {}
        error_reset = error.get('resetsAt') if isinstance(error.get('resetsAt'), (int, float)) else None
        reset = max(filter(None, (_reset_at(snapshot.get('data') or {}), error_reset)), default=None)
        planned_at = reset if reset and reset > now else None
        due = now + _auth_backoff(auth_attempt) if cause == 'auth' else _next_check(reset, now)
        auth_marker = self.usage_resume_auth_marker(account_key) if cause == 'auth' else None
        return dict(id=_identity(agent, account_key, turn_id), status='scheduled', accountKey=account_key,
                    threadId=agent['threadId'], epoch=agent['epoch'], turnId=turn_id,
                    cause=cause, failedAt=now, authAttempt=auth_attempt if cause == 'auth' else 0,
                    authRefreshMarker=auth_marker,
                    dueAt=due, plannedAt=planned_at, resetAt=reset, reason=None)

    def usage_resume_auth_marker(self, account_key):
        try:
            with self.accounts.lock:
                account = dict(self.accounts._row(account_key))
            if account.get('provider') == 'claude':
                return None
            path = Path(account['home']) / 'auth.json'
            mtime = path.stat().st_mtime_ns
            try:
                with path.open() as source:
                    refresh = json.load(source).get('last_refresh')
                if isinstance(refresh, (str, int, float)) and not isinstance(refresh, bool):
                    return 'refresh:' + hashlib.sha256(str(refresh).encode()).hexdigest()
            except (OSError, ValueError, TypeError, AttributeError):
                pass
            return 'mtime:' + str(mtime)
        except (OSError, KeyError, ValueError, AttributeError):
            return None

    def usage_resume_completed(self, db, agent, turn, known_turn):
        if turn.get('status') == 'completed':
            now = time.time()
            rows = db.execute("SELECT id,agent,record FROM runtime_usage_resumes "
                              "WHERE json_extract(record,'$.status')='scheduled' "
                              "AND json_extract(record,'$.cause')='auth' "
                              "AND json_extract(record,'$.accountKey')=?", (agent.get('accountKey', 'default'),)).fetchall()
            for row in rows:
                resume = json.loads(row['record'])
                if resume.get('failedAt', now) < now:
                    resume.update(proofAt=now, dueAt=now)
                    db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                               (json.dumps(resume), row['id']))
                    target = self.agent(row['agent'], db)
                    if (target.get('usageResume') or {}).get('id') == row['id']:
                        target['usageResume'] = resume
                        self.put(db, 'agents', target)
            current_resume = agent.get('usageResume') or {}
            if current_resume.get('cause') == 'auth' and current_resume.get('status') == 'started':
                agent['authResumeAttempt'] = 0
        error = turn.get('error') or {}
        if (not known_turn or turn.get('status') != 'failed' or not turn.get('id') or not agent.get('threadId')
                or agent.get('deletedAt') or not agent.get('autoWake') or not agent.get('usageResumeEnabled', True)
                or agent.get('turnEpoch', agent['epoch']) != agent['epoch']
                or not (_limit_error(error) or _auth_error(error, agent.get('provider')))):
            return
        account_key = agent.get('accountKey', 'default')
        resume_id = _identity(agent, account_key, turn.get('id'))
        if db.execute('SELECT 1 FROM runtime_usage_resumes WHERE id=?', (resume_id,)).fetchone():
            return
        try:
            from codex_native_errors import assert_native_thread_open
            assert_native_thread_open(agent)
        except ValueError:
            return
        auth_attempt = max(0, int(agent.get('authResumeAttempt', 0))) if _auth_error(error, agent.get('provider')) else 0
        resume = self.usage_resume_record(agent, turn['id'], error, auth_attempt=auth_attempt)
        self.usage_resume_save(db, agent, resume)
        if resume.get('cause') == 'auth':
            agent['authResumeAttempt'] = auth_attempt + 1

    def usage_resume_action(self, key, resume_id, enabled):
        if type(enabled) is not bool:
            raise ValueError('Automatic resume choice must be true or false')
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            row = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=? AND agent=?', (resume_id, key)).fetchone()
            if not row:
                raise ValueError('Unknown automatic resume')
            resume = json.loads(row[0])
            if (agent.get('usageResume') or {}).get('id') != resume_id:
                return agent.get('usageResume')
            agent['usageResumeEnabled'] = enabled
            if not enabled and resume['status'] == 'scheduled':
                resume.update(status='cancelled', dueAt=None, reason='Automatic resume is off for this chat.')
            elif enabled and resume['status'] == 'cancelled' and resume.get('reason') == 'Automatic resume is off for this chat.':
                turn_id = agent.get('lastCompletedTurn')
                error = agent.get('error') or {}
                if (agent.get('nativeFailureHold') and agent.get('lastCompletedTurnStatus') == 'failed'
                        and turn_id and (_limit_error(error) or _auth_error(error, agent.get('provider')))):
                    current_id = _identity(agent, agent.get('accountKey', 'default'), turn_id)
                    if current_id != resume_id:
                        existing = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=? AND agent=?',
                                               (current_id, key)).fetchone()
                        if existing:
                            resume = json.loads(existing['record'])
                        else:
                            attempt = max(0, int(agent.get('authResumeAttempt', 0)))
                            resume = self.usage_resume_record(agent, turn_id, error, auth_attempt=attempt)
                            if resume.get('cause') == 'auth':
                                agent['authResumeAttempt'] = attempt + 1
                    if current_id == resume['id'] and resume['status'] in {'scheduled', 'cancelled'}:
                        resume.update(status='scheduled', reason=None)
                        now = time.time()
                        reset = resume.get('resetAt')
                        resume['dueAt'] = now + POLL_SECONDS if resume.get('cause') == 'auth' else _next_check(reset, now)
            self.usage_resume_save(db, agent, resume)
            self.put(db, 'agents', agent)
            self.changed.set()
            return resume

    def usage_resume_cancel(self, db, agent, reason):
        resume = agent.get('usageResume') or {}
        if resume.get('status') in {'scheduled', 'started'}:
            resume.update(status='cancelled', dueAt=None, reason=reason)
            self.usage_resume_save(db, agent, resume)

    def usage_resume_tick(self):
        now = time.time()
        with self.lock, self.db() as db:
            due = db.execute("SELECT id,agent,record FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_extract(record,'$.dueAt')<=?", (now,)).fetchall()
        if not due:
            return
        by_account = {}
        for row in due:
            resume = json.loads(row['record'])
            by_account.setdefault(resume['accountKey'], []).append((row['agent'], resume))
        results = {}
        for account_key, candidates in by_account.items():
            try:
                redact = any(resume.get('cause') == 'auth' for _, resume in candidates)
                results[account_key] = self.limits(account_key, force=True, redact_errors=redact)
            except Exception:
                results[account_key] = None
        auth_proofs = {}
        for account_key, candidates in by_account.items():
            auth_candidates = [resume for _, resume in candidates if resume.get('cause') == 'auth']
            if not auth_candidates:
                continue
            limits = results.get(account_key) or {}
            current_marker = self.usage_resume_auth_marker(account_key)
            changed_mtime = {resume['id'] for resume in auth_candidates
                             if current_marker is not None and resume.get('authRefreshMarker') is not None
                             and current_marker != resume['authRefreshMarker']}
            for resume in auth_candidates:
                auth_proofs[resume['id']] = (
                    resume.get('proofAt', 0) > resume.get('failedAt', 0)
                    or (not limits.get('error') and not limits.get('stale')
                        and limits.get('accountKey', account_key) == account_key
                        and isinstance(limits.get('at'), (int, float))
                        and limits['at'] > resume.get('failedAt', 0))
                    or resume['id'] in changed_mtime)
        with self.lock, self.db() as db:
            for account_key, candidates in by_account.items():
                limits = results.get(account_key) or {}
                data = limits.get('data') or {}
                fresh = (not limits.get('error') and not limits.get('stale')
                         and limits.get('accountKey', account_key) == account_key
                         and isinstance(limits.get('at'), (int, float)) and limits['at'] >= now - 5)
                allowed = fresh and _allowed(data, now)
                reset_at = _reset_at(data)
                next_due = _next_check(reset_at, now)
                for key, resume in candidates:
                    agent = self.agent(key, db)
                    stored = agent.get('usageResume') or {}
                    if stored.get('id') != resume['id'] or stored.get('status') != 'scheduled':
                        continue
                    parent = agent
                    parent_running = True
                    seen = set()
                    while parent.get('parentId'):
                        if parent['id'] in seen:
                            parent_running = False
                            break
                        seen.add(parent['id'])
                        parent = self.agent(parent['parentId'], db)
                        if not parent.get('autoWake') or parent.get('deletedAt'):
                            parent_running = False
                            break
                    if (agent.get('deletedAt') or not agent.get('autoWake') or not agent.get('usageResumeEnabled', True)
                            or agent.get('accountKey', 'default') != account_key or not parent_running):
                        self.usage_resume_cancel(db, agent, 'The chat stopped or changed accounts before automatic resume.')
                        self.put(db, 'agents', agent)
                        continue
                    if (agent.get('threadId') != resume['threadId'] or agent.get('epoch') != resume['epoch']
                            or agent.get('lastCompletedTurn') != resume['turnId'] or not agent.get('nativeFailureHold')):
                        self.usage_resume_cancel(db, agent, 'The failed turn or chat state changed before automatic resume.')
                        self.put(db, 'agents', agent)
                        continue
                    auth_recovered = resume.get('cause') == 'auth' and auth_proofs.get(resume['id'], False)
                    if not (auth_recovered if resume.get('cause') == 'auth' else allowed):
                        resume.update(dueAt=now + _auth_backoff(resume.get('authAttempt', 0)) if resume.get('cause') == 'auth' else next_due,
                                      plannedAt=reset_at if reset_at and reset_at > now else None,
                                      resetAt=reset_at, lastCheckedAt=limits.get('at'),
                                      waitingForAuth=resume.get('cause') == 'auth',
                                      reason=('Waiting for the account sign-in.' if resume.get('cause') == 'auth'
                                              else None if fresh else 'Waiting for a current account limits result.'))
                        self.usage_resume_save(db, agent, resume)
                        self.put(db, 'agents', agent)
                        continue
                    event_id = 'usage-resume:' + resume['id']
                    self.enqueue(db, agent, 'followup',
                                 AUTH_CONTINUATION if resume.get('cause') == 'auth' else CONTINUATION,
                                 event_id)
                    resume.update(status='started', dueAt=None, startedAt=now, reason=None)
                    self.usage_resume_save(db, agent, resume)
                    agent.pop('nativeFailureHold', None)
                    agent.update(status='queued', error=None)
                    self.put(db, 'agents', agent)
