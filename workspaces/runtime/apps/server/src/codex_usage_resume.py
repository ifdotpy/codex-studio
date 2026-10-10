"""Durable continuation after account usage, rate, or authentication failures."""
import hashlib
import json
import re
import time
from pathlib import Path
from typing import TYPE_CHECKING, Literal, Protocol

from codex_records import RecordStore
from codex_native_errors import error_kind

if TYPE_CHECKING:
    import sqlite3
    from threading import Event
    from typing import Any, ContextManager

    from codex_records import (
        AccountDataRecord,
        AccountSnapshotRecord,
        AgentRecord,
        JsonObject,
        NativeTurnRecord,
        RateLimitBucketRecord,
        RateLimitDataRecord,
        RateLimitSnapshotRecord,
        RateLimitWindowRecord,
        UsageResumeRecord,
    )


POLL_SECONDS = 180
AUTH_WAIT_NOTICE_SECONDS = 1800
AUTH_BACKOFF_SECONDS = (180, 600, 1800)
# Check once just after a known reset. Poll only without a reset time, or when
# the account is still blocked after it. Early relief (for example a reset
# credit) arrives through limit updates and is handled at once.
RESET_GRACE_SECONDS = 5
# A short rate limit can fail a turn while every window still has room.
# Wait this long after the failure so relief cannot resume in a tight loop.
RESUME_MIN_SECONDS = 60


def _next_check(reset: int | float | None, now: float) -> float:
    return reset + RESET_GRACE_SECONDS if reset and reset > now else now + POLL_SECONDS


def _auth_backoff(attempt: int | None) -> int:
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


def _identity(agent: "AgentRecord", account_key: str, turn_id: str) -> str:
    return hashlib.sha256(json.dumps([
        agent['id'], account_key, agent.get('threadId'), agent.get('epoch'), turn_id,
    ]).encode()).hexdigest()


def _limit_error(error: object) -> bool:
    return error_kind(error) in {'usageLimitExceeded', 'rateLimitExceeded'}


def _auth_error(error: object, provider: str | None = None) -> bool:
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


def _reset_at(data: "RateLimitDataRecord") -> int | float | None:
    """When the exhausted windows reset. Windows with room left do not delay resume.

    The latest reset among exhausted windows is when every blocking window has
    room again. Without an exhausted window the snapshot names no reset time.
    """
    buckets: list[RateLimitBucketRecord] = [data.get('rateLimits') or {}, *(data.get('rateLimitsByLimitId') or {}).values()]
    resets: list[int | float] = []
    for bucket in buckets:
        if not isinstance(bucket, dict):
            continue
        windows: list[RateLimitWindowRecord | None] = [bucket.get(key) for key in ('primary', 'secondary')]  # type: ignore[misc]  # typed-narrowing: keys are fixed primary names
        windows = [window for window in windows if isinstance(window, dict)]
        full = [window for window in windows
                if isinstance(window.get('usedPercent'), (int, float)) and window['usedPercent'] >= 100]  # type: ignore[index,union-attr]  # typed-narrowing: isinstance check narrows optional window
        # A reached bucket without a full window blocks until its windows reset.
        blocking = full or (windows if bucket.get('rateLimitReachedType') is not None else [])
        resets.extend(window['resetsAt'] for window in blocking  # type: ignore[index]  # typed-narrowing: filter guarantees reset key presence
                      if isinstance(window.get('resetsAt'), (int, float)))  # type: ignore[union-attr]  # typed-narrowing: dictionary filter guards reset lookup
    return max(resets) if resets else None


def _allowed(data: "RateLimitDataRecord", now: float) -> bool:
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


class UsageAccounts(Protocol):
    lock: "ContextManager[object]"

    def snapshot(self, *, refresh: bool = True) -> "AccountSnapshotRecord": ...
    def _row(self, key: object) -> "AccountDataRecord": ...


class UsageResumeRuntime(RecordStore, Protocol):
    def read_db(self) -> "ContextManager[sqlite3.Connection]": ...
    lock: "ContextManager[object]"
    changed: "Event"
    accounts: UsageAccounts

    def db(self) -> "ContextManager[sqlite3.Connection]": ...
    def agent(self, key: str, db: "sqlite3.Connection | None" = None) -> "AgentRecord": ...
    def rate_limits_for(self, account_key: str) -> "RateLimitSnapshotRecord": ...
    def limits(self, account_key: str, *, force: bool = False, redact_errors: bool = False) -> "RateLimitSnapshotRecord": ...
    def continuation_work_claims(self, db: "sqlite3.Connection", agent: "AgentRecord") -> list[str]: ...
    def continuation_work_claims_valid(self, db: "sqlite3.Connection", agent: "AgentRecord",
                                       claims: list[str]) -> bool: ...
    def permanent_worker_hold(self, db: "sqlite3.Connection", agent: "AgentRecord", operation_id: str,
                              transition: str, reason: str) -> None: ...
    def enqueue(self, db: "sqlite3.Connection", agent: "AgentRecord", kind: str,
                text: str, event_id: str) -> None: ...
    def usage_resume_save(self, db: "sqlite3.Connection", agent: "AgentRecord",
                          resume: "UsageResumeRecord") -> None: ...
    def usage_resume_record(self, agent: "AgentRecord", turn_id: str, error: "JsonObject",
                            *, auth_attempt: int = 0) -> "UsageResumeRecord": ...
    def usage_resume_auth_marker(self, account_key: str) -> str | None: ...
    def usage_resume_cancel(self, db: "sqlite3.Connection", agent: "AgentRecord", reason: str) -> None: ...


class UsageResumeMixin:
    def accounts_snapshot(self: "UsageResumeRuntime") -> "AccountSnapshotRecord":
        snapshot = self.accounts.snapshot(refresh=False)
        now = time.time()
        with self.read_db() as db:
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

    def usage_resume_save(self: "UsageResumeRuntime", db: "sqlite3.Connection", agent: "AgentRecord",
                          resume: "UsageResumeRecord") -> None:
        resume['updatedAt'] = time.time()
        db.execute('INSERT OR REPLACE INTO runtime_usage_resumes VALUES (?,?,?)',
                   (resume['id'], agent['id'], json.dumps(resume)))
        agent['usageResume'] = resume

    def usage_resume_limits_changed(self: "UsageResumeRuntime", account_key: str,
                                    value: "RateLimitSnapshotRecord") -> None:
        data = value.get('data') or {}
        if value.get('error') or not data:
            return
        # The usual account has no scheduled resume. A read-only precheck keeps
        # routine limit telemetry from waiting for the shared runtime lock.
        # A newly scheduled resume reads the current cache in usage_resume_record;
        # the tick also refreshes its decision from the current limits.
        with self.db() as probe:
            if not probe.execute("SELECT 1 FROM runtime_usage_resumes "
                                 "WHERE json_extract(record,'$.status')='scheduled' "
                                 "AND json_extract(record,'$.accountKey')=? LIMIT 1",
                                 (account_key,)).fetchone():
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
                # Records from before typed causes were always usage or rate limits.
                if resume.get('cause', 'usage_limit') not in {'usage_limit', 'rate_limit'}:
                    continue
                resume['resetAt'] = reset
                resume['plannedAt'] = reset if reset and reset > now else None
                if allowed:
                    resume['dueAt'] = max(now, resume.get('failedAt', now) + RESUME_MIN_SECONDS)
                elif reset and reset > now:
                    resume['dueAt'] = _next_check(reset, now)
                resume['updatedAt'] = now
                db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                           (json.dumps(resume), resume['id']))
                agent = self.agent(row['agent'], db)
                if (agent.get('usageResume') or {}).get('id') == resume['id']:
                    agent['usageResume'] = resume
                    self.put(db, 'agents', agent)

    def usage_resume_record(self: "UsageResumeRuntime", agent: "AgentRecord", turn_id: str,
                            error: "JsonObject", *, auth_attempt: int = 0) -> "UsageResumeRecord":
        account_key = agent.get('accountKey', 'default')
        now = time.time()
        cause: Literal['usage_limit', 'rate_limit', 'auth'] = (
            'usage_limit' if error.get('codexErrorInfo') == 'usageLimitExceeded' else
            'rate_limit' if error.get('codexErrorInfo') == 'rateLimitExceeded' else 'auth')
        snapshot: RateLimitSnapshotRecord = self.rate_limits_for(account_key) if cause != 'auth' else {}
        error_reset = error.get('resetsAt') if isinstance(error.get('resetsAt'), (int, float)) else None
        reset: int | float | None = max(filter(None, (_reset_at(snapshot.get('data') or {}), error_reset)), default=None)  # type: ignore[assignment,type-var]  # typed-narrowing: filter keeps only numeric reset timestamps
        planned_at = reset if reset and reset > now else None
        due = now + _auth_backoff(auth_attempt) if cause == 'auth' else _next_check(reset, now)
        auth_marker = self.usage_resume_auth_marker(account_key) if cause == 'auth' else None
        return dict(id=_identity(agent, account_key, turn_id), status='scheduled', accountKey=account_key,
                    threadId=agent['threadId'], epoch=agent['epoch'], turnId=turn_id,  # type: ignore[typeddict-item]  # typed-suspect: caller may lack a thread id
                    cause=cause, failedAt=now, authAttempt=auth_attempt if cause == 'auth' else 0,
                    authRefreshMarker=auth_marker,
                    dueAt=due, plannedAt=planned_at, resetAt=reset, reason=None)

    def usage_resume_auth_marker(self: "UsageResumeRuntime", account_key: str) -> str | None:
        try:
            with self.accounts.lock:
                account = dict(self.accounts._row(account_key))
            if account.get('provider') == 'claude':
                return None
            path = Path(account['home']) / 'auth.json'  # type: ignore[arg-type]  # typed-narrowing: account snapshot home is string
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

    def usage_resume_completed(self: "UsageResumeRuntime", db: "sqlite3.Connection", agent: "AgentRecord",
                               turn: "NativeTurnRecord", known_turn: bool) -> None:
        if turn.get('status') == 'completed':
            now = time.time()
            rows = db.execute("SELECT id,agent,record FROM runtime_usage_resumes "
                              "WHERE json_extract(record,'$.status')='scheduled' "
                              "AND json_extract(record,'$.cause')='auth' "
                              "AND json_extract(record,'$.accountKey')=?", (agent.get('accountKey', 'default'),)).fetchall()
            for row in rows:
                resume: "UsageResumeRecord" = json.loads(row['record'])
                if resume.get('failedAt', now) < now:
                    resume.update(proofAt=now, dueAt=now)  # type: ignore[call-arg]  # typed-update
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
        resume_id = _identity(agent, account_key, turn.get('id'))  # type: ignore[arg-type]  # typed-narrowing: truthy turn-id guard proves string
        if db.execute('SELECT 1 FROM runtime_usage_resumes WHERE id=?', (resume_id,)).fetchone():
            return
        try:
            from codex_native_errors import assert_native_thread_open
            assert_native_thread_open(agent)
        except ValueError:
            return
        auth_attempt = max(0, int(agent.get('authResumeAttempt', 0))) if _auth_error(error, agent.get('provider')) else 0
        resume = self.usage_resume_record(agent, turn['id'], error, auth_attempt=auth_attempt)
        resume['taskClaims'] = self.continuation_work_claims(db, agent)
        self.usage_resume_save(db, agent, resume)
        if resume.get('cause') == 'auth':
            agent['authResumeAttempt'] = auth_attempt + 1

    def usage_resume_action(self: "UsageResumeRuntime", key: str, resume_id: str,
                            enabled: bool) -> "UsageResumeRecord | None":
        if type(enabled) is not bool:
            raise ValueError('Automatic resume choice must be true or false')
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            row = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=? AND agent=?', (resume_id, key)).fetchone()
            if not row:
                raise ValueError('Unknown automatic resume')
            resume: "UsageResumeRecord" = json.loads(row[0])
            if (agent.get('usageResume') or {}).get('id') != resume_id:
                return agent.get('usageResume')
            agent['usageResumeEnabled'] = enabled
            if not enabled and resume['status'] == 'scheduled':
                resume.update(status='cancelled', dueAt=None, reason='Automatic resume is off for this chat.')  # type: ignore[call-arg]  # typed-update
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
                            resume = self.usage_resume_record(agent, turn_id, error, auth_attempt=attempt)  # type: ignore[arg-type]  # typed-suspect: stored error may not be an object
                            if resume.get('cause') == 'auth':
                                agent['authResumeAttempt'] = attempt + 1
                    if current_id == resume['id'] and resume['status'] in {'scheduled', 'cancelled'}:
                        resume.update(status='scheduled', reason=None)  # type: ignore[call-arg]  # typed-update
                        now = time.time()
                        reset = resume.get('resetAt')
                        resume['dueAt'] = now + POLL_SECONDS if resume.get('cause') == 'auth' else _next_check(reset, now)
            self.usage_resume_save(db, agent, resume)
            self.put(db, 'agents', agent)
            self.changed.set()
            return resume

    def usage_resume_cancel(self: "UsageResumeRuntime", db: "sqlite3.Connection", agent: "AgentRecord",
                            reason: str) -> None:
        resume = agent.get('usageResume') or {}
        if resume.get('status') in {'scheduled', 'started'}:
            resume.update(status='cancelled', dueAt=None, reason=reason)  # type: ignore[call-arg]  # typed-update
            self.usage_resume_save(db, agent, resume)

    def usage_resume_tick(self: "UsageResumeRuntime") -> None:
        now = time.time()
        with self.lock, self.db() as db:
            due = db.execute("SELECT id,agent,record FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_extract(record,'$.dueAt')<=?", (now,)).fetchall()
        if not due:
            return
        by_account: dict[str, list[tuple[str, UsageResumeRecord]]] = {}
        for row in due:
            resume: "UsageResumeRecord" = json.loads(row['record'])
            by_account.setdefault(resume['accountKey'], []).append((row['agent'], resume))
        results: dict[str, RateLimitSnapshotRecord | None] = {}
        for account_key, candidates in by_account.items():
            try:
                redact = any(resume.get('cause') == 'auth' for _, resume in candidates)
                results[account_key] = self.limits(account_key, force=True, redact_errors=redact)
            except Exception:
                results[account_key] = None
        auth_proofs: dict[str, bool] = {}
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
                        and limits['at'] > resume.get('failedAt', 0))  # type: ignore[operator]  # typed-narrowing: numeric check narrows both timestamps
                    or resume['id'] in changed_mtime)
        with self.lock, self.db() as db:
            for account_key, candidates in by_account.items():
                limits = results.get(account_key) or {}
                data = limits.get('data') or {}
                fresh = (not limits.get('error') and not limits.get('stale')
                         and limits.get('accountKey', account_key) == account_key
                         and isinstance(limits.get('at'), (int, float)) and limits['at'] >= now - 5)  # type: ignore[operator]  # typed-narrowing: numeric check narrows current timestamp
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
                        parent = self.agent(parent['parentId'], db)  # type: ignore[arg-type]  # typed-narrowing: parent id branch proves string value
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
                    if not self.continuation_work_claims_valid(db, agent, resume.get('taskClaims', [])):
                        reason = 'The assigned task changed before automatic continuation.'
                        self.usage_resume_cancel(db, agent, reason)
                        self.put(db, 'agents', agent)
                        self.permanent_worker_hold(db, agent, resume['id'], 'task-changed', reason)
                        continue
                    auth_recovered = resume.get('cause') == 'auth' and auth_proofs.get(resume['id'], False)
                    if not (auth_recovered if resume.get('cause') == 'auth' else allowed):
                        resume.update(dueAt=now + _auth_backoff(resume.get('authAttempt', 0)) if resume.get('cause') == 'auth' else next_due,  # type: ignore[call-arg]  # typed-update
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
                    resume.update(status='started', dueAt=None, startedAt=now, reason=None)  # type: ignore[call-arg]  # typed-update
                    self.usage_resume_save(db, agent, resume)
                    agent.pop('nativeFailureHold', None)
                    agent.update(status='queued', error=None)  # type: ignore[call-arg]  # typed-update
                    self.put(db, 'agents', agent)
