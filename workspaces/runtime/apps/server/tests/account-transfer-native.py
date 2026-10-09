#!/usr/bin/env python3
"""Installed native Codex: paginated context, tools and repeated profile handoff."""
import importlib.util
import json
from pathlib import Path
import shutil
import threading
import unittest

spec=importlib.util.spec_from_file_location('native_fixture',Path(__file__).with_name('native-primitives-integration.py'))
n=importlib.util.module_from_spec(spec);spec.loader.exec_module(n)
from codex_account_transfer import AccountTransfers


class NativeTransfer(unittest.TestCase):
    def test_paginated_history_and_dynamic_tools_survive_two_hops(self):
        with n.native_server() as (source,tid,provider,events,_):
            provider.release.set()
            start=source.call('thread/start', {'cwd':str(source.root) if hasattr(source,'root') else '/tmp',
                'model':'gpt-5.6-sol','config':n.n.THREAD_CONFIG,
                'dynamicTools':[{'name':'transfer_fixture','description':'Return fixture status',
                    'inputSchema':{'type':'object','properties':{}}}]})
            tid=start['thread']['id']
            turn=source.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'Transfer context marker cedar-192. Preserve this decision.'}]})['turn']['id']
            n.n.until(lambda:any(e.get('method')=='turn/completed' and e['params']['turn']['id']==turn for e in events),'source turn')
            thread=source.call('thread/read',{'threadId':tid})['thread']
            self.assertEqual(thread['historyMode'],'paginated')
            home=Path(thread['path']).parents[4]
            other=home.parent/'target';other.mkdir();shutil.copyfile(home/'config.toml',other/'config.toml')
            root=home.parent/'target-server';root.mkdir()
            target_events=[]
            target=n.n.AppServer(root,target_events.append,lambda _:None,lambda:None,home=other,isolated=True)
            store=object.__new__(AccountTransfers);store.copy_lock=threading.Lock()
            try:
                source.call('thread/unsubscribe',{'threadId':tid})
                imported=store.copy_history(home,other,thread['path'])
                fork=target.call('thread/fork',{'threadId':tid,'path':str(imported),'excludeTurns':True,
                    'deferGoalContinuation':True,'config':n.n.THREAD_CONFIG})['thread']
                turn2=target.call('turn/start',{'threadId':fork['id'],'input':[{'type':'text','text':'Second account marker maple-284.'}]})['turn']['id']
                n.n.until(lambda:any(e.get('method')=='turn/completed' and e['params']['turn']['id']==turn2 for e in target_events),'target turn')
                self.assertIn('cedar-192',json.dumps(provider.requests[-1]))
                self.assertIn('transfer_fixture',json.dumps(provider.requests[-1]))
                target.call('thread/unsubscribe',{'threadId':fork['id']})
                import os, subprocess
                scanner = os.environ.get('STUDIO_TRANSFER_COST_SCANNER')
                if scanner:
                    bills = []
                    for profile in (home, other):
                        bill = json.loads(subprocess.check_output([scanner, '--home', str(profile),
                            '--cache', str(profile / 'transfer-test-costs')], text=True))[0]
                        bills.append(bill['sessionTokens'])
                    self.assertEqual(bills, [15, 15], bills)
                    print(json.dumps({'accountTokensAfterTransfer': bills}))
                imported_back=store.copy_history(other,home,fork['path'])
                back=source.call('thread/fork',{'threadId':fork['id'],'path':str(imported_back),'excludeTurns':True,
                    'deferGoalContinuation':True,'config':n.n.THREAD_CONFIG})['thread']
                count=len(provider.requests)
                source.call('turn/start',{'threadId':back['id'],'input':[{'type':'text','text':'Continue on the first account.'}]})
                n.n.until(lambda:len(provider.requests)>count,'return request')
                body=json.dumps(provider.requests[-1])
                self.assertIn('cedar-192',body);self.assertIn('maple-284',body)
                self.assertIn('transfer_fixture',body)
                print(json.dumps({'nativeVersion':'0.153.4','hops':2,'paginatedContext':True,'dynamicTools':True,'cloudRequests':0}))
            finally:
                target.close()
                for stream in (target.proc.stdin,target.proc.stdout,target.proc.stderr):
                    if stream:stream.close()


if __name__=='__main__':unittest.main()
