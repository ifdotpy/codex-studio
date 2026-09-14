#!/usr/bin/env python3
"""Native history repair with local Responses and blocked external requests."""
import importlib.util
import json
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

    def run_case(self, compacted, failed=False, tool_failed=False):
        with n.native_server(FailureHandler) as (server, tid, provider, notifications, _):
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
                'compacted': compacted, 'failedSource': failed, 'failedDynamicTool': tool_failed, 'savedForkBytes': len(resumed.encode()), 'repairModelRequests': 0, 'externalRequests': provider.unexpected}))


if __name__ == '__main__':
    unittest.main(verbosity=2)
