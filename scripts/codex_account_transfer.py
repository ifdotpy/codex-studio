"""Move managed identities at native idle boundaries, without replaying model input."""
import concurrent.futures
import copy
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import threading
import time
import uuid

TERMINAL = {'completed', 'cancelled'}
MEMBER_TERMINAL = {'completed', 'left'}
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
            db.execute("CREATE INDEX IF NOT EXISTS runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'))")
            for op in rt.records(db, 'account_transfers'):
                if op.get('status') in TERMINAL:
                    continue
                dirty = False
                for member in op['members'].values():
                    if member['phase'] == 'reading':
                        member['phase'] = 'lazy' if member.get('lazy') else 'waiting'
                        dirty = True
                    elif member['phase'] in {'submitted', 'lazy_submitted'}:
                        member.update(phase='unknown', error='The server restarted before the transfer receipt arrived. No request was repeated.')
                        dirty = True
                    elif member['phase'] == 'interrupting':
                        member.update(phase='blocked', interruptOutcome='unknown',
                                      error='The server restarted before the turn interrupt receipt arrived. No request was repeated.')
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
        # A finished earlier transfer does not own the summary. A lead that is
        # already on the target has no exact pointer to the new operation.
        if current.get('id') not in {None, op['id']} and lead.get('accountTransferId') != op['id']:
            if current.get('status') not in TERMINAL or not self.newer(db, op, current['id']):
                return
        members = list(op['members'].values())
        lead['accountTransfer'] = {k: op.get(k) for k in ('id', 'targetAccountKey', 'status', 'updated', 'scope', 'finishHistory')}
        lead['accountTransfer'].update(total=len(members), completed=sum(m['phase'] in MEMBER_TERMINAL for m in members),
            moved=sum(m.get('lazy') or m['phase'] == 'completed' for m in members),
            nativeHistoryPending=sum(bool(m.get('lazy') and m['phase'] != 'completed') for m in members),
            movingNow=sum(m['phase'] in {'reading', 'submitted', 'lazy_submitted', 'interrupting'} for m in members),
            # Old clients also show automatic progress without a new button.
            canFinishHistory=False, finishHistory=True,
            interrupted=[{'id': aid, 'name': m.get('name'), 'reason': m.get('interruptReason')}
                         for aid, m in op['members'].items() if m.get('interruptReason')],
            leftOnSource=[{'id': aid, 'name': m.get('name'), 'provider': m.get('provider'),
                           'reason': m.get('reason')} for aid, m in op['members'].items()
                          if m['phase'] == 'left'],
            blocked=[{'id': aid, 'name': m.get('name'), 'reason': m.get('error')}
                     for aid, m in op['members'].items() if m['phase'] in {'blocked', 'unknown'}],
            waitingCount=sum(m['phase'] not in MEMBER_TERMINAL and m['phase'] not in {'blocked', 'unknown', 'lazy', 'lazy_submitted'}
                             for m in members),
            waiting=next((m.get('error') or m.get('waiting') for m in members if m.get('error') or m.get('waiting')), None),
            needsAttention=any(m['phase'] in {'blocked', 'unknown'} for m in members),
            canRetry=any(m['phase'] == 'blocked' and not m.get('archiveInvalidated') for m in members))
        self.rt.put(db, 'agents', lead)

    @staticmethod
    def already_committed(op, m, a):
        thread = ((m.get('result') or {}).get('thread') or {}).get('id')
        return bool(thread and a.get('threadId') == thread and not a.get('deletedAt')
                    and a.get('accountKey', 'default') == op['targetAccountKey']
                    and a.get('accountTransferId') in {None, op['id']})

    def newer(self, db, op, other_id):
        row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (other_id,)).fetchone()
        return row is None or op.get('created', 0) > json.loads(row[0]).get('created', 0)

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
        current = next((row for row in rows if agent['model'] in (row.get('model'), row.get('resolvedModel'))), None)
        if changed or current is None:
            from codex_runtime import DEFAULT_LEAD_MODEL
            default = 'default' if provider == 'claude' else DEFAULT_LEAD_MODEL
            current = next((row for row in rows if row.get('isDefault')), None) or next(
                (row for row in rows if row.get('model') == default), None)
            if current is None:
                raise ValueError('The destination account has no available default model')
        model = (agent['model'] if not changed and current.get('resolvedModel') == agent['model']
                 else current['model'])
        fast = bool(agent.get('fastMode', False)) and any(
            tier.get('id') == 'priority' for tier in current.get('serviceTiers', []))
        source_effort = None if changed else agent.get('effort')
        effort, native = rt.validate_execution(catalog, model, source_effort, fast,
                                               fallback_effort=changed or source_effort is None)
        # A provider change clears the source-specific override. Keep the
        # destination's resolved effort in nativeEffort for the first request.
        from codex_daybreak import resolve_program
        daybreak = False if changed else agent.get("daybreakEnabled", False)
        program = resolve_program(catalog, model, daybreak, provider)
        resolved = dict(provider=provider, model=model, effort=effort, nativeEffort=native, fastMode=fast,
                        daybreakEnabled=daybreak, cyberAccessProgram=program)
        # New team workers use the destination. A provider switch needs native
        # defaults for that provider; same-provider transfers preserve choices.
        worker_defaults = rt.worker_defaults(agent)
        if changed:
            worker_defaults.update(model=None, effort='medium', fastMode=False,
                                   daybreakEnabled=False, cyberAccessProgram='standard')
        worker_defaults['accountKey'] = target
        resolved['workerDefaults'] = worker_defaults
        if agent.get('pendingSettings'):
            if agent.get('pendingSettingsAccountKey', agent.get('accountKey', 'default')) != agent.get('accountKey', 'default'):
                raise ValueError('Queued settings belong to another account. Save them again before transfer')
            if changed:
                resolved.update(pendingSettings=None, pendingSettingsAccountKey=None)
            else:
                pending = agent['pendingSettings']
                pending_effort, pending_native = rt.validate_execution(
                    catalog, pending['model'], pending.get('effort'), pending.get('fastMode', False))
                pending_program = resolve_program(catalog, pending['model'], pending.get('daybreakEnabled', False), provider)
                resolved.update(pendingSettings={**pending, 'effort': pending_effort, 'nativeEffort': pending_native,
                                                'cyberAccessProgram': pending_program},
                                pendingSettingsAccountKey=target)
        return resolved

    def request(self, key, target, request_id, scope='team'):
        if not isinstance(request_id, str):
            raise ValueError('Supply a transfer request id')
        uuid.UUID(request_id)
        if scope not in {'team', 'subagents'}:
            raise ValueError('Choose a team or subagents account transfer')
        rt = self.rt
        rt.accounts.get(target)
        with rt.lock, rt.db() as db:
            lead = rt.checked_actor(db, key)
            if not lead.get('isLead'):
                raise ValueError('Choose the orchestrator to transfer its team')
            alias = next(((op, op.get('requests', {}).get(request_id))
                          for op in rt.records(db, 'account_transfers')
                          if request_id in op.get('requests', {})), None)
            if alias:
                op, receipt = alias
                if (receipt['leadId'] != key or receipt['targetAccountKey'] != target
                        or receipt.get('scope', 'team') != scope):
                    raise ValueError('This transfer id has different content')
                return op
            row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (request_id,)).fetchone()
            if row:
                op = json.loads(row[0])
                if (op['leadId'] != key or op['targetAccountKey'] != target
                        or op.get('scope', 'team') != scope):
                    raise ValueError('This transfer id has different content')
                return op
            old = lead.get('accountTransfer') or {}
            if old and old['status'] not in TERMINAL:
                if old['targetAccountKey'] == target and old.get('scope', 'team') == scope:
                    op = self.get(db, old['id'])
                    members = rt.team_agents(db, key, include_id=key)
                    op.setdefault('targetProvider', rt.accounts.get(target).get('provider', 'codex'))
                    self.adopt(db, op, members, include_later=True)
                    if lead.get('accountKey', 'default') == target:
                        defaults = copy.deepcopy(lead.get('workerDefaults') or rt.worker_defaults(lead))
                        defaults['accountKey'] = target
                        lead['workerDefaults'] = defaults
                        rt.put(db, 'agents', lead)
                    op.setdefault('requests', {})[request_id] = {
                        'leadId': key, 'targetAccountKey': target, 'scope': scope}
                    if all(m['phase'] in MEMBER_TERMINAL for m in op['members'].values()):
                        op['status'] = 'completed'
                    self.save(db, op)
                    rt.changed.set()
                    return op
                raise ValueError('Finish or cancel the current transfer first')
            target_account = rt.accounts.get(target)
            if target_account.get("deleted"):
                raise ValueError("This account was deleted. Select another destination")
            if target_account.get("disconnected"):
                raise ValueError("Reconnect this account before transferring a team to it")
            members = rt.team_agents(db, key, include_id=key)
            if any(agent.get('environment') == 'linux' and not agent.get('deletedAt')
                   and agent.get('accountKey', 'default') != target for agent in members):
                raise ValueError('Archive the Linux VM workers before changing the team account. Their native account identity stays fixed.')
            target_provider = rt.accounts.get(target).get('provider', 'codex')
            op = {'id': request_id, 'leadId': key, 'targetAccountKey': target,
                  'status': 'pending', 'scope': scope, 'created': time.time(), 'members': {},
                  'requests': {request_id: {'leadId': key, 'targetAccountKey': target, 'scope': scope}}}
            # Subagent-only changes take effect now; team transfers set this as the lead moves.
            root = rt.agent(key, db)
            op['targetProvider'] = target_provider
            self.adopt(db, op, members)
            if scope == 'subagents' or root.get('accountKey', 'default') == target:
                defaults = copy.deepcopy(root.get('workerDefaults') or rt.worker_defaults(root))
                defaults['accountKey'] = target
                root['workerDefaults'] = defaults
                rt.put(db, 'agents', root)
            if all(m['phase'] in MEMBER_TERMINAL for m in op['members'].values()):
                op["status"] = "completed"
            self.save(db, op)
        rt.changed.set()
        return op

    def adopt(self, db, op, agents, include_later=False):
        destination_catalog = None
        destination_catalog_checked = False
        destination_catalog_error = None
        for a in agents:
            # This is a snapshot. A later request can adopt descendants created afterwards.
            if (a.get('deletedAt') or (a['id'] != op['leadId'] and a.get('rootId') != op['leadId'])
                    or a['id'] in op['members']):
                continue
            if not include_later and op.get('created') and a.get('created', 0) > op['created']:
                continue
            if op.get('scope', 'team') == 'subagents' and a['id'] == op['leadId']:
                continue
            provider = self.rt.accounts.get(a.get('accountKey', 'default')).get('provider', 'codex')
            if (a['id'] != op['leadId']
                    and provider != op.get('targetProvider', self.rt.accounts.get(op['targetAccountKey']).get('provider', 'codex'))):
                op['members'][a['id']] = {'phase': 'left', 'sourceAccountKey': a.get('accountKey', 'default'),
                    'sourceThreadId': a.get('threadId'), 'provider': provider, 'name': a.get('name'),
                    'reason': f'Uses {provider}; the destination account uses {op["targetProvider"]}'}
                continue
            self.check_destination(a, op['targetAccountKey'], db)
            done = a.get('accountKey', 'default') == op['targetAccountKey']
            active = bool(a.get('inFlight') or a.get('status') in ACTIVE)
            lazy = not done and not active
            source_account = a.get('accountKey', 'default')
            source_thread = a.get('threadId')
            resolved = None
            validation_error = None
            source_settings = self.settings_snapshot(a) if lazy else None
            source_pending = copy.deepcopy(a.get('pendingSettings'))
            source_pending_account = a.get('pendingSettingsAccountKey')
            if lazy:
                try:
                    if destination_catalog is None:
                        if not destination_catalog_checked:
                            destination_catalog_checked = True
                            try:
                                destination_catalog = self.rt.catalog(op['targetAccountKey'])
                            except Exception as error:
                                destination_catalog_error = str(error)
                        if destination_catalog_error:
                            raise ValueError(destination_catalog_error)
                    resolved = self.destination_settings(a, op['targetAccountKey'], destination_catalog)
                    current = self.rt.agent(a['id'], db)
                    if (self.settings_snapshot(current) != source_settings
                            or current.get('pendingSettings') != source_pending
                            or current.get('pendingSettingsAccountKey') != source_pending_account):
                        raise TransferSettingsConflict(
                            'Agent settings changed during transfer. The newer choice is preserved')
                except Exception as error:
                    validation_error = str(error)
            op['members'][a['id']] = {'phase': 'completed' if done else
                                      ('blocked' if validation_error else ('waiting' if active else 'lazy')),
                'sourceAccountKey': a.get('accountKey', 'default'), 'sourceThreadId': a.get('threadId'),
                'name': a.get('name'), 'provider': provider, 'lazy': lazy,
                **({'targetSettings': resolved} if resolved is not None else {}),
                **({'error': validation_error} if validation_error else {}),
                'pendingSettings': copy.deepcopy(a.get('pendingSettings')),
                'sourcePendingSettings': copy.deepcopy(a.get('pendingSettings')),
                'sourceClaudeOptions': copy.deepcopy(a.get('claudeOptions')),
                **({'continueAfterTransfer': bool(a.get('autoWake'))} if active else {})}
            if not done and not validation_error:
                a['accountTransferId'] = op['id']
                if lazy:
                    a['lazyAccountTransfer'] = {'id': op['id'], 'sourceAccountKey': source_account,
                                                'sourceThreadId': source_thread,
                                                'sourceState': {field: copy.deepcopy(a.get(field)) for field in (
                                                    'provider', 'model', 'effort', 'nativeEffort', 'fastMode',
                                                    'daybreakEnabled', 'cyberAccessProgram', 'workerDefaults',
                                                    'pendingSettings', 'pendingSettingsAccountKey', 'claudeOptions',
                                                    'executionSettingsAccountKey', 'status', 'error',
                                                    'nativeFailureHold')}}
                    a['accountKey'] = op['targetAccountKey']
                    if provider != op.get('targetProvider', provider):
                        from codex_runtime import DEFAULT_LEAD_MODEL
                        a.update(provider=op['targetProvider'],
                                 model='default' if op['targetProvider'] == 'claude' else DEFAULT_LEAD_MODEL,
                                 effort=None, nativeEffort='medium', fastMode=False, daybreakEnabled=False,
                                 cyberAccessProgram='standard')
                        a.pop('claudeOptions', None)
                        a.pop('pendingSettings', None)
                        a.pop('pendingSettingsAccountKey', None)
                    defaults = copy.deepcopy(a.get('workerDefaults') or self.rt.worker_defaults(a))
                    if provider != op.get('targetProvider', provider):
                        defaults.update(model=None, effort='medium', fastMode=False,
                                        daybreakEnabled=False, cyberAccessProgram='standard')
                    defaults['accountKey'] = op['targetAccountKey']
                    a['workerDefaults'] = defaults
                    a['pendingSettingsAccountKey'] = op['targetAccountKey'] if a.get('pendingSettings') else None
                    if not a.get('pendingSettings'):
                        a['executionSettingsAccountKey'] = source_account
                self.rt.put(db, 'agents', a)

    def action(self, key, action):
        if action not in {'cancel', 'retry', 'finish_history'}:
            raise ValueError('Choose cancel, retry, or finish_history')
        rt = self.rt
        with rt.lock, rt.db() as db:
            op = self.get(db, key)
            if op['status'] in TERMINAL:
                return op
            if action == 'finish_history':
                # Persist only the intent. The scheduler uses the original
                # member identities and receipt guards without a model turn.
                op['finishHistory'] = True
            elif action == 'cancel':
                lazy_active = [m for m in op['members'].values() if m.get('lazy')
                               and m['phase'] not in MEMBER_TERMINAL and m['phase'] != 'lazy']
                if lazy_active:
                    raise ValueError('A native history move has started. Resolve its receipt before cancelling.')
                unresolved = [aid for aid, member in op['members'].items()
                              if member.get('interruptSubmittedAt')
                              and member.get('interruptOutcome') != 'acknowledged']
                if unresolved:
                    raise ValueError('Retry the unresolved turn interruption before cancelling this transfer')
                active_interrupts = [aid for aid, member in op['members'].items()
                                     if member.get('interruptOutcome') == 'acknowledged'
                                     and rt.agent(aid, db).get('inFlight')]
                if active_interrupts:
                    raise ValueError('Wait for the active turn interruption to finish before cancelling this transfer')
                op['status'] = 'cancelled'
                for aid in op['members']:
                    a = rt.agent(aid, db)
                    if a.get('accountTransferId') == key and op['members'][aid]['phase'] != 'reading':
                        member = op['members'][aid]
                        lazy = a.get('lazyAccountTransfer') or {}
                        if lazy.get('id') == key and member.get('lazy'):
                            a['accountKey'] = lazy['sourceAccountKey']
                            for field in ('provider', 'model', 'effort', 'nativeEffort', 'fastMode',
                                          'daybreakEnabled', 'cyberAccessProgram', 'workerDefaults',
                                          'pendingSettings', 'pendingSettingsAccountKey', 'claudeOptions',
                                          'executionSettingsAccountKey'):
                                if field in lazy.get('sourceState', {}):
                                    value = lazy['sourceState'][field]
                                    if value is None:
                                        a.pop(field, None)
                                    else:
                                        a[field] = copy.deepcopy(value)
                            a.pop('lazyAccountTransfer', None)
                        if member.get('interruptReason') and member.get('continueAfterTransfer') and a.get('autoWake'):
                            rt.enqueue(db, a, 'followup',
                                'The account transfer was cancelled after this turn was interrupted. '
                                'Continue the existing task on this account from saved context. '
                                'Preserve completed work and check existing receipts before any command with an unknown outcome.',
                                'account-transfer-cancel:' + op['id'] + ':' + aid)
                            a['status'] = 'queued'
                        a.pop('accountTransferId', None)
                        rt.put(db, 'agents', a)
            else:
                if any(m.get('archiveInvalidated') for m in op['members'].values()):
                    raise ValueError('The source changed after history export. Cancel this transfer and start a new one.')
                for aid, m in op['members'].items():
                    if m['phase'] == 'blocked':
                        if m.get('lazy'):
                            a = rt.agent(aid, db)
                            if not a.get('lazyAccountTransfer'):
                                try:
                                    resolved = self.destination_settings(a, op['targetAccountKey'],
                                                                         rt.catalog(op['targetAccountKey']))
                                except Exception as error:
                                    m.update(error=str(error))
                                    continue
                                source_account = a.get('accountKey', 'default')
                                source_thread = a.get('threadId')
                                a['accountTransferId'] = key
                                a['lazyAccountTransfer'] = {'id': key, 'sourceAccountKey': source_account,
                                    'sourceThreadId': source_thread,
                                    'sourceState': {field: copy.deepcopy(a.get(field)) for field in (
                                        'provider', 'model', 'effort', 'nativeEffort', 'fastMode',
                                        'daybreakEnabled', 'cyberAccessProgram', 'workerDefaults',
                                        'pendingSettings', 'pendingSettingsAccountKey', 'claudeOptions',
                                        'executionSettingsAccountKey', 'status', 'error', 'nativeFailureHold')}}
                                a['accountKey'] = op['targetAccountKey']
                                a.update(resolved)
                                m['targetSettings'] = resolved
                                rt.put(db, 'agents', a)
                        m.update(phase='lazy' if m.get('lazy') else ('ready' if m.get('result') else 'waiting'),
                                 error=None, nextCheck=0)
                        if m.get('interruptSubmittedAt'):
                            for field in ('interruptTurnId', 'interruptSubmittedAt', 'interruptOutcome'):
                                m.pop(field, None)
                    # Unknown native mutations keep their original callback and receipt.
            self.save(db, op)
        rt.changed.set()
        return op

    def tick(self, agents):
        if self.closing:
            return
        rt = self.rt
        with rt.lock, rt.db() as db:
            referenced = set()
            pending = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_account_transfers "
                "WHERE json_extract(record,'$.status')='pending' ORDER BY rowid")]
            # Read only agent records that can reference a pending transfer.
            transfer_agents = (db.execute(
                "SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.accountTransferId') IS NOT NULL OR "
                "json_extract(record,'$.accountTransfer.id') IS NOT NULL").fetchall() if pending else [])
            for (raw,) in transfer_agents:
                agent = json.loads(raw)
                if agent.get('accountTransferId'):
                    referenced.add(agent['accountTransferId'])
                summary_id = (agent.get('accountTransfer') or {}).get('id')
                if summary_id:
                    referenced.add(summary_id)
            # A transfer with no surviving owner reference and no submitted native
            # mutation cannot advance. Settle it through the same durable receipt path.
            for orphan in pending:
                if (orphan.get('status') != 'pending' or orphan['id'] in referenced
                        or any(member['phase'] not in {'waiting', 'lazy', *MEMBER_TERMINAL}
                               for member in orphan['members'].values())):
                    continue
                orphan['status'] = 'cancelled'
                orphan['cancelledAt'] = time.time()
                self.save(db, orphan)
            # Repair a pending operation whose lead summary still names a
            # finished earlier transfer; the scan below finds it by summary.
            for op in pending:
                if op.get('status') != 'pending':
                    continue
                lead = rt.agent(op['leadId'], db)
                current = lead.get('accountTransfer') or {}
                if (not lead.get('accountTransferId') and current.get('id') not in {None, op['id']}
                        and current.get('status') in TERMINAL and self.newer(db, op, current['id'])):
                    self.save(db, op)
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
                summary = rt.agent(op['leadId'], db).get('accountTransfer') or {}
                dirty = (before != len(op['members']) or summary.get('canFinishHistory') is not False
                         or not summary.get('finishHistory'))
                for aid, member in list(op['members'].items()):
                    a = rt.agent(aid, db)
                    if member['phase'] in MEMBER_TERMINAL:
                        continue
                    if (member['phase'] == 'unknown' and aid not in self.running
                            and (key, aid) not in self.futures and self.already_committed(op, member, a)):
                        member.update(phase='completed', error=None, waiting=None)
                        dirty = True
                        continue
                    if (a.get('deletedAt') and member['phase'] not in {'submitted', 'unknown', 'ready'}
                            and aid not in self.running and (key, aid) not in self.futures):
                        member.update(phase='completed', waiting=None)
                        if (a.get('accountTransferId') == key
                                and not any(member.get(field) for field in ('submittedAt', 'nativeMethod', 'nativeParams', 'result'))):
                            a.pop('accountTransferId')
                            rt.put(db, 'agents', a)
                        dirty = True
                    if member['phase'] == 'interrupting':
                        if not a.get('inFlight') and a['status'] not in ACTIVE:
                            member.update(phase='waiting', waiting=None, interruptConfirmedAt=time.time())
                            dirty = True
                        else:
                            member['waiting'] = 'Waiting for the active turn to stop'
                            continue
                    finish_lazy = member['phase'] == 'lazy'
                    if (member['phase'] not in {'waiting', 'ready'} and not finish_lazy) or aid in self.running:
                        continue
                    if member['phase'] == 'waiting' and (a.get('inFlight') or a['status'] in ACTIVE):
                        if not a.get('turnId'):
                            member['waiting'] = 'Waiting for the active turn id before interrupting'
                            dirty = True
                            continue
                        if member.get('interruptTurnId') == a.get('turnId'):
                            member.update(phase='interrupting', waiting='Waiting for the active turn to stop')
                            dirty = True
                            continue
                        member.update(phase='interrupting', interruptTurnId=a['turnId'],
                                      interruptSubmittedAt=time.time(), waiting='Interrupting active turn')
                        member['continueAfterTransfer'] = bool(a.get('autoWake'))
                        reason = f'Moved to account {self.rt.accounts.get(op["targetAccountKey"]).get("label") or op["targetAccountKey"]}'
                        member['interruptReason'] = reason
                        a['error'] = reason
                        rt.put(db, 'agents', a)
                        self.save(db, op)
                        db.commit()
                        self.running.add(aid)
                        worker = threading.Thread(target=self.interrupt_for_transfer,
                            args=(key, aid, a.copy(), member['interruptTurnId']), daemon=True,
                            name='studio-account-transfer-interrupt')
                        self.workers.add(worker)
                        worker.start()
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
                    member['waiting'] = None
                    if not finish_lazy:
                        member['nextCheck'] = time.time() + 10
                    dirty = True
                    # Commit the reservation before the worker reads its receipt.
                    self.save(db, op)
                    db.commit()
                    if finish_lazy:
                        self.futures[(key, aid)] = concurrent.futures.Future()
                    worker = threading.Thread(target=self.run_lazy if finish_lazy else self.run,
                        args=(key, aid), daemon=True, name='studio-account-transfer')
                    self.workers.add(worker)
                    worker.start()
                if all(m['phase'] in MEMBER_TERMINAL for m in op['members'].values()):
                    op['status'] = 'completed'
                    dirty = True
                if dirty:
                    self.save(db, op)

    def local_blocker(self, db, a):
        rt = self.rt
        from codex_context_repair import blocked, retire_unsent_wait_for_transfer
        from codex_native_tools import account_reserved
        accounts = {a.get('accountKey', 'default')}
        if a.get('accountTransferId'):
            accounts.add(self.get(db, a['accountTransferId'])['targetAccountKey'])
        if any(account_reserved(rt, key) for key in accounts):
            return 'Waiting for the account tool catalog update'
        retire_unsent_wait_for_transfer(rt, db, a)
        if blocked(a):
            return "Waiting for the exact context repair receipt"
        lazy_start = bool(a.get('lazyAccountTransfer') and (a.get('startAttempt') or {}).get('submitted') is False)
        if (a.get('inFlight') or a['status'] in ACTIVE) and not lazy_start:
            return 'Waiting for the current turn'
        if a.get('workspaceOperation'):
            return 'Waiting for the workspace operation'
        prep = rt.preparations.get(a['id'])
        if prep and not prep['future'].done():
            return 'Waiting for the native thread receipt'
        # A previous backend cannot still deliver its RPC. Carry its uncertainty
        # unchanged; it is not an active operation and must never be replayed.
        boot = rt.started_at
        attempt_events = (a.get('startAttempt') or {}).get('events', []) if lazy_start else []
        event_filter = ''
        event_values = []
        if attempt_events:
            event_filter = ' AND id NOT IN (' + ','.join('?' for _ in attempt_events) + ')'
            event_values = attempt_events
        if db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND "
                      "(status IN ('reserved','dispatching') OR (status='uncertain' AND created>=?))" +
                      event_filter + " LIMIT 1",
                      (a['id'], a['epoch'], boot, *event_values)).fetchone():
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

    def run_lazy(self, key, aid):
        from codex_runtime import PreparationPending
        try:
            with self.rt.lock, self.rt.db() as db:
                op = self.get(db, key)
                if self.rt.closed or self.closing or op['status'] != 'pending':
                    return
            self.move_lazy(key, aid, background=True)
        except (PreparationPending, RuntimeError):
            # move_lazy saves failures and deferred checks before returning.
            pass
        finally:
            with self.rt.lock:
                self.running.discard(aid)
                self.workers.discard(threading.current_thread())
                future = self.futures.pop((key, aid), None)
                if future and not future.done():
                    future.set_result(None)
            self.rt.changed.set()

    def move_lazy(self, key, aid, *, background=False):
        """Move one rebound idle member's native history before its first start."""
        rt = self.rt
        saved_result = None
        with rt.lock, rt.db() as db:
            op = self.get(db, key)
            a = rt.agent(aid, db)
            member = op['members'][aid]
            lazy = a.get('lazyAccountTransfer') or {}
            if lazy.get('id') != key:
                return a
            background_receipt = self.futures.get((key, aid))
            if not background and background_receipt and not background_receipt.done():
                from codex_runtime import PreparationPending
                raise PreparationPending(background_receipt)
            if member.get('result'):
                saved_result = copy.deepcopy(member['result'])
            if saved_result is None and member['phase'] == 'unknown':
                raise RuntimeError(member.get('error') or 'Native history move outcome is unknown; no request was repeated')
            if saved_result is None and member['phase'] in {'lazy_submitted', 'submitted'}:
                raise RuntimeError('Native history move receipt is still pending; no request was repeated')
            if saved_result is None and member['phase'] == 'blocked':
                raise RuntimeError(member.get('error') or 'Native history move is blocked; retry it explicitly')
            if saved_result is None and member['phase'] != 'lazy':
                raise RuntimeError('Native history move is not ready')
            delay = member.get('nextCheck', 0) - time.time() if saved_result is None else 0
            if delay > 0:
                future = concurrent.futures.Future()
                timer = threading.Timer(delay, lambda: not future.done() and future.set_result(None))
                timer.daemon = True
                timer.start()
                from codex_runtime import PreparationPending
                raise PreparationPending(future)
            reason = self.local_blocker(db, a)
            if reason:
                member.update(phase='blocked', error=reason)
                self.save(db, op)
                db.commit()
                raise RuntimeError(reason)
            if saved_result is None:
                member.update(phase='reading', error=None, source={
                    'epoch': a.get('epoch'), 'accountKey': lazy['sourceAccountKey'],
                    'threadId': lazy.get('sourceThreadId'), 'cwd': a.get('cwd')},
                    settings=self.settings_snapshot(a),
                    pendingSettings=copy.deepcopy(a.get('pendingSettings')),
                    pendingSettingsAccountKey=a.get('pendingSettingsAccountKey'))
            self.save(db, op)
            db.commit()
            snapshot = copy.deepcopy(a)
        if saved_result is not None:
            try:
                reusable_settings = False
                with rt.lock, rt.db() as db:
                    op = self.get(db, key)
                    member = op['members'][aid]
                    a = rt.agent(aid, db)
                    if isinstance(member.get('targetSettings'), dict):
                        try:
                            self.assert_settings(member, a)
                        except TransferSettingsConflict:
                            pass
                        else:
                            reusable_settings = True
                if reusable_settings:
                    return self.commit_lazy(key, aid, saved_result, background=background)
                catalog = rt.catalog(op['targetAccountKey'])
                resolved = self.destination_settings(snapshot, op['targetAccountKey'], catalog)
                with rt.lock, rt.db() as db:
                    op = self.get(db, key)
                    member = op['members'][aid]
                    a = rt.agent(aid, db)
                    if (a.get('lazyAccountTransfer') or {}).get('id') != key:
                        return a
                    member.update(targetSettings=resolved, settings=self.settings_snapshot(a),
                                  pendingSettings=copy.deepcopy(a.get('pendingSettings')),
                                  pendingSettingsAccountKey=a.get('pendingSettingsAccountKey'))
                    self.save(db, op)
                    db.commit()
                return self.commit_lazy(key, aid, saved_result, background=background)
            except Exception as error:
                with rt.lock, rt.db() as db:
                    op = self.get(db, key)
                    member = op['members'][aid]
                    member.update(phase='blocked', result=saved_result, error=str(error))
                    self.save(db, op)
                raise RuntimeError(str(error)) from error
        try:
            target = op['targetAccountKey']
            source_key = lazy['sourceAccountKey']
            source_thread = lazy.get('sourceThreadId')
            from codex_native_tools import account_reserved
            if account_reserved(rt, target) or account_reserved(rt, source_key):
                raise RuntimeError('Waiting for the account tool catalog update')
            source = rt.connect(source_key) if source_thread else None
            server = rt.connect(target)
            catalog = rt.catalog(target)
            resolved = self.destination_settings(snapshot, target, catalog)
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                a = rt.agent(aid, db)
                if (a.get('lazyAccountTransfer') or {}).get('id') != key:
                    return a
                self.assert_settings(member, a)
            source_path = None
            portable = 'claude' in {rt.accounts.get(source_key).get('provider', 'codex'),
                                    rt.accounts.get(target).get('provider', 'codex')}
            native = None
            if source:
                native = source.call('thread/read', {'threadId': source_thread, 'includeTurns': False}, timeout=10)['thread']
                if native.get('id') != source_thread or native.get('status', {}).get('type') not in {'idle', 'notLoaded', 'systemError'}:
                    raise ValueError('Source native thread is not idle; the saved input remains queued')
                if self.wait_for_native_queue(key, aid, source, source_thread):
                    raise ValueError('Source native thread has queued input; the saved input remains queued')
                if native.get('status', {}).get('type') in {'idle', 'systemError'}:
                    jobs = source.call('thread/backgroundTerminals/list', {'threadId': source_thread}, timeout=10)
                    if jobs.get('data') or jobs.get('nextCursor'):
                        raise ValueError('Source native thread has background commands; the saved input remains queued')
                    flushed = rt.submit_reserved(source, 'thread/unsubscribe', {'threadId': source_thread})
                    source.wait(flushed, timeout=10)
                if portable:
                    with rt.lock, rt.db() as db:
                        op = self.get(db, key)
                        op['members'][aid]['archiveSourceThread'] = copy.deepcopy(native)
                        self.save(db, op)
                if not portable:
                    if not native.get('path'):
                        raise ValueError('Codex returned no saved context path')
                    source_path = self.copy_history(rt.accounts.home(source_key), rt.accounts.home(target), native['path'])
                if self.wait_for_native_queue(key, aid, source, source_thread):
                    raise ValueError('Source native thread has queued input; the saved input remains queued')
            if portable:
                from codex_portable_history import export_history
                portable_snapshot = {**snapshot, 'accountKey': source_key, 'threadId': source_thread}
                descriptor = export_history(rt, portable_snapshot, key, source)
                resolved['portableHistory'] = descriptor
                with rt.lock, rt.db() as db:
                    op = self.get(db, key)
                    op['members'][aid].update(portableHistory=descriptor, targetSettings=resolved)
                    self.save(db, op)
                if not self.archive_source_current(key, aid):
                    raise RuntimeError('The source changed during history export; cancel this transfer and start a new one')
            method = 'thread/start'
            params = rt.new_thread_params({**snapshot, **resolved, 'accountKey': target})
            params.pop('dynamicTools', None)
            if source_path:
                method = 'thread/fork'
                params.update(threadId=source_thread, path=str(source_path), excludeTurns=True, deferGoalContinuation=True)
            else:
                params['dynamicTools'] = rt.tool_definitions({**snapshot, **resolved, 'accountKey': target})
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                a = rt.agent(aid, db)
                member = op['members'][aid]
                if (a.get('lazyAccountTransfer') or {}).get('id') != key:
                    return a
                self.assert_settings(member, a)
                if account_reserved(rt, target):
                    raise RuntimeError('Waiting for the account tool catalog update')
                member.update(phase='lazy_submitted', nativeMethod=method, nativeParams=copy.deepcopy(params),
                              targetSettings=resolved, targetConnection=rt.connection_ids[target], submittedAt=time.time())
                self.save(db, op)
                db.commit()
                future = rt.submit_reserved(server, method, params)
            result = server.wait(future, timeout=60)
            if not result.get('thread', {}).get('id'):
                raise RuntimeError('Native history move returned no thread identity; outcome unknown')
            # Persist an exact successful receipt before any local validation
            # can fail. A retry can then reuse this fork and never submit twice.
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                if member.get('phase') == 'lazy_submitted':
                    member['result'] = copy.deepcopy(result)
                    self.save(db, op)
            if not self.archive_source_current(key, aid):
                raise RuntimeError('The source changed after history export; the saved fork will not be repeated')
            return self.commit_lazy(key, aid, result, background=background)
        except Exception as error:
            if self.retry_preparation(key, aid, error):
                with rt.db() as db:
                    member = self.get(db, key)['members'][aid]
                    delay = max(0.0, member.get('nextCheck', 0) - time.time())
                future = concurrent.futures.Future()
                timer = threading.Timer(delay, lambda: not future.done() and future.set_result(None))
                timer.daemon = True
                timer.start()
                from codex_runtime import PreparationPending
                raise PreparationPending(future) from error
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                from codex_runtime import SubmissionRejected
                rejected = isinstance(error, SubmissionRejected) or str(error) == 'Codex app-server is offline'
                # commit_lazy can reject adoption after saving the exact fork
                # receipt (for example, a settings or epoch race). Keep that
                # known receipt blocked and reusable; do not relabel it unknown.
                if member.get('phase') == 'blocked' and member.get('result'):
                    db.commit()
                    raise RuntimeError(str(error)) from error
                if member.get('phase') == 'lazy_submitted' and member.get('result'):
                    member.update(phase='blocked', error=str(error))
                    self.save(db, op)
                    db.commit()
                    raise RuntimeError(str(error)) from error
                unknown = member.get('phase') == 'lazy_submitted' and not rejected
                member.update(phase='unknown' if unknown else 'blocked', error=str(error))
                if rejected:
                    for field in ('nativeMethod', 'nativeParams', 'targetConnection', 'submittedAt'):
                        member.pop(field, None)
                self.save(db, op)
                a = rt.agent(aid, db)
                a['error'] = 'Native history move blocked: ' + str(error)
                rt.put(db, 'agents', a)
                db.commit()
            rt.changed.set()
            raise RuntimeError('Native history move blocked: ' + str(error)) from error

    def commit_lazy(self, key, aid, result, *, background=False):
        rt = self.rt
        with rt.lock, rt.db() as db:
            op = self.get(db, key)
            a = rt.agent(aid, db)
            member = op['members'][aid]
            lazy = a.get('lazyAccountTransfer') or {}
            if lazy.get('id') != key:
                return a
            source_key, source_thread = lazy['sourceAccountKey'], lazy.get('sourceThreadId')
            if member.get('targetConnection') != rt.connection_ids[op['targetAccountKey']]:
                member.update(phase='unknown', error='Destination connection changed after history move submission; no request was repeated')
                self.save(db, op)
                db.commit()
                raise RuntimeError(member['error'])
            expected = member.get('source') or {}
            if any(a.get(field) != value for field, value in expected.items()
                   if field in {'epoch', 'threadId', 'cwd'}):
                member.update(phase='blocked', result=result,
                              error='Agent state changed while native history moved; the saved fork will not be repeated')
                self.save(db, op)
                db.commit()
                raise RuntimeError(member['error'])
            if not isinstance(member.get('targetSettings'), dict):
                member.update(phase='blocked', result=result,
                              error='Destination settings need validation before this saved history move can finish')
                self.save(db, op)
                db.commit()
                raise RuntimeError(member['error'])
            try:
                self.assert_settings(member, a)
            except TransferSettingsConflict as error:
                member.update(phase='blocked', result=result, error=str(error))
                self.save(db, op)
                db.commit()
                raise
            reason = self.local_blocker(db, a)
            if reason:
                member.update(phase='blocked', result=result, error=reason)
                self.save(db, op)
                db.commit()
                raise RuntimeError(reason)
            from codex_native_tools import mark_current, needs_refresh
            same_codex_provider = (member.get('provider') == 'codex'
                                   and op.get('targetProvider') == 'codex')
            inherited_catalog = (a.get('nativeToolCatalog') if same_codex_provider
                                 and member.get('nativeMethod') == 'thread/fork'
                                 and not needs_refresh(a, rt.tool_definitions(a)) else None)
            history = {'transferId': key, 'accountKey': source_key, 'threadId': source_thread,
                       'provider': member.get('provider'), 'targetAccountKey': op['targetAccountKey'],
                       'targetProvider': op.get('targetProvider'), 'targetThreadId': result['thread']['id'], 'at': time.time()}
            if member.get('provider') != op.get('targetProvider') and member.get('sourcePendingSettings'):
                history['settingsDiscarded'] = {'reason': 'provider_changed',
                                                'pendingSettings': member.get('sourcePendingSettings')}
            if member.get('provider') != op.get('targetProvider') and member.get('sourceClaudeOptions'):
                history['providerOptionsDiscarded'] = {'reason': 'provider_changed',
                    'claudeOptions': member['sourceClaudeOptions']}
            if member.get('portableHistory'):
                history['portableHistory'] = member['portableHistory']
            a.setdefault('accountHistory', []).append(history)
            a.update(threadId=result['thread']['id'], turnId=None,
                     sandbox=result.get('sandbox', a.get('sandbox')),
                     approvalPolicy=result.get('approvalPolicy', a.get('approvalPolicy')))
            if inherited_catalog:
                mark_current(a, rt.tool_definitions(a))
            a.update(member.get('targetSettings') or {})
            if member.get('portableHistory'):
                a['portableHistory'] = copy.deepcopy(member['portableHistory'])
            a.pop('lazyAccountTransfer', None)
            a.pop('accountTransferId', None)
            a.pop('error', None)
            a.pop('nativeFailureHold', None)
            a.pop('prepareAttempt', None)
            a['executionSettingsAccountKey'] = op['targetAccountKey']
            source_state = lazy.get('sourceState') or {}
            resume_failed = bool(a.get('autoWake') and source_state.get('status') in {'failed', 'interrupted'}
                                 and not op.get('finishHistory') and not background)
            pending_input = db.execute(
                "SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                "AND status IN ('pending','reserved','dispatching','uncertain') LIMIT 1",
                (aid, a['epoch'])).fetchone()
            if resume_failed and not pending_input:
                rt.enqueue(db, a, 'followup',
                    'The owner transferred this team to another account. Continue the existing task from its saved context. '
                    'Preserve completed work. Check existing receipts before any command with an unknown outcome.',
                    'account-transfer:' + key + ':' + aid)
            rt.put(db, 'agents', a)
            member.update(phase='completed', result=result, error=None)
            if all(m['phase'] in MEMBER_TERMINAL for m in op['members'].values()):
                op['status'] = 'completed'
            self.save(db, op)
            db.commit()
            if member.get('nativeMethod') == 'thread/start':
                # A newly started, history-free thread may not have a rollout
                # file until its first turn. Keep this live session loaded so
                # the first turn does not try to resume an unmaterialized ID.
                rt.loaded.add(aid)
            else:
                rt.loaded.discard(aid)
            rt.preparations.pop(aid, None)
        rt.changed.set()
        return a

    def before_start(self, agent):
        lazy = agent.get('lazyAccountTransfer') or {}
        if lazy.get('id'):
            return self.move_lazy(lazy['id'], agent['id'])
        return agent

    @staticmethod
    def missing_rollout_error(error, thread_id):
        message = str(error or '')
        return (f'no rollout found for thread id {thread_id}' in message
                or f'invalid paginated history lineage for {thread_id}: missing source rollout' in message)

    @staticmethod
    def _thread_has_completed_turn(db, agent, thread_id):
        rows = db.execute('SELECT record FROM runtime_items WHERE agent=? AND '
                          "json_extract(record,'$.threadId')=?", (agent['id'], thread_id)).fetchall()
        for item in rows:
            record = json.loads(item[0])
            turn_id = record.get('turnId')
            if record.get('turnStatus') == 'completed' or (turn_id and db.execute(
                    'SELECT 1 FROM runtime_completed_turns WHERE id=?',
                    (agent['id'] + ':' + str(turn_id),)).fetchone()):
                return True
        attempt = agent.get('startAttempt') or {}
        attempt_thread = attempt.get('threadId') or (attempt.get('actionIdentity') or {}).get('threadId')
        attempt_turn = attempt.get('turnId')
        return bool(attempt_thread == thread_id and attempt_turn and db.execute(
            'SELECT 1 FROM runtime_completed_turns WHERE id=?',
            (agent['id'] + ':' + str(attempt_turn),)).fetchone())

    @staticmethod
    def source_history_missing(home, thread_id, reported_error=None):
        """Prove a source rollout and its native paginated records are absent."""
        home = Path(home).resolve()
        state_db = home / 'state_5.sqlite'
        history_db = home / 'thread_history_1.sqlite'
        if not state_db.is_file() or not history_db.is_file():
            return None
        try:
            state = sqlite3.connect(state_db.as_uri() + '?mode=ro', uri=True)
            try:
                row = state.execute('SELECT rollout_path,history_mode FROM threads WHERE id=?',
                                    (thread_id,)).fetchone()
            finally:
                state.close()
            if not row or row[1] != 'paginated' or not row[0]:
                return None
            rollout = Path(row[0]).resolve()
            if not rollout.is_relative_to(home) or rollout.exists():
                return None
            if reported_error is not None and str(rollout) not in str(reported_error):
                return None
            history = sqlite3.connect(history_db.as_uri() + '?mode=ro', uri=True)
            try:
                tables = {r[0] for r in history.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'")}
                required = {'thread_turns', 'thread_items'}
                if not required.issubset(tables):
                    return None
                names = required | ({'thread_realtime_items'} & tables)
                counts = {name: history.execute(
                    f'SELECT count(*) FROM "{name}" WHERE thread_id=?', (thread_id,)).fetchone()[0]
                          for name in names}
            finally:
                history.close()
        except (OSError, sqlite3.Error, ValueError):
            return None
        if any(counts.values()):
            return None
        return {'threadId':thread_id, 'rolloutMissing':True, 'nativeTurns':0,
                'nativeItems':0, 'nativeRealtimeItems':counts.get('thread_realtime_items', 0)}

    def missing_source_transfer(self, db, agent):
        """Find a completed member whose source rollout copy failed before submission."""
        thread_id = agent.get('threadId')
        if not thread_id:
            return None
        for row in db.execute('SELECT record FROM runtime_account_transfers'):
            op = json.loads(row[0])
            member = (op.get('members') or {}).get(agent['id']) or {}
            if (member.get('phase') != 'completed' or member.get('sourceThreadId') != thread_id
                    or member.get('sourceAccountKey') != agent.get('accountKey', 'default')
                    or not member.get('error') or '[Errno 2]' not in str(member['error'])
                    or member.get('result') or member.get('nativeMethod')):
                continue
            proof = self.source_history_missing(self.rt.accounts.home(member['sourceAccountKey']),
                                                thread_id, member['error'])
            if proof:
                return op, member, proof
        return None

    def recover_empty_transferred_thread(self, aid):
        """Replace a proven empty transferred thread without touching input receipts."""
        rt = self.rt
        saved = None
        with rt.lock, rt.db() as db:
            a = rt.agent(aid, db)
            recovery = a.get('emptyTransferRecovery') or {}
            if recovery.get('phase') == 'completed':
                return copy.deepcopy(recovery['report'])
            if recovery.get('phase') in {'submitting', 'unknown'}:
                return {'status':'unknown', 'agentId':aid, 'threadId':recovery.get('sourceThreadId'),
                        'requestId':recovery.get('id'),
                        'error':recovery.get('error') or 'The fresh thread receipt is unresolved; no request was repeated.',
                        'replayed':False}
            thread_id = a.get('threadId')
            source_transfer = self.missing_source_transfer(db, a)
            if (not thread_id or (not source_transfer
                                  and not self.missing_rollout_error(a.get('error'), thread_id))):
                raise ValueError('The agent has no exact missing-rollout failure to recover')
            history = next((item for item in reversed(a.get('accountHistory') or [])
                            if item.get('targetThreadId') == thread_id and item.get('threadId') is None), None)
            source_history_missing = None
            if source_transfer:
                op, member, source_history_missing = source_transfer
                transfer_id = op['id']
                source_account = member['sourceAccountKey']
                target_account = op['targetAccountKey']
            elif history:
                transfer_id = history.get('transferId')
                source_account = history.get('accountKey', 'default')
                target_account = history.get('targetAccountKey', a.get('accountKey', 'default'))
            else:
                raise ValueError('The missing thread has no transfer receipt proving an empty source')
            row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (transfer_id,)).fetchone()
            if not row:
                raise ValueError('The source transfer receipt is unavailable')
            op = json.loads(row[0])
            member = (op.get('members') or {}).get(aid) or {}
            result = member.get('result') or {}
            new_empty_thread = (member.get('nativeMethod') == 'thread/start'
                                and member.get('sourceThreadId') is None
                                and result.get('thread', {}).get('id') == thread_id)
            missing_source = (source_history_missing is not None
                              and member.get('sourceThreadId') == thread_id
                              and member.get('sourceAccountKey') == a.get('accountKey', 'default'))
            if member.get('phase') != 'completed' or not (new_empty_thread or missing_source):
                raise ValueError('The transfer receipt does not prove this was a new empty thread')
            missing_history_evidence = copy.deepcopy(source_history_missing)
            if new_empty_thread and self.missing_rollout_error(a.get('error'), thread_id):
                missing_history_evidence = {'threadId':thread_id, 'rolloutMissing':True,
                                            'lineageSourceMissing':True, 'nativeTurns':0}
            if self._thread_has_completed_turn(db, a, thread_id):
                raise ValueError('The transferred thread has a completed turn and cannot be replaced')
            pending = db.execute("SELECT id,kind,status FROM runtime_events WHERE agent=? AND epoch=? "
                                 "AND status IN ('reserved','dispatching','uncertain')", (aid, a['epoch'])).fetchall()
            if pending:
                raise ValueError('An input receipt is unresolved; the empty thread cannot be replaced yet')
            connection_id = rt.connection_ids.get(target_account)
            if not connection_id:
                raise ValueError('The target account is offline')
            settings = self.settings_snapshot(a)
            if source_history_missing and member.get('settings') and member['settings'] != settings:
                raise ValueError('Agent settings changed after the source history transfer failed')
            target_settings = copy.deepcopy(member.get('targetSettings') or {})
            if not target_settings and target_account != a.get('accountKey', 'default'):
                target_settings = self.destination_settings(a, target_account, rt.catalog(target_account))
            if recovery.get('phase') == 'checking':
                if (recovery.get('sourceThreadId') != thread_id or recovery.get('epoch') != a['epoch']
                        or recovery.get('accountKey') != target_account
                        or recovery.get('settings') != settings):
                    raise ValueError('The saved empty-thread check belongs to a different agent state')
            else:
                recovery = {'id':str(uuid.uuid4()), 'phase':'checking', 'sourceThreadId':thread_id,
                            'transferId':op['id'], 'agentId':aid, 'epoch':a['epoch'],
                            'accountKey':target_account, 'sourceAccountKey':source_account,
                            'connectionId':connection_id,
                            'settings':settings,
                            'sourceHistoryMissing':copy.deepcopy(missing_history_evidence),
                            'startedAt':time.time(),
                            'failedInputIds':[r[0] for r in db.execute(
                                "SELECT id FROM runtime_events WHERE agent=? AND epoch=? AND kind='user' "
                                "AND status='failed' ORDER BY created,id",
                                (aid,a['epoch']))]}
                params = rt.new_thread_params({**a, **target_settings, 'accountKey':target_account})
                params['dynamicTools'] = rt.tool_definitions({**a, **target_settings, 'accountKey':target_account})
                recovery['targetSettings'] = target_settings
                recovery['nativeParams'] = copy.deepcopy(params)
            a['emptyTransferRecovery'] = copy.deepcopy(recovery)
            rt.put(db, 'agents', a)
            db.commit()
            saved = copy.deepcopy(a)
        account = target_account
        agent_account = source_account if source_history_missing else target_account
        server = rt.connect(account)
        mutation_submitted = False
        try:
            # Confirm that the native thread has no turns. A successful empty
            # page is sufficient; only the exact missing-rollout errors qualify.
            verifier = rt.connect(source_account) if source_history_missing else server
            try:
                native = verifier.call('thread/read', {'threadId':thread_id, 'includeTurns':False}, timeout=10)
                thread = native.get('thread') if isinstance(native, dict) else None
                if not isinstance(thread, dict) or thread.get('id') != thread_id:
                    raise ValueError('Native thread identity changed during recovery')
            except Exception as error:
                if not self.missing_rollout_error(error, thread_id):
                    raise
            try:
                page = verifier.call('thread/turns/list', {'threadId':thread_id, 'limit':100,
                                      'sortDirection':'asc', 'itemsView':'full'}, timeout=10)
                if not isinstance(page, dict) or not isinstance(page.get('data'), list):
                    raise ValueError('Native turn history returned an invalid page')
                if page['data'] or page.get('nextCursor'):
                    raise ValueError('The transferred thread has saved turns and cannot be replaced')
            except Exception as error:
                if not self.missing_rollout_error(error, thread_id):
                    raise
            # Mark the one mutating request immediately before sending it. A
            # restart during read-only checking can safely repeat those reads.
            with rt.lock, rt.db() as db:
                current = rt.agent(aid, db)
                stored = current.get('emptyTransferRecovery') or {}
                if (stored.get('id') != saved['emptyTransferRecovery']['id']
                        or stored.get('phase') != 'checking' or current.get('threadId') != thread_id
                        or current.get('epoch') != saved['epoch']
                        or current.get('accountKey', 'default') != agent_account
                        or stored.get('accountKey') != account
                        or rt.connection_ids.get(account) != connection_id
                        or self.settings_snapshot(current) != settings):
                    raise ValueError('Agent or connection changed before fresh thread start')
                if self._thread_has_completed_turn(db, current, thread_id):
                    raise ValueError('The transferred thread has a completed turn and cannot be replaced')
                unresolved = db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                                        "AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                                        (aid, current['epoch'])).fetchone()
                if unresolved:
                    raise ValueError('An input receipt became unresolved; the empty thread cannot be replaced yet')
                stored['phase'] = 'submitting'
                rt.put(db, 'agents', current)
                db.commit()
                params = copy.deepcopy(stored['nativeParams'])
                saved['emptyTransferRecovery'] = copy.deepcopy(stored)
            mutation_submitted = True
            result = server.call('thread/start', params, timeout=30)
            new_thread = (result.get('thread') or {}).get('id') if isinstance(result, dict) else None
            if not isinstance(new_thread, str) or not new_thread or new_thread == thread_id:
                raise RuntimeError('Fresh thread start returned no new identity; outcome unknown')
        except Exception as error:
            with rt.lock, rt.db() as db:
                current = rt.agent(aid, db)
                stored = current.get('emptyTransferRecovery') or {}
                expected_phase = 'submitting' if mutation_submitted else 'checking'
                if (stored.get('id') == saved['emptyTransferRecovery']['id']
                        and stored.get('phase') == expected_phase):
                    stored.update(phase='unknown' if mutation_submitted else 'checking', error=str(error)[:1000])
                    current['emptyTransferRecovery'] = stored
                    rt.put(db, 'agents', current)
                    db.commit()
            raise RuntimeError('Fresh thread recovery is unresolved; no retry was submitted: ' + str(error)) from error
        with rt.lock, rt.db() as db:
            current = rt.agent(aid, db)
            stored = current.get('emptyTransferRecovery') or {}
            if (stored.get('id') != saved['emptyTransferRecovery']['id']
                    or current.get('threadId') != thread_id or current.get('epoch') != saved['epoch']
                    or current.get('accountKey', 'default') != agent_account
                    or stored.get('accountKey') != account
                    or rt.connection_ids.get(account) != connection_id
                    or self.settings_snapshot(current) != settings):
                stored.update(phase='unknown', error='Agent or connection changed after thread start; saved receipt retained.')
                current['emptyTransferRecovery'] = stored
                rt.put(db, 'agents', current)
                db.commit()
                raise RuntimeError(stored['error'])
            stored.pop('error', None)
            report = {'status':'recovered', 'agentId':aid, 'transferId':op['id'],
                      'oldThreadId':thread_id, 'threadId':new_thread,
                      'failedInputIds':stored.get('failedInputIds', []),
                      'resendableFailedInputIds':stored.get('failedInputIds', []), 'replayed':False,
                      'next':'The owner may explicitly resend the listed failed user inputs after reviewing them. '
                            'Pending events remain pending. No input was replayed.'}
            current.setdefault('accountHistory', []).append({
                'transferId':op['id'], 'recoveryId':stored['id'], 'accountKey':source_account,
                'threadId':thread_id, 'targetAccountKey':account, 'targetThreadId':new_thread,
                'reason':'empty_transferred_thread', 'sourceHistoryMissing':copy.deepcopy(missing_history_evidence),
                'at':time.time()})
            transfer_receipt = self.get(db, op['id'])
            receipt_member = (transfer_receipt.get('members') or {}).get(aid) or {}
            receipt_member['emptyThreadRecovery'] = {
                'recoveryId':stored['id'], 'sourceThreadId':thread_id,
                'replacementThreadId':new_thread, 'failedInputIds':stored.get('failedInputIds', []),
                'replayed':False, 'sourceHistoryMissing':copy.deepcopy(missing_history_evidence),
                'completedAt':time.time()}
            if missing_history_evidence:
                receipt_member['sourceHistoryMissing'] = copy.deepcopy(missing_history_evidence)
            if source_history_missing:
                receipt_member['result'] = copy.deepcopy(result)
                receipt_member['nativeMethod'] = 'thread/start'
                receipt_member.pop('error', None)
            transfer_receipt['members'][aid] = receipt_member
            self.save(db, transfer_receipt)
            if source_history_missing:
                current['accountKey'] = account
                current.update(stored.get('targetSettings') or {})
                current['executionSettingsAccountKey'] = account
                current.pop('lazyAccountTransfer', None)
                current.pop('accountTransferId', None)
            current.update(threadId=new_thread, turnId=None, status='failed',
                           error=('Source native history is missing. A fresh native thread is ready. '
                                  'Saved inputs were not replayed.' if source_history_missing else
                                  'Previous transferred thread had no saved rollout. A fresh native thread is ready. '
                                  'Failed inputs were not resent; see recovery.resendableFailedInputIds.'),
                           emptyTransferRecovery={**stored, 'phase':'completed', 'threadId':new_thread,
                                                  'completedAt':time.time(), 'report':report})
            current.pop('prepareAttempt', None)
            current.pop('preparedContext', None)
            rt.put(db, 'agents', current)
            db.commit()
            rt.loaded.discard(aid)
            rt.preparations.pop(aid, None)
            rt.loaded.add(aid)
        rt.changed.set()
        return report

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
                from codex_daybreak import resolve_program
                effort, native_effort = rt.validate_execution(catalog, a['model'], a.get('effort'), a.get('fastMode', False))
                daybreak = a.get('daybreakEnabled', False)
                program = resolve_program(catalog, a['model'], daybreak, target_provider)
                defaults = rt.worker_defaults(a)
                resolved = dict(effort=effort, nativeEffort=native_effort,
                                daybreakEnabled=daybreak, cyberAccessProgram=program,
                                workerDefaults=defaults)
                if a.get('pendingSettings'):
                    if a.get('pendingSettingsAccountKey', a.get('accountKey', 'default')) != a.get('accountKey', 'default'):
                        raise ValueError('Queued settings belong to another account. Save them again before transfer')
                    pending = a['pendingSettings']
                    pending_effort, pending_native = rt.validate_execution(
                        catalog, pending['model'], pending.get('effort'), pending.get('fastMode', False))
                    pending_program = resolve_program(catalog, pending['model'], pending.get('daybreakEnabled', False), target_provider)
                    resolved.update(pendingSettings={**pending, 'effort': pending_effort, 'nativeEffort': pending_native,
                                                    'cyberAccessProgram': pending_program},
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
            if self.retry_preparation(key, aid, error):
                return
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

    def interrupt_for_transfer(self, key, aid, agent, turn_id):
        """Interrupt one active native turn without touching queued event receipts."""
        rt = self.rt
        try:
            server = rt.servers.get(agent.get('accountKey', 'default'))
            if not server or not agent.get('threadId'):
                raise RuntimeError('The active native turn cannot be reached to interrupt it')
            stream = getattr(rt, '_stream_buffer', None)
            if stream:
                with rt.lock, rt.db() as db:
                    stream.flush_locked(db, account=agent.get('accountKey', 'default'),
                                        thread_id=agent['threadId'], force=True)
            try:
                server.call('turn/interrupt', {'threadId': agent['threadId'], 'turnId': turn_id}, timeout=10)
            except Exception as error:
                current = rt.agent(aid)
                if current.get('inFlight') and current.get('turnId') == turn_id:
                    raise RuntimeError(f'Interrupt acknowledgement is unknown: {error}')
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                if member.get('phase') == 'interrupting' and member.get('interruptTurnId') == turn_id:
                    member['interruptOutcome'] = 'acknowledged'
                    member['waiting'] = 'Waiting for the active turn to stop'
                    self.save(db, op)
        except Exception as error:
            with rt.lock, rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                if member.get('phase') == 'interrupting' and member.get('interruptTurnId') == turn_id:
                    member.update(phase='blocked', error=str(error), waiting=None)
                    self.save(db, op)
        finally:
            with rt.lock:
                self.running.discard(aid)
                self.workers.discard(threading.current_thread())
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
        from codex_catalog import CatalogPending
        if isinstance(error, CatalogPending):
            with self.rt.lock, self.rt.db() as db:
                op = self.get(db, key)
                member = op['members'][aid]
                if (op['status'] != 'pending' or member['phase'] != 'reading'
                        or any(member.get(field) for field in ('submittedAt', 'nativeMethod', 'result'))):
                    return False
                member.update(phase='lazy' if member.get('lazy') else 'waiting', error=None,
                              waiting='Waiting for the destination model list', nextCheck=time.time() + 1)
                self.save(db, op)
            return True
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
            if op['status'] != 'pending' or member['phase'] not in {'submitted', 'unknown', 'lazy_submitted'}:
                return False
            rejections = member.setdefault('preparationRejections', [])
            if not any(r['submittedAt'] == member.get('submittedAt') for r in rejections):
                rejections.append({'submittedAt': member.get('submittedAt'),
                                   'at': time.time(), 'error': error.error,
                                   'outcome': 'fork_not_created'})
            if len(rejections) > 3:
                member.update(phase='blocked', error=message, waiting=None)
            else:
                member.update(phase='lazy' if member.get('lazy') else 'waiting', error=None,
                              waiting='Codex history database is busy; retrying',
                              nextCheck=time.time() + 2 ** len(rejections))
            self.save(db, op)
        return True

    def commit(self, db, op, a):
        rt = self.rt
        m = op['members'][a['id']]
        if self.already_committed(op, m, a):
            # A second commit of the same receipt: the first one moved the agent.
            m.update(phase='completed', error=None, waiting=None)
            if op['status'] == 'pending' and all(member['phase'] in MEMBER_TERMINAL
                                                 for member in op['members'].values()):
                op['status'] = 'completed'
            self.save(db, op)
            return
        self.assert_source(m, a)
        if rt.closed or self.closing or a.get('accountTransferId') != op['id'] or self.local_blocker(db, a):
            return
        self.assert_settings(m, a)
        if not isinstance(m.get('targetSettings'), dict):
            raise TransferSettingsConflict('Destination settings need validation before this saved transfer can finish')
        target = op['targetAccountKey']
        self.check_destination(a, target, db)
        rt.usage_resume_cancel(db, a, 'The chat moved to another account.')
        result = m['result']
        resume_failed = a.get('autoWake') and (a.get('status') in {'failed', 'interrupted'}
                                                or m.get('continueAfterTransfer'))
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
        if a.get('isLead'):
            defaults = copy.deepcopy(a.get('workerDefaults') or rt.worker_defaults(a))
            defaults['accountKey'] = target
            a['workerDefaults'] = defaults
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
            if not pending or m.get('continueAfterTransfer'):
                rt.enqueue(db, a, 'followup',
                    'The owner transferred this team to another account. Continue the existing task from its saved context. '
                    'Preserve completed work. Check existing receipts before any command with an unknown outcome.',
                    'account-transfer:' + op['id'] + ':' + a['id'])
            if pending or m.get('continueAfterTransfer'):
                a['status'] = 'queued'
        rt.put(db, 'agents', a)
        m.update(phase='completed', error=None, waiting=None)
        if op['status'] == 'pending' and all(member['phase'] in MEMBER_TERMINAL
                                             for member in op['members'].values()):
            op['status'] = 'completed'
        self.save(db, op)
        db.commit()
        if m.get('nativeMethod') == 'thread/start':
            # thread/start can return before Codex materializes a paginated
            # rollout. The first turn must use this live session directly.
            rt.loaded.add(a['id'])
        else:
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
