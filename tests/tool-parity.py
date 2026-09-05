#!/usr/bin/env python3
"""Capture real Codex tool manifests with a local model fixture. No inference."""
import json
import re
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import AppServer, Runtime, TOOLS, THREAD_CONFIG

captures=[]
class Model(BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        raw=self.rfile.read(int(self.headers.get('Content-Length','0')))
        if self.headers.get('Content-Encoding')=='gzip':
            import gzip
            raw=gzip.decompress(raw)
        data=json.loads(raw)
        captures.append(data)
        item={'id':'msg-fixture','type':'message','role':'assistant','content':[{'type':'output_text','text':'OK'}]}
        events=[{'type':'response.created','response':{'id':'resp-fixture'}},
                {'type':'response.output_item.done','output_index':0,'item':item},
                {'type':'response.completed','response':{'id':'resp-fixture','output':[item],
                    'usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}}]
        body=''.join('data: '+json.dumps(e)+'\n\n' for e in events).encode()
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
root=Path(tempfile.mkdtemp(prefix='codex-tool-parity-'))
model=ThreadingHTTPServer(('127.0.0.1',0),Model)
threading.Thread(target=model.serve_forever,daemon=True).start()
home=root/'profile';home.mkdir()
cache=Path(os.environ.get('CODEX_HOME',str(Path.home()/'.codex')))/'models_cache.json'
if cache.exists():shutil.copyfile(cache,home/'models_cache.json')
(home/'config.toml').write_text(f'''model = "gpt-6-astra"
model_provider = "fixture"
sandbox_mode = "read-only"
approval_policy = "never"
[features]
multi_agent = true
[model_providers.fixture]
name = "Local test only"
base_url = "http://127.0.0.1:{model.server_port}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
''')
os.environ['CODEX_HOME']=str(home)
notifications=[]
def request(m):
    if m['method']=='currentTime/read':server.write({'id':m['id'],'result':{'currentTimeAt':int(time.time())}})
    else:server.write({'id':m['id'],'error':{'code':-32601,'message':'Unsupported fixture request'}})
server=AppServer(root,notifications.append,request,lambda:None)
try:
    for managed in (False,True):
        before=len(captures)
        params={'cwd':str(root),'model':'gpt-6-astra'}
        if managed:params.update(config=THREAD_CONFIG.copy(),dynamicTools=TOOLS)
        t=server.call('thread/start',params)['thread']['id']
        server.call('turn/start',{'threadId':t,'input':[{'type':'text','text':'Return OK without tools.'}]})
        end=time.monotonic()+35
        while time.monotonic()<end and len(captures)==before:time.sleep(.05)
        assert len(captures)>before, ('No model request',notifications[-5:])
        (root/('managed.json' if managed else 'native.json')).write_text(json.dumps(captures[-1],indent=2))
    def names(payload):
        result=set()
        def visit(tool,prefix=''):
            name=tool.get('name',tool.get('type','unknown'))
            if tool.get('type')=='namespace':
                for nested in tool.get('tools',[]):visit(nested,name+'.')
            else:result.add(prefix+name)
        for tool in payload.get('tools',[]):visit(tool)
        for item in payload.get('input',[]):
            if item.get('type')=='additional_tools':
                for tool in item.get('tools',[]):visit(tool)
        # Code mode publishes nested callable tools in its executor schema.
        result.update(re.findall(r'declare const tools: \{ ([a-zA-Z0-9_]+)\(', json.dumps(payload)))
        return result
    native,managed=map(names,captures[-2:])
    difference={'native':sorted(native),'managed':sorted(managed),'removed':sorted(native-managed),'added':sorted(managed-native)}
    print('Evidence:',root,flush=True)
    print(json.dumps(difference,indent=2),flush=True)
    (root/'result.json').write_text(json.dumps(difference,indent=2))
    assert native and managed, 'No tools captured'
    assert not any(n.startswith('collaboration.') for n in managed), 'Native agents bypass the managed scheduler'
    assert any('orchestration_monitor' in n for n in managed), 'Monitor missing from model manifest'
    assert all(any(part in n for part in ('spawn_agent','send_input','send_message','followup_task','wait_agent','list_agents','interrupt_agent','resume_agent','close_agent','collaboration')) for n in native-managed), 'An unrelated Codex tool was removed'
finally:
    server.close();model.shutdown()
