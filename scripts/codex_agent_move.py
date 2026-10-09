"""Self moves over the paired channel, with exact native history and no input replay."""
from __future__ import annotations

import base64
import copy
from concurrent.futures import Future
import hashlib
import json
from pathlib import Path
import platform
import threading
import time
from typing import Any, cast
import uuid

from codex_exact_history import export_codex, frozen_codex_parameters, private_write, unpack_codex, export_claude, extend_codex_history, MAX_HISTORY_BYTES
from codex_multi_server_orchestration import encoded, identity

CHUNK = 96 * 1024
DEADLINE = 300
_TERMINAL = {'complete', 'unknown', 'failed'}

def move_tools(tool: Any, text: dict[str, Any]) -> list[dict[str, Any]]:
    return [tool('orchestration_move',
        'Move your own conversation and execution to local or a paired server. Prepare the target folder first. '
        'Supply an absolute cwd and a stable request_id. Native history, model, instructions, and tool order stay exact. '
        'Codex and Claude retain native provider history. Active background commands and pending spawns prevent a move. '
        'Finish this turn after acceptance. The next turn continues on the target. Credentials never move. '
        'Same account identity preserves the prompt cache. A different Codex account requires explicit accept_cache_loss=true.',
        {'server': text, 'cwd': text, 'account_key': text, 'note': text, 'request_id': text,
         'accept_cache_loss': {'type': 'boolean', 'description': 'Explicit approval for this move to a different Codex account. The provider prompt cache will be lost.'}}, ['server', 'cwd'])]


class AgentMoves:
    def __init__(self, service: Any) -> None:
        self.service = service
        self.runtime = service.runtime
        self.running: set[str] = set()
        self.lock = threading.Lock()

    def _get(self, db: Any, key: str) -> dict[str, Any] | None:
        row = db.execute('SELECT record FROM runtime_agent_moves WHERE id=?', (key,)).fetchone()
        return json.loads(row[0]) if row else None

    def _save(self, db: Any, record: dict[str, Any]) -> None:
        db.execute('INSERT INTO runtime_agent_moves VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record',
                   (record['id'], encoded(record)))

    def _folder(self, key: str) -> Path:
        uuid.UUID(key)
        folder = Path(self.runtime.root) / 'agent-moves' / key
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
        return folder

    def _exchange(self, server: str, action: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
        with self.runtime.db() as db:
            self.service.queue(db, server, action, payload, key)
        receipt = self.service.deliver(key)
        if receipt['outcome'] == 'not_applied':
            raise ValueError(receipt['error'])
        if receipt['outcome'] != 'applied':
            raise RuntimeError('The move receipt is unknown. No native input was repeated')
        return cast(dict[str, Any], receipt['value'])

    def _account_identity(self, row: dict[str, Any]) -> dict[str, Any]:
        identity = {key: row.get(key) for key in ('provider', 'email', 'accountId')}
        if row.get('provider') == 'claude':
            native = self.runtime.connect(row['id'])
            account = native.call('claude/moveIdentity', {}, timeout=20)
            if account['email'] != row.get('email') or not account.get('organizationId'):
                raise ValueError('The Claude account or organization changed before the move')
            identity['organizationId'] = account['organizationId']
        return identity

    def _select_account(self, payload: dict[str, Any]) -> tuple[dict[str, Any], str | None]:
        if payload['provider'] not in {'codex', 'claude'}:
            raise ValueError('Only Codex and Claude support moves')
        accounts = self.runtime.accounts.list()
        eligible = [row for row in accounts if row.get('provider', 'codex') == payload['provider'] and row.get('status') == 'ready']
        requested = payload.get('account_key')
        source = payload['accountIdentity']
        if requested:
            selected = next((row for row in accounts if row['id'] == requested), None)
            if not selected or selected.get('provider', 'codex') != payload['provider']:
                raise ValueError('A move cannot change providers. Select a target account for the same provider')
            if selected not in eligible:
                raise ValueError('The target account is not signed in')
        else:
            selected = next((row for row in eligible if row.get('email') == source.get('email')
                             and row.get('accountId') == source.get('accountId')), None)
            if selected is None:
                selected = next((row for row in eligible if row.get('email') == source.get('email')), None)
            if selected is None:
                default = self.runtime.accounts.data.get('defaultAccountKey', 'default')
                selected = next((row for row in eligible if row['id'] == default), None)
        if selected is None:
            raise ValueError('No signed-in target account exists for this provider')
        warning = None
        identity_unknown = payload['provider'] == 'codex' and any(
            not selected.get(key) or not source.get(key) for key in ('email', 'accountId'))
        if identity_unknown or any(selected.get(key) != source.get(key) for key in ('email', 'accountId')):
            warning = ('The provider account identity cannot be verified. The prompt cache may be lost' if identity_unknown
                       else 'The target account identity differs. The provider prompt cache will be lost')
            if payload.get('accept_cache_loss') is not True:
                raise ValueError('The exact target account identity is unavailable. Refusing a prompt cache reset; explicit accept_cache_loss=true is required for this move')
        if payload['provider'] == 'claude' and warning:
            raise ValueError('Claude moves require the same provider account and organization on the target')
        return selected, warning

    def _capabilities(self, agent: dict[str, Any]) -> dict[str, Any]:
        server = self.runtime.connect_agent(agent)
        if agent.get('provider') == 'claude':
            versions = server.call('claude/moveVersions', {}, timeout=20)
            return {'provider': 'claude', 'cli': versions['cli'], 'sdk': versions['sdk'], 'platform': platform.system()}
        native = getattr(server, 'native_binary', None) or {}
        # MCP (Model Context Protocol) tool definitions can alter the prefix even
        # when Studio's dynamic tools and native history have identical bytes.
        mcp = server.call('mcpServerStatus/list', {'limit': 100}, timeout=20)
        if not native.get('version') or 'data' not in mcp:
            raise ValueError('The native version or complete MCP tool catalog is unavailable')
        if mcp.get('nextCursor'):
            raise ValueError('The complete native MCP tool catalog exceeds the move preflight limit')
        return {'version': native.get('version'), 'platform': platform.system(),
                'mcp': hashlib.sha256(encoded(mcp.get('data', [])).encode()).hexdigest()}

    def _native_owner(self, db: Any, actor: str, thread: str, account: str, provider: str) -> None:
        uuid.UUID(thread)
        owned = False
        for row in db.execute('SELECT record FROM runtime_agents'):
            other = json.loads(row[0])
            identities = [(other.get('threadId'), other.get('accountKey', 'default'))]
            identities.extend((entry.get('threadId'), entry.get('accountKey', other.get('accountKey', 'default')))
                              for entry in other.get('executionArchives', []))
            if (thread, account) in identities:
                if other['id'] != actor:
                    raise PermissionError('The native identity is owned by another target agent')
                owned = True
        for row in db.execute("SELECT record FROM runtime_agent_moves WHERE json_extract(record,'$.side')='target'"):
            descriptor = json.loads(row[0])['descriptor']
            if (descriptor['nativeThread'] == thread and descriptor['target']['accountKey'] == account
                    and descriptor['agent']['id'] != actor):
                raise PermissionError('The native identity is owned by another pending target import')
        if not owned:
            home = self.runtime.accounts.home(account)
            roots = ('projects',) if provider == 'claude' else ('sessions', 'archived_sessions')
            if any(next((home / root).rglob('*' + thread + '*.jsonl'), None) is not None for root in roots):
                raise PermissionError('The native identity already exists without this Studio owner in the target account')

    def _descriptor_identity(self, db: Any, descriptor: dict[str, Any]) -> None:
        preflight, agent = descriptor['preflight'], descriptor['agent']
        thread = descriptor['nativeThread']
        receipt = db.execute('SELECT signature,result FROM runtime_server_inbox WHERE id=?',
                             (identity(descriptor['move'], 'validate'),)).fetchone()
        signature = hashlib.sha256(encoded([preflight['sourceServer'], 'move_validate', preflight]).encode()).hexdigest()
        if (not receipt or receipt['signature'] != signature or not receipt['result']
                or json.loads(receipt['result']).get('value') != descriptor['target']):
            raise PermissionError('The native identity descriptor differs from its saved preflight receipt')
        if (agent['id'] != preflight['agentId'] or agent['threadId'] != thread
                or preflight['nativeThread'] != thread or agent.get('provider', 'codex') != preflight['provider']):
            raise PermissionError('The descriptor and preflight native identity differ')
        if preflight['provider'] == 'claude':
            session = descriptor['bridgeSession']
            if session['id'] != thread or session.get('nativeId', thread) != thread:
                raise PermissionError('The Claude session native identity differs from preflight')
        self._native_owner(db, agent['id'], thread, descriptor['target']['accountKey'], preflight['provider'])

    def _idle_checks(self, db: Any, agent: dict[str, Any], key: str) -> None:
        if agent.get('provider', 'codex') not in {'codex', 'claude'}:
            raise ValueError('Only Codex and Claude support moves')
        if not agent.get('threadId'):
            raise ValueError('The conversation must have a native history before it can move')
        for field in ('accountTransferId', 'contextRepair', 'nativeToolUpdate', 'workspaceOperation', 'nativeReview', 'pendingSettings'):
            if agent.get(field):
                raise ValueError('Finish the active context, account, settings, or workspace operation before a move')
        other_tools = [tool for tool in agent.get('activeTools', [])
                       if tool.get('name') not in {'orchestration_move', 'mcp__studio__orchestration_move'} and tool.get('id') != key.rsplit(':', 1)[-1]]
        if other_tools:
            raise ValueError('Finish the other active tools before a move')
        if db.execute("SELECT 1 FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                      "AND json_extract(record,'$.tool')='orchestration_spawn' "
                      "AND json_extract(record,'$.stage') IN ('queued','running') LIMIT 1", (agent['id'],)).fetchone():
            raise ValueError('Finish the pending child spawn before a move')
        if db.execute("SELECT 1 FROM runtime_server_outbox WHERE state='queued' "
                      "AND ((json_extract(body,'$.action')='spawn' AND json_extract(body,'$.payload.parent')=?) "
                      "OR (json_extract(body,'$.action') IN ('move_spawn','move_child') AND json_extract(body,'$.payload.worker')=?)) LIMIT 1",
                      (agent['id'], agent['id'])).fetchone():
            raise ValueError('Finish the pending remote child spawn before a move')
        if db.execute("SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=? "
                      "AND json_extract(record,'$.status') IN ('starting','running','approval') LIMIT 1", (agent['id'],)).fetchone():
            raise ValueError('Finish the active command monitors before a move')

    def start(self, actor: dict[str, Any], args: dict[str, Any], tool_key: str) -> dict[str, Any]:
        server = args.get('server')
        if server == 'local':
            server = self.service.server_id
        if not isinstance(server, str) or (server != self.service.server_id
                and server not in {row['id'] for row in self.service.transport.servers()}):
            raise ValueError('Select local or a paired server')
        cwd = args.get('cwd')
        if not isinstance(cwd, str) or not Path(cwd).is_absolute() or '\x00' in cwd:
            raise ValueError('Supply an absolute target cwd')
        note = args.get('note', '')
        if not isinstance(note, str) or len(note) > 12000:
            raise ValueError('The move note must have at most 12000 characters')
        key = identity('move', actor['id'], args.get('request_id') or tool_key)
        fingerprint = hashlib.sha256(encoded(args).encode()).hexdigest()
        with self.runtime.lock, self.runtime.db() as db:
            old = self._get(db, key)
            imported = self._get(db, identity(key, 'target'))
            if old is None and imported and imported['descriptor']['agent']['id'] == actor['id']:
                descriptor = imported['descriptor']
                if descriptor['fingerprint'] != fingerprint:
                    raise ValueError('This move request has different content')
                return cast(dict[str, Any], descriptor['acceptedResult'])
            if old:
                if old['fingerprint'] != fingerprint or old['agent'] != actor['id']:
                    raise ValueError('This move request has different content')
                return cast(dict[str, Any], old['result'])
            agent = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if agent['epoch'] != actor['epoch'] or agent.get('executionMove') or agent.get('movedTo'):
                raise ValueError('The agent already has a move or its execution changed')
            self._idle_checks(db, agent, tool_key)
        native = self.runtime.connect_agent(agent)
        background = native.call('thread/backgroundTerminals/list', {'threadId': agent['threadId']}, timeout=20)
        if 'data' not in background or background.get('data') or background.get('nextCursor'):
            raise ValueError('Stop the native background commands before a move')
        account = self.runtime.accounts.get(agent.get('accountKey', 'default'))
        origin = agent.get('remoteOrigin')
        if origin and server != self.service.server_id:
            canonical = self._exchange(origin['home'], 'move_home_validate',
                {'agent': agent['id'], 'epoch': agent['epoch'], 'controlEpoch': agent.get('remoteControlEpoch', 0),
                 'link': origin['link'], 'target': server}, identity(key, 'home-validate'))
        else:
            canonical = None
        preflight = {'routeHome': origin['home'] if origin else self.service.server_id, 'agentId': agent['id'], 'nativeThread': agent['threadId'], 'sourceServer': self.service.server_id, 'provider': agent.get('provider', 'codex'), 'cwd': cwd,
                     'account_key': args.get('account_key'), 'accountIdentity': self._account_identity(account),
                     'accept_cache_loss': args.get('accept_cache_loss') is True,
                     'capabilities': self._capabilities(agent), 'model': agent['model']}
        if preflight['provider'] == 'claude':
            preflight['claudeProof'] = native.call('claude/moveProof', {'threadId': agent['threadId']}, timeout=20)
        target = self._exchange(server, 'move_validate', preflight, identity(key, 'validate'))
        result = {'requestId': key, 'agentId': agent['id'], 'server': server, 'cwd': cwd,
                  'status': 'accepted', 'accountKey': target['accountKey'],
                  'warning': target.get('warning'),
                  'cacheLossApproved': bool(target.get('warning') and args.get('accept_cache_loss') is True),
                  'cacheProof': target.get('cacheProof', 'exact_native_history_and_catalog'),
                  'delivery': 'Finish this turn. The next turn continues on the target server'}
        operation = {'id': key, 'side': 'source', 'agent': agent['id'], 'epoch': agent['epoch'],
                     'server': server, 'args': copy.deepcopy(args), 'fingerprint': fingerprint, 'result': result,
                     'phase': 'waiting', 'created': time.time(), 'deadline': time.time() + DEADLINE,
                     'target': target, 'toolKey': tool_key, 'preflight': preflight, 'canonical': canonical}
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != agent['epoch'] or current.get('executionMove'):
                raise ValueError('The agent changed before the move was accepted')
            self._idle_checks(db, current, tool_key)
            current['executionMove'] = {'id': key, 'phase': 'waiting', 'server': server, 'cwd': cwd}
            self._save(db, operation)
            self.runtime.put(db, 'agents', current)
            self.runtime.changed.set()
        return result

    def register_review_child(self, actor: dict[str, Any], child_id: str, key: str) -> None:
        """Keep a review on the new server, with its owner and controls at home."""
        with self.runtime.lock, self.runtime.read_db() as db:
            child = self.runtime.agent(child_id, db)
            if not child.get('moveReviewPending'):
                return
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch'] or child.get('parentId') != actor['id']:
                raise ValueError('The review parent changed before registration')
            summary = {name: child.get(name) for name in
                ('id', 'name', 'cwd', 'model', 'effort', 'nativeEffort', 'fastMode', 'provider', 'role', 'epoch', 'created', 'prompt')}
        receipt = self.service.worker_call(actor, 'move_child', {'child': summary}, 'review-child:' + key)
        if not receipt.get('registered'):
            raise RuntimeError('The review registration is unknown. The child stays paused. Read the saved request')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch']:
                raise ValueError('The review parent was stopped during registration')
            child = self.runtime.agent(child_id, db)
            origin = current['remoteOrigin']
            link = self.service.link(db, origin['link'])
            if child_id not in link['workers']:
                link['workers'].append(child_id)
                db.execute('UPDATE runtime_server_links SET record=? WHERE id=?', (encoded(link), link['id']))
            child.update(remoteOrigin=copy.deepcopy(origin), autoWake=True, status='queued')
            child.pop('moveReviewPending', None)
            self.runtime.put(db, 'agents', child)

    def status(self, actor: str, request: str) -> dict[str, Any]:
        key = identity('move', actor, request)
        with self.runtime.read_db() as db:
            operation = self._get(db, key) or self._get(db, identity(key, 'target'))
            if not operation:
                return {'phase': 'unknown', 'requestId': key}
            agent = operation.get('agent') or operation.get('descriptor', {}).get('agent', {}).get('id')
            if agent != actor:
                raise PermissionError('The move belongs to another agent')
        if operation['side'] == 'source' and operation['phase'] == 'complete' and not operation.get('firstTurnCache'):
            try:
                observed = self._exchange(operation['server'], 'move_status', {'move': key},
                    identity(key, 'cache-status', str(int(time.time() // 20))))
                if observed.get('firstTurnCache'):
                    with self.runtime.db() as db:
                        operation = self._get(db, key) or operation
                        operation['firstTurnCache'] = observed['firstTurnCache']
                        self._save(db, operation)
            except Exception:
                pass
        return {'requestId': key, 'phase': operation['phase'], 'error': operation.get('error'),
                'acceptance': operation.get('result') or operation['descriptor']['acceptedResult'],
                'server': operation.get('server') or self.service.server_id,
                'firstTurnCache': operation.get('firstTurnCache')}

    def capture_cache(self, db: Any, agent: dict[str, Any], notification: dict[str, Any]) -> None:
        moved = agent.get('movedFrom')
        if (not moved or not agent.get('inFlight')
                or notification.get('turnId') and notification['turnId'] != agent.get('turnId')):
            return
        operation = self._get(db, identity(moved['move'], 'target'))
        if not operation or operation['phase'] != 'active' or operation.get('firstTurnCache'):
            return
        usage = notification.get('requestUsage') or notification.get('tokenUsage', {}).get('last', {})
        cached = usage.get('cachedInputTokens')
        if not isinstance(cached, int) or isinstance(cached, bool):
            return
        operation['firstTurnCache'] = {'turnId': agent.get('turnId'), 'cachedInputTokens': cached,
            'cacheWriteInputTokens': usage.get('cacheWriteInputTokens'), 'inputTokens': usage.get('inputTokens'),
            'observedAt': time.time()}
        self._save(db, operation)

    def tick(self) -> None:
        with self.runtime.read_db() as db:
            pending_reviews = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_agents WHERE json_extract(record,'$.moveReviewPending')=1")]
        for child in pending_reviews:
            parent = self.runtime.agent(child['parentId'])
            if self.runtime.closed or parent.get('inFlight') or parent.get('activeTools') or not parent.get('autoWake'):
                continue
            review_key = 'review-child:' + child['id']
            with self.lock:
                if review_key in self.running:
                    continue
                self.running.add(review_key)
            self.runtime.delivery_executor().submit(self._recover_review, parent, child, review_key)
        with self.runtime.read_db() as db:
            uncertain = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_agent_moves WHERE json_extract(record,'$.side')='source' "
                "AND json_extract(record,'$.phase')='unknown'")]
        for operation in uncertain:
            key = operation['id']
            with self.lock:
                if key in self.running or self.runtime.closed:
                    continue
                self.running.add(key)
            self.runtime.delivery_executor().submit(self._reconcile, operation)
        with self.runtime.read_db() as db:
            operations = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_agent_moves WHERE json_extract(record,'$.side')='source' "
                "AND json_extract(record,'$.phase') NOT IN ('complete','unknown','failed')")]
        for operation in operations:
            key = operation['id']
            with self.lock:
                if key in self.running or self.runtime.closed:
                    continue
                if operation['phase'] == 'waiting':
                    agent = self.runtime.agent(operation['agent'])
                    if agent.get('inFlight') or agent.get('turnId'):
                        if time.time() > operation['deadline']:
                            self._hold(operation, 'The source turn did not finish within 300 seconds. No target turn was started')
                        continue
                self.running.add(key)
            self.runtime.delivery_executor().submit(self._run, key)

    def _recover_review(self, parent: dict[str, Any], child: dict[str, Any], running_key: str) -> None:
        try:
            self.register_review_child(parent, child['id'], child['nativeReview']['requestId'])
        except Exception:
            # Registration uses the same durable receipt and never submits native input.
            pass
        finally:
            with self.lock:
                self.running.discard(running_key)

    def _reconcile(self, operation: dict[str, Any]) -> None:
        key = operation['id']
        try:
            # A fresh read identity observes newer state; it cannot resume or start input.
            target = self._exchange(operation['server'], 'move_status', {'move': key},
                                    identity(key, 'status', str(int(time.time() // 20))))
            phase = operation['uncertainPhase']
            if phase == 'waiting':
                return
            if phase == 'exported' and target['phase'] == 'receiving':
                # Begin and chunks are immutable compare-and-set writes. The
                # target receipt proves the same transfer exists; exact chunks
                # resume from its saved bytes, never from a new native export.
                pass
            elif target['phase'] not in {'ready', 'active'}:
                return
            elif phase == 'exported':
                phase = 'prepared'
            if phase == 'importing':
                phase = 'prepared'
            operation.update(phase=phase)
            with self.runtime.db() as db:
                self._save(db, operation)
            self._run(key)
        except Exception:
            pass
        finally:
            with self.lock:
                self.running.discard(key)

    def _hold(self, operation: dict[str, Any], message: str) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            operation.update(uncertainPhase=operation['phase'], phase='unknown', error=message)
            self._save(db, operation)
            agent = self.runtime.agent(operation['agent'], db)
            if agent.get('executionMove', {}).get('id') == operation['id']:
                agent['executionMove']['phase'] = 'unknown'
                agent['error'] = message
                self.runtime.put(db, 'agents', agent)
            self.runtime.changed.set()

    def _export(self, operation: dict[str, Any]) -> dict[str, Any]:
        agent = self.runtime.agent(operation['agent'])
        if agent['epoch'] != operation['epoch'] or not agent.get('autoWake'):
            raise ValueError('The source agent was stopped before the move')
        native = self.runtime.connect_agent(agent)
        background = native.call('thread/backgroundTerminals/list', {'threadId': agent['threadId']}, timeout=20)
        if 'data' not in background or background.get('data') or background.get('nextCursor'):
            raise ValueError('Native background commands prevent the move')
        snapshot = native.call('thread/read', {'threadId': agent['threadId'], 'includeTurns': False}, timeout=20)['thread']
        if snapshot.get('status', {}).get('type') == 'active':
            raise ValueError('The source native turn is still active')
        native.call('thread/unsubscribe', {'threadId': agent['threadId']}, timeout=20)
        barrier: Future[None] = Future()
        native.after_events(lambda: barrier.set_result(None))
        barrier.result(timeout=20)
        self.runtime.loaded.discard(agent['id'])
        folder = self._folder(operation['id'])
        if agent.get('provider') == 'claude':
            exported = native.call('claude/moveExport', {'threadId': agent['threadId']}, timeout=20)
            export_claude(Path(exported['path']), folder / 'native.zip')
            bridge_session = exported['session']
            proof = bridge_session.get('moveProof')
            preflight_proof = operation['preflight']['claudeProof']
            if proof and any(proof.get(field) != preflight_proof.get(field) for field in ('snapshotHash', 'optionsHash')):
                raise ValueError('The saved Claude prompt snapshot or tool options changed after preflight')
            parameters = {'baseInstructions': '', 'developerInstructions': bridge_session.get('developerInstructions', ''),
                          'model': agent['model'], 'dynamicTools': bridge_session['dynamicTools']}
        else:
            path = Path(snapshot['path'])
            export_codex(self.runtime.accounts.home(agent.get('accountKey', 'default')), path, folder / 'native.zip')
            parameters = frozen_codex_parameters(path)
            bridge_session = None
        parent = self.runtime.agent(agent['parentId']) if agent.get('parentId') else agent
        link = {'id': identity(operation['id'], 'link'), 'home': self.service.server_id,
                'parent': parent['id'], 'parentEpoch': parent['epoch'], 'root': agent['rootId'],
                'workers': [agent['id']], 'concurrency': self.runtime.agent(parent['rootId'])['concurrency'],
                'yoloMode': agent.get('yoloMode', False), 'defaults': self.runtime.worker_defaults(parent)}
        if operation.get('canonical'):
            link = {**copy.deepcopy(operation['canonical']['link']), 'id': identity(operation['id'], 'link'), 'workers': [agent['id']]}
            link.pop('server', None)
            link.pop('side', None)
        archived_agent = copy.deepcopy(agent)
        workspace_archive = {field: value for field, value in agent.items()
            if field.startswith(('imageWorkspace', 'workerBase', 'worktree')) or field in {'cwd', 'sourceCwd'}}
        archived_agent.setdefault('executionArchives', []).append({'move': operation['id'], 'server': self.service.server_id,
            'threadId': agent['threadId'], 'accountKey': agent.get('accountKey', 'default'), 'workspace': workspace_archive, 'at': operation['created']})
        descriptor = {'move': operation['id'], 'fingerprint': operation['fingerprint'], 'acceptedResult': operation['result'], 'agent': archived_agent,
                      'workspaceArchive': workspace_archive,
                      'nativeThread': agent['threadId'], 'nativeParameters': parameters, 'bridgeSession': bridge_session,
                      'target': operation['target'], 'preflight': operation['preflight'],
                      'cwd': operation['args']['cwd'], 'note': operation['args'].get('note', ''), 'link': link,
                      'files': [{'name': name, 'size': (folder / name).stat().st_size,
                                 'sha256': hashlib.sha256((folder / name).read_bytes()).hexdigest()}
                                for name in ('native.zip',)]}
        if len(encoded(descriptor).encode()) > 240 * 1024:
            raise ValueError('The exact native settings exceed 240 KiB. No instructions were truncated')
        return descriptor

    def _run(self, key: str) -> None:
        operation: dict[str, Any] | None = None
        try:
            with self.runtime.read_db() as db:
                operation = self._get(db, key)
            if operation is None or operation['phase'] in _TERMINAL:
                return
            if operation['phase'] == 'waiting':
                descriptor = self._export(operation)
                operation.update(phase='exported', descriptor=descriptor)
                with self.runtime.db() as db:
                    self._save(db, operation)
            if operation['phase'] == 'exported':
                descriptor = operation['descriptor']
                self._exchange(operation['server'], 'move_begin', descriptor, identity(key, 'begin'))
                folder = self._folder(key)
                for entry in descriptor['files']:
                    with (folder / entry['name']).open('rb') as stream:
                        offset = 0
                        while chunk := stream.read(CHUNK):
                            if self.runtime.closed or time.time() > operation['deadline']:
                                raise RuntimeError('The move reached its transfer deadline. No input was repeated')
                            payload = {'move': key, 'name': entry['name'], 'offset': offset,
                                       'data': base64.b64encode(chunk).decode(), 'sha256': hashlib.sha256(chunk).hexdigest()}
                            self._exchange(operation['server'], 'move_chunk', payload,
                                           identity(key, entry['name'], str(offset)))
                            offset += len(chunk)
                operation['phase'] = 'importing'
                with self.runtime.db() as db:
                    self._save(db, operation)
            if operation['phase'] == 'importing':
                self._exchange(operation['server'], 'move_prepare', {'move': key}, identity(key, 'prepare'))
                operation['phase'] = 'prepared'
                with self.runtime.db() as db:
                    self._save(db, operation)
            if operation['phase'] == 'prepared':
                descriptor = operation['descriptor']
                origin = descriptor['agent'].get('remoteOrigin')
                if origin and operation['server'] not in {self.service.server_id, origin['home']}:
                    self._exchange(origin['home'], 'move_home_relocate',
                        {'agent': operation['agent'], 'epoch': operation['epoch'], 'link': origin['link'],
                         'controlEpoch': descriptor['agent'].get('remoteControlEpoch', 0),
                         'move': key, 'target': operation['server'], 'newLink': descriptor['link']}, identity(key, 'home-relocate'))
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(operation['agent'], db)
                    if agent['epoch'] != operation['epoch'] or not agent['autoWake']:
                        raise ValueError('The source agent was stopped before move activation')
                    agent['executionArchives'] = copy.deepcopy(operation['descriptor']['agent']['executionArchives'])
                    if operation['server'] != self.service.server_id:
                        link = operation['descriptor']['link']
                        if link['home'] == self.service.server_id:
                            db.execute('INSERT OR IGNORE INTO runtime_server_links VALUES (?,?)',
                                       (link['id'], encoded({**link, 'side': 'home', 'server': operation['server']})))
                        agent['remoteWorker'] = {'server': operation['server'], 'link': link['id']}
                        agent['movedTo'] = {'server': operation['server'], 'agentId': agent['id'], 'move': key}
                        agent['threadId'] = None
                        agent['inFlight'] = False
                        agent['status'] = 'waiting'
                    self.runtime.put(db, 'agents', agent)
                    operation['phase'] = 'redirected'
                    self._save(db, operation)
                    self.service.queue(db, operation['server'], 'move_activate', {'move': key}, identity(key, 'activate'))
            if operation['phase'] == 'redirected':
                self._exchange(operation['server'], 'move_activate', {'move': key}, identity(key, 'activate'))
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(operation['agent'], db)
                    agent.pop('executionMove', None)
                    if operation['server'] != self.service.server_id:
                        for row in db.execute("SELECT * FROM runtime_events WHERE agent=? AND status='pending' AND epoch=?",
                                              (agent['id'], agent['epoch'])).fetchall():
                            self.service.event(db, agent, row['kind'], row['text'], row['id'])
                            db.execute("UPDATE runtime_events SET status='stored_only' WHERE id=?", (row['id'],))
                    self.runtime.put(db, 'agents', agent)
                    operation.update(phase='complete', finished=time.time())
                    self._save(db, operation)
                    self.runtime.changed.set()
        except Exception as error:
            self.service._unknown_diagnostic(key, 'move', error)
            if operation is not None:
                # Export/import errors can follow an atomic file publication or
                # native resume. Retain the frozen source and never start input.
                self._hold(operation, 'The move outcome is unknown. Read the saved request. No target input was repeated')
        finally:
            with self.lock:
                self.running.discard(key)

    def _home_owner(self, db: Any, principal: str, payload: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
        link = self.service.link(db, payload['link'])
        agent = self.runtime.agent(payload['agent'], db)
        if (link['side'] != 'home' or link['server'] != principal or agent['id'] not in link['workers']
                or agent.get('remoteWorker') != {'server': principal, 'link': link['id']}):
            raise PermissionError('The move source no longer owns this execution route')
        parent = self.runtime.agent(link['parent'], db)
        if (not agent['autoWake'] or not parent['autoWake'] or parent['epoch'] != link['parentEpoch']
                or payload['epoch'] < agent.get('remoteEpoch', 0) or payload['controlEpoch'] != agent['epoch']):
            raise ValueError('The home team was stopped or its execution changed')
        return agent, link

    def _route(self, principal: str, action: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
        if action == 'move_route_ready':
            with self.runtime.read_db() as db:
                operation = self._get(db, identity(payload['move'], 'target'))
                if (not operation or operation['phase'] not in {'ready', 'active'}
                        or operation['descriptor']['link']['home'] != principal
                        or operation['descriptor']['link']['id'] != payload['link']
                        or operation['descriptor']['agent']['id'] != payload['agent']):
                    raise PermissionError('The target has no confirmed import for this home route')
            return {'ready': True}
        if action == 'move_home_event':
            with self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(payload['agent'], db)
                alias = {'server': principal, 'link': payload['link']}
                if alias not in agent.get('executionRouteAliases', []):
                    raise PermissionError('The event source is outside this execution route')
                if payload['kind'] not in {'child_result', 'child_stopped', 'agent_message', 'work_review', 'followup', 'user'}:
                    raise ValueError('Unsupported move event kind')
                self.runtime.enqueue_recovery_event(db, agent, payload['kind'], payload['text'], key)
                return {'eventId': key}
        if action == 'move_route_status':
            with self.runtime.read_db() as db:
                agent = self.runtime.agent(payload['agent'], db)
                if (agent.get('remoteWorker') != {'server': principal, 'link': payload['link']}
                        or agent.get('movedTo', {}).get('move') != payload['move'] or not agent['autoWake']
                        or payload['controlEpoch'] != agent['epoch']):
                    raise PermissionError('The home server has not confirmed this execution route')
            return {'redirected': True}
        with self.runtime.read_db() as db:
            agent, old_link = self._home_owner(db, principal, payload)
        target = payload['target']
        if target != self.service.server_id and target not in {row['id'] for row in self.service.transport.servers()}:
            raise ValueError('Pair the original home server with the target before a move')
        if action == 'move_home_validate':
            return {'link': old_link}
        new_link = payload['newLink']
        expected = {**old_link, 'id': identity(payload['move'], 'link'), 'workers': [agent['id']]}
        expected.pop('side', None)
        expected.pop('server', None)
        if new_link != expected or target in {principal, self.service.server_id}:
            raise PermissionError('The new execution route does not match the home team')
        self._exchange(target, 'move_route_ready', {'agent': agent['id'], 'move': payload['move'], 'link': new_link['id']},
                       identity(payload['move'], 'route-ready'))
        with self.runtime.lock, self.runtime.db() as db:
            agent, _old = self._home_owner(db, principal, payload)
            aliases = agent.setdefault('executionRouteAliases', [])
            alias = {'server': principal, 'link': old_link['id']}
            if alias not in aliases:
                aliases.append(alias)
            db.execute('INSERT INTO runtime_server_links VALUES (?,?)',
                       (new_link['id'], encoded({**new_link, 'side': 'home', 'server': target})))
            for field in ('remoteStateSequence', 'remoteAdmissionRequest', 'remoteReservation', 'remoteLastAdmission', 'remoteAdmission'):
                agent.pop(field, None)
            agent.update(remoteWorker={'server': target, 'link': new_link['id']}, remoteEpoch=payload['epoch'],
                         movedTo={'server': target, 'agentId': agent['id'], 'move': payload['move']}, inFlight=False, status='waiting')
            self.runtime.put(db, 'agents', agent)
        return {'redirected': True}

    def receive(self, principal: str, action: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
        if action in {'move_home_validate', 'move_home_relocate', 'move_home_event', 'move_route_ready', 'move_route_status'}:
            return self._route(principal, action, payload, key)
        if action == 'move_validate':
            if payload['routeHome'] != self.service.server_id and payload['routeHome'] not in {row['id'] for row in self.service.transport.servers()}:
                raise ValueError('Pair the target with the original home server before the move')
            cwd = Path(payload['cwd'])
            if not cwd.is_absolute() or not cwd.is_dir():
                raise ValueError('The target cwd must be an existing absolute folder')
            with self.runtime.read_db() as db:
                existing = db.execute('SELECT record FROM runtime_agents WHERE id=?', (payload['agentId'],)).fetchone()
            if existing and payload['sourceServer'] != self.service.server_id:
                previous = json.loads(existing[0])
                if previous.get('remoteOrigin') and previous['remoteOrigin']['home'] != payload['routeHome']:
                    raise ValueError('The existing target identity belongs to another home team')
                if not previous.get('movedTo') or previous['movedTo']['server'] != payload['sourceServer'] or previous.get('inFlight'):
                    raise ValueError('The target already has this active Studio identity')
            account, warning = self._select_account(payload)
            with self.runtime.read_db() as db:
                self._native_owner(db, payload['agentId'], payload['nativeThread'], account['id'], payload['provider'])
            temporary = {'provider': payload['provider'], 'accountKey': account['id'], 'id': 'move-preflight'}
            capabilities = self._capabilities(temporary)
            if capabilities.get('platform') != payload['capabilities'].get('platform'):
                raise ValueError('The source and target OS differ. The builtin tool catalog cannot be proved identical')
            if capabilities != payload['capabilities'] and payload['provider'] == 'claude':
                source = payload['capabilities']
                raise ValueError(f"Claude versions differ. Source CLI {source['cli']}, SDK {source['sdk']}; target CLI {capabilities['cli']}, SDK {capabilities['sdk']}. Update the CLI and SDK on the target to the source versions before a move")
            if capabilities != payload['capabilities']:
                raise ValueError('The native version, platform, or MCP tools differ. The exact prompt prefix cannot be preserved')
            account_identity = self._account_identity(account)
            if payload['provider'] == 'claude' and account_identity != payload['accountIdentity']:
                raise ValueError('Claude moves require the same provider account and organization on the target')
            cache_proof = 'exact_native_history_and_catalog'
            if payload['provider'] == 'claude':
                proof = self.runtime.connect(account['id']).call('claude/movePreflight',
                    {'proof': payload['claudeProof'], 'cwd': payload['cwd'], 'model': payload['model']}, timeout=25)
                cache_proof = proof.get('proofMethod', 'saved_snapshot_and_identical_studio_options')
            catalog = self.runtime.catalog(account['id'])
            if not any(row.get('model') == payload['model'] for row in catalog.get('data', [])):
                raise ValueError('The target account does not offer the source model')
            return {'accountKey': account['id'], 'accountIdentity': account_identity, 'warning': warning, 'cacheProof': cache_proof}
        move = payload.get('move')
        if not isinstance(move, str):
            raise ValueError('Supply a move identity')
        uuid.UUID(move)
        with self.runtime.lock, self.runtime.db() as db:
            operation = self._get(db, identity(move, 'target'))
            if action == 'move_begin':
                if payload.get('link', {}).get('home') != payload.get('preflight', {}).get('routeHome'):
                    raise PermissionError('The source move identity differs from the paired server')
                if payload['preflight']['sourceServer'] != principal:
                    raise PermissionError('The native identity source differs from the paired principal')
                if operation:
                    if operation['principal'] != principal or operation['descriptor'] != payload:
                        raise PermissionError('The move identity belongs to another source or content')
                    return {'phase': operation['phase']}
                self._descriptor_identity(db, payload)
                existing = db.execute('SELECT record FROM runtime_agents WHERE id=?', (payload['agent']['id'],)).fetchone()
                if existing and principal != self.service.server_id:
                    previous = json.loads(existing[0])
                    if (not previous.get('movedTo') or previous['movedTo']['server'] != principal
                            or previous.get('inFlight') or previous.get('turnId')):
                        raise ValueError('The destination already has this Studio identity')
                operation = {'id': identity(move, 'target'), 'move': move, 'side': 'target', 'principal': principal,
                             'descriptor': copy.deepcopy(payload), 'phase': 'receiving', 'created': time.time()}
                self._save(db, operation)
                return {'phase': 'receiving'}
            if operation is None and action == 'move_status':
                return {'phase': 'not_started'}
            if operation is None or operation['principal'] != principal:
                raise PermissionError('The move belongs to another source server')
            if action == 'move_status':
                return {'phase': operation['phase'], 'firstTurnCache': operation.get('firstTurnCache')}
        if action == 'move_chunk':
            if operation['phase'] != 'receiving':
                raise ValueError('The move no longer accepts history chunks')
            entry = next((entry for entry in operation['descriptor']['files'] if entry['name'] == payload.get('name')), None)
            if entry is None or entry['name'] != 'native.zip':
                raise ValueError('The history chunk file is invalid')
            data = base64.b64decode(payload['data'], validate=True)
            offset = payload['offset']
            if (not isinstance(offset, int) or isinstance(offset, bool) or offset < 0 or len(data) > CHUNK
                    or offset + len(data) > entry['size'] or entry['size'] > MAX_HISTORY_BYTES + 1024 * 1024
                    or hashlib.sha256(data).hexdigest() != payload['sha256']):
                raise ValueError('The history chunk has an invalid boundary or checksum')
            path = self._folder(move) / ('incoming-' + entry['name'])
            with self.lock:
                if path.exists() and path.stat().st_size > offset:
                    with path.open('rb') as stream:
                        stream.seek(offset)
                        if stream.read(len(data)) != data:
                            raise ValueError('This history chunk has different bytes')
                    return {'offset': offset + len(data)}
                if offset != (path.stat().st_size if path.exists() else 0):
                    raise ValueError('The history chunks are not in order')
                with path.open('ab') as stream:
                    path.chmod(0o600)
                    stream.write(data)
                    stream.flush()
                    __import__('os').fsync(stream.fileno())
            return {'offset': offset + len(data)}
        if action == 'move_prepare':
            return self._prepare(operation)
        if action == 'move_activate':
            return self._activate(operation)
        raise ValueError('Unknown move operation')

    def _prepare(self, operation: dict[str, Any]) -> dict[str, Any]:
        if operation['phase'] in {'ready', 'active'}:
            return {'phase': operation['phase']}
        if operation['phase'] != 'receiving':
            raise RuntimeError('The native import outcome is unknown. It was not repeated')
        descriptor = operation['descriptor']
        with self.runtime.read_db() as db:
            self._descriptor_identity(db, descriptor)
        folder = self._folder(operation['move'])
        for entry in descriptor['files']:
            path = folder / ('incoming-' + entry['name'])
            if not path.is_file() or path.stat().st_size != entry['size'] or hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']:
                raise ValueError('The complete history checksum does not match')
        account, warning = self._select_account({**descriptor['preflight'], 'account_key': descriptor['target']['accountKey']})
        if self._account_identity(account) != descriptor['target']['accountIdentity']:
            raise ValueError('The target account identity changed after preflight')
        with self.runtime.read_db() as db:
            existing = db.execute('SELECT record FROM runtime_agents WHERE id=?', (descriptor['agent']['id'],)).fetchone()
            previous_home = json.loads(existing[0]) if existing else None
        returning_home = bool(previous_home and previous_home.get('movedTo') and not previous_home.get('remoteOrigin'))
        if returning_home and previous_home and (not previous_home['autoWake']
                or previous_home['epoch'] != descriptor['agent'].get('remoteControlEpoch', descriptor['agent']['epoch'])):
            raise ValueError('The original home agent was stopped or changed before import')
        agent = copy.deepcopy(descriptor['agent'])
        for field in list(agent):
            if field.startswith(('imageWorkspace', 'workerBase', 'worktree', 'remote')) or field in {'startAttempt', 'executionMove', 'movedTo'}:
                agent.pop(field, None)
        agent.update(accountKey=account['id'], cwd=descriptor['cwd'], sourceCwd=descriptor['cwd'],
                     environment='host', worktree=False, worktreeReady=True, inFlight=False, turnId=None,
                     activeTools=[], activity=None, error=None, autoWake=False, status='paused', threadId=descriptor['nativeThread'],
                     movedFrom={'server': operation['principal'], 'move': operation['move']},
                     frozenNativeParams=copy.deepcopy(descriptor['nativeParameters']))
        agent.update(moveImportPending=True,
                     executionMove={'id': operation['move'], 'phase': 'ready', 'server': self.service.server_id},
                     lastCompletedTurn=None, lastCompletedTurnStatus=None)
        if self._capabilities(agent) != descriptor['preflight']['capabilities']:
            raise ValueError('The native tools or version changed after preflight')
        if agent.get('provider') == 'claude':
            params = {'session': descriptor['bridgeSession'], 'cwd': agent['cwd'], 'path': str(folder / 'incoming-native.zip')}
            method = 'claude/moveImport'
        else:
            source = unpack_codex(folder / 'incoming-native.zip', folder / 'staging',
                                  descriptor['nativeThread'], descriptor['nativeParameters'])
            from codex_account_transfer import transfer_store
            copied = transfer_store(self.runtime).copy_history(folder / 'staging',
                self.runtime.accounts.home(account['id']) / 'sessions' / '.studio-moves' / operation['move'], source)
            params = self.runtime.new_thread_params(agent)
            params.pop('dynamicTools', None)
            params.update(threadId=descriptor['nativeThread'], path=str(copied), excludeTurns=True)
            method = 'thread/resume'
        with self.runtime.db() as db:
            operation['phase'] = 'native_submitted'
            self._save(db, operation)
        # A saved submission is never repeated after a missing native receipt.
        native = self.runtime.connect_agent(agent)
        if (agent.get('provider') != 'claude' and previous_home
                and previous_home.get('accountKey', 'default') == account['id']):
            extend_codex_history(native, self.runtime.accounts.home(account['id']), descriptor['nativeThread'],
                                 copied, folder / 'previous-native.jsonl')
            params.pop('path')
        result = native.call(method, params, timeout=30)
        if result['thread']['id'] != descriptor['nativeThread']:
            raise RuntimeError('The native import returned a different conversation identity')
        if not agent.get('isLead') and operation['principal'] != self.service.server_id and not returning_home:
            anchor_id = identity(operation['move'], 'anchor')
            anchor = self.runtime.create({'id': anchor_id, 'name': 'Moved parent ' + descriptor['link']['parent'][:8],
                                         'prompt': '', 'cwd': agent['cwd'], 'concurrency': descriptor['link']['concurrency']}, draft=True)
            anchor['remoteAnchor'] = {'home': descriptor['link']['home'], 'link': descriptor['link']['id']}
            agent.update(parentId=anchor_id, rootId=anchor_id)
        else:
            anchor = None
        if returning_home and previous_home:
            agent.update(parentId=previous_home['parentId'], rootId=previous_home['rootId'])
        with self.runtime.lock, self.runtime.db() as db:
            if returning_home and previous_home:
                current_home = self.runtime.agent(agent['id'], db)
                if current_home['epoch'] != previous_home['epoch'] or not current_home['autoWake']:
                    raise ValueError('The home agent was stopped during native import. No target input was started')
                retired_link = self.service.link(db, previous_home['remoteWorker']['link'])
                aliases = copy.deepcopy(previous_home.get('executionRouteAliases', []))
                aliases.append({'server': operation['principal'], 'link': retired_link['id']})
                agent['executionRouteAliases'] = aliases
            if operation['principal'] != self.service.server_id:
                if not returning_home:
                    agent['remoteOrigin'] = {'home': descriptor['link']['home'], 'link': descriptor['link']['id']}
                    agent['remoteControlEpoch'] = descriptor['agent'].get('remoteControlEpoch', descriptor['agent']['epoch'])
                db.execute('INSERT OR IGNORE INTO runtime_server_links VALUES (?,?)',
                           (descriptor['link']['id'], encoded({**descriptor['link'], 'side': 'remote'})))
            if anchor:
                self.runtime.put(db, 'agents', anchor)
            if operation['principal'] == self.service.server_id:
                current = self.runtime.agent(agent['id'], db)
                for field in list(current):
                    if field.startswith(('imageWorkspace', 'workerBase', 'worktree')):
                        current.pop(field, None)
                current.update(cwd=agent['cwd'], sourceCwd=agent['cwd'], worktree=False, worktreeReady=True,
                               frozenNativeParams=agent['frozenNativeParams'], movedFrom=agent['movedFrom'])
                agent = current
            self.runtime.put(db, 'agents', agent)
            operation.update(phase='ready', warning=warning, preparedEpoch=agent['epoch'])
            self._save(db, operation)
            self.runtime.loaded.add(agent['id'])
        return {'phase': 'ready'}

    def _activate(self, operation: dict[str, Any]) -> dict[str, Any]:
        descriptor = operation['descriptor']
        home = descriptor['link']['home']
        if home != self.service.server_id and operation['principal'] != self.service.server_id:
            self._exchange(home, 'move_route_status', {'agent': descriptor['agent']['id'], 'move': operation['move'],
                'link': descriptor['link']['id'], 'controlEpoch': descriptor['agent'].get('remoteControlEpoch', descriptor['agent']['epoch'])}, identity(operation['move'], 'route-status'))
        with self.runtime.lock, self.runtime.db() as db:
            current = self._get(db, operation['id'])
            if current and current['phase'] == 'active':
                return {'phase': 'active', 'agentId': current['descriptor']['agent']['id']}
            if not current or current['phase'] != 'ready':
                raise RuntimeError('The native import is not confirmed. No target turn was started')
            descriptor = current['descriptor']
            agent = self.runtime.agent(descriptor['agent']['id'], db)
            if agent['epoch'] != current['preparedEpoch'] or agent.get('deletedAt') or agent.get('inFlight'):
                raise ValueError('The destination agent was stopped or changed before activation')
            agent.update(autoWake=True, status='queued', inFlight=False)
            agent.pop('executionMove', None)
            agent.pop('moveImportPending', None)
            self.runtime.put(db, 'agents', agent)
            text = '[Studio move] Moved from ' + operation['principal'] + '. Current server: ' + self.service.server_id + '. Current cwd: ' + agent['cwd'] + '. '
            text += 'Continue the existing task. Historical instructions and tool descriptions retain their original prefix. Use this cwd for new work.'
            if descriptor.get('note'):
                text += '\n' + descriptor['note']
            if current.get('warning'):
                text += '\n' + current['warning']
            self.runtime.enqueue(db, agent, 'followup', text, identity(operation['move'], 'handoff'))
            current.update(phase='active', activated=time.time())
            self._save(db, current)
            self.runtime.changed.set()
        return {'phase': 'active', 'agentId': agent['id']}
