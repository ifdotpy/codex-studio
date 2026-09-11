#!/usr/bin/env python3
"""Restart preserves admitted work without replaying unknown operations."""
import copy
import importlib.util
import json
import subprocess
import sys
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('connection-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_connection_recovery import recover
from codex_restart_recovery import capture, restore


class RestartContract(fixture.ConnectionRecoveryContract):
    def restart(self, **values):
        self.update(status='running', autoWake=True, inFlight=True, error=None, **values)
        native = copy.deepcopy(self.server.native)
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.native = native
        self.server.calls.clear()
        return self.runtime.agent(self.key)

    def test_real_restart_restores_completed_delivery(self):
        a = self.restart()
        self.assertEqual(a['disconnectRecovery']['source'], 'restart')
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
        a = self.runtime.agent(self.key)
        self.assertTrue(a['autoWake'])
        self.assertEqual(a['lastAnswer'], 'Full final answer')
        self.assertEqual(a['status'], 'completed')
        self.read_calls_only()

    def test_interrupted_turn_continues_once_across_second_restart(self):
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.restart()
        result = recover(self.runtime, self.key, automatic=True)
        self.assertTrue(result['continued'])
        a = self.runtime.agent(self.key)
        self.assertEqual(a['status'], 'queued')
        self.assertTrue(a['autoWake'])
        key = a['restartRecovery']['eventId']
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        with self.runtime.db() as db:
            events = db.execute("SELECT id,status FROM runtime_events WHERE id=?", (key,)).fetchall()
        self.assertEqual([(r[0],r[1]) for r in events], [(key,'pending')])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'queued')

    def test_unsubmitted_input_restores_exact_batch(self):
        with self.runtime.db() as db:
            a=self.runtime.agent(self.key,db)
            self.runtime.enqueue(db,a,'user','Never submitted','reserved-input')
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='reserved-input'")
        self.restart(startAttempt={'id':'attempt','epoch':self.a['epoch'],'accountKey':'default',
                                   'submitted':False,'events':['reserved-input']})
        a=self.runtime.agent(self.key)
        self.assertEqual(a['status'],'queued')
        self.assertTrue(a['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='reserved-input'").fetchone()[0],'pending')

    def test_submitted_input_without_turn_is_held(self):
        self.update(turnId=None)
        self.restart(startAttempt={'id':'attempt','epoch':self.a['epoch'],'accountKey':'default',
                                   'submitted':True,'events':['possibly-submitted']})
        a=self.runtime.agent(self.key)
        self.assertFalse(a['autoWake'])
        self.assertEqual(a['restartRecovery']['stage'],'held')

    def test_explicit_pause_after_capture_is_not_undone(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        capture(a)
        with self.runtime.db() as db:self.runtime.put(db,'agents',a)
        self.runtime.stop(self.key,descendants=False)
        self.runtime.close()
        self.runtime=fixture.Runtime(Path(self.temp.name),fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        a=self.runtime.agent(self.key)
        self.assertEqual(a['status'],'paused')
        self.assertFalse(a['autoWake'])

    def test_lost_command_does_not_auto_continue(self):
        self.server.native['turns'][0]['status']='interrupted'
        with self.runtime.db() as db:
            self.runtime.put(db,'tasks',{'id':'lost-command','agent':self.key,'status':'running','kind':'command'})
        self.restart()
        self.assertNotIn('continued',recover(self.runtime,self.key,automatic=True))
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db,'tasks')[0]['status'],'lost')

    def test_unobserved_native_command_blocks_continuation(self):
        self.server.native['turns'][0].update(status='interrupted',items=[
            {'id':'missed-command','type':'commandExecution','status':'inProgress'}])
        self.restart()
        self.assertNotIn('continued',recover(self.runtime,self.key,automatic=True))
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])

    def test_abrupt_process_exit_restores_only_committed_unsent_input(self):
        child_root=Path(self.temp.name)/'crashed-runtime'
        script="""
import os,sys
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from codex_runtime import Runtime
class Quiet(Runtime):
    def schedule(self): pass
r=Quiet(Path(sys.argv[2]))
a=r.create({'name':'Crash fixture','cwd':sys.argv[2],'prompt':''},draft=True,defer=True)
with r.db() as db:
    a.update(status='starting',autoWake=True,inFlight=True,
        startAttempt={'id':'before-send','epoch':a['epoch'],'accountKey':a['accountKey'],
                      'submitted':False,'events':['committed-input']})
    r.put(db,'agents',a)
    r.enqueue(db,a,'user','Saved before crash','committed-input')
    db.execute("UPDATE runtime_events SET status='reserved' WHERE id='committed-input'")
with r.db() as db:
    db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
        ('not-committed',a['id'],'user','Uncommitted','pending',0,a['epoch'],None,None))
    os._exit(9)
"""
        child=subprocess.run([sys.executable,'-B','-c',script,
            str(Path(__file__).resolve().parents[1]/'scripts'),str(child_root)],capture_output=True,text=True)
        self.assertEqual(child.returncode,9,child.stderr)
        runtime=fixture.Runtime(child_root,fixture.fixture.RecoveryServer)
        self.addCleanup(runtime.close)
        with runtime.db() as db:
            events=db.execute('SELECT id,status FROM runtime_events').fetchall()
            actor=runtime.records(db,'agents')[0]
        self.assertEqual([(r[0],r[1]) for r in events],[('committed-input','pending')])
        self.assertTrue(actor['autoWake'])
        self.assertEqual(actor['status'],'queued')

    def test_crash_capture_uses_persisted_authority(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        with self.runtime.db() as db:
            self.assertTrue(restore(db,a))
        self.assertEqual(a['restartRecovery']['stage'],'pending')
        self.assertEqual(a['turnId'],'lost-turn')
        self.assertFalse(a['autoWake'])

    def test_new_turn_replaces_previous_completed_recovery_marker(self):
        self.restart()
        recover(self.runtime,self.key,automatic=True)
        a=self.update(status='running',autoWake=True,inFlight=True,turnId='new-turn')
        with self.runtime.db() as db:self.assertTrue(restore(db,a))
        self.assertEqual(a['restartRecovery']['turnId'],'new-turn')
        self.assertEqual(a['restartRecovery']['stage'],'pending')

    def test_local_question_survives_restart_and_answer_is_durable(self):
        self.update(status='waiting',autoWake=True,inFlight=False,turnId=None,error=None)
        question={'id':'local-question','agent':self.key,'epoch':self.a['epoch'],
                  'method':'agent/asyncQuestion','status':'pending',
                  'params':{'questions':[{'id':'choice','question':'Which option?'}]}}
        with self.runtime.db() as db:self.runtime.put(db,'requests',question)
        self.runtime.close()
        self.runtime=fixture.Runtime(Path(self.temp.name),fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.assertEqual(self.runtime.answer('local-question',{'answers':{'choice':{'answers':['A']}}}),{'status':'answered'})
        with self.runtime.db() as db:
            row=db.execute("SELECT status,text FROM runtime_events WHERE id='local-question:answer'").fetchone()
            self.assertEqual(row[0],'pending')
            self.assertIn('A',row[1])

    def test_account_change_invalidates_restart_authority(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        capture(a);a['accountKey']='replacement'
        with self.runtime.db() as db:self.assertFalse(restore(db,a))
        self.assertEqual(a['restartRecovery']['stage'],'superseded')


if __name__=='__main__':unittest.main()
