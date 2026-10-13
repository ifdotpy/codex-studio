#!/usr/bin/env python3
"""One Python-written provider turn delivered through either backend after restart."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
import sqlite3
import unittest
from contextlib import closing
from urllib.parse import urlencode
from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot

def wait_for_agent_preparation(backend, agent_id):
    """Local #246 helper until BackendProcess exposes this on the base."""
    if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
        return
    if backend._provider_hold_method != "thread/start":
        raise RuntimeError("Agent preparation needs a held thread/start provider gate")

    def prepared():
        if not backend.provider_gate_started():
            return False
        with closing(sqlite3.connect(f"file:{backend.state / 'canvas.sqlite3'}?mode=ro", uri=True)) as database:
            row = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()
        agent = json.loads(row[0]) if row else {}
        return bool(agent.get("prepareAttempt"))

    backend.wait_until(prepared, f"agent {agent_id} held at provider thread/start")

class TranscriptDeliveryHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        command=os.environ.pop('STUDIO_HTTP_BACKEND_COMMAND',None)
        try:
            cls.backend=start_contract_backend(cls,'restart-mid-turn-supervisor-replay.json',hold_provider_method='thread/start')
            cls.headers={'Origin':cls.backend.origin or '', 'X-Canvas-Token':cls.backend.session['token'], 'X-Studio-API-Schema':cls.backend.schema_hash}
            response=cls.backend.request('POST','/api/leads',body={'cwd':str(cls.backend.cwd),'workspaceMode':'worktree'},headers=cls.headers)
            assert response.status==200,response.body
            cls.agent=response.json()['id']
            response=cls.backend.request('POST','/api/messages',body={'id':'bf7154d4-c4a6-4b0c-8031-614ad88f7101','room':cls.agent,'text':'Reply with fixture output.','delivery':'queue','assets':[]},headers=cls.headers)
            assert response.status==200,response.body
            wait_for_agent_preparation(cls.backend, cls.agent)
            cls.backend.release_provider_gate()
            def completed():
                with closing(sqlite3.connect(cls.backend.state/'canvas.sqlite3')) as db:
                    row=db.execute('SELECT record FROM runtime_agents WHERE id=?',(cls.agent,)).fetchone()
                    if not row: return False
                    agent=json.loads(row[0])
                    identity={key:agent.get(key, 'default' if key=='accountKey' else None) for key in ('accountKey','threadId','name')}
                    return agent.get('status')=='completed' and agent.get('nativeNameSynced')==identity
            cls.backend.wait_until(completed,'provider replay and native-name receipt complete')
            # Retire only the disposable replay actor before exact no-write proofs;
            # completion alone still permits asynchronous preparation callbacks.
            response=cls.backend.request('POST','/api/stop',body={'id':cls.agent,'descendants':False},headers=cls.headers)
            assert response.status==200,response.body
            with closing(sqlite3.connect(cls.backend.state/'canvas.sqlite3')) as db:
                cls.identity=db.execute("SELECT id FROM runtime_items WHERE agent=? AND record LIKE '%Recovered output.%'",(cls.agent,)).fetchone()[0]
            cls.paths=[
                '/api/transcript?'+urlencode({'id':cls.agent}),
                '/api/transcript/page?'+urlencode({'id':cls.agent,'limit':1}),
                '/api/transcript/page?'+urlencode({'id':cls.agent,'around':cls.identity,'limit':1}),
                '/api/search?'+urlencode({'q':'Recovered output.'}),
                '/api/search/item?'+urlencode({'id':cls.identity}),
            ]
            cls.expected={path:cls.backend.request('GET',path,headers=cls.headers).json() for path in cls.paths}
            cls.backend.stop()
            if command: os.environ['STUDIO_HTTP_BACKEND_COMMAND']=command
            cls.backend.ready_samples.append(cls.backend.start())
            session=cls.backend.request('GET','/api/session')
            assert session.status==200,session.body
            cls.headers['X-Canvas-Token']=session.json()['token']
            cls.headers['Origin']=cls.backend.origin or ''
        finally:
            if command: os.environ['STUDIO_HTTP_BACKEND_COMMAND']=command

    def test_reader_contract_and_sync_cursor_after_restart(self):
        before=no_write_snapshot(self.backend.state)
        for path in self.paths:
            with self.subTest(path=path):
                response=self.backend.request('GET',path,headers=self.headers)
                self.assertEqual(response.status,200,response.body)
                self.assertEqual(response.json(),self.expected[path])
        invalid=self.backend.request('GET','/api/transcript/page?'+urlencode({'id':self.agent,'before':self.identity,'after':self.identity}),headers=self.headers)
        self.assertEqual(invalid.status,400,invalid.body)
        if os.environ.get('STUDIO_HTTP_BACKEND_COMMAND'):
            native = self.backend.request('GET', '/api/transcript?id=native-unmanaged-thread', headers=self.headers)
            self.assertEqual(native.status, 501, native.body)
            self.assertEqual(native.json(), {'error':'Not implemented: requires transcript/workspace-state'})
        self.assertEqual(before,no_write_snapshot(self.backend.state),'history/search reads wrote state')
        scope='transcript:'+self.agent
        response=self.backend.request('GET','/api/sync/pull?'+urlencode({'scope':scope,'after':0}),headers=self.headers)
        self.assertEqual(response.status,200,response.body)
        pull=response.json()
        self.assertEqual(json.loads(pull['documents'][0]['payload']),self.expected[self.paths[0]])
        seq=pull['checkpoint']['seq']
        before=no_write_snapshot(self.backend.state)
        response=self.backend.request('GET','/api/sync/pull?'+urlencode({'scope':scope,'after':seq}),headers=self.headers)
        self.assertEqual(response.status,200,response.body)
        self.assertEqual(response.json()['documents'],[])
        self.assertEqual(response.json()['checkpoint'],{'seq':seq})
        self.assertEqual(before,no_write_snapshot(self.backend.state),'unchanged sync pull rewrote checkpoint')

if __name__=='__main__': unittest.main(verbosity=2)
