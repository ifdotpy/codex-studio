"""Real transfer methods, fake runtime ports, SQLite only; no Runtime construction."""
import copy
import json
import os
from pathlib import Path
import sqlite3
import sys
import threading
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'turn_lifecycle'))
from oracle_guard import install_guard
install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / 'workspaces/runtime/apps/server/src'))
from codex_account_transfer import AccountTransfers
from studio_api.agents.router import _public_transfer


class RuntimePort:
    def __init__(self):
        self.connection = sqlite3.connect(':memory:')
        self.connection.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
        self.connection.execute('CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
        self.lock = threading.RLock()
        self.changed = threading.Event()
        self.followups = []
    def db(self):
        return self.connection
    def agent(self, key, db=None):
        return json.loads(self.connection.execute('SELECT record FROM runtime_agents WHERE id=?', (key,)).fetchone()[0])
    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_' + table + ' ORDER BY rowid')]
    def put(self, db, table, value):
        db.execute('INSERT OR REPLACE INTO runtime_' + table + '(id, record) VALUES(?, ?)', (value['id'], json.dumps(value, ensure_ascii=False)))
    def enqueue(self, db, agent, kind, text, key):
        self.followups.append(dict(agent=agent['id'], id=key, text=text))


def base(phase='waiting', lazy=False):
    member = dict(phase=phase, sourceAccountKey='source', sourceThreadId='source-thread', name='Lead', provider='codex', lazy=lazy)
    op = dict(id='41e0b552-0b0c-4c5c-9c37-2e7483cd05c9', leadId='lead', targetAccountKey='target', status='pending', scope='team', created=1.0,
              members={'lead':member}, requests={})
    agent = dict(id='lead', rootId='lead', epoch=3, status='idle', isLead=True, accountKey='source', threadId='source-thread', cwd='/synthetic/project', accountTransferId=op['id'], autoWake=False)
    if lazy:
        agent['accountKey'] = 'target'
        agent['lazyAccountTransfer'] = dict(id=op['id'], sourceAccountKey='source', sourceThreadId='source-thread', sourceState=dict(model='source-model',effort=None,workerDefaults={'accountKey':'source'}))
        agent.update(model='destination-model', effort='high', workerDefaults={'accountKey':'target'})
    return op, [agent]


def run(name, command, op, agents):
    rt = RuntimePort()
    for a in agents:
        rt.put(rt.connection, 'agents', a)
    rt.put(rt.connection, 'account_transfers', op)
    rt.connection.commit()
    before = dict(raw=rt.connection.execute('SELECT record FROM runtime_account_transfers').fetchone()[0],
                  agents=[(a['id'],rt.connection.execute('SELECT record FROM runtime_agents WHERE id=?',(a['id'],)).fetchone()[0]) for a in agents])
    with patch('codex_account_transfer.time.time', return_value=100.0):
        # Action tests do not invoke startup recovery first: these are distinct boundaries.
        if command == 'restart':
            transfers = AccountTransfers(rt)
            error = None
        else:
            transfers = object.__new__(AccountTransfers)
            transfers.rt = rt
            try:
                transfers.action(op['id'], command)
                error = None
            except ValueError as exc:
                error = str(exc)
    saved = transfers.get(rt.connection, op['id'])
    result = dict(operation=saved, agents={a['id']:rt.agent(a['id']) for a in agents}, followups=rt.followups,
                  public=_public_transfer(saved).model_dump(mode='json', exclude_unset=True), error=error)
    rt.connection.close()
    return dict(name=name,command=command,**before,expected=result)


cases = []
phases = ['waiting','lazy','reading','interrupting','submitted','lazy_submitted','ready','blocked','unknown','completed','left']
for phase in phases:
    for lazy in (False, True):
        for action in ('restart','cancel','retry','finish_history'):
            if not os.environ.get('STUDIO_RUNTIME_CORPUS_FULL') and phase not in {'waiting','reading','submitted','interrupting','blocked','unknown'}:
                continue
            op, agents = base(phase,lazy)
            if phase in {'submitted','lazy_submitted','unknown','ready'}:
                op['members']['lead'].update(submittedAt=5.0,nativeMethod='thread/fork',nativeParams={'threadId':'source-thread'},targetConnection='connection')
            if phase == 'ready': op['members']['lead']['result'] = {'thread':{'id':'target-thread'}}
            cases.append(run(f'{action}/{phase}/lazy={lazy}',action,op,agents))
for name, change in [
    ('interrupt-unknown', {'interruptSubmittedAt':4.0,'interruptOutcome':'unknown'}),
    ('archive-invalid', {'archiveInvalidated':True}),
    ('saved-result', {'result':{'thread':{'id':'target-thread'}}}),
    ('archive-invalid-saved', {'archiveInvalidated':True,'submittedAt':4.0,'result':{'thread':{'id':'target-thread'}}}),
]:
    for action in ('cancel','retry'):
        op, agents = base('blocked', True); op['members']['lead'].update(change)
        cases.append(run(name+'/'+action,action,op,agents))
for status in ('completed','cancelled'):
    for action in ('restart','cancel','retry','finish_history'):
        op, agents = base('submitted'); op['status']=status
        cases.append(run(status+'/'+action,action,op,agents))
op, agents = base('waiting'); agents[0].update(autoWake=True)
op['members']['lead'].update(interruptReason='account moved',continueAfterTransfer=True,interruptOutcome='acknowledged',interruptSubmittedAt=4.0)
cases.append(run('cancel-followup','cancel',op,agents))
# Order matters: distinct reasons and intentionally reverse-alphabetical member IDs.
op, agents = base('blocked'); op['members']={'z':dict(op['members']['lead'],error='z reason'),'a':dict(op['members']['lead'],phase='left',reason='a reason')}
agents += [dict(agents[0],id='z',isLead=False),dict(agents[0],id='a',isLead=False)]
cases.append(run('member-order','finish_history',op,agents))
print(json.dumps(cases, ensure_ascii=False, allow_nan=False))
