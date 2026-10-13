"""Real Python native maintenance/review/history functions on disposable state."""
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import copy
import os
import json
import sqlite3
import tempfile
import threading
from contextlib import contextmanager
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
from codex_runtime import Runtime
from codex_agent_review import request as review_request
from codex_context_repair import sanitized_rollout
from codex_efficiency import EfficiencyMixin
from studio_api.persisted_models import PersistedAgentRecord

NOW = 1234567.5

def agent():
    value = dict(id="lead", rootId="lead", parentId=None, epoch=3, status="idle", name="Lead", cwd=os.environ["TMPDIR"],
                 isLead=True, autoWake=True, inFlight=False, created=1.0, threadId="thread", accountKey="default",
                 model="gpt-6", effort="high", fastMode=False, concurrency=32)
    PersistedAgentRecord.model_validate(value)
    return value

class RuntimePort:
    closed = False
    native_action = Runtime.native_action
    review_defaults = staticmethod(Runtime.review_defaults)
    def __init__(self, actor):
        self.lock = threading.RLock()
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY,record TEXT);")
        self.put(self.connection,"agents",actor)
        self.spec = None
        self.root = Path(os.environ["TMPDIR"])
    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection
    def put(self, db, table, value):
        if table == "agents": PersistedAgentRecord.model_validate(value)
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES(?,?)",(value["id"],json.dumps(value)))
    def agent(self, key, db=None):
        return json.loads((db or self.connection).execute("SELECT record FROM runtime_agents WHERE id=?",(key,)).fetchone()[0])
    def assert_workspace_available(self,*_): pass
    def dispatch_active_slots(self,*_): return []
    def capacity_reset(self,*_): pass
    def run_native_action(self,*_): return {"fixture":True}
    def tool_request(self,*_): return None
    def checked_actor(self,db,key,*_): return self.agent(key,db)
    def create(self,spec,parent_id,**_):
        self.spec = copy.deepcopy(spec)
        child = dict(agent(),id=spec['id'],rootId='lead',parentId=parent_id,isLead=False,status='queued',
                     name=spec['name'],cwd=spec['cwd'],model=spec['model'],effort=spec['effort'],fastMode=spec['fast_mode'])
        child.pop('concurrency',None)
        return child

output = {"outcomes":[],"actions":[],"reviews":[],"rewrites":[],"commands":[],"cancellations":[],"forks":[]}
for action in ("compact","review"):
    for context in ({},{"epoch":3},{"accountKey":"default","threadId":"thread","epoch":3},{"threadId":"thread Ω 😀"}):
        a=agent(); rt=RuntimePort(a)
        if "threadId" in context:
            a["threadId"] = context["threadId"]
            rt.put(rt.connection,"agents",a)
        request=dict(requestId="action-request",agent="lead",action=action,context=context)
        with patch("codex_runtime.uid",return_value="attempt"), patch("time.time",return_value=NOW), patch("codex_budget.budget_admission"), patch("codex_agent_modes.assert_worker_input"):
            rt.native_action("lead",action,request_id="action-request",context=context)
        row=rt.connection.execute("SELECT * FROM runtime_native_action_receipts").fetchone()
        output['actions'].append(dict(request=request,agent=a,after=rt.agent("lead"),row=[row[0],row[1],json.loads(row[2]),json.loads(row[3])]))
        from codex_native_action_receipts import record
        for status in ('pending','unknown','failed','acknowledged'):
            record(rt,dict(actionRequestId='action-request'),status,None if status=='acknowledged' else 'fixture failure')
            outcome=json.loads(rt.connection.execute('SELECT outcome FROM runtime_native_action_receipts').fetchone()[0])
            output['outcomes'].append(dict(agent=rt.agent('lead'),receipt=json.loads(row[2]),outcome=outcome))
        rt.connection.close()

for target in ({"type":"uncommittedChanges"},{"type":"baseBranch","branch":"main"},{"type":"commit","sha":"abc","title":None},{"type":"custom","instructions":"Inspect Unicode Ω carefully"}):
    for options in ([{}, {"model":"gpt-other"}, {"effort":"low"}] + ([{"model":"gpt-6"}, {"model":"gpt-other","effort":"medium"}] if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL") else [])):
        a=agent()
        if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL"):
            a["reviewDefaults"] = dict(model="gpt-review", effort="medium")
        rt=RuntimePort(a); args={"target":target,**options}
        with patch("codex_agent_review.require_repository"), patch("codex_worker_accounts.resolve",return_value=("default",{})), patch("time.time",return_value=NOW):
            result=review_request(rt,a,args,"review-request")
        child=rt.agent(result['agentId'])
        output['reviews'].append(dict(actor=a,root=a,args=args,key="review-request",directory=a["cwd"],spec=rt.spec,child=child,result=result))
        rt.connection.close()

for kind in ('monitor_exit','agent_message','work_review','work_decision'):
    for mode in ('exact','quoted','wrong-turn','repeated'):
        event=dict(id="event",kind=kind,text="data Ω "*750,turn_id="turn")
        text=f"[Orchestration event: {kind}]\n{event['text']}"
        if mode == 'quoted': text='A user quotes: '+text
        records=[dict(type='session_meta',payload=dict(id='thread')),dict(type='turn_context',payload=dict(turn_id='turn')),
                 dict(type='response_item',payload=dict(type='message',id='message',role='user',content=[dict(type='input_text',text=text)],
                     internal_chat_message_metadata_passthrough=dict(turn_id='other' if mode=='wrong-turn' else 'turn'))),
                 dict(type='event_msg',payload=dict(type='task_complete',turn_id='turn',completed_at=1.0))]
        records.insert(2,dict(type='compacted'))
        records.insert(3,dict(type='response_item'))
        if mode == 'repeated':
            records.insert(5,copy.deepcopy(records[4])); records[5]['payload']['id']='another-message'
        with tempfile.TemporaryDirectory(prefix='wave7-p11-rewrite-') as tmp:
            source=Path(tmp)/'thread.jsonl'; destination=Path(tmp)/'copy-thread.jsonl'
            source.write_text(''.join(json.dumps(r,ensure_ascii=False)+'\n' for r in records))
            try:
                with patch('codex_context_repair.uuid.uuid4',return_value='import'):
                    report=sanitized_rollout(source,destination,'thread',[event],terminal_turn='turn')
                copied=Path(report['copyPath']).read_text() if 'copyPath' in report else None
                expected=[json.loads(line) for line in copied.splitlines()] if copied else None
                result=dict(records=expected,ids=report['eventIds'])
            except ValueError:
                result=dict(error=True)
        output['rewrites'].append(dict(records=records,events=[event],summaries={event['id']:EfficiencyMixin.bounded_event(event,event['text'],3000)},result=result))

from codex_workspace import WorkspaceMixin
for text in ('', 'line\nnext', 'Ω\\"quote', 'x'*16000):
    a=agent(); task=dict(id='task',agent='lead',kind='command',status='running',processId='17',itemId='item',created=1.0)
    class CommandPort:
        def task_detail(self,_): return task
        def agent(self,_): return a
        def send(self,aid,instruction): return dict(agent=aid,text=instruction)
    result=WorkspaceMixin.native_command_action(CommandPort(),dict(id='task',action='input',text=text))
    output['commands'].append(dict(agent=a,task=task,text=text,result=result))

for pages in (([{'itemId':'other','processId':'17'}], [{'itemId':'item','processId':'17'}]),
              ([{'itemId':'item','processId':'17'}],)):
    a=agent(); task=dict(id='task',agent='lead',kind='command',status='running',processId='17',itemId='item',created=1.0)
    class NativeServer:
        def __init__(self): self.calls=[]
        def call(self, method, params):
            self.calls.append((method,copy.deepcopy(params)))
            if method == 'thread/backgroundTerminals/list':
                index=len([call for call in self.calls if call[0] == method])-1
                return {'data':pages[index],'nextCursor':'next' if index+1 < len(pages) else None}
            return {'terminated':True}
    class CancelPort:
        def task_detail(self, _): return task
        def agent(self, _): return a
        def connect_agent(self, _): return server
    server=NativeServer()
    result=WorkspaceMixin.native_command_action(CancelPort(), {'id':'task','action':'cancel'})
    output['cancellations'].append(dict(task=task,agent=a,pages=pages,calls=server.calls,result=result))

# Execute the deterministic Python identity/source paths for branch and
# restore. Full execution requires a real Git worktree and native fork I/O,
# which the guarded oracle deliberately blocks.
branch_data={'id':'branch-request','message_id':'lead:message','before':True}
restore_data={'checkpoint_id':'checkpoint','expectedTree':'tree-hash'}
a=agent()
output['forks'].append(dict(agent=a,source=WorkspaceMixin._workspace_source(a),
    branch=dict(data=branch_data,id=WorkspaceMixin._workspace_operation_id('branch','lead',branch_data)),
    restore=dict(data=restore_data,id=WorkspaceMixin._workspace_operation_id('restore','lead',restore_data),
                 signature=WorkspaceMixin._workspace_restore_signature('lead',restore_data)),
    operation=dict(id='branch-request',agent='lead',kind='branch',phase='provider_pending',
                   source=WorkspaceMixin._workspace_source(a),signature='frozen',created=1.0)))
print(json.dumps(output,ensure_ascii=False))
