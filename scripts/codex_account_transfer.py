"""Move managed identities at native idle boundaries, without replaying model input."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import threading
import time
import uuid

TERMINAL = {'completed', 'cancelled'}
ACTIVE = {'running', 'starting', 'approval'}


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
                for member in op['members'].values():
                    if member['phase'] == 'reading':
                        member['phase'] = 'waiting'
                    elif member['phase'] == 'submitted':
                        member.update(phase='unknown', error='The server restarted before the transfer receipt arrived. No request was repeated.')
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
        if (lead.get('accountTransfer') or {}).get('id') not in {None, op['id']}:
            return
        members = list(op['members'].values())
        lead['accountTransfer'] = {k: op.get(k) for k in ('id', 'targetAccountKey', 'status', 'updated')}
        lead['accountTransfer'].update(total=len(members), completed=sum(m['phase'] == 'completed' for m in members),
            waiting=next((m.get('error') or m.get('waiting') for m in members if m.get('error') or m.get('waiting')), None),
            needsAttention=any(m['phase'] in {'blocked', 'unknown'} for m in members),
            canRetry=any(m['phase'] == 'blocked' for m in members))
        self.rt.put(db, 'agents', lead)

    def check_destination(self, actor, target, db=None):
        self.rt.accounts.home(target)  # Validate the registered account identity.
        # Older Studio versions also have account/project admission rules.
        checker = getattr(self.rt, 'check_account_project', None)
        if checker:
            checker({**actor, 'accountKey': target}, db)

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
            members = [a for a in rt.records(db, 'agents') if a['rootId'] == key and not a.get('deletedAt')]
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
            if a['rootId'] != op['leadId'] or a.get('deletedAt') or a['id'] in op['members']:
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
            operations = {a['accountTransfer']['id'] for a in agents if a.get('isLead')
                          and (a.get('accountTransfer') or {}).get('status') == 'pending'}
            for key in operations:
                op = self.get(db, key)
                before = len(op['members'])
                self.adopt(db, op, agents)
                dirty = before != len(op['members'])
                for aid, member in op['members'].items():
                    a = rt.agent(aid, db)
                    if a.get('deletedAt') and member['phase'] not in {'submitted', 'unknown', 'ready'}:
                        member.update(phase='completed', waiting=None)
                        dirty = True
                    if member['phase'] not in {'waiting', 'ready'} or aid in self.running:
                        continue
                    if member.get('nextCheck', 0) > time.time() or len(self.running) >= 2:
                        continue
                    reason = self.local_blocker(db, a)
                    if reason:
                        if member.get('waiting') != reason:
                            member['waiting'] = reason
                            dirty = True
                        continue
                    self.running.add(aid)
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
        if a.get('inFlight') or a['status'] in ACTIVE:
            return 'Waiting for the current turn'
        if a.get('workspaceOperation'):
            return 'Waiting for the workspace operation'
        prep = rt.preparations.get(a['id'])
        if prep and not prep['future'].done():
            return 'Waiting for the native thread receipt'
        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                      (a['id'], a['epoch'])).fetchone():
            return 'Waiting for confirmed message delivery'
        for table, statuses in [('monitors', ACTIVE), ('tasks', ACTIVE | {'pending', 'unknown'}), ('requests', {'pending'})]:
            if db.execute(f"SELECT 1 FROM runtime_{table} WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.status') IN ({','.join('?' for _ in statuses)}) LIMIT 1",
                          (a['id'], *statuses)).fetchone():
                return 'Waiting for background work or a tool response'
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_tool_requests'").fetchone():
            if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? AND (json_extract(record,'$.stage') IN ('queued','running') OR json_extract(record,'$.outcome')='unknown') LIMIT 1", (a['id'],)).fetchone():
                return 'Waiting for the tool receipt'
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='voice_sessions'").fetchone():
            columns = {r[1] for r in db.execute('PRAGMA table_info(voice_sessions)')}
            if 'state' in columns and db.execute('SELECT 1 FROM voice_sessions WHERE agent=? AND state IS NOT NULL AND ended IS NULL', (a['id'],)).fetchone():
                return 'End voice to transfer this agent'
        return None

    def update(self, key, aid, **changes):
        with self.rt.lock, self.rt.db() as db:
            op = self.get(db, key)
            op['members'][aid].update(changes)
            self.save(db, op)
        return op

    def run(self, key, aid):
        rt = self.rt
        try:
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                a = rt.agent(aid, db)
                m = op['members'][aid]
                if rt.closed or self.closing or op['status'] != 'pending' or a.get('accountTransferId') != key:
                    return
                if m['phase'] == 'ready':
                    self.commit(db, op, a)
                    return
                if self.local_blocker(db, a):
                    return
                m.update(phase='reading', source={k: a.get(k) for k in ('epoch', 'accountKey', 'threadId', 'cwd')},
                         settings=rt.preparation_settings(a), error=None)
                self.save(db, op)
            target = op['targetAccountKey']
            self.check_destination(a, target)
            server = rt.connect(target)
            catalog = rt.catalog(target)
            rt.validate_execution(catalog, a['model'], a.get('effort'), a.get('fastMode', False))
            source_path = None
            if a.get('threadId'):
                source = rt.connect(a.get('accountKey', 'default'))
                native = source.call('thread/read', {'threadId': a['threadId'], 'includeTurns': False}, timeout=10)['thread']
                if native.get('id') != a['threadId']:
                    raise ValueError('Source native thread identity changed')
                if native.get('status', {}).get('type') not in {'idle', 'notLoaded'}:
                    self.update(key, aid, phase='waiting', waiting='Waiting for the native turn')
                    return
                if native['status']['type'] == 'idle':
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
                        unsubscribed = rt.submit_reserved(source, 'thread/unsubscribe', {'threadId': a['threadId']})
                    source.wait(unsubscribed, timeout=10)
                    barrier = concurrent.futures.Future()
                    source.after_events(lambda: barrier.set_result(None))
                    barrier.result(10)
                with rt.lock:
                    rt.loaded.discard(aid)
                if not native.get('path'):
                    raise ValueError('Codex returned no saved context path')
                source_path = self.copy_history(rt.accounts.home(a.get('accountKey', 'default')), rt.accounts.home(target), native['path'])
            params = rt.new_thread_params({**a, 'accountKey': target})
            params.pop('dynamicTools', None)
            params['excludeTurns'] = True
            if source_path:
                method = 'thread/fork'
                params.update(threadId=a['threadId'], path=str(source_path), deferGoalContinuation=True)
            else:
                method = 'thread/start'
                params.pop('excludeTurns', None)
                params['dynamicTools'] = rt.tool_definitions()
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                current = rt.agent(aid, db)
                if rt.closed or self.closing or op['status'] != 'pending' or current.get('accountTransferId') != key:
                    return
                self.assert_source(op['members'][aid], current)
                if self.local_blocker(db, current):
                    op['members'][aid]['phase'] = 'waiting'
                    self.save(db, op)
                    return
                op['members'][aid].update(phase='submitted', targetConnection=rt.connection_ids[target], submittedAt=time.time())
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

    def received(self, key, aid, future):
        rt = self.rt
        try:
            result = future.result()
            if not result.get('thread', {}).get('id'):
                raise ValueError('Native transfer returned no thread identity; outcome unknown')
            self.update(key, aid, phase='ready', result=result, error=None)
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                if op['status'] == 'pending' and not rt.closed:
                    self.commit(db, op, rt.agent(aid, db))
        except Exception as error:
            # Protocol validation rejects before execution. Internal failures may apply.
            from codex_native_errors import NativeRpcError
            self.update(key, aid, phase='blocked' if isinstance(error, NativeRpcError) and error.code in {-32601, -32602} else 'unknown', error=str(error))
        finally:
            self.futures.pop((key, aid), None)
            rt.changed.set()

    def commit(self, db, op, a):
        rt = self.rt
        m = op['members'][a['id']]
        self.assert_source(m, a)
        if rt.closed or self.closing or a.get('accountTransferId') != op['id'] or self.local_blocker(db, a):
            return
        if rt.preparation_settings(a) != m['settings']:
            raise ValueError('Agent settings changed during transfer')
        target = op['targetAccountKey']
        self.check_destination(a, target, db)
        result = m['result']
        resume_failed = a.get('autoWake') and a.get('status') in {'failed', 'interrupted'}
        a.setdefault('accountHistory', []).append({'transferId': op['id'], 'accountKey': a.get('accountKey', 'default'),
            'threadId': a.get('threadId'), 'at': time.time()})
        a.update(accountKey=target, threadId=result['thread']['id'], turnId=None,
                 sandbox=result.get('sandbox', a.get('sandbox')), approvalPolicy=result.get('approvalPolicy', a.get('approvalPolicy')))
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
