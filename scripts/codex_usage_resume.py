"""Durable continuation after an exact account usage or rate limit failure."""
import hashlib
import json
import time


POLL_SECONDS = 180
CONTINUATION = (
    "The previous turn stopped because this account reached a usage or rate limit. "
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


def _reset_at(data):
    limits = data.get('rateLimits') or {}
    windows = [limits.get('primary'), limits.get('secondary')]
    windows.extend(bucket.get(key) for bucket in (data.get('rateLimitsByLimitId') or {}).values()
                   for key in ('primary', 'secondary'))
    resets = [window['resetsAt'] for window in windows
              if isinstance(window, dict) and isinstance(window.get('resetsAt'), (int, float))]
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
                    resume['dueAt'] = min(now + POLL_SECONDS, reset)
                resume['updatedAt'] = now
                db.execute('UPDATE runtime_usage_resumes SET record=? WHERE id=?',
                           (json.dumps(resume), resume['id']))
                agent = self.agent(row['agent'], db)
                if (agent.get('usageResume') or {}).get('id') == resume['id']:
                    agent['usageResume'] = resume
                    self.put(db, 'agents', agent)

    def usage_resume_record(self, agent, turn_id, error):
        account_key = agent.get('accountKey', 'default')
        now = time.time()
        due = now + POLL_SECONDS
        snapshot = self.rate_limits_for(account_key)
        error_reset = error.get('resetsAt') if isinstance(error.get('resetsAt'), (int, float)) else None
        reset = max(filter(None, (_reset_at(snapshot.get('data') or {}), error_reset)), default=None)
        planned_at = reset if reset and reset > now else None
        if reset and reset > now:
            due = min(due, reset)
        return dict(id=_identity(agent, account_key, turn_id), status='scheduled', accountKey=account_key,
                    threadId=agent['threadId'], epoch=agent['epoch'], turnId=turn_id,
                    dueAt=due, plannedAt=planned_at, resetAt=reset, reason=None)

    def usage_resume_completed(self, db, agent, turn, known_turn):
        error = turn.get('error') or {}
        if (not known_turn or turn.get('status') != 'failed' or not turn.get('id') or not agent.get('threadId')
                or agent.get('deletedAt') or not agent.get('autoWake') or not agent.get('usageResumeEnabled', True)
                or agent.get('turnEpoch', agent['epoch']) != agent['epoch'] or not _limit_error(error)):
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
        resume = self.usage_resume_record(agent, turn['id'], error)
        self.usage_resume_save(db, agent, resume)

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
                        and turn_id and _limit_error(error)):
                    current_id = _identity(agent, agent.get('accountKey', 'default'), turn_id)
                    if current_id != resume_id:
                        existing = db.execute('SELECT record FROM runtime_usage_resumes WHERE id=? AND agent=?',
                                               (current_id, key)).fetchone()
                        if existing:
                            resume = json.loads(existing['record'])
                        else:
                            resume = self.usage_resume_record(agent, turn_id, error)
                    if current_id == resume['id'] and resume['status'] in {'scheduled', 'cancelled'}:
                        resume.update(status='scheduled', reason=None)
                        now = time.time()
                        reset = resume.get('resetAt')
                        resume['dueAt'] = min(now + POLL_SECONDS, reset) if reset and reset > now else now + POLL_SECONDS
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
        for account_key in by_account:
            try:
                results[account_key] = self.limits(account_key, force=True)
            except Exception:
                results[account_key] = None
        with self.lock, self.db() as db:
            for account_key, candidates in by_account.items():
                limits = results.get(account_key) or {}
                data = limits.get('data') or {}
                fresh = (not limits.get('error') and not limits.get('stale')
                         and limits.get('accountKey', account_key) == account_key
                         and isinstance(limits.get('at'), (int, float)) and limits['at'] >= now - 5)
                allowed = fresh and _allowed(data, now)
                reset_at = _reset_at(data)
                next_due = min(now + POLL_SECONDS, reset_at) if reset_at and reset_at > now else now + POLL_SECONDS
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
                    if not allowed:
                        resume.update(dueAt=next_due, plannedAt=reset_at if reset_at and reset_at > now else None,
                                      resetAt=reset_at, lastCheckedAt=limits.get('at'),
                                      reason=None if fresh else 'Waiting for a current account limits result.')
                        self.usage_resume_save(db, agent, resume)
                        self.put(db, 'agents', agent)
                        continue
                    event_id = 'usage-resume:' + resume['id']
                    self.enqueue(db, agent, 'followup', CONTINUATION, event_id)
                    resume.update(status='started', dueAt=None, startedAt=now, reason=None)
                    self.usage_resume_save(db, agent, resume)
                    agent.pop('nativeFailureHold', None)
                    agent.update(status='queued', error=None)
                    self.put(db, 'agents', agent)
