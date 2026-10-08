"""Durable orchestration over the owner-paired server channel.

The transport authenticates both ends. This module never accepts an address or
credential from an agent. Agent IDs remain UUIDs; remote workers have a local
proxy and an explicit remote-parent link. SQLite owns queues and receipts.
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import selectors
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from typing import Any, Protocol, cast
import uuid

TIMEOUT = 20
MAX_BUNDLE = 256 * 1024 * 1024
CHUNK = 128 * 1024
PATH = '/api/servers/orchestration'


class Transport(Protocol):
    @property
    def local_server_id(self) -> str: ...
    def servers(self) -> list[dict[str, Any]]: ...
    def request(self, server: str, envelope: dict[str, Any], *, timeout: int) -> dict[str, Any]: ...


def identity(*parts: str) -> str:
    return str(uuid.uuid5(uuid.NAMESPACE_URL, 'studio-cross-server:' + ':'.join(parts)))


def encoded(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)


class MultiServerService:
    def __init__(self, runtime: Any, transport: Transport) -> None:
        self.runtime, self.transport = runtime, transport
        self._running = False
        self._claim_lock = threading.RLock()
        with runtime.db() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT);
                CREATE INDEX IF NOT EXISTS runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
                CREATE TABLE IF NOT EXISTS runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT);
                CREATE TABLE IF NOT EXISTS runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT);
            ''')

    @property
    def server_id(self) -> str:
        return self.transport.local_server_id

    def link(self, db: Any, link_id: str) -> dict[str, Any]:
        row = db.execute('SELECT record FROM runtime_server_links WHERE id=?', (link_id,)).fetchone()
        if not row:
            raise ValueError('Unknown remote-parent link')
        return json.loads(row[0])  # type: ignore[no-any-return]

    def queue(self, db: Any, server: str, action: str, payload: dict[str, Any], key: str) -> str:
        body = encoded({'requestId': key, 'action': action, 'payload': payload})
        if len(body.encode('utf-8')) > 256 * 1024:
            raise ValueError('The remote request exceeds 256 KiB; use a smaller batch')
        row = db.execute('SELECT server,body FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
        if row and (row[0] != server or row[1] != body):
            raise ValueError('This request id has different content')
        db.execute('INSERT OR IGNORE INTO runtime_server_outbox(id,server,body,state) VALUES (?,?,?,?)',
                   (key, server, body, 'queued'))
        self.runtime.changed.set()
        return key

    def deliver(self, key: str) -> dict[str, Any]:
        # Retry only the same durable envelope. The receiver never repeats an
        # uncertain operation. Do not hold runtime or SQLite locks during HTTP.
        with self.runtime.read_db() as db:
            row = db.execute('SELECT * FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
            if row['state'] == 'complete':
                return json.loads(row['result'])  # type: ignore[no-any-return]
            envelope, server = json.loads(row['body']), row['server']
        try:
            result = self.transport.request(server, envelope, timeout=TIMEOUT)
            if result.get('requestId') != key or result.get('outcome') not in {'applied', 'not_applied', 'unknown'}:
                raise RuntimeError('The paired server returned an invalid receipt')
        except (OSError, RuntimeError, TimeoutError) as error:
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_server_outbox SET attempts=attempts+1,next_at=?,error=? WHERE id=? AND state!='complete'",
                           (time.time() + 5, type(error).__name__, key))
            return {'requestId': key, 'outcome': 'unknown', 'status': 'offline', 'queued': True}
        with self.runtime.lock, self.runtime.db() as db:
            if result['outcome'] != 'unknown':
                db.execute("UPDATE runtime_server_outbox SET state='complete',result=?,error=NULL WHERE id=?",
                           (encoded(result), key))
                if envelope['action'] == 'spawn':
                    if result['outcome'] == 'applied':
                        self._spawn_received(db, envelope['payload'], result['value'])
                    elif result['outcome'] == 'not_applied':
                        for worker_id in envelope['payload']['workers']:
                            proxy = self.runtime.agent(worker_id, db)
                            proxy.update(status='failed', inFlight=False, error=result['error'])
                            self.runtime.put(db, 'agents', proxy)
                        self.runtime.release_failed_work(db, self.runtime.release_work_agents(db), force=True)
                if envelope['action'] == 'admit' and result['outcome'] == 'applied':
                    worker = self.runtime.agent(envelope['payload']['worker'], db)
                    admission = worker.get('remoteAdmission') or {}
                    if admission.get('id') == key and worker['epoch'] == envelope['payload']['epoch']:
                        admission['state'] = 'granted'
                        worker['remoteLastAdmission'] = key
                        worker['remoteAdmission'] = admission
                        self.runtime.put(db, 'agents', worker)
                        self.runtime.changed.set()
            else:
                db.execute('UPDATE runtime_server_outbox SET attempts=attempts+1,next_at=?,error=NULL WHERE id=?',
                           (time.time() + 5, key))
        return result

    def tick(self) -> None:
        with self._claim_lock:
            if self._running or self.runtime.closed:
                return
            self._running = True
        def run() -> None:
            try:
                with self.runtime.read_db() as db:
                    keys = [r[0] for r in db.execute("SELECT id FROM runtime_server_outbox WHERE state='queued' AND next_at<=? ORDER BY rowid LIMIT 8", (time.time(),))]
                for key in keys:
                    if self.runtime.closed:
                        break
                    self.deliver(key)
            finally:
                with self._claim_lock:
                    self._running = False
        self.runtime.delivery_executor().submit(run)

    def spawn(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        specs = args.get('agents')
        if not actor.get('isLead') or not isinstance(specs, list) or not 1 <= len(specs) <= 64:
            raise ValueError('Only the lead can create 1 to 64 remote workers')
        targets = {s.get('server', args.get('server')) for s in specs if isinstance(s, dict)}
        if len(targets) != 1:
            raise ValueError('Use one server per spawn batch')
        server = targets.pop()
        if not isinstance(server, str):
            raise ValueError('Select a paired server')
        if server not in {s['id'] for s in self.transport.servers()}:
            raise ValueError('Select a paired server')
        for spec in specs:
            if not isinstance(spec, dict) or not isinstance(spec.get('cwd'), str) or not Path(spec['cwd']).is_absolute():
                raise ValueError('Supply an absolute cwd on the remote server for every worker')
            if not isinstance(spec.get('name'), str) or not 1 <= len(spec['name'].strip()) <= 100 or not isinstance(spec.get('prompt'), str) or not 1 <= len(spec['prompt'].strip()) <= 32000:
                raise ValueError('Every worker needs a name and task')
        link_id = identity(self.server_id, actor['id'], key)
        workers = [str(uuid.uuid5(uuid.NAMESPACE_URL, 'remote-worker:' + link_id + ':' + str(i))) for i in range(len(specs))]
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch']:
                raise ValueError('The parent was stopped')
            from codex_agent_modes import assert_delegation
            assert_delegation(current)
            # A remote batch reserves execution slots before the remote server
            # can start. Unknown/offline batches retain these slots.
            previous_request = db.execute('SELECT server,body,state,result,error FROM runtime_server_outbox WHERE id=?', (identity(link_id, 'spawn'),)).fetchone()
            if previous_request:
                old = json.loads(previous_request['body'])['payload']
                if previous_request['server'] != server or old['specs'] != [{k:v for k,v in s.items() if k != 'server'} for s in specs]:
                    raise ValueError('This request id has different content')
                return {'requestId': key, 'remoteRequestId': identity(link_id, 'spawn'), 'server': server,
                        'agents': [{'id': w, 'name': s['name'], 'cwd': s['cwd'], 'server': server} for w,s in zip(workers,specs)],
                        'outcome': json.loads(previous_request['result'])['outcome'] if previous_request['result'] else 'unknown',
                        'recovery': 'Read the saved remote request receipt.'}
            existing = self.runtime.team_agents(db, current['rootId'])
            busy = self.runtime.dispatch_active_slots(db)
            from codex_runtime import team_capacity_counts
            active, _finished = team_capacity_counts(existing, current['rootId'])
            if active + len(workers) > current['maxAgents']:
                raise ValueError('The remote batch exceeds the team agent limit')
            if sum(a['rootId'] == current['rootId'] and a['id'] != current['rootId'] for a in busy) + len(workers) > current['concurrency']:
                raise ValueError('The remote batch exceeds available execution slots')
            tasks = [s['task_id'] for s in specs if s.get('task_id')]
            if len(tasks) != len(set(tasks)):
                raise ValueError('Assign each task to one worker')
            works = {}
            for task in tasks:
                work = self.runtime.work_by_id(db, task, current['rootId'])
                if not work or work.get('owner') or work['status'] in {'review', 'accepted', 'cancelled'}:
                    raise ValueError('The remote task is unknown or already assigned')
                works[task] = work
            payload = {'link': link_id, 'home': self.server_id, 'parent': actor['id'],
                       'parentEpoch': actor['epoch'], 'root': actor['rootId'], 'workers': workers,
                       'concurrency': current['concurrency'], 'yoloMode': current.get('yoloMode', False),
                       'defaults': self.runtime.worker_defaults(current), 'leadModel': current['model'],
                       'specs': [{k: v for k, v in s.items() if k != 'server'} for s in specs],
                       'tasks': works}
            remote_key = identity(link_id, 'spawn')
            prior = db.execute('SELECT body FROM runtime_server_outbox WHERE id=?', (remote_key,)).fetchone()
            if prior:
                # Validate immutable caller content without regenerating task
                # versions or permission settings on a retry.
                old = json.loads(prior[0])['payload']
                if old['specs'] != payload['specs'] or old['parent'] != actor['id']:
                    raise ValueError('This request id has different content')
            else:
                db.execute('INSERT INTO runtime_server_links VALUES (?,?)', (link_id, encoded({**payload, 'server': server, 'side': 'home'})))
                for worker_id, spec in zip(workers, specs):
                    proxy = {**current, 'id': worker_id, 'isLead': False, 'parentId': current['id'],
                             'rootId': current['rootId'], 'name': spec['name'], 'prompt': spec['prompt'],
                             'cwd': spec['cwd'], 'role': spec.get('role', 'implementer'),
                             'threadId': None, 'turnId': None, 'epoch': 0, 'turnEpoch': 0,
                             'status': 'starting', 'inFlight': True, 'created': time.time(),
                             'worktree': False, 'imageWorkspace': False,
                             'remoteWorker': {'server': server, 'link': link_id}, 'remoteReservation': True, 'error': None}
                    for field in ('startAttempt', 'workerDefaults', 'reviewDefaults', 'quickCreate', 'needsTitle'):
                        proxy.pop(field, None)
                    self.runtime.put(db, 'agents', proxy)
                    if spec.get('task_id'):
                        work = works[spec['task_id']]
                        work.update(owner=worker_id, version=work['version'] + 1, updated=time.time())
                        self.runtime.put(db, 'work', work)
                self.queue(db, server, 'spawn', payload, remote_key)
        receipt = self.deliver(remote_key)
        return {'requestId': key, 'remoteRequestId': remote_key, 'server': server,
                'outcome': receipt['outcome'], 'status': receipt.get('status'),
                'agents': ([{**summary, 'server': server} for summary in receipt['value']['agents']]
                           if receipt['outcome'] == 'applied' else
                           [{'id': w, 'name': s['name'], 'cwd': s['cwd'], 'server': server} for w, s in zip(workers, specs)]),
                'delivery': 'Remote events wait in a durable queue while the server is offline.'}

    def _spawn_received(self, db: Any, payload: dict[str, Any], value: dict[str, Any]) -> None:
        for summary in value['agents']:
            proxy = self.runtime.agent(summary['id'], db)
            # A late spawn receipt cannot replace a newer worker snapshot.
            if not proxy['autoWake'] or proxy.get('remoteStateSequence'):
                continue
            proxy.update(cwd=summary['cwd'], branch=summary.get('branch'), status='starting')
            self.runtime.put(db, 'agents', proxy)

    def event(self, db: Any, agent: dict[str, Any], kind: str, text: str, key: str) -> str | None:
        remote = agent.get('remoteWorker')
        if remote:
            return self.queue(db, remote['server'], 'input', {'link': remote['link'], 'worker': agent['id'],
                'kind': kind, 'text': text, 'epoch': agent.get('remoteEpoch', 0), 'controlEpoch': agent['epoch']}, identity('input', key))
        anchor = agent.get('remoteAnchor')
        if anchor:
            return self.queue(db, anchor['home'], 'event', {'link': anchor['link'], 'kind': kind,
                'text': text}, identity('event', key))
        return None

    def admission(self, db: Any, worker: dict[str, Any]) -> bool:
        admission = worker.get('remoteAdmission') or {}
        if admission.get('epoch') == worker['epoch']:
            return admission.get('state') == 'granted'
        key = identity(worker['id'], 'admit', str(worker['epoch']), uuid.uuid4().hex)
        origin = worker['remoteOrigin']
        self.queue(db, origin['home'], 'admit', {'link': origin['link'], 'worker': worker['id'],
            'epoch': worker['epoch']}, key)
        worker['remoteAdmission'] = {'id': key, 'epoch': worker['epoch'], 'state': 'pending'}
        self.runtime.put(db, 'agents', worker)
        return False

    def state(self, db: Any, agent: dict[str, Any], previous: dict[str, Any] | None) -> None:
        origin = agent.get('remoteOrigin')
        if origin and agent.get('remoteAdmission') and agent['status'] in {'completed', 'failed', 'paused', 'interrupted'} and not agent.get('inFlight'):
            agent.pop('remoteAdmission', None)
            self.runtime.put(db, 'agents', agent)
        fields = ('status', 'cwd', 'branch', 'autoWake', 'epoch', 'inFlight', 'tokensUsed', 'error')
        if origin and (previous is None or any(agent.get(k) != previous.get(k) for k in fields)):
            key = identity(agent['id'], 'state', uuid.uuid4().hex)
            sequence = db.execute('SELECT COALESCE(MAX(rowid),0)+1 FROM runtime_server_outbox').fetchone()[0]
            self.queue(db, origin['home'], 'state', {'link': origin['link'], 'worker': agent['id'],
                'sequence': sequence, 'record': {**{k: agent.get(k) for k in fields},
                    'admissionId': (agent.get('remoteAdmission') or {}).get('id') or agent.get('remoteLastAdmission')}}, key)
        remote = agent.get('remoteWorker')
        if remote and previous and previous['autoWake'] and not agent['autoWake']:
            self.queue(db, remote['server'], 'stop', {'link': remote['link'], 'worker': agent['id'],
                'reason': agent.get('error') or 'Stopped by the parent', 'controlEpoch': agent['epoch']}, identity(agent['id'], 'stop', str(agent['epoch'])))

    def worker_call(self, actor: dict[str, Any], action: str, args: dict[str, Any], key: str) -> dict[str, Any]:
        origin = actor['remoteOrigin']
        key = identity(actor['id'], action, key)
        payload = {'link': origin['link'], 'worker': actor['id'], 'epoch': actor['epoch'], 'args': args}
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch']:
                raise ValueError('The worker was stopped')
            self.queue(db, origin['home'], action, payload, key)
        receipt = self.deliver(key)
        if receipt['outcome'] == 'not_applied':
            raise ValueError(receipt['error'])
        if receipt['outcome'] == 'unknown':
            return {**receipt, 'detail': 'The request is saved. Do not submit another identity.'}
        return receipt['value']  # type: ignore[no-any-return]

    def receive(self, principal: str, envelope: dict[str, Any]) -> dict[str, Any]:
        # principal is authenticated by the pairing boundary, never by payload.
        if principal not in {s['id'] for s in self.transport.servers()}:
            raise PermissionError('The server is not paired')
        key, action, payload = envelope.get('requestId'), envelope.get('action'), envelope.get('payload')
        if not isinstance(key, str) or not 1 <= len(key) <= 200 or not isinstance(action, str) or not isinstance(payload, dict):
            raise ValueError('Invalid cross-server request')
        signature = hashlib.sha256(encoded([principal, action, payload]).encode()).hexdigest()
        with self._claim_lock, self.runtime.db() as db:
            row = db.execute('SELECT * FROM runtime_server_inbox WHERE id=?', (key,)).fetchone()
            if row:
                if row['signature'] != signature:
                    raise ValueError('This request id has different content')
                if row['result']:
                    return json.loads(row['result'])  # type: ignore[no-any-return]
                # Preserve the crash boundary. Only an exact durable effect can
                # reconcile a running receipt; no second execution is allowed.
                if (action in {'admit', 'projects', 'folders', 'git', 'chunk', 'directory', 'chat_read', 'context'}
                        or action == 'task' and payload.get('args', {}).get('action') in {'list', 'get', 'history'}
                        or action == 'complaint' and payload.get('args', {}).get('action') == 'read'):
                    # Reads have no effect. Slot admission is a compare-and-set reservation, with no
                    # native input. A denied reservation has no effect to replay.
                    pass
                else:
                    evidence = self._evidence(db, action, payload, key)
                    if evidence is None:
                        return {'requestId': key, 'outcome': 'unknown'}
                    result = {'requestId': key, 'outcome': 'applied', 'value': evidence}
                    db.execute("UPDATE runtime_server_inbox SET state='complete',result=? WHERE id=?", (encoded(result), key))
                    return result
            db.execute('INSERT OR IGNORE INTO runtime_server_inbox VALUES (?,?,?,NULL)', (key, signature, 'running'))
        try:
            value = self._receive(principal, action, payload, key)
            if action == 'admit' and not value.get('granted'):
                return {'requestId': key, 'outcome': 'unknown'}
            result = {'requestId': key, 'outcome': 'applied', 'value': value}
        except (ValueError, PermissionError) as error:
            result = {'requestId': key, 'outcome': 'not_applied', 'error': str(error)}
        except Exception:
            # Native/OS failures can follow a committed effect. Keep unknown.
            return {'requestId': key, 'outcome': 'unknown'}
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='complete',result=? WHERE id=?", (encoded(result), key))
        return result

    def _evidence(self, db: Any, action: str, payload: dict[str, Any], key: str) -> dict[str, Any] | None:
        row = db.execute('SELECT result FROM runtime_operation_receipts WHERE id=?', (key,)).fetchone()
        if row:
            value = json.loads(row[0])
            if action == 'task':
                return {**self.runtime.task_brief(value), 'requestId': key,
                    'detail': {'tool': 'orchestration_task', 'action': 'get', 'task_id': value['id']}}
            return cast(dict[str, Any], value)
        if action == 'spawn':
            spawn_key = 'remote-worker:' + payload['link']
            row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (spawn_key,)).fetchone()
            if row:
                from codex_payloads import resolve_result
                result = resolve_result(self.runtime.root, row[0])
                if result.get('success'):
                    return cast(dict[str, Any], json.loads(result['contentItems'][0]['text']))
        if action in {'input', 'event'}:
            row = db.execute('SELECT status,error FROM runtime_events WHERE id=?', (key,)).fetchone()
            if row:
                return {'id': key, 'status': row['status'], 'error': row['error']}
        if action == 'export':
            export_id = identity(key, 'bundle')
            path = self.runtime.root / 'server-exports' / (export_id + '.bundle')
            if path.is_file():
                return {'export': export_id, 'bytes': path.stat().st_size, 'sha256': file_digest(path), 'branch': payload['branch']}
        if action == 'stop':
            worker = self.runtime.agent(payload['worker'], db)
            if worker.get('remoteStopRequest') == key:
                return {'stopped': [worker['id']]}
        if action == 'state':
            worker = self.runtime.agent(payload['worker'], db)
            if worker.get('remoteStateSequence', 0) >= payload['sequence']:
                return {'stored': True, 'reconciled': True}
        return None

    def _receive(self, principal: str, action: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if action == 'spawn':
            return self._remote_spawn(principal, p, key)
        if action in {'projects', 'folders', 'git', 'export', 'chunk'}:
            return self._git_action(action, p, key)
        with self.runtime.lock, self.runtime.db() as db:
            link_id = p.get('link')
            if not isinstance(link_id, str):
                raise ValueError('Supply a remote-parent link')
            link = self.link(db, link_id)
            expected = link['server'] if link['side'] == 'home' else link['home']
            if principal != expected:
                raise PermissionError('The remote-parent link belongs to another server')
            if action in {'input', 'stop'}:
                if link['side'] != 'remote' or p.get('worker') not in link['workers']:
                    raise PermissionError('Worker is outside this remote-parent link')
                worker = self.runtime.agent(p['worker'], db)
            elif action in {'state', 'task', 'message', 'admit', 'directory', 'complaint', 'chat_read', 'context'}:
                if link['side'] != 'home' or p.get('worker') not in link['workers']:
                    raise PermissionError('Worker is outside this home team')
                worker = self.runtime.agent(p['worker'], db)
            elif action == 'event':
                if link['side'] != 'home':
                    raise PermissionError('The event is not addressed to this home team')
            else:
                raise ValueError('Unknown cross-server operation')
            if action == 'admit':
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch'] or not parent['autoWake'] or not worker['autoWake']:
                    raise ValueError('The home team was stopped')
                if p['epoch'] < worker.get('remoteEpoch', 0):
                    raise ValueError('The remote worker epoch changed')
                worker['remoteEpoch'] = p['epoch']
                root = self.runtime.agent(worker['rootId'], db)
                from codex_budget import budget_admission
                budget_admission(self.runtime, db, worker)
                if root['concurrency'] == 0:
                    return {'granted': False}
                if worker.get('remoteAdmissionRequest') == key:
                    return {'granted': True}
                if not worker.get('remoteReservation'):
                    slots = self.runtime.dispatch_active_slots(db)
                    if sum(a['rootId']==root['id'] and a['id']!=root['id'] for a in slots) >= root['concurrency']:
                        return {'granted': False}
                worker.update(remoteReservation=True, remoteAdmissionRequest=key, inFlight=True, status='starting')
                self.runtime.put(db, 'agents', worker)
                return {'granted': True}
            if action == 'state':
                record = p['record']
                if p['sequence'] <= worker.get('remoteStateSequence', 0):
                    return {'stored': False, 'reason': 'A newer remote state already exists'}
                worker['remoteStateSequence'] = p['sequence']
                worker['remoteEpoch'] = max(worker.get('remoteEpoch', 0), record['epoch'])
                if not worker['autoWake'] or self.runtime.agent(link['parent'], db)['epoch'] != link['parentEpoch']:
                    if record.get('status') in {'paused','failed','completed','interrupted'} and not record.get('inFlight'):
                        worker['inFlight'] = False
                    self.runtime.put(db, 'agents', worker)
                    return {'stored': False, 'reason': 'The parent or worker was stopped'}
                if (record.get('status') in {'completed','failed','paused','interrupted'}
                        and worker.get('remoteAdmissionRequest')
                        and record.get('admissionId') != worker['remoteAdmissionRequest']):
                    self.runtime.put(db, 'agents', worker)
                    return {'stored': False, 'reason': 'This snapshot belongs to an earlier admission'}
                worker.update({k: record[k] for k in ('status', 'cwd', 'branch', 'inFlight', 'tokensUsed', 'error') if k in record})
                if record.get('status') in {'completed', 'failed', 'paused', 'idle', 'interrupted'} and not record.get('inFlight'):
                    worker['remoteReservation'] = False
                worker['inFlight'] = bool(worker.get('remoteReservation') or record.get('inFlight'))
                worker['remoteEpoch'] = record['epoch']
                self.runtime.put(db, 'agents', worker)
                return {'stored': True}
            if action == 'event':
                if p['kind'] not in {'child_result', 'child_stopped', 'agent_message', 'work_review'}:
                    raise ValueError('Unsupported remote event kind')
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch']:
                    return {'stored': False, 'reason': 'The parent was stopped'}
                if p['kind'] == 'child_result':
                    details = json.loads(p['text'])
                    if details.get('agent_id') not in link['workers']:
                        raise PermissionError('The result is outside this remote-parent link')
                    proxy = self.runtime.agent(details['agent_id'], db)
                    proxy['lastAnswer'] = details.get('result', details.get('reason', ''))
                    proxy['lastCompletedTurn'] = key
                    self.runtime.put(db, 'agents', proxy)
                self.runtime.enqueue_recovery_event(db, parent, p['kind'], p['text'], key)
                return {'eventId': key}
            if action in {'task', 'message', 'directory', 'complaint', 'chat_read', 'context'}:
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch'] or not parent['autoWake'] or not worker['autoWake']:
                    raise ValueError('The home team was stopped')
                if p['epoch'] < worker.get('remoteEpoch', 0):
                    raise ValueError('The worker epoch changed')
                worker['remoteEpoch'] = p['epoch']
                self.runtime.put(db, 'agents', worker)
            if action == 'input':
                if p.get('controlEpoch', 0) != worker.get('remoteControlEpoch', 0):
                    raise ValueError('The remote worker control epoch changed')
                if p['kind'] in {'user', 'followup'}:
                    # send handles explicit resumption and preserves native input
                    # receipts. Other events never reopen a stopped worker.
                    pass
                else:
                    self.runtime.enqueue_recovery_event(db, worker, p['kind'], p['text'], key)
                    return {'eventId': key}
        if action == 'input':
            return cast(dict[str, Any], self.runtime.send(worker['id'], p['text'], key, manual=p['kind']=='user', resume=True))
        if action == 'stop':
            return cast(dict[str, Any], self.runtime.stop(worker['id'], True, reason=p['reason'],
                _remote_request={'id': key, 'controlEpoch': p['controlEpoch']}))
        if action == 'chat_read':
            return cast(dict[str, Any], self.runtime.chat_read(p['args']['room_id'], worker['id'], p['args'].get('before'), model=True))
        if action == 'context':
            if p['args'].get('topic') not in {'plan','complaints'}:
                raise ValueError('This context belongs to the destination server')
            return cast(dict[str, Any], self.runtime.model_context(worker['id'], p['args']))
        if action == 'directory':
            if p['args'].get('tool') not in {'orchestration_status','orchestration_peers'}:
                raise ValueError('Unknown remote directory tool')
            return cast(dict[str, Any], self.runtime.model_directory(worker['id'], p['args']['tool'], p['args'].get('arguments', {})))
        if action == 'complaint':
            return cast(dict[str, Any], self.runtime.complaint(worker['id'], p['args'], key, worker['epoch']))
        if action == 'task':
            return cast(dict[str, Any], self.runtime.model_work(worker['id'], p['args'], key, worker['epoch']))
        if action == 'message':
            return cast(dict[str, Any], self.runtime.chat_message(worker['id'], p['args']['target'], p['args']['text'], key, worker['epoch'],
                importance=p['args'].get('importance', 'message'), progress_key=p['args'].get('progress_key'),
                progress_version=p['args'].get('progress_version')))
        raise ValueError('Unknown cross-server operation')

    def _remote_spawn(self, principal: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if p.get('home') != principal:
            raise PermissionError('Remote-parent identity differs from the paired server')
        specs = p['specs']
        if not specs or len(specs) != len(p['workers']):
            raise ValueError('Invalid remote worker batch')
        anchor_id = identity(p['link'], 'anchor')
        # Use the destination's account catalog and project defaults. Local
        # account keys from the home server are never inferred to exist here.
        anchor = self.runtime.create({'id': anchor_id, 'name': 'Remote parent ' + p['parent'][:8],
            'prompt': '', 'cwd': specs[0]['cwd'], 'yolo_mode': p['yoloMode'],
            'concurrency': p['concurrency']}, draft=True)
        with self.runtime.lock, self.runtime.db() as db:
            anchor['remoteAnchor'] = {'link': p['link'], 'home': principal}
            self.runtime.put(db, 'agents', anchor)
            db.execute('INSERT OR IGNORE INTO runtime_server_links VALUES (?,?)', (p['link'], encoded({**p, 'side': 'remote'})))
        # Existing spawn owns account/base validation and atomic worker + input
        # commits. Its UUID derivation is used on both servers.
        spawn_key = 'remote-worker:' + p['link']
        expected_ids = [str(uuid.uuid5(uuid.NAMESPACE_URL, spawn_key + ':' + str(i))) for i in range(len(specs))]
        if expected_ids != p['workers']:
            raise ValueError('Remote worker identities differ')
        resolved = []
        for spec in specs:
            defaults = p['defaults']
            clean = {'model': defaults.get('model') or p['leadModel'], 'effort': defaults.get('effort'),
                     'fast_mode': defaults.get('fastMode', False),
                     **{k: v for k, v in spec.items() if k != 'task_id'}}
            if spec.get('task_id'):
                task = p['tasks'][spec['task_id']]
                clean['prompt'] += '\n\n[Remote Studio task ' + task['id'] + '] ' + task['title'] + '\nSubmit evidence through orchestration_task.'
            clean['_remoteOrigin'] = {'home': principal, 'link': p['link']}
            resolved.append(clean)
        return cast(dict[str, Any], self.runtime.spawn_agents(anchor, {'agents': resolved}, spawn_key))

    def tools(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        if not actor.get('isLead'):
            raise ValueError('Only the lead can use remote server tools')
        action = args.get('action', 'list')
        if action == 'list':
            with self.runtime.read_db() as db:
                pending = {r[0] for r in db.execute("SELECT DISTINCT server FROM runtime_server_outbox WHERE state='queued' AND error IS NOT NULL")}
            return {'localServer': self.server_id, 'servers': [{**s, **({'status': 'offline'} if s['id'] in pending else {})} for s in self.transport.servers()]}
        server = args.get('server')
        if not isinstance(server, str):
            raise ValueError('Select a paired server')
        if server not in {s['id'] for s in self.transport.servers()}:
            raise ValueError('Select a paired server')
        if action == 'fetch':
            return self._fetch(actor, args, key)
        if action not in {'projects', 'folders', 'git', 'receipt'}:
            raise ValueError('Select list, projects, folders, git, fetch, or receipt')
        if action == 'receipt':
            with self.runtime.read_db() as db:
                row = db.execute('SELECT server,state,result,error FROM runtime_server_outbox WHERE id=?', (args.get('request_id'),)).fetchone()
                if not row:
                    row = db.execute('SELECT server,state,result,NULL AS error FROM runtime_server_fetch WHERE id=?', (args.get('request_id'),)).fetchone()
            if not row or row['server'] != server:
                raise ValueError('Unknown remote request receipt')
            return {'state': row['state'], 'result': json.loads(row['result']) if row['result'] else None, 'error': row['error']}
        payload: dict[str, Any] = {k: args[k] for k in ('cwd', 'argv') if k in args}
        key = identity(actor['id'], action, key)
        with self.runtime.db() as db:
            self.queue(db, server, action, payload, key)
        return self.deliver(key)

    def _git_action(self, action: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if action == 'projects':
            return cast(dict[str, Any], self.runtime.projects())
        if action == 'folders':
            directory = Path(p.get('cwd', '')).expanduser()
            if not directory.is_absolute() or not directory.is_dir():
                raise ValueError('Supply an existing absolute folder')
            # No recursive traversal or file content. The owner can choose a
            # folder outside a registered project, as with local spawn.
            names = []
            for child in directory.iterdir():
                if child.is_dir():
                    names.append(child.name)
                    if len(names) == 200:
                        break
            return {'cwd': str(directory), 'folders': sorted(names), 'truncated': len(names) == 200}
        if action == 'chunk':
            path = self.runtime.root / 'server-exports' / (str(uuid.UUID(p['export'])) + '.bundle')
            size = path.stat().st_size
            offset = p.get('offset')
            if type(offset) is not int or not 0 <= offset < size:
                raise ValueError('Invalid bundle offset')
            with path.open('rb') as stream:
                stream.seek(offset)
                data = stream.read(CHUNK)
            return {'offset': offset, 'nextOffset': offset + len(data),
                    'data': base64.b64encode(data).decode(), 'sha256': hashlib.sha256(data).hexdigest()}
        directory = Path(p.get('cwd', ''))
        if not directory.is_absolute() or not directory.is_dir():
            raise ValueError('Supply an existing absolute repository folder')
        if action == 'export':
            branch = p.get('branch')
            if not isinstance(branch, str) or branch.startswith('-'):
                raise ValueError('Supply a worker branch')
            subprocess.run(['git', 'check-ref-format', '--branch', branch], check=True, capture_output=True, timeout=10)
            export_id = identity(key, 'bundle')
            folder = self.runtime.root / 'server-exports'
            folder.mkdir(exist_ok=True)
            path = folder / (export_id + '.bundle')
            if not path.exists():
                if shutil.disk_usage(folder).free < MAX_BUNDLE:
                    raise RuntimeError('Insufficient disk space for a Git bundle')
                temporary = folder / (export_id + '.partial')
                export_bundle(directory, branch, temporary)
                temporary.replace(path)
            return {'export': export_id, 'bytes': path.stat().st_size, 'sha256': file_digest(path), 'branch': branch}
        argv = p.get('argv')
        if not isinstance(argv, list) or not argv or len(argv) > 8 or any(not isinstance(a, str) or len(a) > 1024 for a in argv):
            raise ValueError('Supply a bounded Git argument list')
        # Allow complete command forms, not arbitrary Git switches. This blocks
        # aliases, external diff/textconv, config writers and option injection.
        allowed = (argv in [['status', '--porcelain=v1'], ['branch', '--list'],
                            ['rev-parse', 'HEAD'], ['rev-parse', '--show-toplevel']]
                   or argv[:2] in [['log', '-n'], ['show', '--no-patch']])
        if argv[:2] == ['log', '-n']:
            allowed = len(argv) == 3 and argv[2].isdigit() and 1 <= int(argv[2]) <= 50
        elif argv[:2] == ['show', '--no-patch']:
            allowed = len(argv) == 3 and len(argv[2]) in {40, 64} and all(c in '0123456789abcdef' for c in argv[2])
        if not allowed:
            raise ValueError('Unsupported read-only Git command')
        command = ['git', '--no-pager', '--no-optional-locks', '-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null',
                   '-C', str(directory), *argv]
        raw, code, truncated = bounded_command(command)
        return {'exitCode': code, 'output': raw.decode('utf-8', errors='replace'), 'truncated': truncated}

    def _fetch(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        worker = self.runtime.agent(args.get('agent_id'))
        if worker['rootId'] != actor['rootId'] or not worker.get('remoteWorker') or worker['remoteWorker']['server'] != args['server']:
            raise ValueError('Select a remote worker in this team')
        branch, destination = args.get('branch'), args.get('destination', actor['cwd'])
        if not isinstance(branch, str) or not branch or branch.startswith('-'):
            raise ValueError('Supply a worker branch')
        subprocess.run(['git', 'check-ref-format', '--branch', branch], check=True, capture_output=True, timeout=10)
        destination = str(Path(destination).expanduser().resolve())
        subprocess.run(['git', '-C', destination, 'rev-parse', '--git-dir'], check=True, capture_output=True, timeout=10)
        with self.runtime.lock, self.runtime.db() as db:
            signature, previous = self.runtime.operation_receipt(db, key, {'worker': worker['id'], 'branch': branch, 'destination': destination})
            if previous is not None:
                return previous  # type: ignore[no-any-return]
            prior_fetch = db.execute('SELECT * FROM runtime_server_fetch WHERE id=?', (key,)).fetchone()
            if prior_fetch and prior_fetch['signature'] != signature:
                raise ValueError('This request id has different content')
            if prior_fetch and prior_fetch['state'] == 'fetching':
                return {'requestId': key, 'outcome': 'unknown', 'detail': 'The local Git fetch has no final receipt. Inspect FETCH_HEAD; do not repeat it.'}
            db.execute('INSERT OR IGNORE INTO runtime_server_fetch VALUES (?,?,?,?,NULL)',
                (key, args['server'], signature, 'preparing'))
            export_key = identity(key, 'export')
            self.queue(db, args['server'], 'export', {'cwd': worker['cwd'], 'branch': branch}, export_key)
        receipt = self.deliver(export_key)
        if receipt['outcome'] != 'applied':
            return receipt
        metadata = receipt['value']
        if type(metadata.get('bytes')) is not int or not 0 < metadata['bytes'] <= MAX_BUNDLE:
            raise ValueError('Invalid remote bundle size')
        with tempfile.TemporaryDirectory(prefix='studio-server-fetch-') as temporary:
            path = Path(temporary) / 'worker.bundle'
            if shutil.disk_usage(temporary).free < metadata['bytes'] * 2:
                raise RuntimeError('Insufficient disk space for the remote Git bundle')
            offset = 0
            digest = hashlib.sha256()
            with path.open('wb') as stream:
                while offset < metadata['bytes']:
                    chunk_key = identity(key, 'chunk', str(offset))
                    with self.runtime.db() as db:
                        self.queue(db, args['server'], 'chunk', {'export': metadata['export'], 'offset': offset}, chunk_key)
                    chunk_receipt = self.deliver(chunk_key)
                    if chunk_receipt['outcome'] != 'applied':
                        return chunk_receipt
                    chunk = chunk_receipt['value']
                    data = base64.b64decode(chunk['data'], validate=True)
                    if (not data or len(data) > CHUNK or chunk['offset'] != offset or chunk['nextOffset'] != offset + len(data)
                            or offset + len(data) > metadata['bytes'] or hashlib.sha256(data).hexdigest() != chunk['sha256']):
                        raise ValueError('Invalid remote bundle chunk')
                    stream.write(data)
                    digest.update(data)
                    offset += len(data)
            if digest.hexdigest() != metadata['sha256']:
                raise ValueError('The remote bundle checksum differs')
            subprocess.run(['git', '-C', destination, 'bundle', 'verify', str(path)], check=True, capture_output=True, timeout=30)
            # FETCH_HEAD is the only ref changed; branch integration belongs to
            # the orchestrator's separate review and merge action.
            with self.runtime.db() as db:
                claimed = db.execute("UPDATE runtime_server_fetch SET state='fetching' WHERE id=? AND state='preparing'", (key,)).rowcount
            if not claimed:
                return {'requestId': key, 'outcome': 'unknown', 'detail': 'The original local Git fetch is already reserved.'}
            subprocess.run(['git', '-C', destination, 'fetch', '--no-tags', str(path), 'refs/heads/' + branch],
                           check=True, capture_output=True, timeout=60)
            commit = subprocess.check_output(['git', '-C', destination, 'rev-parse', 'FETCH_HEAD'], text=True, timeout=10).strip()
        value = {'requestId': key, 'server': args['server'], 'branch': branch, 'commit': commit,
                 'bytes': metadata['bytes'], 'destination': destination}
        with self.runtime.db() as db:
            self.runtime.save_receipt(db, key, signature, value)
            db.execute("UPDATE runtime_server_fetch SET state='complete',result=? WHERE id=?", (encoded(value), key))
        return value


def export_bundle(directory: Path, branch: str, path: Path) -> None:
    command = ['git', '-C', str(directory), 'bundle', 'create', '-', 'refs/heads/' + branch]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    assert process.stdout is not None
    size = 0
    deadline = time.monotonic() + 60
    try:
        with path.open('wb') as output, selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, 60)
                if not selector.select(remaining):
                    continue
                chunk = os.read(process.stdout.fileno(), 128 * 1024)
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_BUNDLE:
                    raise ValueError('The Git bundle exceeds 256 MiB')
                output.write(chunk)
        if process.wait(timeout=2) != 0:
            raise ValueError('Git could not export the worker branch')
    except BaseException:
        path.unlink(missing_ok=True)
        raise
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=2)
        process.stdout.close()


def bounded_command(command: list[str]) -> tuple[bytes, int, bool]:
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    assert process.stdout is not None
    data = bytearray()
    deadline = time.monotonic() + 10
    truncated = False
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, 10)
                if not selector.select(remaining):
                    continue
                chunk = os.read(process.stdout.fileno(), 8192)
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > 64 * 1024:
                    truncated = True
                    process.kill()
                    break
        return bytes(data[:64 * 1024]), process.wait(timeout=2), truncated
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=2)
        process.stdout.close()


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while data := stream.read(1024 * 1024):
            digest.update(data)
    return digest.hexdigest()


def server_tools(tool: Any, text: Any) -> list[dict[str, Any]]:
    return [tool('orchestration_servers', 'List paired servers, projects and folders. Run bounded read-only Git commands or fetch a remote worker branch into local FETCH_HEAD. Every request keeps a durable receipt; offline requests keep their identity.',
        {'action': {'type': 'string', 'enum': ['list', 'projects', 'folders', 'git', 'fetch', 'receipt']},
         'server': text, 'cwd': text, 'agent_id': text, 'branch': text, 'destination': text,
         'argv': {'type': 'array', 'items': text, 'maxItems': 8}, 'request_id': text}, ['action'])]
