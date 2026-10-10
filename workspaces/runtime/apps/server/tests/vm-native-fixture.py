#!/usr/bin/env python3
"""Studio spawn and native provider contracts with a fake guest transport."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location('linux_runtime_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_process_supervisor import Supervisor

FAKE_NATIVE = r'''import json,sys
for line in sys.stdin:
    message=json.loads(line)
    if 'id' not in message: continue
    method=message.get('method')
    if method=='initialize': result={'userAgent':'linux-fixture/1'}
    elif method in ('thread/start','thread/resume','thread/read'):
        result={'thread':{'id':'linux-thread','turns':[],'status':{'type':'idle'}}}
    else: result={'ok':True}
    print(json.dumps({'id':message['id'],'result':result}),flush=True)
'''


class NativeGuest:
    def __init__(self, root):
        self.supervisor = Supervisor(root / 'guest-native')
        self.script = root / 'fake-native.py'
        self.script.write_text(FAKE_NATIVE)
        self.calls = []
        self.fail = False
        self.starts = []

    def request(self, method, params=None, **options):
        self.calls.append((method, copy.deepcopy(params), options))
        if self.fail:
            raise ConnectionError('The VM transport is unavailable')
        if method == 'health': return {'protocol':1}
        if method == 'provider.start':
            self.starts.append(copy.deepcopy(params))
            return self.supervisor.handle({'action':'open','handle':params['handle'],
                'command':[sys.executable,str(self.script)],'env':dict(os.environ),'cwd':str(self.script.parent)})
        if method == 'provider.list':
            return {'providers':[{'handle':key,'state':'running' if child.process.poll() is None else 'exited'}
                                 for key,child in self.supervisor.children.items()]}
        if method == 'provider.stop':
            row = next(row for row in self.supervisor.handle({'action':'health'})['handles'] if row['id']==params['handle'])
            child = self.supervisor.children[params['handle']]
            result = self.supervisor.handle({'action':'adminCloseHandle','handle':params['handle'],
                'expectedPid':row['pid'],'expectedStartTime':row['startTime'],'expectedSignature':row['signature']})
            self.close_streams(child)
            return result
        if method == 'provider.rpc':
            return self.supervisor.handle(params)
        raise AssertionError(method)

    @staticmethod
    def close_streams(child):
        child.reader.join(timeout=5)
        child.stderr.join(timeout=5)
        for stream in (child.process.stdin,child.process.stdout,child.process.stderr):
            stream.close()

    def close(self):
        for child in self.supervisor.children.values():
            child.process.terminate()
            child.process.wait(timeout=5)
            self.close_streams(child)
        if self.supervisor.journal:
            self.supervisor.journal.close()
