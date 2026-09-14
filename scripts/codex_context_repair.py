"""Repair verified Studio event inputs through an immutable native rollout copy.

No model request or command is sent. The source and Studio history stay intact.
An ambiguous fork receipt blocks another fork, including after a server restart.
"""
import copy
import concurrent.futures
import hashlib
import json
import os
import shutil
from pathlib import Path
import time
import uuid

from codex_efficiency import EfficiencyMixin

ACTIVE = {'preparing', 'submitted', 'unknown', 'ready'}
KINDS = {'monitor_exit', 'agent_message', 'work_review', 'work_decision'}
IDENTITY = ('id', 'accountKey', 'epoch', 'threadId')
WAIT_SECONDS = 10


def blocked(agent):
    return (agent.get('contextRepair') or {}).get('phase') in ACTIVE


def assert_context_available(agent, *, attempt_id=None):
    if blocked(agent):
        raise ValueError('Context repair awaits its exact native receipt: ' + agent['contextRepair']['id'])


def _attempt(agent):
    attempt = agent.get('startAttempt') or {}
    return attempt.get('id') if not attempt.get('submitted') else None


def _identity(agent):
    return {**{k: agent.get(k) for k in IDENTITY}, 'attemptId': _attempt(agent)}


def _current(rt, db, op):
    a = rt.agent(op['agent'], db)
    if (rt.closed or a.get('deletedAt') or _identity(a) != op['source']
            or (a.get('contextRepair') or {}).get('id') != op['id']
            or rt.preparation_settings(a) != op['settings']
            or ('connectionId' in op and not rt.connection_current(a.get('accountKey', 'default'), op['connectionId']))):
        raise ValueError('The agent changed during context repair. The original session is preserved.')
    return a


def _save(rt, db, a, op):
    op['updated'] = time.time()
    a['contextRepair'] = copy.deepcopy(op)
    rt.put(db, 'agents', a)


def _local_idle(rt, db, a, attempt_id):
    from codex_native_errors import assert_native_thread_open
    from codex_safety_buffering import active as safety_active
    assert_native_thread_open(a)
    browser = a.get('browserRecovery') or {}
    browser_same = all(browser.get(k) == a.get(k) for k in ('epoch', 'accountKey', 'threadId'))
    browser_request = browser.get('nativeRequest') or {}
    restart = a.get('restartRecovery') or {}
    restart_same = all(restart.get(k) == a.get(k) for k in ('epoch', 'accountKey', 'threadId'))
    if (safety_active(a) or (browser_same and (browser.get('stage') in {'pending', 'reconnecting'}
            or (browser_request.get('submittedAt') and browser_request.get('outcome') != 'received')))
            or (restart_same and restart.get('stage') in {'pending', 'held'})):
        raise ValueError('Context repair waits for the existing native recovery receipt')
    attempt = a.get('startAttempt') or {}
    if attempt_id:
        if attempt.get('id') != attempt_id or attempt.get('submitted'):
            raise ValueError('The native turn has already started or changed')
    elif a.get('inFlight') or a.get('status') in {'starting', 'running', 'compacting'}:
        raise ValueError('Context repair requires an idle agent')
    if a.get('accountTransferId') or a.get('workspaceOperation') or a.get('activeTools'):
        raise ValueError('Context repair waits for the current agent operation')
    prep = rt.preparations.get(a['id'])
    if prep and not prep['future'].done():
        raise ValueError('Context repair waits for the native preparation receipt')
    for table, column, statuses in (
        ('tasks', 'status', ('starting', 'running', 'pending', 'unknown')),
        ('monitors', 'status', ('starting', 'running', 'pending', 'stopping')),
        ('requests', 'status', ('pending',)),
        ('tool_requests', 'stage', ('queued', 'running')),
        ('tool_requests', 'outcome', ('unknown',)),
    ):
        if not db.execute('SELECT 1 FROM sqlite_master WHERE name=?', ('runtime_' + table,)).fetchone():
            continue
        if db.execute(f"SELECT 1 FROM runtime_{table} WHERE json_extract(record,'$.agent')=? "
                      f"AND json_extract(record,'$.{column}') IN ({','.join('?' for _ in statuses)}) LIMIT 1",
                      (a['id'], *statuses)).fetchone():
            raise ValueError('Context repair waits for commands, monitors, and tool receipts')
    permitted = set(attempt.get('events', [])) if attempt_id else set()
    unsettled = db.execute("SELECT id FROM runtime_events WHERE agent=? AND epoch=? "
                           "AND status IN ('reserved','dispatching','uncertain')", (a['id'], a['epoch']))
    if any(row[0] not in permitted for row in unsettled):
        raise ValueError('Context repair waits for a confirmed input receipt')
    if db.execute("SELECT 1 FROM sqlite_master WHERE name='voice_sessions'").fetchone():
        columns = {r[1] for r in db.execute('PRAGMA table_info(voice_sessions)')}
        if 'state' in columns and db.execute('SELECT 1 FROM voice_sessions WHERE agent=? AND state IS NOT NULL AND ended IS NULL', (a['id'],)).fetchone():
            raise ValueError('Context repair waits for voice to end')


def verified_events(db, a):
    receipt = a.get('contextRepair') or {}
    repaired = set(receipt.get('repairedEventIds', [])) if receipt.get('newThreadId') == a.get('threadId') else set()
    checked_thread = receipt.get('newThreadId') if receipt.get('phase') == 'completed' else (receipt.get('source') or {}).get('threadId')
    if (receipt.get('phase') in {'unchanged', 'completed'} and checked_thread == a.get('threadId')
            and receipt.get('compactions') == a.get('compactions', 0)):
        repaired.update(receipt.get('checkedEventIds', []))
    events = [dict(r) for r in db.execute(
        "SELECT e.* FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id "
        "WHERE e.agent=? AND e.status='delivered' "
        "AND e.kind IN ('monitor_exit','agent_message','work_review','work_decision') "
        "AND length(e.text)>3000 AND coalesce(json_extract(m.record,'$.modelEventProjection'),0)!=1 "
        "ORDER BY e.created", (a['id'],)) if r['turn_id'] and r['id'] not in repaired]
    if not events:
        return []
    # A user can quote an actual event in a mixed native input batch. The exact
    # synthetic prefix alone cannot authorize replacement inside that user text.
    users = {}
    for turn, text in db.execute("SELECT turn_id,text FROM runtime_events WHERE agent=? "
                                 "AND kind IN ('user','followup') AND status='delivered'", (a['id'],)):
        users.setdefault(turn, []).append(text)
    return [e for e in events if not any('[Orchestration event: ' + e['kind'] + ']\n' + e['text'] in text
                                        for text in users.get(e['turn_id'], []))]


def sanitized_rollout(source, destination, thread_id, events):
    """Change exact event prefixes only, with matching native turn provenance.

    Instructions, user text, attachments, summaries and tool receipts retain their
    values. Unknown input structures never authorize text replacement.
    """
    source, destination = Path(source), Path(destination)
    with source.open('rb') as stream:
        first = json.loads(stream.readline())
    if first.get('type') != 'session_meta' or first['payload'].get('id') != thread_id:
        raise ValueError('The native rollout identity does not match the agent')
    inherited = bool(first['payload'].get('history_base'))
    # Native paginated threads reject alternate paths for their registered UUID.
    # The private import has its own UUID. The receipt retains the source UUID.
    import_id = str(uuid.uuid4())
    destination = destination.with_name(destination.name.replace(thread_id, import_id))
    by_turn = {}
    for event in events:
        if event.get('kind') in KINDS and event.get('turn_id'):
            by_turn.setdefault(event['turn_id'], []).append(event)
    changes, saved, matched_messages = [], 0, {}

    def repair_item(item, turn=None):
        nonlocal saved
        if (item.get('type') != 'message' or item.get('role') != 'user'
                or not isinstance(item.get('id'), str) or not item['id']):
            return False
        meta = item.get('internal_chat_message_metadata_passthrough') or {}
        turn = meta.get('turn_id', turn)
        changed = False
        for content in item.get('content', []):
            if content.get('type') != 'input_text' or not isinstance(content.get('text'), str):
                continue
            text = content['text']
            # Synthetic events lead their input block. Never search user prose for
            # matching substrings, even when that prose quotes a real event.
            position, replacements = 0, []
            while position < len(text):
                candidates = []
                for event in by_turn.get(turn, []):
                    full = '[Orchestration event: ' + event['kind'] + ']\n' + event['text']
                    if text.startswith(full, position) and (position + len(full) == len(text) or text.startswith('\n\n', position + len(full))):
                        candidates.append((event, full))
                if len(candidates) != 1:
                    break
                event, full = candidates[0]
                matched_messages.setdefault((turn, event['id']), set()).add(item['id'])
                small = '[Orchestration event: ' + event['kind'] + ']\n' + EfficiencyMixin.bounded_event(event, event['text'], 3000)
                replacements.append(small)
                saved += len(full.encode()) - len(small.encode())
                changes.append(event['id'])
                position += len(full)
                if text.startswith('\n\n', position):
                    replacements.append('\n\n')
                    position += 2
            if replacements:
                content['text'] = ''.join(replacements) + text[position:]
                changed = True
        return changed

    turn, snapshot = None, False
    source_hash, clean_hash = hashlib.sha256(), hashlib.sha256()
    source_bytes, clean_bytes = 0, 0
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    created = False
    try:
        with source.open('rb') as incoming, destination.open('xb') as outgoing:
            created = True
            os.chmod(destination, 0o600)
            for index, raw in enumerate(incoming):
                if not raw.endswith(b'\n'):
                    raise ValueError('The native rollout is not fully persisted')
                source_hash.update(raw)
                source_bytes += len(raw)
                record = json.loads(raw)
                payload = record.get('payload') or {}
                changed = False
                if index == 0:
                    if record != first:
                        raise ValueError('The source context changed before its copy')
                    payload['id'] = import_id
                    changed = True
                elif record.get('type') == 'turn_context':
                    turn = payload.get('turn_id')
                elif record.get('type') == 'response_item':
                    changed = repair_item(payload, turn)
                elif record.get('type') == 'compacted':
                    snapshot = snapshot or isinstance(payload.get('replacement_history'), list)
                    for item in payload.get('replacement_history') or []:
                        changed = repair_item(item) or changed
                clean = (json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n').encode() if changed else raw
                outgoing.write(clean)
                clean_hash.update(clean)
                clean_bytes += len(clean)
            if any(len(ids) > 1 for ids in matched_messages.values()):
                raise ValueError('The native event identity is ambiguous; original context is preserved')
            if inherited and not snapshot:
                raise ValueError('Inherited native context needs a persisted context snapshot before repair')
            outgoing.flush()
            os.fsync(outgoing.fileno())
    except Exception:
        # This UUID directory and file belong only to this repair attempt.
        if created:
            destination.unlink(missing_ok=True)
        raise
    report = {'sourcePath': str(source), 'sourceSha256': source_hash.hexdigest(),
              'sourceBytes': source_bytes, 'importThreadId': import_id,
              'savedBytes': saved, 'eventIds': sorted(set(changes))}
    if changes:
        report.update(copyPath=str(destination), copySha256=clean_hash.hexdigest(), copyBytes=clean_bytes)
    else:
        destination.unlink()
    return report


def _file_identity(path):
    stat = Path(path).stat()
    return [stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns]


def _hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def _native_idle(server, tid):
    native = server.call('thread/read', {'threadId': tid, 'includeTurns': False}, timeout=10)['thread']
    status = native.get('status', {}).get('type')
    if native.get('id') != tid or status not in {'idle', 'notLoaded', 'systemError'}:
        raise ValueError('Context repair requires a confirmed idle native thread; native status: '
                         + json.dumps(native.get('status'), sort_keys=True))
    for method in ('thread/backgroundTerminals/list', 'thread/queue/list'):
        result = server.call(method, {'threadId': tid}, timeout=10)
        if result.get('data') or result.get('nextCursor'):
            raise ValueError('Context repair waits for native commands and queued input')
    turns = server.call('thread/turns/list', {'threadId': tid, 'limit': 1,
                       'sortDirection': 'desc', 'itemsView': 'notLoaded'}, timeout=10)
    # Native systemError is loaded and idle after failure. It still needs the
    # same queue, command, terminal-turn, and tool-receipt evidence as idle.
    if status == 'systemError' and not turns.get('data'):
        raise ValueError('Context repair needs a terminal native turn for systemError')
    if turns.get('data'):
        turn = turns['data'][0]
        if turn.get('status') not in {'completed', 'failed', 'interrupted'}:
            raise ValueError('Context repair requires a terminal native turn')
        page = server.call('thread/items/list', {'threadId': tid, 'turnId': turn['id'],
                           'limit': 1000, 'sortDirection': 'asc'}, timeout=10)
        from codex_connection_recovery import native_operations_settled
        if page.get('nextCursor') or not native_operations_settled({'items': [e['item'] for e in page.get('data', [])]}):
            raise ValueError('Context repair waits for complete native tool receipts')
    if not native.get('path'):
        raise ValueError('The native context has no saved rollout path')
    return native


def _settle(rt, op, future):
    try:
        result = future.result()
        tid = (result.get('thread') or {}).get('id')
        if not isinstance(tid, str) or not tid or tid == op['source']['threadId']:
            raise RuntimeError('The context fork returned no new thread identity; outcome unknown')
        with rt.lock, rt.db() as db:
            a = rt.agent(op['agent'], db)
            stored = a.get('contextRepair') or {}
            if stored.get('id') != op['id'] or stored.get('phase') == 'completed':
                return
            stored.update(result=result, phase='ready')
            _save(rt, db, a, stored)
            db.commit()
            a = _current(rt, db, stored)
            _local_idle(rt, db, a, stored['source']['attemptId'])
            a.setdefault('accountHistory', []).append({'contextRepairId': stored['id'],
                'accountKey': stored['source']['accountKey'], 'threadId': stored['source']['threadId'], 'at': time.time()})
            a.update(threadId=tid, turnId=None)
            if stored['source']['attemptId']:
                a['startAttempt']['threadId'] = tid
            stored.update(phase='completed', newThreadId=tid, error=None,
                          repairedEventIds=sorted(set(stored.get('previousRepairedEventIds', [])) | set(stored['snapshot']['eventIds'])))
            _save(rt, db, a, stored)
            rt.loaded.discard(a['id'])
            rt.preparations.pop(a['id'], None)
            rt.changed.set()
    except Exception as error:
        _fail(rt, op, error, unknown=True)


def _fail(rt, op, error, *, unknown):
    if rt.closed:
        return
    with rt.lock, rt.db() as db:
        a = rt.agent(op['agent'], db)
        stored = a.get('contextRepair') or {}
        if stored.get('id') != op['id'] or stored.get('phase') == 'completed':
            return
        stored.update(phase='unknown' if unknown else 'failed', error=str(error))
        _save(rt, db, a, stored)


def repair_before_start(rt, agent):
    return _repair(rt, agent['id'], _attempt(agent))


def repair_idle(rt, key):
    return _repair(rt, key, None)


def _repair(rt, key, attempt_id):
    with rt.lock, rt.db() as db:
        a = rt.agent(key, db)
        old = a.get('contextRepair') or {}
        if blocked(a):
            raise ValueError('Context repair awaits its exact native receipt: ' + old['id'])
        if not a.get('threadId'):
            return a
        events = verified_events(db, a)
        if not events:
            return a
        _local_idle(rt, db, a, attempt_id)
        checked = sorted(e['id'] for e in events)
        checked_thread = old.get('newThreadId') if old.get('phase') == 'completed' else (old.get('source') or {}).get('threadId')
        if checked_thread == a.get('threadId') and old.get('compactions') == a.get('compactions', 0):
            checked = sorted(set(checked) | set(old.get('checkedEventIds', [])))
        op = {'id': str(uuid.uuid4()), 'agent': key, 'source': _identity(a),
              'settings': rt.preparation_settings(a), 'phase': 'preparing', 'created': time.time(),
              'checkedEventIds': checked, 'compactions': a.get('compactions', 0),
              'previousRepairedEventIds': old.get('repairedEventIds', []) if old.get('newThreadId') == a.get('threadId') else []}
        if old:
            a.setdefault('contextRepairHistory', []).append(old)
        _save(rt, db, a, op)
    submitted = False
    try:
        server = rt.connect(a.get('accountKey', 'default'))
        op['connectionId'] = rt.connection_ids.get(a.get('accountKey', 'default'))
        native = _native_idle(server, a['threadId'])
        # Unsubscribe only an idle thread. This flushes its rollout without a model
        # request and leaves the source available for rollback and inspection.
        if native['status']['type'] in {'idle', 'systemError'}:
            server.call('thread/unsubscribe', {'threadId': a['threadId']}, timeout=10)
        with rt.lock, rt.db() as db:
            current = _current(rt, db, op)
            _local_idle(rt, db, current, attempt_id)
            rt.loaded.discard(key)
            rt.preparations.pop(key, None)
        source = Path(native['path']).resolve()
        home = Path(rt.accounts.home(a.get('accountKey', 'default'))).resolve()
        relative = source.relative_to(home)
        if relative.parts[0] not in {'sessions', 'archived_sessions'} or source.suffix != '.jsonl':
            raise ValueError('Unsupported native history path')
        destination = home / 'sessions' / '.studio-context-repairs' / op['id'] / source.name
        # Native fork can make its own history copy. Keep capacity for both copies.
        if shutil.disk_usage(rt.root).free < source.stat().st_size * 3 + 64 * 1024 * 1024:
            raise ValueError('Context repair needs free space for three copies of this rollout')
        report = sanitized_rollout(source, destination, a['threadId'], events)
        source_hash = _hash_file(source)
        report['sourceFileIdentity'] = _file_identity(source)
        with rt.lock, rt.db() as db:
            current = _current(rt, db, op)
            _local_idle(rt, db, current, attempt_id)
            op['snapshot'] = report
            if not report['eventIds']:
                op['phase'] = 'unchanged'
                _save(rt, db, current, op)
                return current
            if source_hash != report['sourceSha256']:
                raise ValueError('The source context changed before its fork')
            params = rt.new_thread_params(current)
            params.pop('dynamicTools', None)
            params.update(threadId=report['importThreadId'], path=report['copyPath'], excludeTurns=True, deferGoalContinuation=True)
            op.update(phase='submitted', connectionId=rt.connection_ids.get(a.get('accountKey', 'default')),
                      rpcMethod='thread/fork')
            _save(rt, db, current, op)
            db.commit()
            submitted = True
            ticket = rt.submit_reserved(server, 'thread/fork', params)
            if isinstance(ticket, tuple):
                op['requestId'] = ticket[0]
                _save(rt, db, current, op)
        settled = concurrent.futures.Future()
        def receive(future):
            _settle(rt, op, future)
            with rt.lock, rt.db() as db:
                receipt = (rt.agent(key, db).get('contextRepair') or {})
            if receipt.get('phase') == 'completed':
                settled.set_result(None)
            else:
                settled.set_exception(RuntimeError(receipt.get('error') or 'Context repair outcome unknown'))
        server.on_result(ticket, receive)
        try:
            settled.result(WAIT_SECONDS)
        except concurrent.futures.TimeoutError:
            _fail(rt, op, RuntimeError('Context fork response pending; outcome unknown'), unknown=True)
            if attempt_id:
                from codex_runtime import PreparationPending
                raise PreparationPending(settled) from None
            raise RuntimeError('Context fork response pending; outcome unknown') from None
        with rt.lock, rt.db() as db:
            current = rt.agent(key, db)
            if (current.get('contextRepair') or {}).get('phase') != 'completed':
                raise ValueError('Context repair awaits its exact native receipt: ' + op['id'])
            return current
    except Exception as error:
        _fail(rt, op, error, unknown=submitted)
        raise
