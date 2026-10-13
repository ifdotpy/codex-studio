"""Notification transitions for the shared JSON-lines runtime oracle.

Every database is initialized by Runtime. Timers are manual; provider connections
and background workers never run. The real notification/stream/item/sync/search
writers execute unchanged.
"""
import copy
import hashlib
import json
import pathlib
import sqlite3
import shutil
import time
from unittest.mock import patch

import codex_runtime
import codex_streaming
from codex_runtime import Runtime

TABLES = ('runtime_budget', 'runtime_budget_usage', 'runtime_tasks', 'runtime_agents', 'runtime_items', 'runtime_item_fulltext',
          'runtime_events', 'runtime_event_meta', 'runtime_operation_receipts',
          'runtime_requests', 'runtime_complaints', 'runtime_context_manifests', 'runtime_context_reminders', 'sync_entities',
          'sync_documents', 'sync_versions', 'runtime_search', 'runtime_search_rows',
          'runtime_search_indexed', 'runtime_search_next_meta')
class ManualTimer:
    def __init__(self, interval, callback): self.callback = callback
    def start(self): pass
    def cancel(self): pass


def snapshot(root):
    import __main__ as oracle
    result = oracle.snapshot(root, tables=TABLES)
    prepared = pathlib.Path(root).parent / 'prepared.json'
    if prepared.exists(): result['prepared'] = json.loads(prepared.read_text())
    return result


_created_agent = None


def seed(runtime, case):
    global _created_agent
    if _created_agent is None:
        _created_agent = runtime.create({'name': 'notification fixture', 'cwd': str(runtime.root), 'prompt': 'seed'}, defer=True)
    agent = copy.deepcopy(_created_agent)
    with runtime.db() as db:
        db.execute('DELETE FROM runtime_agents WHERE id=?', (agent['id'],))
        db.execute("DELETE FROM sync_entities WHERE collection='agent' AND id=?", (agent['id'],))
        db.execute("CREATE VIRTUAL TABLE IF NOT EXISTS runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61')")
        created_agent = agent.copy()
        # Fixed, minimal but real-writer seed removes generated IDs from the corpus.
        agent = dict(id='agent', rootId='agent', epoch=1, status='running',
                     cwd='/fixture', threadId='thread', accountKey='default',
                     turnId='replay-turn-1', inFlight=True, events=0, activeTools=[],
                     autoWake=False, isLead=True, agentMode='multi', agentModeRevision=0,
                     agentModeSupported=True, concurrency=32, maxAgentsExplicit=False,
                     subagentConcurrencyVersion=2, prompt='A short parity fixture.')
        if case.get('submission'):
            created_agent.update(agent)
            agent = created_agent
            agent.update(autoWake=True, model='fixture-model', cwd=str(runtime.root), executionSettingsAccountKey='default',
                         startAttempt={'id':'attempt','epoch':1,'accountKey':'default','events':['input-0'], 'submitted':False,
                                       'activeAtReservation':True, 'settingsFixed':True, 'created':10.0})
        if case.get('submission'):
            agent['startAttempt']['events'] = ['input-' + str(i) for i in range(len(case.get('events', [{'kind':'user','text':'hello'}])))]
        agent.update(case.get('agent', {}))
        if case.get('expect_refusal'):
            agent['corpusPrivate'] = 'SECRET_STORED_SENTINEL'
        for key in case.get('remove', []):
            agent.pop(key, None)
        runtime.put(db, 'agents', agent)
        for asset in case.get('assets', []):
            runtime.put(db, 'assets', asset)
        for item in case.get('items', []):
            runtime.item(db, 'agent', item['id'], 'assistant', item.get('text', ''), **item.get('metadata', {}))
            if 'created' in item:
                db.execute('UPDATE runtime_items SET created=? WHERE id=?', (item['created'], 'agent:' + item['id']))
        for related in case.get('related', []):
            record = copy.deepcopy(related['record'])
            def paths(value):
                if isinstance(value, str): return value.replace('$project', str(runtime.root))
                if isinstance(value, list): return [paths(v) for v in value]
                if isinstance(value, dict): return {k:paths(v) for k,v in value.items()}
                return value
            runtime.put(db, related['table'], paths(record))
        if case.get('submission'):
            for index, event in enumerate(case.get('events', [{'kind':'user','text':'hello'}])):
                event_id = 'input-' + str(index)
                runtime.enqueue(db, agent, event['kind'], event['text'], event_id)
                if event.get('metadata') is not None:
                    db.execute('INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)', (event_id,json.dumps(event['metadata'])))
                elif event.get('no_metadata'):
                    db.execute('DELETE FROM runtime_event_meta WHERE id=?', (event_id,))
            db.execute("UPDATE runtime_events SET status='dispatching' WHERE agent='agent'")
        if 'search_phase' in case:
            db.execute('UPDATE runtime_search_rollout SET phase=?', (case['search_phase'],))
    runtime._stream_buffer = codex_streaming.StreamBuffer(runtime)
    runtime.connection_current = lambda *args: True
    # Analytics is a separately owned database; this corpus covers notification
    # writes to the runtime database, including the complete agent/sync records.
    runtime.analytics_safe = lambda *a, **k: None
    runtime._dispatch_token_rate_observation = lambda *a, **k: None
    if case.get('submission'):
        import __main__ as oracle
        from codex_team_isolation import assert_events
        with runtime.read_db() as db:
            rows = [dict(row) for row in db.execute("SELECT * FROM runtime_events WHERE agent='agent' ORDER BY created,id")]
            try:
                assert_events(runtime, db, runtime.agent('agent', db), rows)
            except ValueError:
                assert case.get('expect_refusal'), 'unexpected Python policy refusal'
                prepared = {'firstEventId':rows[0]['id'], 'text':runtime.model_event_text(rows), 'assets':[],
                            'submittedAt':1234.5,'createdAt':1234.5,
                            'inputs':[{'id':r['id'],'at':r['created'],'kind':r['kind'],'text':r['text'],'assets':[]} for r in rows]}
                (runtime.root.parent / 'prepared.json').write_text(json.dumps(prepared))
                return
        captured = oracle.capture_submission(runtime, stop='before_item')
        args, kwargs = captured['args'], captured['kwargs']
        assert_submission_boundary(runtime)
        runtime.corpus_submission = captured
        prepared = {'firstEventId':args[1], 'text':args[3], 'assets':kwargs.get('assets', []),
                    'submittedAt':1234.5, 'createdAt':1234.5,
                    'inputs':[{'id':r['id'], 'at':r['created'], 'kind':r['kind'], 'text':r['text'], 'assets':r.get('assets', [])} for r in kwargs['inputs']]}
        (runtime.root.parent / 'prepared.json').write_text(json.dumps(prepared))
    for corruption in case.get('corrupt', []):
        table, key = corruption['table'], corruption['id'].replace('$project', str(runtime.root))
        assert table in TABLES or table in {'runtime_rooms','runtime_projects'}
        with runtime.db() as db:
            if 'column' in corruption:
                assert table == 'runtime_events' and corruption['column'] == 'created'
                db.execute('UPDATE runtime_events SET created=? WHERE id=?', (corruption['value'], key))
                continue
            record = json.loads(db.execute('SELECT record FROM '+table+' WHERE id=?',(key,)).fetchone()[0])
            if 'field' in corruption: record[corruption['field']] = corruption['value']
            else: record = corruption['value']
            db.execute('UPDATE '+table+' SET record=? WHERE id=?',(json.dumps(record),key))

    if 'receipt_result' in case:
        step = case['steps'][0]
        identity = {'providerHandle':'account:default','generation':step.get('generation',7),'cursor':step.get('cursor',1)}
        with runtime.db() as db:
            runtime.save_receipt(db, 'provider-notification:' + digest(identity), digest({'agent':'agent','notification':step['frame']}), case['receipt_result'])


def assert_submission_boundary(runtime):
    contract = json.loads(pathlib.Path(__file__).with_name('submission_boundary.json').read_text())
    def field(record, path):
        for part in path.split('.'):
            assert isinstance(record, dict) and part in record, 'missing prepared field ' + path
            record = record[part]
        return record
    with runtime.read_db() as db:
        agent = runtime.agent('agent', db)
        for path in contract['caller_owned_fields']['runtime_agents.record']:
            field(agent, path)
        for path, value in contract['agent_values'].items():
            assert field(agent, path) == value, 'prepared agent field ' + path
        for index, event_id in enumerate(agent['startAttempt']['events']):
            row = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (event_id,)).fetchone())
            for path, value in contract['event_values'].items():
                assert row[path] == value, 'prepared event field ' + path
            meta = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (event_id,)).fetchone()[0])
            for path in contract['required_event_metadata'] + (contract['required_first_event_metadata'] if index == 0 else []):
                field(meta, path)
            assert meta['modelEventProjection'] == 1


def transition(runtime, case):
    if case.get('routing_refusal'):
        import __main__ as oracle
        before = snapshot(runtime.root)
        with patch.object(codex_streaming.threading, 'Timer', ManualTimer):
            for step in case['steps']:
                account = step.get('provider_handle', 'account:default').removeprefix('account:')
                result = runtime.notification(step['frame'], account, 'connection-7')
                assert result is None, 'Python routing refusal return'
            with runtime.db() as db:
                runtime._stream_buffer.flush_locked(db, force=True)
        assert snapshot(runtime.root) == before, 'Python foreign routing wrote state'
        return
    if case.get('expect_refusal'): return
    if case.get('submission'):
        captured = runtime.corpus_submission
        prepared = json.loads((runtime.root.parent / 'prepared.json').read_text())
        with patch.object(time, 'time', lambda:1234.5), runtime.db() as db:
            Runtime.item(runtime, db, *captured['args'], **captured['kwargs'])
            record = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?', ('agent:' + prepared['firstEventId'],)).fetchone()[0])
            signature = digest({'agent':'agent','firstEventId':prepared['firstEventId'],'submittedAt':1234.5,'createdAt':1234.5,'record':record,'text':prepared['text'],'inputTexts':[i['text'] for i in prepared['inputs']]})
            runtime.save_receipt(db,'provider-submission:' + hashlib.sha256(('agent:' + prepared['firstEventId']).encode()).hexdigest(),signature,{'status':'written'})
        return
    with patch.object(codex_streaming.threading, 'Timer', ManualTimer), patch.object(time, 'time', lambda:1234.5):
        for index, step in enumerate(case['steps']):
            if step.get('rollback'): continue  # Discarded Rust caller transaction has no committed Python equivalent.
            agent_id = step.get('agent', 'agent')
            frame = step['frame']
            generation = step.get('generation', 7)
            params = frame.get('params', {})
            key = ('default', 'connection-' + str(generation), params.get('threadId'), params.get('turnId'), params.get('itemId'), frame['method'])
            outcome = {'status': 'ignored', 'reason': 'closed-stream'} if key in runtime._stream_buffer.closed else {'status': 'written'}
            agent = runtime.agent(agent_id)
            if agent.get('deletedAt'):
                outcome = {'status':'ignored','reason':'deleted-agent'}
            elif params.get('turnId') and params['turnId'] != agent.get('turnId'):
                outcome = {'status':'ignored','reason':'stale-turn-item'}
            if step.get('declared_unhandled', case.get('declared_unhandled')):
                reason = step.get('declared_unhandled', case.get('declared_unhandled'))
                outcome = {'status':'unhandled','kind':frame['method'],'reason':reason}
                counter = agent.setdefault('unhandledNotifications', {}).setdefault(frame['method'], {'count':0,'reasons':{}})
                counter['count'] += 1
                counter['reasons'][reason] = counter['reasons'].get(reason, 0) + 1
                with runtime.db() as db: runtime.put(db,'agents',agent)
            else:
                runtime.notification(frame, 'default', 'connection-' + str(generation))
            identity = {'providerHandle': 'account:default', 'generation': generation, 'cursor': step.get('cursor', index + 1)}
            with runtime.db() as db:
                runtime.save_receipt(db, 'provider-notification:' + digest(identity), digest({'agent':agent_id, 'notification':frame}), outcome)
            if step.get('tick'):
                runtime._stream_buffer._tick()
        with runtime.db() as db:
            runtime._stream_buffer.flush_locked(db, force=True)

def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()


# Cache only Python-written pre-state, never a Python post-state or a Rust result.
# One Runtime holds the disposable root; each case receives an independent SQLite
# backup, fresh stream buffer, and empty decoded-record cache.
_runtime = None
_pristine = None
_seeds = {}


def backup(source):
    target = sqlite3.connect(":memory:")
    source.backup(target)
    return target


def run_case(command, template, runtime_type):
    if command['command'] == 'metrics':
        import resource
        import os
        usage = resource.getrusage(resource.RUSAGE_SELF)
        return {'userSeconds':usage.ru_utime,'systemSeconds':usage.ru_stime,'clockTicks':os.sysconf('SC_CLK_TCK')}
    global _runtime, _pristine
    if _runtime is None:
        root = pathlib.Path(command['root'])
        shutil.copytree(template, root)
        _runtime = runtime_type(root, lambda *_a, **_kw: None)
        with _runtime.read_db() as db:
            _pristine = backup(db)
    runtime, case = _runtime, command['case']
    key = json.dumps({k:case[k] for k in ('submission','agent','remove','items','related','events','search_phase','corrupt','expect_refusal','receipt_result','assets') if k in case}, sort_keys=True)
    runtime.close_notification_database()
    runtime.__dict__.pop('_agent_records_cache', None)
    prepared_path = runtime.root.parent / 'prepared.json'
    if key not in _seeds:
        with sqlite3.connect(runtime.db_path) as db:
            _pristine.backup(db)
        prepared_path.unlink(missing_ok=True)
        seed(runtime, case)
        with runtime.read_db() as db:
            saved = backup(db)
        _seeds[key] = (saved, copy.deepcopy(getattr(runtime, 'corpus_submission', None)),
                       prepared_path.read_text() if prepared_path.exists() else None)
    saved, captured, prepared = _seeds[key]
    with sqlite3.connect(runtime.db_path) as db:
        saved.backup(db)
    runtime.corpus_submission = copy.deepcopy(captured)
    if prepared is None:
        prepared_path.unlink(missing_ok=True)
    else:
        prepared_path.write_text(prepared)
    runtime._stream_buffer = codex_streaming.StreamBuffer(runtime)
    rust = pathlib.Path(command['rust'])
    rust.mkdir(parents=True)
    shutil.copytree(runtime.root / 'multi-server', rust / 'multi-server')
    shutil.copyfile(template / 'analytics.sqlite3', rust / 'analytics.sqlite3')
    with sqlite3.connect(rust / 'canvas.sqlite3') as db:
        saved.backup(db)
    if prepared is not None:
        (rust.parent / 'prepared.json').write_text(prepared)
    import __main__ as oracle
    oracle.assert_backup_equivalent(runtime.root, rust)
    before = snapshot(runtime.root)
    assert snapshot(rust) == before, 'pre-states differ'
    import threading
    runtime.__dict__.setdefault('_callback_db', threading.local()).reuse = True
    transition(runtime, case)
    return {'before':before, 'after':snapshot(runtime.root), 'pythonRoot':str(runtime.root)}


def close():
    if _runtime is not None:
        _runtime.close()
        _pristine.close()
    for saved, _, _ in _seeds.values():
        saved.close()
