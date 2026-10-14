"""Actual Claude action writes with injected workspace and native ports."""
import sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import copy,json,sqlite3,threading
from types import SimpleNamespace
sys.path.insert(0,str(Path(__file__).resolve().parents[6]/"workspaces/runtime/apps/server/src"))
import codex_claude_controls as controls
from codex_workspace import WorkspaceMixin
controls.time.time=lambda:120.0

def case(kind,thread,options):
    agent=dict(id="agent",rootId="agent",epoch=2,cwd="/fixture",provider="claude",accountKey="account",threadId=thread,status="paused",autoWake=False)
    body=dict(id="agent",action=kind,request_id="control-one",**(dict(settings=options) if kind=="settings" else dict(turn_id="two")))
    native=dict(tasks=[],turns=[dict(id=k,status="completed") for k in ("one","two","three")]);queue=dict(data=[])
    result=dict(settings=options) if kind=="settings" else dict(threadId=thread,nativeId="native-restored")
    db=sqlite3.connect(":memory:");db.row_factory=sqlite3.Row
    db.executescript("CREATE TABLE runtime_events(agent TEXT,status TEXT); CREATE TABLE runtime_items(id TEXT,agent TEXT,record TEXT);")
    for turn in ("one","two","three"):db.execute("INSERT INTO runtime_items VALUES (?,?,?)",(turn,"agent",json.dumps(dict(id=turn,turnId=turn,role="assistant",text=turn))))
    operations={};stages=[]
    class Runtime(WorkspaceMixin):
        lock=threading.RLock();accounts=SimpleNamespace(get=lambda _:dict(provider="claude"));loaded=set(["agent"]);changed=threading.Event()
        def db(self):return db
        def checked_actor(self,db,key):return copy.deepcopy(agent)
        def _workspace_operations(self,db,key):return list(operations.values())
        def _workspace_operation(self,db,key):return copy.deepcopy(operations.get(key))
        def _workspace_idle_snapshot(self,db,a):return dict(idle=True)
        def _resolve_workspace_idle(self,snapshot,a):return snapshot
        def _assert_workspace_idle(self,*args,**kwargs):pass
        def _put_workspace_operation(self,db,op):
            operations[op['id']]=copy.deepcopy(op);stages.append(dict(agent=copy.deepcopy(agent),operation=copy.deepcopy(op)))
        def put(self,db,table,value):agent.clear();agent.update(copy.deepcopy(value))
        def connect_agent(self,a):return SimpleNamespace(call=lambda method,params,**kw:copy.deepcopy(native if method=="claude/state" else queue if method=="thread/queue/list" else result))
    rt=Runtime();before=copy.deepcopy(agent);returned=controls.action(rt,body)
    final_items=[json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_items ORDER BY id")]
    db.close()
    return dict(name=f"{kind}-{thread}-{options}",body=body,agent=before,native=native,queue=queue,provider=result,stages=stages,expected=dict(agent=agent,operation=list(operations.values())[0],result=returned,items=final_items))
cases=[case("settings",thread,options) for thread in (None,"thread") for options in ({},{"permissionMode":"plan"},{"permissionMode":"bypassPermissions","thinking":True},{"autoCompactWindow":100000},{"autoCompactWindow":None})]+[case("rollback","thread",None)]
settings=[]
for value in [None,[],{},dict(thinking=1),dict(thinking=None),dict(permissionMode="unknown"),dict(permissionMode=None),dict(autoCompactWindow=True),dict(autoCompactWindow=99999),dict(autoCompactWindow=1000000),dict(autoCompactWindow=1000001),dict(unknown=True)]:
    try:result=controls._settings(value);error=False
    except ValueError:result=None;error=True
    settings.append(dict(input=value,expected=result,error=error))
reads=[]
for kind, thread in (("state",None),("state","thread"),("commands","thread"),("stop_task","thread")):
    agent=dict(id="agent",rootId="agent",epoch=2,cwd="/fixture",provider="claude",accountKey="account",threadId=thread,status="paused")
    body=dict(id="agent",action=kind,task_id="task")
    connection=sqlite3.connect(":memory:")
    native=dict(tasks=[],turns=[dict(id=k,status="completed") for k in ("one","two","three")])
    class ReadRuntime:
        lock=threading.RLock();accounts=SimpleNamespace(get=lambda _:dict(provider="claude"))
        def db(self):return connection
        def checked_actor(self,db,key):return copy.deepcopy(agent)
        def _workspace_operations(self,db,key):return []
        def connect(self,key):return self.connect_agent(agent)
        def connect_agent(self,a):return SimpleNamespace(call=lambda method,params,**kw:copy.deepcopy(native if method=="claude/state" else dict(commands=["/fixture"]) if method=="claude/commands" else dict(stopped=True)))
    reads.append(dict(agent=agent,body=body,expected=controls.action(ReadRuntime(),body)))
    connection.close()
print(json.dumps(dict(cases=cases,settings=settings,reads=reads)))
if '--store-template' in sys.argv:
    root=Path(sys.argv[sys.argv.index('--store-template')+1])
    cwd=Path(sys.argv[sys.argv.index('--cwd')+1])
    schema=json.loads((Path(__file__).resolve().parents[3]/'runtime-store/tests/fixtures/live-state-schema.json').read_text())
    for name in ('canvas.sqlite3','analytics.sqlite3'):
        connection=sqlite3.connect(root/name)
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("PRAGMA synchronous=FULL")
        connection.execute("BEGIN")
        for item in sorted(schema[name]['sqlite_master'],key=lambda v:dict(table=0,index=1,trigger=2).get(v['type'],3)):
            if item['name'] in ('runtime_search_next_config','runtime_search_next_data','runtime_search_next_docsize','runtime_search_next_idx'):continue
            if item.get('sql'):connection.execute(item['sql'])
        if name=='canvas.sqlite3':
            actor=copy.deepcopy(cases[-1]['agent']);actor['cwd']=str(cwd)
            connection.execute('INSERT INTO runtime_agents(id,record) VALUES (?,?)',('agent',json.dumps(actor)))
            for turn in ('one','two','three'):
                record=dict(id=turn,agent='agent',turnId=turn,role='assistant',text=turn)
                connection.execute('INSERT INTO runtime_items(id,agent,created,record) VALUES (?,?,?,?)',(turn,'agent',100,json.dumps(record)))
        connection.commit();connection.close()
