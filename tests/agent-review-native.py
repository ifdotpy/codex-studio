#!/usr/bin/env python3
"""Exercise the agent review tool with real Codex and a loopback provider.

No cloud calls or user state. The native parent calls the actual dynamic tool.
Its second response remains open while the managed native review completes.
"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'review_native_fixture', Path(__file__).with_name('workspace-native-turn.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import Runtime, AppServer

MODEL = 'gpt-5.6-sol'
REVIEW_MODEL = 'gpt-5.6-luna'
MARKER = 'STUDIO_NATIVE_REVIEW_FINDING'


class ReviewProvider(f.Provider):
    def __init__(self, project):
        super().__init__()
        self.RequestHandlerClass = ReviewHandler
        self.project = project
        self.parent_requests = 0
        self.review_requests = []
        self.parent_waiting = threading.Event()


class ReviewHandler(f.ResponsesHandler):
    def do_POST(self):
        if self.path != '/v1/responses':
            return self.reject()
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        # The native review delegate supplies its own review rubric and target.
        review = any(item.get('role') == 'user' and 'NATIVE_REVIEW_TARGET' in json.dumps(item.get('content'))
                     for item in request.get('input', []))
        with self.server.lock:
            self.server.requests.append(request)
            number = len(self.server.requests)
            if review:
                self.server.review_requests.append(request)
            else:
                self.server.parent_requests += 1
            parent_number = self.server.parent_requests
            review_number = len(self.server.review_requests)
        if not review and parent_number == 1:
            item = {'id': 'fc_review', 'type': 'function_call',
                    'call_id': 'native_review_tool_call', 'status': 'completed',
                    'name': 'orchestration_review', 'arguments': json.dumps({
                        'target': {'type': 'custom', 'instructions':
                            'NATIVE_REVIEW_TARGET: review sample.py without changing files.'}})}
        elif review and review_number == 1:
            item = {'id': 'fc_write', 'type': 'function_call',
                    'call_id': 'native_review_denied_write', 'status': 'completed',
                    'name': 'exec_command', 'arguments': json.dumps({
                        'cmd': 'printf forbidden-review-write > ' + shlex.quote(str(self.server.project / 'sample.py')),
                        'max_output_tokens': 200, 'login': False})}
        else:
            answer = json.dumps({'findings': [{
                'title': '[P1] ' + MARKER,
                'body': 'The sample returns the wrong value for the admitted input.',
                'confidence_score': 0.99,
                'code_location': {'absolute_file_path': str(self.server.project / 'sample.py'),
                                  'line_range': {'start': 1, 'end': 1}},
                'priority': 1}],
                'overall_correctness': 'patch is incorrect',
                'overall_explanation': MARKER,
                'overall_confidence_score': 0.99}) if review else 'Parent native fixture complete.'
            item = {'id': f'msg_{number}', 'type': 'message', 'role': 'assistant',
                    'status': 'completed', 'content': [
                        {'type': 'output_text', 'text': answer, 'annotations': []}]}
        response = {'id': f'resp_review_{number}', 'object': 'response',
                    'status': 'completed', 'model': request['model'], 'output': [item],
                    'usage': {'input_tokens': 100, 'output_tokens': 20, 'total_tokens': 120}}
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Connection', 'close')
        self.end_headers()
        try:
            self.event('response.created', response={**response, 'status': 'in_progress', 'output': []})
            if not review and parent_number > 1:
                self.server.parent_waiting.set()
                if not self.server.release.wait(45):
                    raise TimeoutError('The native test did not release the parent response')
            self.event('response.output_item.added', output_index=0,
                       item={**item, 'status': 'in_progress'})
            self.event('response.output_item.done', output_index=0, item=item)
            self.event('response.completed', response=response)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            self.close_connection = True


class ReviewRuntime(Runtime):
    def __init__(self, root):
        self.native_events = []
        self.review_completed = threading.Event()
        self.child_thread = None
        super().__init__(root, AppServer)

    def schedule(self):
        # Advance the production scheduler explicitly. Do not start an extra
        # provider turn when the parent's completion event becomes pending.
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()

    def analytics_history_start(self):
        pass

    def notification(self, message, account_key='default', connection_id=None):
        self.native_events.append(copy.deepcopy(message))
        super().notification(message, account_key, connection_id)
        params = message.get('params', {})
        if (message.get('method') == 'turn/completed'
                and params.get('threadId') == self.child_thread):
            self.review_completed.set()


class AgentReviewNative(unittest.TestCase):
    def exercise(self, configured_review_model):
        with tempfile.TemporaryDirectory(prefix='studio-agent-review-native-') as directory:
            root = Path(directory)
            home, project = root / 'codex-home', root / 'project'
            home.mkdir()
            project.mkdir()
            subprocess.run(['git', 'init', '-q', str(project)], check=True)
            sample = project / 'sample.py'
            sample.write_text('answer = 41\n')
            initial_files = {str(p.relative_to(project)): p.read_bytes()
                             for p in project.rglob('*') if p.is_file()}
            provider = ReviewProvider(project)
            threading.Thread(target=provider.serve_forever, daemon=True).start()
            endpoint = f'http://127.0.0.1:{provider.server_port}'
            review_setting = f'review_model = "{configured_review_model}"\n' if configured_review_model else ''
            (home / 'config.toml').write_text(f'''model = "gpt-6-astra"
{review_setting}model_provider = "local-review"
[features]
plugins = false
remote_plugin = false
apps = false
skip_host_skill_discovery = true
[model_providers.local-review]
name = "Local review fixture"
base_url = "{endpoint}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
''')
            environment = {'CODEX_HOME': str(home), 'OPENAI_API_KEY': '', 'CODEX_API_KEY': '',
                           'CODEX_BOARD_STATE_DIR': str(root / 'board'),
                           'NO_PROXY': '127.0.0.1,localhost', 'no_proxy': '127.0.0.1,localhost'}
            environment.update({name: endpoint for name in (
                'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy')})
            runtime = None
            calls = []
            original_submit, original_wait = AppServer.submit, AppServer.wait
            delayed_responses = []

            def submit(server, method, params):
                calls.append((method, copy.deepcopy(params)))
                if method == 'review/start':
                    runtime.child_thread = params['threadId']
                return original_submit(server, method, params)

            def wait(server, submitted, timeout=60):
                result = original_wait(server, submitted, timeout)
                if submitted[1] == 'review/start':
                    # Hold only Studio's response consumption. Codex and the
                    # notification callback continue normally on their threads.
                    if not runtime.review_completed.wait(20):
                        raise TimeoutError('No native review completion before response acceptance')
                    delayed_responses.append(result['turn']['id'])
                return result

            try:
                with patch.dict(os.environ, environment), patch.object(AppServer, 'submit', submit), \
                        patch.object(AppServer, 'wait', wait):
                    runtime = ReviewRuntime(root / 'state')
                    lead = runtime.create({'name': 'Review tool native parent', 'prompt':
                        'Call the native review tool, then wait.', 'cwd': str(project),
                        'model': MODEL, 'concurrency': 2, 'maxAgents': 2})
                    runtime.dispatch()

                    def child_record():
                        with runtime.db() as db:
                            return next((a for a in runtime.records(db, 'agents')
                                         if a.get('nativeReview')), None)

                    child = f.until(child_record, 'native tool creates reviewer', timeout=25)
                    self.assertTrue(provider.parent_waiting.wait(15), 'Parent did not resume after its tool call')
                    parent_before = runtime.agent(lead['id'])
                    self.assertEqual(parent_before['status'], 'running')
                    self.assertEqual(child['model'], MODEL)
                    runtime.dispatch()

                    def completed_child():
                        current = runtime.agent(child['id'])
                        return current if current.get('lastCompletedTurn') else None

                    child = f.until(completed_child, 'managed native review completion', timeout=25)
                    f.until(lambda: delayed_responses, 'delayed review response accepted')
                    self.assertEqual(child['lastCompletedTurnStatus'], 'completed', child.get('error'))
                    parent_after = runtime.agent(lead['id'])
                    self.assertEqual(parent_after['turnId'], parent_before['turnId'])
                    self.assertEqual(parent_after['status'], 'running')
                    self.assertTrue(parent_after['inFlight'])
                    native_parent = runtime.server.call('thread/read', {
                        'threadId': parent_after['threadId'], 'includeTurns': False})['thread']
                    self.assertEqual(native_parent['status']['type'], 'active')
                    review_calls = [p for m, p in calls if m == 'review/start']
                    self.assertEqual(len(review_calls), 1)
                    self.assertEqual(review_calls[0]['threadId'], child['threadId'])
                    self.assertEqual(review_calls[0]['delivery'], 'inline')
                    self.assertFalse(any(m == 'turn/start' and p.get('threadId') == child['threadId']
                                         for m, p in calls))
                    self.assertFalse(any(m == 'turn/interrupt' and p.get('threadId') == parent_before['threadId']
                                         for m, p in calls))
                    settings = next(p for m, p in calls if m == 'thread/settings/update'
                                    and p.get('threadId') == child['threadId'])
                    self.assertEqual(settings['sandboxPolicy']['type'], 'readOnly')
                    self.assertEqual(settings['model'], MODEL)
                    self.assertEqual(len(provider.review_requests), 2)
                    self.assertEqual({r['model'] for r in provider.review_requests}, {configured_review_model or MODEL})
                    self.assertIn('reviewer', json.dumps(provider.review_requests[0]).lower())
                    with runtime.db() as db:
                        items = [json.loads(row[0]) for row in db.execute(
                            'SELECT record FROM runtime_items WHERE agent=?', (child['id'],))]
                        parent_events = [dict(row) for row in db.execute(
                            "SELECT * FROM runtime_events WHERE agent=? AND kind='child_result'", (lead['id'],))]
                    command_output = next(item['output'] for item in provider.review_requests[-1]['input']
                                          if item.get('type') == 'function_call_output'
                                          and item.get('call_id') == 'native_review_denied_write')
                    self.assertIn('Process exited with code 1', command_output)
                    self.assertRegex(command_output.lower(), 'operation not permitted|permission denied')
                    self.assertEqual(sample.read_text(), 'answer = 41\n')
                    self.assertTrue(any(item.get('role') == 'assistant' and MARKER in item.get('text', '')
                                        for item in items), items)
                    self.assertEqual(len(parent_events), 1)
                    self.assertIn(MARKER, parent_events[0]['text'])
                    self.assertEqual(parent_events[0]['status'], 'pending')
                    self.assertEqual(initial_files, {str(p.relative_to(project)): p.read_bytes()
                                                    for p in project.rglob('*') if p.is_file()})
                    provider.release.set()
                    f.until(lambda: runtime.agent(lead['id']).get('lastCompletedTurn'), 'parent completes after release')
                    self.assertFalse(provider.unexpected, provider.unexpected)
                    print(json.dumps({'reviewModel': configured_review_model or MODEL,
                                      'nativeReviewCalls': len(review_calls), 'parentEvents': len(parent_events),
                                      'completionBeforeAcceptance': True, 'parentUninterrupted': True,
                                      'sandbox': 'readOnly', 'writeDenied': True, 'cloudCalls': 0}))
            finally:
                provider.release.set()
                if runtime:
                    runtime.close()
                provider.shutdown()
                provider.server_close()

    def test_actor_model_and_early_native_completion(self):
        self.exercise(None)

    def test_configured_review_model(self):
        self.exercise(REVIEW_MODEL)


if __name__ == '__main__':
    unittest.main(verbosity=2)
