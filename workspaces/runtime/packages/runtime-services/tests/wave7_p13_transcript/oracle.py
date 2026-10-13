"""Real Python notification writers and transcript readers; no live provider."""
import sys
from pathlib import Path
HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parents[1] / 'turn_lifecycle'))
from oracle_guard import install_guard
install_guard(block_processes=True)

import concurrent.futures
import json
import os
import shutil
import sqlite3
import threading
from types import SimpleNamespace
from unittest.mock import patch
SERVER = HERE.parents[4] / 'apps/server'
sys.path.insert(0, str(SERVER / 'src'))
import codex_runtime
import codex_accounts
import codex_streaming
from codex_runtime import Runtime
from codex_sync import SyncStore

class QuietRuntime(Runtime):
    def paired_access(self): return SimpleNamespace(local_server_id="fixture-server")
    def schedule(self): pass
    def start_review_archives(self): pass
    def resume_read_only_image_bases(self): pass
    def connect(self, *args, **kwargs): raise AssertionError('provider forbidden')
    connect_agent = connect

class ManualTimer:
    def __init__(self, *args): pass
    def start(self): pass
    def cancel(self): pass


def clone(runtime, destination):
    destination.mkdir()
    for name in ('canvas.sqlite3', 'analytics.sqlite3'):
        with sqlite3.connect(runtime.root / name) as source, sqlite3.connect(destination / name) as target:
            source.backup(target)


def review_cases(runtime, root):
    cases = []
    def snapshot(name, *, pages=True, error=None, extra=None):
        destination = root / name
        clone(runtime, destination)
        case = {'path':str(destination), 'phase':name, 'now':1000.0, 'pages':[]}
        if pages:
            with patch('time.time', return_value=1000.0):
                case['pages'] = [{'request':{}, 'value':runtime.transcript('agent')}]
        if error:
            case['pageError'] = error
            try:
                with patch('time.time', return_value=1000.0):
                    value = runtime.transcript('agent')
                case['pythonOutcome'] = {'items':len(value['items'])}
            except Exception as exc:
                case['pythonOutcome'] = {'error':str(exc)}
        if extra: case.update(extra)
        cases.append(case)
        return case

    # Search every supported view and room visibility branch from Python-written records.
    with runtime.db() as db:
        actor = runtime.agent('agent', db)
        runtime.put(db, 'agents', {**actor, 'id':'gone', 'rootId':'agent', 'deletedAt':1.0})
        runtime.put(db, 'projects', {'id':str(root), 'path':str(root), 'name':'fixture',
                                   'peerTeams':[{'id':'peers','name':'Peers','members':['agent','other']}]})
        for table, record in [
            ('work', {'id':'work','rootId':'agent','title':'needle work','status':'ready'}),
            ('work', {'id':'child-work','rootId':'child','title':'needle child root','status':'ready'}),
            ('plans', {'id':'agent','agent':'agent','rootId':'agent','text':'needle plan','version':1,'updated':1.0}),
            ('complaints', {'id':'complaint','leadId':'agent','title':'needle complaint','text':'needle detail','status':'open'}),
        ]: runtime.put(db, table, record)
        for identity, kind, members in [('peer','private',['agent','other']), ('unavailable','private',['agent','gone']),
                                         ('approved','federated',['agent']), ('unapproved','federated',['agent'])]:
            runtime.put(db, 'rooms', {'id':identity,'kind':kind,'rootId':'agent','members':members,'updated':1.0})
            if kind == 'federated':
                record = {'id':identity,'kind':'federated','status':'approved' if identity=='approved' else 'pending',
                          'localApproved':True,'remoteApproved':identity=='approved','localMembers':['agent'],
                          'remoteMembers':[],'peerId':'remote','peerLabel':'Remote','name':'Federated',
                          'shareNames':False,'shareStatus':False,'created':800.0,'updated':800.0}
                db.execute('INSERT INTO runtime_federation_rooms(id,record) VALUES (?,?)', (identity,json.dumps(record)))
            db.execute('INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)',
                       ('chat-'+identity,identity,'agent','needle '+identity,800.0,'{}'))
        runtime.enqueue(db, actor, 'user', 'needle legacy durable body', 'legacy-event')
        db.execute("UPDATE runtime_events SET status='delivered' WHERE id='legacy-event'")
        runtime.item(db,'agent','legacy','user','needle legacy durable body','You',
                     inputs=[{'id':'legacy-event','kind':'user','text':'short','truncated':True}],at=850.0)
        db.execute("UPDATE runtime_items SET record=json_remove(record,'$.text') WHERE id='agent:legacy'")
        db.execute("DELETE FROM runtime_item_fulltext WHERE id='agent:legacy'")
    snapshot('search-views', extra={
        'search':runtime.search_work('needle'), 'scoped':runtime.search_work('needle','agent'),
        'item':runtime.search_item('agent:answer'), 'roomItem':runtime.search_item('chat'),
        'searchItems':{identity:runtime.search_item(identity) for identity in ('agent:legacy','work','agent','complaint','chat-peer','chat-approved')},
        'unavailableItems':['chat-unavailable','chat-unapproved'],
    })
    # Explicit displayed-input cursor and unknown cursor, both with a one-item page bound.
    case = snapshot('input-cursor', pages=False)
    case['pages'] = [{'request':{'around':'agent:legacy-event','limit':1},
                      'value':runtime.transcript('agent',around='agent:legacy-event',limit=1)}]
    case['unknownCursor'] = 'agent:missing-input'

    # A writer commits on the other side of a one-item page boundary.
    request = {'after':'agent:old-7','limit':1}
    before = runtime.transcript('agent', **request)
    case = snapshot('concurrent-append', pages=False)
    with runtime.db() as db:
        runtime.item(db,'agent','concurrent','assistant','concurrent append','Agent',at=150.0)
        db.execute("UPDATE runtime_items SET created=150.0 WHERE id='agent:concurrent'")
        record = json.loads(db.execute("SELECT record FROM runtime_items WHERE id='agent:concurrent'").fetchone()[0])
    case['append'] = {'request':request,'before':before,'after':runtime.transcript('agent',**request),'record':record,'created':150.0}

    with runtime.db() as db:
        answer = db.execute("SELECT record FROM runtime_items WHERE id='agent:answer'").fetchone()[0]
        db.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus','failed') WHERE id='agent:answer'")
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        turn = {'id':'agent:thread:turn','agentId':'agent','threadId':'thread','turnId':'turn',
                'status':'failed','error':{'message':'fixture failure'},'at':300.0}
        analytics.execute('INSERT INTO analytics_turns(id,agent,root,at,record) VALUES (?,?,?,?,?)',
                          (turn['id'],'agent','agent',300.0,json.dumps(turn)))
    snapshot('failed-turn')
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        analytics.execute("UPDATE analytics_turns SET record='[]' WHERE id='agent:thread:turn'")
    snapshot('malformed-turn', pages=False, error='invalid stored object analytics_turns/agent:thread:turn')
    with runtime.db() as db: db.execute("UPDATE runtime_items SET record=? WHERE id='agent:answer'",(answer,))
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        analytics.execute("DELETE FROM analytics_turns WHERE id='agent:thread:turn'")
        original = analytics.execute("SELECT record FROM analytics_items WHERE id='measured'").fetchone()[0]
    for name, invalid in [('array','[]'),('empty','{}'),('wrong-start',json.dumps({**json.loads(original),'startedAt':'wrong'}))]:
        with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
            analytics.execute("UPDATE analytics_items SET record=? WHERE id='measured'",(invalid,))
        snapshot('malformed-reasoning-'+name, pages=False, error='invalid stored object analytics_items/measured')
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        analytics.execute("UPDATE analytics_items SET record=? WHERE id='measured'",(original,))

    with runtime.db() as db:
        runtime.put(db,'assets',{'id':'attachment','agent':'gone','name':'fixture.txt','path':str(root/'absent'),
                                 'size':1,'image':False,'mime':'text/plain','hash':'sha256:fixture','created':1.0})
        oldmeta = db.execute("SELECT record FROM runtime_event_meta WHERE id='pending'").fetchone()
        db.execute("INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES ('pending',?)",(json.dumps({'assets':['attachment']}),))
    snapshot('deleted-asset-owner', pages=False, error='This agent was deleted')
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_event_meta WHERE id='pending'")
        if oldmeta: db.execute("INSERT INTO runtime_event_meta VALUES ('pending',?)",(oldmeta[0],))
        tool = db.execute("SELECT result FROM runtime_tool_results WHERE id='thread:call'").fetchone()[0]
        from codex_payloads import store_bytes
        content = [{'type':'inputText','text':'external tool payload'}]
        ref = store_bytes(runtime.root, json.dumps(content).encode(), db=db)
        db.execute("UPDATE runtime_tool_results SET result=? WHERE id='thread:call'",(json.dumps({'success':True,'contentItems':[], '_payloadBlobs':{'contentItems':ref}}),))
    snapshot('external-payload', pages=False, error='transcript operation requires transcript/payload-store')
    with runtime.db() as db: db.execute("UPDATE runtime_tool_results SET result=? WHERE id='thread:call'",(tool,))

    # Named Rust bounds intentionally differ from Python's unbounded projection.
    with runtime.db() as db:
        for index in range(499):
            runtime.enqueue(db, runtime.agent('agent',db), 'user','bound receipt',f'bound-{index}')
            db.execute('UPDATE runtime_events SET created=900.0 WHERE id=?',(f'bound-{index}',))
    snapshot('receipt-at-limit')
    with runtime.db() as db:
        runtime.enqueue(db, runtime.agent('agent',db), 'user','bound receipt','bound-over')
    snapshot('receipt-over-limit', pages=False, error='transcript operation requires transcript/receipt-page')
    with runtime.db() as db: db.execute("DELETE FROM runtime_events WHERE id LIKE 'bound-%'")
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        saved = list(analytics.execute("SELECT * FROM analytics_items WHERE type='reasoning'"))
        analytics.execute("DELETE FROM analytics_items WHERE type='reasoning'")
        for index in range(8192):
            runtime.analytics_store_item(analytics, {'id':f'bound-{index}','agentId':'agent','rootId':'agent','threadId':'thread',
                'turnId':'turn','at':201.0,'startedAt':201.0,'finishedAt':203.0,'type':'reasoning','name':'reasoning','isTool':False})
    snapshot('reasoning-at-limit')
    with runtime.db(analytics=True) as db, runtime.analytics_connection(db) as analytics:
        runtime.analytics_store_item(analytics, {'id':'bound-over','agentId':'agent','rootId':'agent','threadId':'thread',
            'turnId':'turn','at':201.0,'startedAt':201.0,'finishedAt':203.0,'type':'reasoning','name':'reasoning','isTool':False})
    snapshot('reasoning-over-limit', pages=False, error='transcript operation requires transcript/reasoning-page')
    return cases


def run(root):
    root.mkdir(exist_ok=True)
    runtime = QuietRuntime(root / 'python', lambda *a, **k: None)
    sync=SyncStore(runtime.db,runtime.transcript,runtime=runtime)
    sync.generation()
    runtime.connection_current = lambda *a: True
    runtime.analytics_safe = lambda *a, **k: None
    runtime._dispatch_token_rate_observation = lambda *a, **k: None
    runtime._stream_buffer = codex_streaming.StreamBuffer(runtime)
    actor = dict(id='agent', rootId='agent', epoch=1, status='running', cwd=str(root),
                 threadId='thread', accountKey='default', turnId='turn', inFlight=True,
                 events=0, activeTools=[], autoWake=True, isLead=True, name='Transcript fixture',
                 agentMode='multi', agentModeRevision=0, agentModeSupported=True,
                 concurrency=32, maxAgentsExplicit=False, subagentConcurrencyVersion=2)
    with runtime.db() as db:
        runtime.put(db, 'agents', actor)
        runtime.put(db, 'agents', {**actor, 'id':'other', 'rootId':'other', 'threadId':'other-thread'})
        runtime.put(db, 'agents', {**actor, 'id':'child', 'rootId':'agent', 'parentId':'agent', 'isLead':False, 'threadId':'child-thread'})
        db.execute("CREATE VIRTUAL TABLE IF NOT EXISTS runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61')")
        for i in range(8):
            runtime.item(db, 'agent', f'old-{i}', 'assistant', f'needle history {i}', 'Agent', turnId='old-turn', at=100.0+i)
            db.execute('UPDATE runtime_items SET created=? WHERE id=?', (100.0+i, f'agent:old-{i}'))
        runtime.item(db,'other','other','assistant','needle foreign','Agent')
        runtime.enqueue(db, actor, 'user', 'Needle submitted input', 'input')
        db.execute("UPDATE runtime_events SET status='delivered',created=200.0 WHERE id='input'")
        runtime.item(db,'agent','input','user','Needle submitted input','You', inputs=[{'id':'input','kind':'user','text':'Needle submitted input','truncated':False,'at':200.0,'assets':[]}], turnId='turn',at=200.0)
        db.execute("UPDATE runtime_items SET created=200.0 WHERE id='agent:input'")
        runtime.put(db,'rooms',{'id':'private','kind':'private','rootId':'agent','members':['agent','child'],'updated':1.0})
        db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES ('chat','private','child','needle room',210.0,'{}')")
    with patch.object(codex_streaming.threading,'Timer',ManualTimer), patch('time.time', return_value=300.0):
        for frame in [
            {'method':'item/started','params':{'threadId':'thread','turnId':'turn','item':{'id':'answer','type':'agentMessage','text':''}}},
            {'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'answer','delta':'Needle streamed '}},
            {'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'answer','delta':'answer'}},
            {'method':'item/started','params':{'threadId':'thread','turnId':'turn','item':{'id':'command','type':'commandExecution','command':'printf needle','processId':'42'}}},
            {'method':'item/commandExecution/outputDelta','params':{'threadId':'thread','turnId':'turn','itemId':'command','delta':'needle tool output'}},
            {'method':'item/completed','params':{'threadId':'thread','turnId':'turn','item':{'id':'command','type':'commandExecution','exitCode':0}}},
            {'method':'item/completed','params':{'threadId':'thread','turnId':'turn','item':{'id':'compact','type':'contextCompaction'}}},
        ]:
            runtime.notification(frame,'default','connection-7')
        with runtime.db() as db:
            runtime._stream_buffer.flush_locked(db, force=True)
    requests = [{}, {'limit':3}, {'before':'agent:old-6','limit':2}, {'around':'agent:old-4','limit':3}, {'after':'agent:old-2','limit':2}, {'before':'agent:input','limit':3}, {'around':'agent:input','limit':3}, {'after':'agent:old-7','limit':1}]
    if os.environ.get('STUDIO_RUNTIME_CORPUS_FULL'):
        requests.extend({'limit':limit, cursor:f'agent:old-{i}'} for limit in (1,2,5,500) for cursor in ('before','around','after') for i in range(8))
    cases=[]
    for phase,status,auto in [('live','running',True),('stopped','running',False),('ended','idle',True),('approval','approval',True)]:
        with runtime.db() as db:
            current=runtime.agent('agent',db);current.update(status=status,autoWake=auto);runtime.put(db,'agents',current)
            if phase=='ended': db.execute("INSERT OR IGNORE INTO runtime_completed_turns(id) VALUES ('agent:turn')")
        destination=root / phase
        clone(runtime,destination)
        cases.append({'path':str(destination),'phase':phase,'pages':[{'request':request,'value':runtime.transcript('agent',**request)} for request in requests],
                      'search':runtime.search_work('needle'), 'scoped':runtime.search_work('needle','agent'),
                      'item':runtime.search_item('agent:answer'), 'roomItem':runtime.search_item('chat')})
    if os.environ.get('STUDIO_RUNTIME_CORPUS_FULL'):
        for status in ('reserved','dispatching','uncertain','failed','cancelled'):
            with runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=?,error=? WHERE id=?',(status,'fixture delivery error' if status=='failed' else None,'input'))
                db.execute("INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES (?,?)",('input',json.dumps({'requestedDelivery':'steer','transcriptItemId':'agent:input'})))
            destination=root/('receipt-'+status);clone(runtime,destination)
            cases.append({'path':str(destination),'phase':'receipt-'+status,'pages':[{'request':{},'value':runtime.transcript('agent')}]})
        with runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered',error=NULL WHERE id='input'")
    with runtime.db() as db:
        runtime.enqueue(db, runtime.agent('agent',db), 'user', 'pending receipt needle', 'pending')
        db.execute("UPDATE runtime_events SET created=500.0 WHERE id='pending'")
    pending=root/'pending';clone(runtime,pending)
    cases.append({'path':str(pending),'phase':'pending','pages':[{'request':{},'value':runtime.transcript('agent')}]})
    # A restart sees exactly the committed Python snapshot; sync cursor generation
    # uses the production SyncStore on a copy of that same prestate.
    payload=runtime.transcript('agent')
    revision=sync.transcript_revision('agent')
    pull=sync.transcript_pull('transcript:agent',0,payload,False,revision)
    cases[-1]['pull']=pull
    cases[-1]['revision']=revision
    # Differential update/tombstone and prune/floor transitions, starting from
    # Python's committed checkpoint and using the original delta cursor.
    cursor = pull['checkpoint']['seq']
    with runtime.db() as db:
        item = json.loads(db.execute("SELECT record FROM runtime_items WHERE id='agent:answer'").fetchone()[0])
        item['text'] = 'Needle updated answer'
        db.execute("UPDATE runtime_items SET record=? WHERE id='agent:answer'", (json.dumps(item),))
        db.execute("UPDATE runtime_item_fulltext SET body=? WHERE id='agent:answer'",(item['text'],))
        db.execute("DELETE FROM runtime_items WHERE id='agent:old-0'")
    destination=root/'update';clone(runtime,destination)
    payload=runtime.transcript('agent')
    cases.append({'path':str(destination),'phase':'update','pages':[{'request':{},'value':payload}],
                  'pullAfter':cursor,'faults':[['UPDATE',key] for key in ('item:agent:answer','item:agent:old-0','@order','@revision')]+[['CACHE','@revision']],
                  'pull':sync.transcript_pull('transcript:agent',cursor,payload,False,sync.transcript_revision('agent'))})
    cursor=cases[-1]['pull']['checkpoint']['seq']
    from codex_sync_entities import next_sequence
    with runtime.db() as db:
        for index in range(513):
            db.execute('INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES (?,?,?,?,NULL,1)',
                       ('transcript:agent',f'item:stale-{index}',next_sequence(db),'stale'))
    destination=root/'prune';clone(runtime,destination)
    cases.append({'path':str(destination),'phase':'prune','pages':[{'request':{},'value':payload}],
                  'pullAfter':cursor,'faults':[['DELETE','item:stale-0'],['DELETE','item:agent:old-0'],['UPDATE','@floor'],['CACHE','@revision']],
                  'pull':sync.transcript_pull('transcript:agent',cursor,payload,False,sync.transcript_revision('agent'))})
    # Real durable tool receipt recovers a missing provider item completion.
    with runtime.db() as db:
        runtime.item(db,'agent','dynamic','tool',json.dumps({'id':'call','type':'dynamicToolCall','status':'inProgress','contentItems':None}),'dynamicToolCall',turnId='turn',toolStatus='running',at=600.0)
        db.execute("UPDATE runtime_items SET created=600.0 WHERE id='agent:dynamic'")
        db.execute("INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES ('agent','call','thread:call')")
        db.execute('INSERT INTO runtime_tool_results(id,result) VALUES (?,?)',('thread:call',json.dumps({'success':True,'contentItems':[{'type':'inputText','text':'needle durable tool receipt'}]})))
    destination=root/'tool-receipt';clone(runtime,destination)
    cases.append({'path':str(destination),'phase':'tool-receipt','pages':[{'request':{},'value':runtime.transcript('agent')}]})
    with runtime.analytics_read_db() as _read:
        pass
    with runtime.db(analytics=True) as db:
        with runtime.analytics_connection(db) as analytics:
            for identity,start,end in [('measured',201.0,203.0),('overlap',202.0,204.0),('observed',205.0,None)]:
                runtime.analytics_store_item(analytics,{'id':identity,'agentId':'agent','rootId':'agent','threadId':'thread','turnId':'turn','at':start,'startedAt':start,'finishedAt':end,'type':'reasoning','name':'reasoning','isTool':False})
    destination=root/'reasoning';clone(runtime,destination)
    with patch('time.time',return_value=1000.0):
        expected=runtime.transcript('agent')
    assert any(item['role']=='reasoning' for item in expected['items'])
    cases.append({'path':str(destination),'phase':'reasoning','now':1000.0,'pages':[{'request':{},'value':expected}]})
    # Canvas.transcript's managed fast path never opens an image workspace.
    with runtime.db() as db:
        current = runtime.agent('agent', db)
        current['workspaceMode'] = 'image'
        runtime.put(db, 'agents', current)
    destination = root / 'managed-image'
    clone(runtime, destination)
    with patch('time.time', return_value=1000.0):
        expected = runtime.transcript('agent')
    cases.append({'path':str(destination),'phase':'managed-image','now':1000.0,'pages':[{'request':{},'value':expected}]})
    cases.extend(review_cases(runtime, root))
    runtime.close()
    return cases

with patch.object(codex_accounts,'codex_home',lambda:Path(os.environ['CODEX_HOME'])), \
     patch.object(codex_accounts,'auth_metadata',lambda *a:{'status':'signedOut','accountId':None,'email':None,'plan':None}), \
     patch.object(concurrent.futures.ThreadPoolExecutor,'submit',lambda *a,**k:concurrent.futures.Future()), \
     patch.object(threading.Thread,'start',lambda self:None), \
     patch.object(threading.Thread,'join',lambda self,*a,**k:None), \
     patch.object(codex_runtime,'_RuntimeWalKeeper',lambda *a:None):
    print(json.dumps(run(Path(sys.argv[1]))))
