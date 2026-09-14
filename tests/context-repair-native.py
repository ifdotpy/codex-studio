#!/usr/bin/env python3
"""Native history repair with local Responses and blocked external requests."""
import importlib.util
import json
from contextlib import ExitStack
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('primitive', Path(__file__).with_name('native-primitives-integration.py'))
n = importlib.util.module_from_spec(spec)
spec.loader.exec_module(n)
from codex_context_repair import repair_idle, _native_idle


class FailureHandler(n.s.ShellHandler):
    def do_POST(self):
        latest = getattr(self.server, 'latest_tool', False)
        if latest or (getattr(self.server, 'failed_dynamic_fixture', False) and not self.server.requests):
            self.server.latest_tool = False
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.append(request)
            dynamic = getattr(self.server, 'failed_dynamic_fixture', False)
            call_id = 'fixture-latest-call' if latest else 'fixture-failed-call'
            item = {'id':call_id,'call_id':call_id,'type':'function_call',
                    'name':'repair_fixture_tool' if dynamic else 'exec_command',
                    'arguments':'{}' if dynamic else json.dumps({'cmd':'printf latest-tool-receipt-maple','max_output_tokens':1000}),
                    'status':'completed'}
            response = {'id':'fixture-tool-response','object':'response','status':'completed',
                        'model':request['model'],'output':[item],
                        'usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}
            self.send_response(200)
            self.send_header('Content-Type','text/event-stream')
            self.send_header('Connection','close')
            self.end_headers()
            self.event('response.created',response={**response,'status':'in_progress','output':[]})
            self.event('response.output_item.added',output_index=0,item={**item,'status':'in_progress'})
            self.event('response.output_item.done',output_index=0,item=item)
            self.event('response.completed',response=response)
            self.close_connection=True
            return
        if not getattr(self.server, 'fail_next', False) or self.path != '/v1/responses':
            return super().do_POST()
        self.server.fail_next = False
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.append(request)
        body = json.dumps({'error': {'message': 'Local fixture quota exhausted.',
            'type': 'insufficient_quota', 'code': 'insufficient_quota'}}).encode()
        self.send_response(429)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class NativeContextRepair(unittest.TestCase):
    def test_fork_keeps_history_and_command_receipts_with_smaller_next_input(self):
        self.run_case(False)

    def test_compacted_context_keeps_summary_and_native_tool_receipts(self):
        self.run_case(True)

    def test_failed_native_turn_repairs_idle_system_error_without_inference(self):
        self.run_case(False, failed=True)

    def test_active_native_turn_is_rejected(self):
        with n.native_server() as (server, tid, provider, notifications, _):
            server.call('turn/start', {'threadId':tid, 'input':[{'type':'text','text':'Hold this isolated response.'}]})
            self.assertTrue(provider.started.wait(10))
            with self.assertRaisesRegex(ValueError, 'native status:.*active'):
                _native_idle(server, tid)
            self.assertEqual(len(provider.requests), 1)
            self.assertEqual(provider.unexpected, [])
            provider.release.set()

    def test_unsuccessful_dynamic_tool_receipt_is_terminal_for_maintenance(self):
        self.run_case(False, tool_failed=True)

    def test_transferred_repaired_fork_keeps_history_on_its_first_input(self):
        self.run_case(False, transferred=True)

    def test_unrepaired_transfer_materializes_exact_native_ancestor_prefix(self):
        self.run_unrepaired_transfer(False)

    def test_nested_unrepaired_transfer_with_empty_turn_index_keeps_history(self):
        self.run_unrepaired_transfer(True)

    def run_unrepaired_transfer(self, nested):
        from codex_account_transfer import transfer_store
        with n.native_server(FailureHandler) as (source, tid, provider, notifications, _):
            tid = source.call('thread/start', {'cwd':'/tmp','config':n.n.THREAD_CONFIG,
                'approvalPolicy':'never','sandbox':'danger-full-access',
                'developerInstructions':'Preserve developer policy jade-37.'})['thread']['id']
            provider.command = 'printf ancestor-command-receipt'
            payload = json.dumps({'stdout':'unrepaired-legacy-' * 3000})
            event_text = '[Orchestration event: monitor_exit]\n' + payload
            def source_turn(text):
                turn = source.call('turn/start', {'threadId':tid,'input':[{'type':'text','text':text}]})['turn']['id']
                n.n.until(lambda:any(e.get('method')=='turn/completed' and e['params']['turn']['id']==turn for e in notifications), 'source terminal')
                return turn
            event_turn = source_turn(event_text)
            provider.latest_tool = True
            latest = source_turn('Preserve inherited latest user maple and its latest tool receipt.')
            before = json.dumps(provider.requests[-1])
            original_path = Path(source.call('thread/read', {'threadId':tid,'includeTurns':False})['thread']['path'])
            original_bytes = original_path.read_bytes()
            f = n.w.WorkspaceContract()
            f.setUp()
            try:
                with n.native_server(FailureHandler) as (target, empty, target_provider, target_notifications, _):
                    target_home = Path(target.call('thread/read', {'threadId':empty,'includeTurns':False})['thread']['path']).parents[4]
                    parent_id, parent_path = tid, original_path
                    if nested:
                        parent = source.call('thread/fork', {'threadId':tid,'excludeTurns':True,
                            'deferGoalContinuation':True,'config':n.n.THREAD_CONFIG,
                            'approvalPolicy':'never','sandbox':'danger-full-access'})['thread']
                        parent_id, parent_path = parent['id'], Path(parent['path'])
                    imported = transfer_store(f.runtime).copy_history(original_path.parents[4],target_home,parent_path)
                    imported_prefix = Path(imported).read_bytes()
                    params = {'threadId':parent_id,'path':str(imported),'excludeTurns':True,'deferGoalContinuation':True,
                              'config':n.n.THREAD_CONFIG,'approvalPolicy':'never','sandbox':'danger-full-access'}
                    fork = target.call('thread/fork',params)['thread']
                    base = json.loads(Path(fork['path']).read_text().splitlines()[0])
                    self.assertEqual(base['payload']['history_base']['end_byte_offset'],len(imported_prefix))
                    self.assertEqual(base['payload']['history_base']['end_ordinal_exclusive'],json.loads(imported_prefix.splitlines()[-1])['ordinal']+1)
                    if nested:
                        turns = target.call('thread/turns/list', {'threadId':fork['id'],'limit':1,'sortDirection':'desc','itemsView':'notLoaded'})
                        self.assertEqual(turns['data'],[])
                    from codex_context_repair import _rollout_segments
                    segments = _rollout_segments(target_home,Path(fork['path']),fork['id'])
                    copied_root = Path(segments[0]['path'])
                    self.assertEqual(copied_root.read_bytes(),original_bytes)
                    # A real later source turn is outside the captured native fork boundary.
                    source_turn('EXCLUDED-LATER-ANCESTOR-USER')
                    later_bytes = original_path.read_bytes()
                    self.assertTrue(later_bytes.startswith(original_bytes))
                    with copied_root.open('ab') as stream:
                        stream.write(later_bytes[len(original_bytes):])
                    top_bytes = Path(fork['path']).read_bytes()
                    target_provider.requests = provider.requests
                    target_provider.unexpected = provider.unexpected
                    count = len(provider.requests)
                    a = f.agent_update(f.lead(),threadId=fork['id'],status='idle',inFlight=False)
                    with f.runtime.db() as db:
                        db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                            ('native-ancestry-event',a['id'],'monitor_exit',payload,'delivered',1,a['epoch'],event_turn,None))
                    with patch.object(f.runtime,'connect',return_value=target), patch.object(f.runtime.accounts,'home',return_value=target_home), patch.object(f.runtime,'connection_current',return_value=True), patch.object(f.runtime,'new_thread_params',return_value={'config':n.n.THREAD_CONFIG,'approvalPolicy':'never','sandbox':'danger-full-access'}):
                        repaired = repair_idle(f.runtime,a['id'])
                    self.assertEqual(len(provider.requests),count)
                    self.assertEqual(repaired['contextRepair']['snapshot']['terminalTurnId'],latest)
                    self.assertEqual(len(repaired['contextRepair']['snapshot']['ancestry']),3 if nested else 2)
                    from codex_analytics_history import inherited_usage_threads, rollout_actions
                    allowed = inherited_usage_threads(repaired)
                    context = {'threadId':repaired['threadId'],'allowedSourceThreadIds':allowed}
                    projected = [json.loads(line) for line in Path(repaired['contextRepair']['snapshot']['copyPath']).read_text().splitlines()]
                    usage = [row for row in projected if row['type']=='token_usage_record']
                    self.assertTrue(usage,'Installed native fixture must persist exact response usage')
                    original_usage = [json.loads(line)['payload'] for line in original_bytes.splitlines()
                                      if json.loads(line)['type']=='token_usage_record']
                    for record in usage:
                        actions = rollout_actions(record,context,'native-ancestry-usage',1)
                        self.assertFalse(any(action[1]=='wrongThreadRecord' for action in actions))
                        self.assertEqual(actions[0][2]['responseId'],record['payload']['response_id'])
                        self.assertEqual(actions[0][2]['rawTokenUsageRecord'],record['payload'])
                        self.assertIn(record['payload'],original_usage)

                    self.assertEqual(copied_root.read_bytes(),later_bytes)
                    self.assertEqual(Path(fork['path']).read_bytes(),top_bytes)
                    self.assertEqual(original_path.read_bytes(),later_bytes)
                    result = target.call('turn/start', {'threadId':repaired['threadId'],'input':[{'type':'text','text':'First repaired transferred input.'}]})['turn']['id']
                    n.n.until(lambda:any(e.get('method')=='turn/completed' and e['params']['turn']['id']==result for e in target_notifications),'repaired terminal')
                    after = json.dumps(provider.requests[-1])
                    for marker in ('ancestor-command-receipt','inherited latest user maple','latest-tool-receipt-maple','event:native-ancestry-event','Preserve developer policy jade-37.'):
                        self.assertIn(marker,after)
                    for item in json.loads(before).get('input',[]):
                        if item.get('type') == 'function_call_output':
                            self.assertIn(item,provider.requests[-1]['input'])
                    self.assertNotIn('EXCLUDED-LATER-ANCESTOR-USER',after)
                    self.assertLess(len(after),len(before)-40000)
                    self.assertEqual(provider.requests[-1].get('instructions'),provider.requests[count-2].get('instructions'))
                    self.assertEqual(len(provider.requests),count+1)
                    self.assertEqual(provider.unexpected,[])
                    print(json.dumps({'ancestryDepth':3 if nested else 2,'emptyNativeTurnIndex':nested,
                        'beforeBytes':len(before),'afterBytes':len(after),'repairModelRequests':0,
                        'sourceUnchanged':True,'laterAncestorInputExcluded':True,'externalRequests':[]}))
            finally:
                f.tearDown()

    def run_case(self, compacted, failed=False, tool_failed=False, transferred=False):
        with n.native_server(FailureHandler) as (server, tid, provider, notifications, _), ExitStack() as extra:
            provider.command = 'printf repair-command-receipt-cedar'
            if tool_failed:
                provider.failed_dynamic_fixture = True
                server.request = lambda message: server.write({'id':message['id'], 'result':{
                    'success':False,'contentItems':[{'type':'inputText','text':'repair-command-receipt-cedar: latest-tool-receipt-maple: known tool failure'}]}})
                tid = server.call('thread/start', {'cwd':'/tmp','config':n.n.THREAD_CONFIG,
                    'approvalPolicy':'never','sandbox':'danger-full-access',
                    'dynamicTools':[{'name':'repair_fixture_tool','description':'Return the isolated failure receipt',
                                     'inputSchema':{'type':'object','properties':{}}}]})['thread']['id']
            text = json.dumps({'id': 'monitor-repair', 'status': 'exited', 'exitCode': 0,
                               'stdout': 'legacy-payload-marker-' * 3000})
            original = '[Orchestration event: monitor_exit]\n' + text
            def turn(thread, message):
                result = server.call('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': message}]})['turn']['id']
                n.n.until(lambda: any(e.get('method') == 'turn/completed' and e['params']['turn']['id'] == result for e in notifications), 'native completion')
                return result
            first = turn(tid, 'Preserve user decision birch-192. Run the fixture command once.')
            if tool_failed:
                page = server.call('thread/items/list', {'threadId':tid,'turnId':first,'limit':1000,'sortDirection':'asc'})
                calls = [entry['item'] for entry in page['data'] if entry['item']['type']=='dynamicToolCall']
                self.assertEqual(len(calls),1)
                self.assertEqual(calls[0]['status'],'failed')
                self.assertIs(calls[0]['success'],False)
            event_turn = turn(tid, original + '\n\nKeep this appended instruction unchanged.')
            provider.latest_tool = True
            latest = turn(tid, 'Preserve latest-user-maple and run the latest fixture tool once.')
            if failed:
                provider.fail_next = True
                failed_turn = turn(tid, 'Fail this isolated request without a retry.')
                completion = next(e for e in notifications if e.get('method') == 'turn/completed' and e['params']['turn']['id'] == failed_turn)
                self.assertEqual(completion['params']['turn']['status'], 'failed')
            before = json.dumps(provider.requests[-1])
            native = server.call('thread/read', {'threadId': tid, 'includeTurns': False})['thread']
            if failed:
                self.assertEqual(native['status']['type'], 'systemError')
            if compacted:
                server.call('thread/unsubscribe', {'threadId': tid})
            elif not failed:
                self.assertEqual(native['status']['type'], 'idle')
            source = Path(native['path'])
            if compacted:
                records = [json.loads(line) for line in source.read_text().splitlines()]
                history = [r['payload'] for r in records if r.get('type') == 'response_item']
                history.append({'type':'message','id':'fixture-summary','role':'assistant',
                    'content':[{'type':'output_text','text':'Preserve compacted summary willow-552.'}]})
                with source.open('a') as stream:
                    stream.write(json.dumps({'timestamp':'2026-09-14T00:00:00.000Z','type':'compacted',
                        'payload':{'message':'Preserve compaction summary metadata.', 'replacement_history':history}}) + '\n')
            data = source.read_bytes()
            f = n.w.WorkspaceContract()
            f.setUp()
            try:
                a = f.agent_update(f.lead(), threadId=tid, status='idle', inFlight=False)
                with f.runtime.db() as db:
                    db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                        ('monitor:repair',a['id'],'monitor_exit',text,'delivered',1,a['epoch'],event_turn,None))
                count = len(provider.requests)
                with patch.object(f.runtime, 'connect', return_value=server), \
                        patch.object(f.runtime.accounts, 'home', return_value=source.parents[4]), \
                        patch.object(f.runtime, 'connection_current', return_value=True), \
                        patch.object(f.runtime, 'new_thread_params', return_value={
                            'config':n.n.THREAD_CONFIG, 'approvalPolicy':'never', 'sandbox':'danger-full-access'}):
                    repaired = repair_idle(f.runtime, a['id'])
                self.assertEqual(repaired['contextRepair']['phase'], 'completed')
                self.assertEqual(repaired['contextRepair']['source']['threadId'], tid)
                report = repaired['contextRepair']['snapshot']
                self.assertTrue(report['eventIds'], report)
                fork = {'id': repaired['threadId']}
                self.assertEqual(len(provider.requests), count)
                if transferred:
                    from codex_account_transfer import transfer_store
                    target_server, empty_id, target_provider, target_notifications, _ = extra.enter_context(n.native_server(FailureHandler))
                    target_provider.requests, target_provider.unexpected = provider.requests, provider.unexpected
                    empty_native = target_server.call('thread/read', {'threadId':empty_id,'includeTurns':False})['thread']
                    target_home = Path(empty_native['path']).parents[4]
                    parent_native = server.call('thread/read', {'threadId':fork['id'],'includeTurns':False})['thread']
                    imported = transfer_store(f.runtime).copy_history(source.parents[4],target_home,parent_native['path'])
                    target = target_server.call('thread/fork', {'threadId':fork['id'],'path':str(imported),'excludeTurns':True,
                        'deferGoalContinuation':True,'config':n.n.THREAD_CONFIG,
                        'approvalPolicy':'never','sandbox':'danger-full-access'})['thread']
                    page = target_server.call('thread/turns/list', {'threadId':target['id'],'limit':1,
                        'sortDirection':'desc','itemsView':'notLoaded'})
                    self.assertEqual(page['data'], [])
                    self.assertEqual(target['forkedFromId'],fork['id'])
                    target_source = Path(target['path']).read_bytes()
                    transfer = {'id':'native-transfer','leadId':a['id'],'status':'completed',
                        'targetAccountKey':'fixture-target','members':{a['id']:{'phase':'completed',
                            'sourceAccountKey':a['accountKey'],'sourceThreadId':fork['id'],
                            'source':{'accountKey':a['accountKey'],'threadId':fork['id'],'epoch':a['epoch']},
                            'result':{'thread':target}}}}
                    with f.runtime.db() as db:
                        db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
                        db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)',('native-transfer',json.dumps(transfer)))
                    moved=f.agent_update(repaired,accountKey='fixture-target',threadId=target['id'],
                        accountHistory=[*repaired.get('accountHistory',[]),{'transferId':'native-transfer',
                            'accountKey':a['accountKey'],'threadId':fork['id']}])
                    with patch.object(f.runtime,'connect',side_effect=AssertionError('Checked inherited history needs no new repair')):
                        admitted=repair_idle(f.runtime,moved['id'])
                    self.assertEqual(admitted['threadId'],target['id'])
                    self.assertEqual(admitted['accountKey'],'fixture-target')
                    self.assertEqual(Path(target['path']).read_bytes(),target_source)
                    self.assertEqual(len(provider.requests),count)
                    fork={'id':target['id']}
                    server, notifications = target_server, target_notifications
            finally:
                f.tearDown()
            turn(fork['id'], 'After repair measurement.')
            after = json.dumps(provider.requests[-1])
            self.assertLess(len(after), len(before) - 50000)
            self.assertIn('birch-192', after)
            self.assertIn('repair-command-receipt-cedar', after)
            self.assertIn('latest-user-maple', after)
            self.assertIn('latest-tool-receipt-maple', after)
            self.assertIn('Keep this appended instruction unchanged.', after)
            self.assertIn('event:monitor:repair', after)
            if compacted:
                self.assertIn('Preserve compacted summary willow-552.', after)
            self.assertEqual(source.read_bytes(), data)
            self.assertEqual(provider.unexpected, [])
            server.call('thread/unsubscribe', {'threadId': fork['id']})
            server.call('thread/resume', {'threadId': fork['id'], 'excludeTurns': True, 'config': n.n.THREAD_CONFIG})
            turn(fork['id'], 'Verify the saved repaired fork.')
            resumed = json.dumps(provider.requests[-1])
            self.assertLess(len(resumed), len(before) - 49000)
            self.assertIn('birch-192', resumed)
            self.assertIn('repair-command-receipt-cedar', resumed)
            self.assertEqual(source.read_bytes(), data)
            print(json.dumps({'beforeBytes': len(before.encode()), 'afterBytes': len(after.encode()),
                'compacted': compacted, 'failedSource': failed, 'failedDynamicTool': tool_failed,
                'transferred':transferred, 'savedForkBytes': len(resumed.encode()), 'repairModelRequests': 0, 'externalRequests': provider.unexpected}))


if __name__ == '__main__':
    unittest.main(verbosity=2)
