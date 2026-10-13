"""Actual legacy-start matching and effect functions, with an injected clock."""
import copy
import json
import os
import sqlite3
import threading
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from oracle import turn, writer

at = 1000.0
root = str(Path(os.environ['TMPDIR']) / 'supervisor')
base = dict(id='agent', rootId='agent', epoch=0, status='starting', accountKey='default',
            threadId='thread', turnId=None, inFlight=True, autoWake=True,
            error='turn/start response timed out; outcome unknown',
            startAttempt=dict(id='attempt', epoch=0, accountKey='default', threadId='thread',
                              events=['event'], submitted=True, executionOutcome='unknown',
                              connectionId='connection', created=999.0,
                              supervisorIdentity=dict(stateDir=root,handle='child',generation=1)))
native = dict(id='turn',status='inProgress',items=[],clientUserMessageId='event')
item_form = dict(id='turn',status='inProgress',items=[dict(type='userMessage',clientId='event')])
def page(*rows, cursor=None): return dict(data=list(rows),nextCursor=cursor)
cases = [('client-field',[page(native)],{}), ('item-client',[page(item_form)],{}),
         ('client-no-items',[page({k:v for k,v in native.items() if k != 'items'})],{}),
         ('later-page',[page(cursor='next'),page(native)],{}),
         ('identical-page-duplicate',[page(native,cursor='next'),page(native)],{}),
         ('same-page-duplicate',[page(native,native)],{}),
         ('changed-duplicate',[page(native,cursor='next'),page({**native,'extra':'changed'})],{}),
         ('conflicting-id',[page(native,cursor='next'),page({**native,'id':'other'})],{}),
         ('cursor-cycle',[page(cursor='same'),page(cursor='same')],{}),
         ('complete-absence',[page(cursor='next'),page()],{})]
for state in ['preparing','not_applied']:
    cases += [(f'turn-{state}',[page({**native,'startOutcome':state})],{}),
              (f'item-{state}',[page({**item_form,'items':[{**item_form['items'][0],'deliveryStatus':state}]})],{})]
cases += [('missing-turn-id',[page({**native,'id':''})],{}),
          ('wrong-client',[page({**native,'clientUserMessageId':'different'})],{}),
          ('direct-client-with-filtered-item',[page({**native,'items':[{**item_form['items'][0],'deliveryStatus':'not_applied'}]})],{})]
cases += [('restore',[page()],dict(generation=2,connection='new',created=0)),
          ('waiting',[page()],dict(created=401)),
          ('hold-boundary',[page()],dict(created=400)),
          ('hold',[page()],dict(created=0)),
          ('journal-pending',[page()],dict(created=0,journal=False)),
          ('active',[page()],dict(created=0,state='active')),
          ('active-flags',[page()],dict(created=0,flags=['executing'])),
          ('foreign-child',[page()],dict(created=0,generation=2,connection='new',handle='foreign'))]
output=[]
statement=writer()
for name,replies,options in cases:
    agent=copy.deepcopy(base); agent['startAttempt']['created']=options.get('created',999.0)
    generation=options.get('generation',1); connection=options.get('connection','connection')
    class Server:
        supervisor_mode=True
        def __init__(self):
            self.calls=[]; self.replies=copy.deepcopy(replies)
            self.proc=SimpleNamespace(root=root,handle=options.get('handle','child'),generation=generation,
                call=lambda method:dict(sequence=1,acknowledged=1 if options.get('journal',True) else 0,backpressure=False))
        def call(self, method, params, timeout):
            self.calls.append(dict(method=method,params=copy.deepcopy(params)))
            if method=='thread/turns/list': return self.replies.pop(0)
            assert method=='thread/read'
            return dict(thread=dict(id='thread',status=dict(type=options.get('state','idle'),activeFlags=options.get('flags',[]))))
        def after_events(self,callback): callback()
    server=Server()
    with sqlite3.connect(':memory:') as db:
        db.row_factory=sqlite3.Row
        db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)')
        db.execute('CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,status TEXT,turn_id TEXT,error TEXT)')
        exec(statement,dict(db=db,table='agents',record=agent,json=json))
        db.execute("INSERT INTO runtime_events VALUES('event','agent',0,'uncertain',NULL,'unknown')")
        stored=json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='agent'").fetchone()[0])
        class Runtime:
            lock=threading.RLock(); closed=False; changed=threading.Event(); accepted=None; stopped=[]
            def db(self): return nullcontext(db)
            def agent(self,key,db_arg=None): return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?',(key,)).fetchone()[0])
            def put(self,db,table,record): exec(statement,dict(db=db,table=table,record=record,json=json))
            def connection_current(self,*args): return True
            def start_accepted(self,key,attempt,result): self.accepted=result['turn']
            def child_stopped_event(self,db,agent,status,reason,event_id): self.stopped.append(dict(status=status,reason=reason,id=event_id))
            restore_absent_start=turn.TurnRecoveryMixin.restore_absent_start
            hold_unknown_start=turn.TurnRecoveryMixin.hold_unknown_start
        rt=Runtime();rt.stopped=[]
        expected={k:stored.get(k) for k in ('id','epoch','accountKey','threadId','turnId','startAttempt')}
        with patch('codex_turn_recovery.time.time', return_value=at):
            result=turn.TurnRecoveryMixin.reconcile_start_receipt(rt,server,stored,expected,connection)
        output.append(dict(name=name,history_case=not options,agent=stored,replies=replies,result=result,accepted=rt.accepted,
            failed='error' in result,calls=server.calls,after=rt.agent('agent'),events=[list(r) for r in db.execute('SELECT * FROM runtime_events')],stopped=rt.stopped,
            observation=dict(scope=dict(account='default',connection=connection,generation=generation),at=at,
                supervisor=dict(state_dir=root,handle=options.get('handle','child'),generation=generation),quiet=True,journal_drained=options.get('journal',True),
                account_reserved=False,tools_refreshing=False,preparation_pending=False,completed_turn=None),
            state=dict(type=options.get('state','idle'),activeFlags=options.get('flags',[]))))
print(json.dumps(output))
