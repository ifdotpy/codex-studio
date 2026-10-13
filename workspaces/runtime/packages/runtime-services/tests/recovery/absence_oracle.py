"""Real command absence/source/apply functions; no provider or Runtime.start."""
import copy
import json
import sqlite3
import threading
import time
from contextlib import nullcontext
from types import SimpleNamespace
from unittest.mock import patch
from oracle import task, writer

base=dict(id='agent',rootId='agent',epoch=0,status='completed',accountKey='default',threadId='thread',turnId=None,inFlight=False)
work=dict(id='agent:item',agent='agent',itemId='item',turnId='turn',type='commandExecution',kind='command',status='running',processId='42',created=10)
variants=[('absent',{}),('in-progress-absent',dict(progress=True)),('live-item',dict(terminals=[dict(itemId='item',processId='other')])),
          ('live-process',dict(terminals=[dict(itemId='other',processId='42')])),('paginated-live',dict(pages=[[],[dict(itemId='item',processId='42')]])),
          ('paginated-absent',dict(pages=[[],[dict(itemId='other',processId='other')]])),
          ('busy-before',dict(quiet=False)),('busy-after',dict(quiet_after=False)),('active-thread',dict(state='active')),
          ('terminal-list-unavailable',dict(unavailable=True)),('missing-completed',dict(completed=False)),
          ('reservation',dict(reserved=True)),('refreshing',dict(refreshing=True)),('preparation',dict(preparing=True)),
          ('agent-inflight',dict(agent=dict(inFlight=True))),('agent-running',dict(agent=dict(status='running'))),
          ('agent-tools',dict(agent=dict(activeTools=['tool']))),('moved-account',dict(agent=dict(accountHistory=[dict(at=11,accountKey='other',threadId='thread')]))),
          ('old-move',dict(agent=dict(accountHistory=[dict(at=9,accountKey='other',threadId='thread')]))),
          ('malformed-move-time',dict(agent=dict(accountHistory=[dict(at='unknown')]))),
          ('no-process',dict(task=dict(processId=None))),('integer-process',dict(task=dict(processId=42))),
          ('duplicate-native',dict(duplicate=True)),('unsupported-items',dict(unsupported=True))]
statement=writer(); output=[]
for name, options in variants:
    agent={**base,**options.get('agent',{})}; stored_task={**work,**options.get('task',{})}
    class Server:
        def __init__(self): self.pending={} if options.get('quiet',True) else {'busy':1};self.calls=[];self.barriers=0;self.page=0
        def after_events(self,callback):
            self.barriers+=1
            if self.page: self.pending={} if options.get('quiet_after',True) else {'busy':1}
            callback()
        def call(self,method,params,timeout):
            self.calls.append(dict(method=method,params=copy.deepcopy(params)))
            if method=='thread/items/list':
                if options.get('unsupported'):
                    from codex_native_errors import NativeRpcError
                    raise NativeRpcError(dict(code=-32601,message='unsupported'))
                entries=[]
                if options.get('progress') or options.get('duplicate'):
                    entries=[dict(turnId='turn',item=dict(id='item',type='commandExecution',status='inProgress',processId='42'))]*(2 if options.get('duplicate') else 1)
                return dict(data=entries)
            if method=='thread/read': return dict(thread=dict(id='thread',status=dict(type=options.get('state','idle'))))
            assert method=='thread/backgroundTerminals/list'
            if options.get('unavailable'): raise ValueError('terminal list unavailable')
            pages=options.get('pages',[options.get('terminals',[])])
            index=self.page;self.page+=1
            return dict(data=pages[index],nextCursor='next' if self.page<len(pages) else None)
    server=Server()
    with sqlite3.connect(':memory:') as db:
        db.row_factory=sqlite3.Row
        for table in ['agents','tasks','items']: db.execute(f'CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT'+(',agent TEXT' if table=='items' else '')+')')
        db.execute('CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY)')
        if options.get('completed',True): db.execute("INSERT INTO runtime_completed_turns VALUES('agent:turn')")
        for table,record in [('agents',agent),('tasks',stored_task)]: exec(statement,dict(db=db,table=table,record=record,json=json))
        db.execute('INSERT INTO runtime_items VALUES(?,?,?)',('agent:item',json.dumps(dict(id='agent:item',turnId='turn',toolStatus='inProgress')),'agent'))
        class Runtime:
            closed=False;lock=threading.RLock()
            _native_runtime_reservations={'default':1} if options.get('reserved') else {}
            _native_tools_refreshing={'default'} if options.get('refreshing') else set()
            preparations={'agent':dict(future=SimpleNamespace(done=lambda:False))} if options.get('preparing') else {}
            def db(self):return nullcontext(db)
            def agent(self,key,db_arg=None):return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?',(key,)).fetchone()[0])
            def agent_connection(self,agent):return 'connection'
            def server_for(self,*args):return server
            def connection_current(self,*args):return True
            def put(self,db,table,record):exec(statement,dict(db=db,table=table,record=record,json=json))
            def touch_ui(self,*args):pass
        rt=Runtime()
        agent=rt.agent('agent');raw=db.execute("SELECT record FROM runtime_tasks WHERE id='agent:item'").fetchone()[0];stored_task=json.loads(raw)
        source=task._absence_source(rt,db,agent,stored_task)
        job=task.TaskCheck(stored_task,raw,(10,1),task._scope(agent),'default','thread','connection',server,task.ReceiptState(),None,source)
        errors=[];proofs=task._native_proofs(job,[job],time.monotonic()+5,errors)
        with patch('codex_task_recovery.time.time',return_value=1000.0): applied=task._apply_proofs(rt,[job],proofs)
        output.append(dict(name=name,agent=agent,task=stored_task,source=source is not None,absent=isinstance(proofs.get('agent:item'),task.CommandAbsence),applied=applied,
            after=json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='agent:item'").fetchone()[0]),
            item=json.loads(db.execute("SELECT record FROM runtime_items WHERE id='agent:item'").fetchone()[0]),calls=server.calls,options=options,
            observation=dict(scope=dict(account='default',connection='connection',generation=1),at=1000.0,supervisor=None,
                quiet=options.get('quiet',True) and options.get('quiet_after',True),journal_drained=True,
                account_reserved=options.get('reserved',False),tools_refreshing=options.get('refreshing',False),preparation_pending=options.get('preparing',False),
                completed_turn='agent:turn' if options.get('completed',True) else None)))
print(json.dumps(output))
