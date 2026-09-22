"""Move managed identities at native idle boundaries, without replaying model input."""
import concurrent.futures
import copy
import hashlib
import json
import os
from pathlib import Path
import threading
import time
import uuid

TERMINAL = {'completed', 'cancelled'}
ACTIVE = {'running', 'starting', 'approval'}


class TransferSettingsConflict(ValueError):
    pass


def transfer_store(rt):
    with rt.lock:
        if not hasattr(rt, '_account_transfers'):
            rt._account_transfers = AccountTransfers(rt)
        return rt._account_transfers


class AccountTransfers:
    def __init__(self, rt):
        self.rt = rt
        self.closing = False
        self.running = set()
        self.workers = set()
        self.futures = {}
        self.copy_lock = threading.Lock()
        with rt.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            for op in rt.records(db, 'account_transfers'):
                if op.get('status') in TERMINAL:
                    continue
                dirty = False
                for member in op['members'].values():
                    if member['phase'] == 'reading':
                        member['phase'] = 'waiting'
                        dirty = True
                    elif member['phase'] == 'submitted':
                        member.update(phase='unknown', error='The server restarted before the transfer receipt arrived. No request was repeated.')
                        dirty = True
                if dirty:
                    self.save(db, op)

    def close(self):
        with self.rt.lock:
            self.closing = True
            workers = list(self.workers)
        deadline = time.monotonic() + 30
        for worker in workers:
            worker.join(max(0, deadline - time.monotonic()))
        if any(worker.is_alive() for worker in workers):
            raise RuntimeError('Account transfer is still saving context; runtime lease retained')

    def get(self, db, key):
        row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (key,)).fetchone()
        if not row:
            raise ValueError('Unknown account transfer')
        return json.loads(row[0])

    def save(self, db, op):
        op['updated'] = time.time()
        self.rt.put(db, 'account_transfers', op)
        lead = self.rt.agent(op['leadId'], db)
        current = lead.get('accountTransfer') or {}
        if lead.get('accountTransferId') not in {None, op['id']}:
            return
        if (current.get('id') not in {None, op['id']}
                and current.get('status') not in TERMINAL):
            return
        members = list(op['members'].values())
        lead['accountTransfer'] = {k: op.get(k) for k in ('id', 'targetAccountKey', 'status', 'updated')}
        lead['accountTransfer'].update(total=len(members), completed=sum(m['phase'] == 'completed' for m in members),
            waiting=next((m.get('error') or m.get('waiting') for m in members if m.get('error') or m.get('waiting')), None),
            needsAttention=any(m['phase'] in {'blocked', 'unknown'} for m in members),
            canRetry=any(m['phase'] == 'blocked' and not m.get('archiveInvalidated') for m in members))
        self.rt.put(db, 'agents', lead)

    def check_destination(self, actor, target, db=None):
        self.rt.accounts.home(target)  # Validate the registered account identity.

    def settings_snapshot(self, agent):
        return {**self.rt.preparation_settings(agent),
                'provider': agent.get('provider', 'codex'),
                'workerDefaults': copy.deepcopy(agent.get('workerDefaults')),
                'claudeOptions': copy.deepcopy(agent.get('claudeOptions'))}

    def destination_settings(self, agent, target, catalog):
        """Choose destination-native settings without applying a queued source model."""
        rt = self.rt
        source_provider = rt.accounts.get(agent.get('accountKey', 'default')).get('provider', 'codex')
        provider = rt.accounts.get(target).get('provider', 'codex')
        changed = source_provider != provider
        rows = [row for row in catalog.get('data', []) if not row.get('hidden')]
        current = next((row for row in rows if row.get('model') == agent['model']), None)
        if changed or current is None:
            from codex_runtime import DEFAULT_LEAD_MODEL
            default = 'default' if provider == 'claude' else DEFAULT_LEAD_MODEL
            current = next((row for row in rows if row.get('isDefault')), None) or next(
                (row for row in rows if row.get('model') == default), None)
            if current is None:
                raise ValueError('The destination account has no available default model')
        model = current['model']
        fast = bool(agent.get('fastMode', False)) and any(
            tier.get('id') == 'priority' for tier in current.get('serviceTiers', []))
        effort, native = rt.validate_execution(catalog, model, agent.get('effort'), fast,
                                               fallback_effort=True)
        resolved = dict(provider=provider, model=model, effort=effort, nativeEffort=native, fastMode=fast)
        defaults = rt.worker_defaults({'provider': provider} if changed else agent)
        worker = next((row for row in rows if row.get('model') == (defaults.get('model') or model)), current)
        worker_fast = bool(defaults.get('fastMode', False)) and any(
            tier.get('id') == 'priority' for tier in worker.get('serviceTiers', []))
        worker_effort, _ = rt.validate_execution(catalog, worker['model'], defaults.get('effort'), worker_fast,
                                                fallback_effort=True)
        resolved['workerDefaults'] = dict(model=None if defaults.get('model') is None else worker['model'],
                                          effort=worker_effort, fastMode=worker_fast)
        if changed:
            resolved['claudeOptions'] = {}
        if agent.get('pendingSettings'):
            if agent.get('pendingSettingsAccountKey', agent.get('accountKey', 'default')) != agent.get('accountKey', 'default'):
                raise ValueError('Queued settings belong to another account. Save them again before transfer')
            if changed:
                resolved.update(pendingSettings=None, pendingSettingsAccountKey=None)
            else:
                pending = agent['pendingSettings']
                pending_effort, pending_native = rt.validate_execution(
                    catalog, pending['model'], pending.get('effort'), pending.get('fastMode', False))
                resolved.update(pendingSettings={**pending, 'effort': pending_effort, 'nativeEffort': pending_native},
                                pendingSettingsAccountKey=target)
        return resolved

    def request(self, key, target, request_id):
        if not isinstance(request_id, str):
            raise ValueError('Supply a transfer request id')
        uuid.UUID(request_id)
        rt = self.rt
        rt.accounts.get(target)
        with rt.lock, rt.db() as db:
            lead = rt.checked_actor(db, key)
            if not lead.get('isLead'):
                raise ValueError('Choose the orchestrator to transfer its team')
            row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (request_id,)).fetchone()
            if row:
                op = json.loads(row[0])
                if op['leadId'] != key or op['targetAccountKey'] != target:
                    raise ValueError('This transfer id has different content')
                return op
            old = lead.get('accountTransfer') or {}
            if old and old['status'] not in TERMINAL:
                if old['targetAccountKey'] == target:
                    return self.get(db, old['id'])
                raise ValueError('Finish or cancel the current transfer first')
            if rt.accounts.get(target).get("disconnected"):
                raise ValueError("Reconnect this account before transferring a team to it")
            # Account migration moves the orchestrator only. Subagents keep
            # their source account and continue under their existing context.
            members = [a for a in rt.records(db, 'agents')
                       if a['id'] == key and not a.get('deletedAt')]
            for a in members:
                self.check_destination(a, target, db)
            op = {'id': request_id, 'leadId': key, 'targetAccountKey': target,
                  'status': 'pending', 'created': time.time(), 'members': {}}
            self.adopt(db, op, members)
            if all(m["phase"] == "completed" for m in op["members"].values()):
                op["status"] = "completed"
            self.save(db, op)
        rt.changed.set()
        return op

    def adopt(self, db, op, agents):
        for a in agents:
            # Do not add subagents created after the migration starts.
            if a['id'] != op['leadId'] or a.get('deletedAt') or a['id'] in op['members']:
                continue
            done = a.get('accountKey', 'default') == op['targetAccountKey']
            op['members'][a['id']] = {'phase': 'completed' if done else 'waiting',
                'sourceAccountKey': a.get('accountKey', 'default'), 'sourceThreadId': a.get('threadId')}
            if not done:
                a['accountTransferId'] = op['id']
                self.rt.put(db, 'agents', a)

    def action(self, key, action):
        if action not in {'cancel', 'retry'}:
            raise ValueError('Choose cancel or retry')
        rt = self.rt
        with rt.lock, rt.db() as db:
            op = self.get(db, key)
            if op['status'] in TERMINAL:
                return op
            if action == 'cancel':
                op['status'] = 'cancelled'
                for aid in op['members']:
                    a = rt.agent(aid, db)
                    if a.get('accountTransferId') == key and op['members'][aid]['phase'] != 'reading':
                        a.pop('accountTransferId', None)
                        rt.put(db, 'agents', a)
            else:
                if any(m.get('archiveInvalidated') for m in op['members'].values()):
                    raise ValueError('The source changed after history export. Cancel this transfer and start a new one.')
                for m in op['members'].values():
                    if m['phase'] == 'blocked':
                        m.update(phase='ready' if m.get('result') else 'waiting', error=None, nextCheck=0)
                    # Unknown native mutations keep their original callback and receipt.
            self.save(db, op)
        rt.changed.set()
        return op

    def tick(self, agents):
        if self.closing:
            return
        rt = self.rt
        with rt.lock, rt.db() as db:
            operations = set()
            for a in agents:
                if not a.get('isLead'):
                    continue
                summary = a.get('accountTransfer') or {}
                exact = a.get('accountTransferId')
                if exact:
                    op = self.get(db, exact)
                    # Conflicting active identities require explicit reconciliation.
                    if (op['leadId'] != a['id'] or a['id'] not in op['members']
                            or (summary.get('status') == 'pending' and summary.get('id') != exact)):
                        continue
                    if op['status'] == 'pending':
                        operations.add(exact)
                        if summary.get('id') != exact or summary.get('status') != 'pending':
                            self.save(db, op)
                elif summary.get('status') == 'pending':
                    operations.add(summary['id'])
            # Derive reservations from durable operation identities. A pending
            # receipt retains its destination slot even after cancellation.
            busy_targets = {self.get(db, key)['targetAccountKey'] for key, _ in self.futures}
            for running_id in self.running:
                running_agent = rt.agent(running_id, db)
                running_key = running_agent.get('accountTransferId')
                if running_key:
                    busy_targets.add(self.get(db, running_key)['targetAccountKey'])
                else:
                    # Preserve exclusion if a worker lost its agent pointer.
                    busy_targets.update(op['targetAccountKey'] for op in rt.records(db, 'account_transfers')
                                        if running_id in op['members'] and op['members'][running_id]['phase'] == 'reading')
            for key in operations:
                op = self.get(db, key)
                before = len(op['members'])
                self.adopt(db, op, agents)
                dirty = before != len(op['members'])
                for aid, member in list(op['members'].items()):
                    a = rt.agent(aid, db)
                    if (a.get('deletedAt') and member['phase'] not in {'submitted', 'unknown', 'ready'}
                            and aid not in self.running and (key, aid) not in self.futures):
                        member.update(phase='completed', waiting=None)
                        if (a.get('accountTransferId') == key
                                and not any(member.get(field) for field in ('submittedAt', 'nativeMethod', 'nativeParams', 'result'))):
                            a.pop('accountTransferId')
                            rt.put(db, 'agents', a)
                        dirty = True
                    if member['phase'] not in {'waiting', 'ready'} or aid in self.running:
                        continue
                    # Keep the slot until the native receipt, not only submission.
                    # Paginated forks import into one SQLite history database.
                    if member['phase'] != 'ready' and (member.get('nextCheck', 0) > time.time()
                            or op['targetAccountKey'] in busy_targets):
                        continue
                    reason = self.local_blocker(db, a)
                    if reason:
                        if member.get('waiting') != reason:
                            member['waiting'] = reason
                            dirty = True
                        continue
                    self.running.add(aid)
                    busy_targets.add(op['targetAccountKey'])
                    member.update(nextCheck=time.time() + 10, waiting=None)
                    dirty = True
                    # Commit the reservation before the worker reads its receipt.
                    self.save(db, op)
                    db.commit()
                    worker = threading.Thread(target=self.run, args=(key, aid), daemon=True, name='studio-account-transfer')
                    self.workers.add(worker)
                    worker.start()
                if all(m['phase'] == 'completed' for m in op['members'].values()):
                    op['status'] = 'completed'
                    dirty = True
                if dirty:
                    self.save(db, op)

    def local_blocker(self, db, a):
        rt = self.rt
        from codex_context_repair import blocked
        from codex_native_tools import account_reserved
        accounts = {a.get('accountKey', 'default')}
        if a.get('accountTransferId'):
            accounts.add(self.get(db, a['accountTransferId'])['targetAccountKey'])
        if any(account_reserved(rt, key) for key in accounts):
            return 'Waiting for the account tool catalog update'
        if blocked(a):
            return "Waiting for the exact context repair receipt"
        if a.get('inFlight') or a['status'] in ACTIVE:
            return 'Waiting for the current turn'
        if a.get('workspaceOperation'):
            return 'Waiting for the workspace operation'
        prep = rt.preparations.get(a['id'])
        if prep and not prep['future'].done():
            return 'Waiting for the native thread receipt'
        # A previous backend cannot still deliver its RPC. Carry its uncertainty
        # unchanged; it is not an active operation and must never be replayed.
        boot = rt.started_at
        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND "
                      "(status IN ('reserved','dispatching') OR (status='uncertain' AND created>=?)) LIMIT 1",
                      (a['id'], a['epoch'], boot)).fetchone():
            return 'Waiting for confirmed message delivery'
        for table, statuses in [('monitors', ACTIVE), ('tasks', ACTIVE | {'pending', 'unknown'}), ('requests', {'pending'})]:
            if db.execute(f"SELECT 1 FROM runtime_{table} WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.status') IN ({','.join('?' for _ in statuses)}) LIMIT 1",
                          (a['id'], *statuses)).fetchone():
                return 'Waiting for background work or a tool response'
        rt.reconcile_tool_requests(db, a['id'])
        if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? AND "
                      "(json_extract(record,'$.stage') IN ('queued','running') OR "
                      "(json_extract(record,'$.outcome')='unknown' AND coalesce(json_extract(record,'$.created'),?)>=?)) LIMIT 1",
                      (a['id'], boot, boot)).fetchone():
            return 'Waiting for the tool receipt'
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='voice_sessions'").fetchone():
            if db.execute('SELECT 1 FROM voice_sessions WHERE agent=? AND state IS NOT NULL AND ended IS NULL', (a['id'],)).fetchone():
                return 'End voice to transfer this agent'
        return None

    def update(self, key, aid, **changes):
        with self.rt.lock, self.rt.db() as db:
            op = self.get(db, key)
            op['members'][aid].update(changes)
            self.save(db, op)
        return op

    def wait_for_native_queue(self, key, aid, source, thread_id):
        queue = source.call('thread/queue/list', {'threadId': thread_id}, timeout=10)
        if not isinstance(queue.get('data'), list):
            raise ValueError('Codex returned an invalid native queue page')
        if queue['data'] or queue.get('nextCursor'):
            with self.rt.db() as db:
                archived = self.get(db, key)['members'][aid].get('archiveSourceThread')
            if archived is not None:
                self.invalidate_archive(key, aid)
            else:
                self.update(key, aid, phase='waiting', waiting='Waiting for native queued input')
            return True
        return False

    def invalidate_archive(self, key, aid):
        self.update(key, aid, phase='blocked', archiveInvalidated=True, waiting=None,
                    error='The source changed after history export. Cancel this transfer and start a new one.')

    @staticmethod
    def same_native_history(before, after):
        # Claude historyVersion also detects edits within one timestamp tick.
        # Status is separate because unsubscribe may unload an idle session.
        return all(before.get(key) == after.get(key) for key in ('id', 'updatedAt', 'historyVersion'))

    def archive_source_current(self, key, aid):
        """Check the source again before adopting a saved destination receipt."""
        with self.rt.lock, self.rt.db() as db:
            member = self.get(db, key)['members'][aid]
            if member.get('archiveInvalidated'):
                return False
            before = member.get('archiveSourceThread')
            if before is None:
                return True
            source_key = member['sourceAccountKey']
        source = self.rt.connect(source_key)
        after = source.call('thread/read', {'threadId': before['id'], 'includeTurns': False}, timeout=10)['thread']
        if (after.get('status', {}).get('type') not in {'idle', 'notLoaded', 'systemError'}
                or not self.same_native_history(before, after)):
            self.invalidate_archive(key, aid)
            return False
        return not self.wait_for_native_queue(key, aid, source, before['id'])

    def run(self, key, aid):
        rt = self.rt
        try:
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                a = rt.agent(aid, db)
                m = op['members'][aid]
                if rt.closed or self.closing or op['status'] != 'pending' or a.get('accountTransferId') != key:
                    return
                # Restart can move an interrupted validation back to waiting.
                # A saved native result always forbids another fork.
                reuse_result = bool(m.get('result'))
                if reuse_result:
                    self.assert_source(m, a)
                if m.get('archiveInvalidated') or self.local_blocker(db, a):
                    return
                if reuse_result and isinstance(m.get('targetSettings'), dict) and not m.get('portableHistory'):
                    try:
                        self.assert_settings(m, a)
                    except TransferSettingsConflict:
                        pass  # Explicit retry validates the newer settings below.
                    else:
                        self.commit(db, op, a)
                        return
                m.update(phase='reading', source={k: a.get(k) for k in ('epoch', 'accountKey', 'threadId', 'cwd')},
                         settings=self.settings_snapshot(a),
                         pendingSettings=a.get('pendingSettings'),
                         pendingSettingsAccountKey=a.get('pendingSettingsAccountKey'), error=None)
                self.save(db, op)
            target = op['targetAccountKey']
            self.check_destination(a, target)
            server = rt.connect(target)
            catalog = rt.catalog(target)
            source_provider = rt.accounts.get(a.get('accountKey', 'default')).get('provider', 'codex')
            target_provider = rt.accounts.get(target).get('provider', 'codex')
            portable = 'claude' in {source_provider, target_provider}
            if portable:
                resolved = self.destination_settings(a, target, catalog)
            else:
                # Preserve the native Codex fork's strict settings contract.
                effort, native_effort = rt.validate_execution(catalog, a['model'], a.get('effort'), a.get('fastMode', False))
                resolved = dict(effort=effort, nativeEffort=native_effort)
                if a.get('pendingSettings'):
                    if a.get('pendingSettingsAccountKey', a.get('accountKey', 'default')) != a.get('accountKey', 'default'):
                        raise ValueError('Queued settings belong to another account. Save them again before transfer')
                    pending = a['pendingSettings']
                    pending_effort, pending_native = rt.validate_execution(
                        catalog, pending['model'], pending.get('effort'), pending.get('fastMode', False))
                    resolved.update(pendingSettings={**pending, 'effort': pending_effort, 'nativeEffort': pending_native},
                                    pendingSettingsAccountKey=target)
            if m.get('portableHistory'):
                resolved['portableHistory'] = m['portableHistory']
            if reuse_result and portable and not self.archive_source_current(key, aid):
                return
            # Keep destination-derived values separate until the native receipt.
            with rt.lock, rt.db() as db:
                current_op = self.get(db, key)
                current = rt.agent(aid, db)
                if rt.closed or self.closing or current_op['status'] != 'pending' or current.get('accountTransferId') != key:
                    return
                self.assert_source(current_op['members'][aid], current)
                self.assert_settings(current_op['members'][aid], current)
                current_op['members'][aid]['targetSettings'] = resolved
                self.save(db, current_op)
                if reuse_result:
                    self.commit(db, current_op, current)
                    return
            source_path = None
            source = None
            if a.get('threadId'):
                source = rt.connect(a.get('accountKey', 'default'))
                native = source.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=10)['thread']
                if native.get('id') != a['threadId']:
                    raise ValueError('Source native thread identity changed')
                if m.get('archiveSourceThread') is not None and not self.same_native_history(m['archiveSourceThread'], native):
                    self.invalidate_archive(key, aid)
                    return
                if native.get('status', {}).get('type') not in {'idle', 'notLoaded', 'systemError'}:
                    self.update(key, aid, phase='waiting', waiting='Waiting for the native turn')
                    return
                if self.wait_for_native_queue(key, aid, source, a['threadId']):
                    return
                if native['status']['type'] in {'idle', 'systemError'}:
                    jobs = source.call('thread/backgroundTerminals/list', {'threadId': a['threadId']}, timeout=10)
                    if jobs.get('data') or jobs.get('nextCursor'):
                        self.update(key, aid, phase='waiting', waiting='Waiting for native background commands')
                        return
                    # Unsubscribe flushes native persistence. It starts no model request.
                    with rt.lock, rt.db() as db:
                        current_op = self.get(db, key)
                        if current_op['status'] != 'pending':
                            return
                        self.assert_source(current_op['members'][aid], rt.agent(aid, db))
                        self.assert_settings(current_op['members'][aid], rt.agent(aid, db))
                        unsubscribed = rt.submit_reserved(source, 'thread/unsubscribe', {'threadId': a['threadId']})
                    source.wait(unsubscribed, timeout=10)
                    barrier = concurrent.futures.Future()
                    source.after_events(lambda: barrier.set_result(None))
                    barrier.result(10)
                with rt.lock:
                    rt.loaded.discard(aid)
                if not portable:
                    if not native.get('path'):
                        raise ValueError('Codex returned no saved context path')
                    source_path = self.copy_history(rt.accounts.home(a.get('accountKey', 'default')), rt.accounts.home(target), native['path'])
                # Copying a large history can take time. Recheck the durable queue
                # before submission, including for an unloaded source session.
                if self.wait_for_native_queue(key, aid, source, a['threadId']):
                    return
            if portable:
                if source and m.get('archiveSourceThread') is None:
                    self.update(key, aid, archiveSourceThread={k: native.get(k) for k in ('id', 'updatedAt', 'historyVersion')})
                from codex_portable_history import export_history
                descriptor = export_history(rt, a, key, source)
                resolved['portableHistory'] = descriptor
                with rt.lock, rt.db() as db:
                    current_op = self.get(db, key)
                    current = rt.agent(aid, db)
                    self.assert_source(current_op['members'][aid], current)
                    self.assert_settings(current_op['members'][aid], current)
                    current_op['members'][aid].update(portableHistory=descriptor, targetSettings=resolved)
                    self.save(db, current_op)
                if source:
                    after = source.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=10)['thread']
                    if (after.get('status', {}).get('type') not in {'idle', 'notLoaded', 'systemError'}
                            or not self.same_native_history(native, after)):
                        self.invalidate_archive(key, aid)
                        return
                    if self.wait_for_native_queue(key, aid, source, a['threadId']):
                        return
            params = rt.new_thread_params({**a, **resolved, 'accountKey': target})
            params.pop('dynamicTools', None)
            params['excludeTurns'] = True
            if source_path:
                method = 'thread/fork'
                params.update(threadId=a['threadId'], path=str(source_path), deferGoalContinuation=True)
            else:
                method = 'thread/start'
                params.pop('excludeTurns', None)
                params['dynamicTools'] = rt.tool_definitions({**a, **resolved, 'accountKey': target})
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                current = rt.agent(aid, db)
                if rt.closed or self.closing or op['status'] != 'pending' or current.get('accountTransferId') != key:
                    return
                self.assert_source(op['members'][aid], current)
                self.assert_settings(op['members'][aid], current)
                if self.local_blocker(db, current):
                    op['members'][aid]['phase'] = 'waiting'
                    self.save(db, op)
                    return
                op['members'][aid].update(phase='submitted', nativeMethod=method, nativeParams=copy.deepcopy(params),
                                         targetConnection=rt.connection_ids[target], submittedAt=time.time())
                self.save(db, op)
                db.commit()
                submitted = rt.submit_reserved(server, method, params)
                self.futures[(key, aid)] = submitted
            server.on_result(submitted, lambda future: self.received(key, aid, future))
        except Exception as error:
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                m = op['members'][aid]
                # Once the native mutation can have been sent, never infer absence.
                from codex_runtime import SubmissionRejected
                rejected = isinstance(error, SubmissionRejected) or str(error) == 'Codex app-server is offline'
                m.update(phase='unknown' if m['phase'] == 'submitted' and not rejected else 'blocked', error=str(error))
                self.save(db, op)
        finally:
            with rt.lock, rt.db() as db:
                self.running.discard(aid)
                self.workers.discard(threading.current_thread())
                op = self.get(db, key)
                a = rt.agent(aid, db)
                if op['status'] == 'cancelled' and a.get('accountTransferId') == key:
                    a.pop('accountTransferId', None)
                    rt.put(db, 'agents', a)
            rt.changed.set()

    @staticmethod
    def assert_source(m, a):
        if a.get('deletedAt') or any(a.get(k) != v for k, v in m['source'].items()):
            raise ValueError('Agent state changed during transfer. Its original session is preserved.')

    def assert_settings(self, member, agent):
        expected = self.settings_snapshot(agent)
        if (expected != member['settings']
                or agent.get('pendingSettings') != member.get('pendingSettings')
                or agent.get('pendingSettingsAccountKey') != member.get('pendingSettingsAccountKey')):
            raise TransferSettingsConflict('Agent settings changed during transfer. The newer choice is preserved')

    def received(self, key, aid, future):
        rt = self.rt
        try:
            result = future.result()
            if not result.get('thread', {}).get('id'):
                raise ValueError('Native transfer returned no thread identity; outcome unknown')
            self.update(key, aid, phase='ready', result=result, error=None)
            if not self.archive_source_current(key, aid):
                return
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                if op['status'] == 'pending' and not rt.closed:
                    self.commit(db, op, rt.agent(aid, db))
        except Exception as error:
            if self.retry_preparation(key, aid, error):
                return
            # Protocol validation rejects before execution. Internal failures may apply.
            from codex_native_errors import NativeRpcError
            rejected = isinstance(error, NativeRpcError) and error.code in {-32601, -32602}
            with rt.lock, rt.db() as db:
                member = self.get(db, key)['members'][aid]
                saved_result = member.get('portableHistory') and member.get('result')
            self.update(key, aid, phase='blocked' if rejected or saved_result or isinstance(error, TransferSettingsConflict) else 'unknown', error=str(error))
        finally:
            with rt.lock:
                self.futures.pop((key, aid), None)
            rt.changed.set()

    def retry_preparation(self, key, aid, error):
        from codex_native_errors import NativeRpcError
        # Codex 0.153.4 returns this before fork_thread. Other internal errors
        # can follow creation and must retain their unknown outcome.
        message = error.error.get('message', '') if isinstance(error, NativeRpcError) else ''
        if not (isinstance(error, NativeRpcError) and error.code == -32603
                and message.startswith('failed to prepare paginated fork:')
                and 'database is locked' in message):
            return False
        with self.rt.lock, self.rt.db() as db:
            op = self.get(db, key)
            member = op['members'][aid]
            if op['status'] != 'pending' or member['phase'] not in {'submitted', 'unknown'}:
                return False
            rejections = member.setdefault('preparationRejections', [])
            if not any(r['submittedAt'] == member.get('submittedAt') for r in rejections):
                rejections.append({'submittedAt': member.get('submittedAt'),
                                   'at': time.time(), 'error': error.error,
                                   'outcome': 'fork_not_created'})
            if len(rejections) > 3:
                member.update(phase='blocked', error=message, waiting=None)
            else:
                member.update(phase='waiting', error=None,
                              waiting='Codex history database is busy; retrying',
                              nextCheck=time.time() + 2 ** len(rejections))
            self.save(db, op)
        return True

    def commit(self, db, op, a):
        rt = self.rt
        m = op['members'][a['id']]
        self.assert_source(m, a)
        if rt.closed or self.closing or a.get('accountTransferId') != op['id'] or self.local_blocker(db, a):
            return
        self.assert_settings(m, a)
        if not isinstance(m.get('targetSettings'), dict):
            raise TransferSettingsConflict('Destination settings need validation before this saved transfer can finish')
        target = op['targetAccountKey']
        self.check_destination(a, target, db)
        result = m['result']
        resume_failed = a.get('autoWake') and a.get('status') in {'failed', 'interrupted'}
        source_provider = rt.accounts.get(a.get('accountKey', 'default')).get('provider', 'codex')
        target_provider = rt.accounts.get(target).get('provider', 'codex')
        from codex_native_tools import digest, needs_refresh, mark_current
        inherited_catalog = (a.get('nativeToolCatalog')
                             if source_provider == target_provider == 'codex'
                             and m.get('nativeMethod') == 'thread/fork'
                             and not needs_refresh(a, rt.tool_definitions(a)) else None)
        history = {'transferId': op['id'], 'accountKey': a.get('accountKey', 'default'),
                   'threadId': a.get('threadId'), 'provider': source_provider,
                   'targetAccountKey': target, 'targetProvider': target_provider,
                   'targetThreadId': result['thread']['id'], 'at': time.time()}
        if m.get('portableHistory'):
            history['portableHistory'] = m['portableHistory']
        if source_provider != target_provider and a.get('pendingSettings'):
            history['settingsDiscarded'] = {'reason': 'provider_changed', 'pendingSettings': a['pendingSettings']}
        if source_provider != target_provider and a.get('claudeOptions'):
            history['providerOptionsDiscarded'] = {'reason': 'provider_changed', 'claudeOptions': a['claudeOptions']}
        a.setdefault('accountHistory', []).append(history)
        a.update(accountKey=target, threadId=result['thread']['id'], turnId=None,
                 sandbox=result.get('sandbox', a.get('sandbox')), approvalPolicy=result.get('approvalPolicy', a.get('approvalPolicy')))
        a.update(m['targetSettings'])
        if inherited_catalog and inherited_catalog['digest'] == digest(rt.tool_definitions(a)):
            mark_current(a, rt.tool_definitions(a))
        if source_provider != target_provider:
            a.pop('claudeOptions', None)
            a.pop('pendingSettings', None)
            a.pop('pendingSettingsAccountKey', None)
        for field in ('accountTransferId', 'prepareAttempt', 'startAttempt', 'nativeFailureHold'):
            a.pop(field, None)
        # Resume a confirmed failed turn from context, never by replaying its input.
        # Completed work and explicitly paused agents do not consume a new model turn.
        if resume_failed:
            a['error'] = None
            pending = db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND status='pending' LIMIT 1",
                                 (a['id'], a['epoch'])).fetchone()
            if not pending:
                rt.enqueue(db, a, 'followup',
                    'The owner transferred this team to another account. Continue the existing task from its saved context. '
                    'Preserve completed work. Check existing receipts before any command with an unknown outcome.',
                    'account-transfer:' + op['id'] + ':' + a['id'])
            else:
                a['status'] = 'queued'
        rt.put(db, 'agents', a)
        m.update(phase='completed', error=None, waiting=None)
        if op['status'] == 'pending' and all(member['phase'] == 'completed' for member in op['members'].values()):
            op['status'] = 'completed'
        self.save(db, op)
        db.commit()
        rt.loaded.discard(a['id'])
        rt.preparations.pop(a['id'], None)

    def copy_history(self, source_home, target_home, path):
        """Copy canonical rollout ancestry only. Native Codex rebuilds its own projections."""
        source_home, target_home = Path(source_home).resolve(), Path(target_home).resolve()
        manifest_path = target_home / '.studio-account-imports.json'
        with self.copy_lock:
            manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
            seen = set()
            def copy_one(path, boundary=None):
                path = Path(path).resolve()
                relative = path.relative_to(source_home)
                if relative.parts[0] not in {'sessions', 'archived_sessions'} or path.suffix != '.jsonl':
                    raise ValueError('Unsupported native history format; original context is preserved')
                with path.open('rb') as f:
                    first = f.readline()
                    meta = json.loads(first)
                    if meta.get('type') != 'session_meta':
                        raise ValueError('Native history has no session metadata')
                    tid = meta['payload']['id']
                    if path.name in seen:
                        raise ValueError('Native history ancestry contains a cycle')
                    seen.add(path.name)
                    data = first + (f.read(boundary - len(first)) if boundary is not None else f.read())
                if not data.endswith(b'\n') or (boundary is not None and len(data) != boundary):
                    raise ValueError('Native history is not fully persisted')
                base = meta['payload'].get('history_base')
                if base:
                    ancestor = self.find_rollout(source_home, base['thread_id'])
                    copy_one(ancestor, base['end_byte_offset'])
                # Native rollout lookup visits hidden directories. Cost scanners skip
                # them, so imported history does not become usage on this account.
                destination = target_home / 'sessions' / '.studio-imports' / path.name
                relative = destination.relative_to(target_home)
                destination.parent.mkdir(parents=True, exist_ok=True)
                if destination.exists():
                    with destination.open('rb') as existing:
                        if existing.read(len(data)) != data:
                            raise ValueError('The destination has different native history. No file was overwritten.')
                else:
                    # Register imported usage before publishing the native file.
                    manifest[str(relative)] = {'threadId': tid, 'sha256': hashlib.sha256(data).hexdigest(), 'sourceHome': str(source_home)}
                    temp = manifest_path.with_suffix('.tmp')
                    temp.write_text(json.dumps(manifest))
                    os.chmod(temp, 0o600)
                    os.replace(temp, manifest_path)
                    temp = destination.with_suffix('.studio-transfer-' + str(uuid.uuid4()) + '.tmp')
                    with temp.open('xb') as f:
                        os.chmod(temp, 0o600)
                        f.write(data)
                        f.flush()
                        os.fsync(f.fileno())
                    try:
                        os.link(temp, destination)
                    finally:
                        temp.unlink()
                return destination
            return copy_one(path)

    @staticmethod
    def find_rollout(home, tid):
        uuid.UUID(tid)
        matches = list(home.glob(f'sessions/**/*{tid}.jsonl')) + list(home.glob(f'archived_sessions/**/*{tid}.jsonl'))
        if not matches:
            raise ValueError('Native history ancestor is missing')
        matches.sort(key=lambda p: p.stat().st_size, reverse=True)
        # A previous handoff can leave a shorter immutable copy of this ancestor.
        for other in matches[1:]:
            with matches[0].open('rb') as full:
                if full.read(other.stat().st_size) != other.read_bytes():
                    raise ValueError('Native history ancestor is ambiguous')
        return matches[0]
