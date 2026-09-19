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
    return bool(agent.get('contextRepairWait')) or (agent.get('contextRepair') or {}).get('phase') in ACTIVE


def assert_context_available(agent, *, attempt_id=None):
    if agent.get('contextRepairWait'):
        raise _waiting(agent['contextRepairWait']['error'])
    if blocked(agent):
        raise ValueError('Context repair awaits its exact native receipt: ' + agent['contextRepair']['id'])


def _waiting(message, scope='local'):
    error = ValueError(message)
    error.contextRepairWait = {'scope': scope}
    return error


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
    if ('historicalInputs' in op
            and _unsettled_inputs(db, a, op['source']['attemptId']) != op['historicalInputs']):
        raise _waiting('Context repair waits for unchanged historical input receipts')
    return a


def _save(rt, db, a, op):
    op['updated'] = time.time()
    a['contextRepair'] = copy.deepcopy(op)
    rt.put(db, 'agents', a)


def _unsettled_inputs(db, a, attempt_id):
    attempt = a.get('startAttempt') or {}
    permitted = set(attempt.get('events', [])) if attempt_id else set()
    historical = []
    rows = db.execute("SELECT e.*,m.record AS metadata FROM runtime_events e "
        "LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.agent=? AND e.epoch=? "
        "AND e.status IN ('reserved','dispatching','uncertain') ORDER BY e.id", (a['id'], a['epoch']))
    for row in rows:
        if row['id'] in permitted and row['status'] != 'uncertain':
            continue
        # AppServer.write rejects offline before writing bytes; submit removes
        # its pending future. Preserve legacy uncertainty without replay.
        if (row['status'] == 'uncertain' and row['error'] == 'Codex app-server is offline'
                and row['turn_id'] is None and row['metadata'] is None
                and row['id'] not in attempt.get('events', []) and attempt.get('submitted') is False
                and isinstance(row['created'], (int, float))):
            historical.append({'id':row['id'], 'created':row['created'],
                'sha256':hashlib.sha256(json.dumps(dict(row), sort_keys=True).encode()).hexdigest()})
            continue
        raise _waiting('Context repair waits for a confirmed input receipt: ' + row['id'])
    return historical


def _local_idle(rt, db, a, attempt_id, *, allow_background_work=False):
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
        raise _waiting('Context repair waits for the existing native recovery receipt')
    attempt = a.get('startAttempt') or {}
    if attempt_id:
        if attempt.get('id') != attempt_id or attempt.get('submitted'):
            raise ValueError('The native turn has already started or changed')
    elif a.get('inFlight') or a.get('status') in {'starting', 'running', 'compacting'}:
        raise ValueError('Context repair requires an idle agent')
    if a.get('accountTransferId') or a.get('workspaceOperation') or a.get('activeTools'):
        raise _waiting('Context repair waits for the current agent operation')
    prep = rt.preparations.get(a['id'])
    if prep and not prep['future'].done():
        raise _waiting('Context repair waits for the native preparation receipt')
    for table, column, statuses in (
        ('tasks', 'status', ('starting', 'running', 'pending', 'unknown')),
        ('monitors', 'status', ('starting', 'running', 'pending', 'stopping')),
        ('requests', 'status', ('pending',)),
        ('tool_requests', 'stage', ('queued', 'running')),
    ):
        if table == 'monitors' and allow_background_work:
            continue
        if not db.execute('SELECT 1 FROM sqlite_master WHERE name=?', ('runtime_' + table,)).fetchone():
            continue
        nonblocking = " AND coalesce(json_extract(record,'$.method'),'')!='agent/asyncQuestion'" if table == 'requests' else ''
        if table == 'tasks' and allow_background_work:
            # Only known running commands may continue on the same thread.
            # Unknown outcomes and unfinished tool callbacks remain blockers.
            nonblocking += (" AND NOT (coalesce(json_extract(record,'$.kind'),'')='command'"
                            " AND json_extract(record,'$.status')='running'"
                            " AND coalesce(cast(json_extract(record,'$.processId') AS TEXT),'')!='')")
        row = db.execute(f"SELECT id FROM runtime_{table} WHERE json_extract(record,'$.agent')=? "
                         f"AND json_extract(record,'$.{column}') IN ({','.join('?' for _ in statuses)}){nonblocking} LIMIT 1",
                         (a['id'], *statuses)).fetchone()
        if row:
            raise _waiting('Context repair waits for ' + table + ': ' + row[0])
    historical = _unsettled_inputs(db, a, attempt_id)
    if db.execute("SELECT 1 FROM sqlite_master WHERE name='voice_sessions'").fetchone():
        columns = {r[1] for r in db.execute('PRAGMA table_info(voice_sessions)')}
        if 'state' in columns and db.execute('SELECT 1 FROM voice_sessions WHERE agent=? AND state IS NOT NULL AND ended IS NULL', (a['id'],)).fetchone():
            raise _waiting('Context repair waits for voice to end')
    return historical


def _inherited_checked_events(db, a):
    """Carry a completed repair through exact committed account-transfer forks."""
    history = [h for h in a.get('accountHistory', []) if h.get('transferId')]
    if not history or not db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_account_transfers'").fetchone():
        return set()
    repairs = [a.get('contextRepair') or {}, *a.get('contextRepairHistory', [])]
    scope, seen = (a.get('accountKey', 'default'), a.get('threadId')), set()
    checked = set()
    for _ in range(len(history) + 1):
        if scope in seen:
            return set()
        seen.add(scope)
        for receipt in repairs:
            source = receipt.get('source') or {}
            if (receipt.get('phase') == 'completed' and receipt.get('agent') == a['id']
                    and source.get('id') == a['id'] and source.get('epoch') == a['epoch']
                    and (source.get('accountKey'), receipt.get('newThreadId')) == scope
                    and receipt.get('compactions') == a.get('compactions', 0)):
                checked.update(receipt.get('checkedEventIds', []))
        parents = set()
        for entry in history:
            row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?', (entry['transferId'],)).fetchone()
            if not row:
                continue
            transfer = json.loads(row[0])
            member = (transfer.get('members') or {}).get(a['id']) or {}
            source = member.get('source') or {}
            target = (member.get('result') or {}).get('thread') or {}
            parent = (entry.get('accountKey', 'default'), entry.get('threadId'))
            if (transfer.get('id') == entry['transferId'] and transfer.get('status') in {'pending', 'completed', 'cancelled'}
                    and member.get('phase') == 'completed'
                    and (transfer.get('targetAccountKey'), target.get('id')) == scope
                    and parent == (member.get('sourceAccountKey'), member.get('sourceThreadId'))
                    and parent == (source.get('accountKey'), source.get('threadId'))
                    and source.get('epoch') == a['epoch'] and parent[1]
                    and target.get('forkedFromId') == parent[1]):
                parents.add(parent)
        if not parents:
            return checked
        if len(parents) != 1:
            return set()
        scope = parents.pop()
    return set()


def verified_events(db, a):
    receipt = a.get('contextRepair') or {}
    repaired = set(receipt.get('repairedEventIds', [])) if receipt.get('newThreadId') == a.get('threadId') else set()
    checked_thread = receipt.get('newThreadId') if receipt.get('phase') == 'completed' else (receipt.get('source') or {}).get('threadId')
    if (receipt.get('phase') in {'unchanged', 'completed'} and checked_thread == a.get('threadId')
            and receipt.get('compactions') == a.get('compactions', 0)):
        repaired.update(receipt.get('checkedEventIds', []))
    repaired.update(_inherited_checked_events(db, a))
    events = [dict(r) for r in db.execute(
        "SELECT e.* FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id "
        "WHERE e.agent=? AND e.status='delivered' "
        "AND e.kind IN ('monitor_exit','agent_message','work_review','work_decision') "
        "AND length(e.text)>3000 AND coalesce(json_extract(m.record,'$.modelEventProjection'),0)!=1 "
        "ORDER BY e.created", (a['id'],)) if r['turn_id'] and r['id'] not in repaired]
    # Versioned role blocks are generated after the first event in a batch.
    # Their full text persists as user input across native remote compaction.
    for row in db.execute("SELECT e.*,m.record AS metadata FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id "
                          "WHERE e.agent=? AND e.status='delivered' AND e.kind IN ('monitor_exit','agent_message','work_review','work_decision') "
                          "AND json_extract(m.record,'$.contextManifest.versions.roleSkill') IS NOT NULL", (a['id'],)):
        row = dict(row)
        context_id = 'studio-role:' + row['id']
        if row['turn_id'] and context_id not in repaired:
            version = json.loads(row['metadata'])['contextManifest']['versions']['roleSkill']
            events.append({**row, 'id':context_id, 'kind':'studio_role', 'roleVersion':version,
                           'prefix':'[Orchestration event: ' + row['kind'] + ']\n' + row['text']})
    if not events:
        return []
    # A user can quote an actual event in a mixed native input batch. The exact
    # synthetic prefix alone cannot authorize replacement inside that user text.
    users = {}
    for turn, text in db.execute("SELECT turn_id,text FROM runtime_events WHERE agent=? "
                                 "AND kind IN ('user','followup') AND status='delivered'", (a['id'],)):
        users.setdefault(turn, []).append(text)
    return [e for e in events if not (e['kind'] == 'studio_role' and users.get(e['turn_id']))
            and not any('[Orchestration event: ' + e['kind'] + ']\n' + e['text'] in text
                                        for text in users.get(e['turn_id'], []))]


def _prefix_records(path, end):
    """Read exactly the native byte boundary, never an ancestor's later suffix."""
    with Path(path).open('rb') as stream:
        remaining = end
        while remaining:
            raw = stream.readline(min(remaining, 64 * 1024 * 1024 + 1))
            if len(raw) > 64 * 1024 * 1024:
                raise ValueError('Native context record exceeds the 64 MiB line limit')
            if not raw or not raw.endswith(b'\n'):
                raise _waiting('Context repair waits for the complete saved rollout tail', 'native')
            remaining -= len(raw)
            yield raw


def _rollout_segments(home, source, thread_id):
    """Validate native history_base byte and ordinal boundaries within one home."""
    home, source = Path(home).resolve(), Path(source).resolve()
    roots = [home / 'sessions', home / 'archived_sessions']
    segments, seen, total = [], set(), 0
    path, tid, end, ordinal_end = source, thread_id, source.stat().st_size, None
    for _ in range(32):
        if tid in seen:
            raise ValueError('Native context ancestry contains a cycle')
        seen.add(tid)
        relative = path.relative_to(home)
        if relative.parts[0] not in {'sessions', 'archived_sessions'} or path.suffix != '.jsonl':
            raise ValueError('Unsupported native ancestry path')
        total += end
        if total > 8 * 1024 ** 3:
            raise ValueError('Native context ancestry exceeds the copy size limit')
        digest, first, last = hashlib.sha256(), None, None
        for raw in _prefix_records(path, end):
            digest.update(raw)
            record = json.loads(raw)
            if first is None:
                first = record
            ordinal = record.get('ordinal')
            if ordinal_end is not None or first.get('payload', {}).get('history_base'):
                if type(ordinal) is not int or (last is not None and ordinal != last + 1):
                    raise ValueError('Native context ancestry has an invalid ordinal boundary')
            last = ordinal
        if not first or first.get('type') != 'session_meta' or first.get('payload', {}).get('id') != tid:
            raise ValueError('The native ancestry identity does not match its history reference')
        base = first['payload'].get('history_base')
        if ordinal_end is not None and (last is None or last + 1 != ordinal_end):
            raise ValueError('Native context ancestry has an invalid ordinal cutoff')
        if base:
            if (not isinstance(base, dict) or set(base) != {'thread_id', 'end_ordinal_exclusive', 'end_byte_offset'}
                    or any(type(base[k]) is not int or base[k] <= 0 for k in ('end_ordinal_exclusive', 'end_byte_offset'))):
                raise ValueError('Unsupported native context ancestry boundary')
            try:
                if str(uuid.UUID(base['thread_id'])) != base['thread_id']:
                    raise ValueError()
            except (ValueError, TypeError, AttributeError):
                raise ValueError('Unsupported native context ancestry identity') from None
            if first.get('ordinal') != base['end_ordinal_exclusive']:
                raise ValueError('Native context ancestry has an invalid starting ordinal')
        elif ordinal_end is not None and first.get('ordinal') != 0:
            raise ValueError('Native context ancestry does not start at ordinal zero')
        segments.append({'path':str(path), 'threadId':tid, 'endByteOffset':end,
                         'endOrdinalExclusive':ordinal_end, 'sha256':digest.hexdigest(), 'first':first})
        if not base:
            return list(reversed(segments))
        tid, end, ordinal_end = base['thread_id'], base['end_byte_offset'], base['end_ordinal_exclusive']
        candidates = []
        for root in roots:
            for candidate in root.rglob('*' + tid + '*.jsonl'):
                candidate = candidate.resolve()
                if not candidate.is_relative_to(root.resolve()):
                    raise ValueError('Native context ancestry leaves the account home')
                if candidate.stat().st_size < end:
                    continue
                metadata = json.loads(next(_prefix_records(candidate, end)))
                if metadata.get('type') == 'session_meta' and metadata.get('payload', {}).get('id') == tid:
                    candidates.append(candidate)
        if not candidates:
            raise ValueError('The native context ancestor is missing: ' + tid)
        hashes = {_prefix_hash(candidate, end) for candidate in candidates}
        if len(hashes) != 1:
            raise ValueError('The native context ancestor has conflicting saved prefixes: ' + tid)
        path = sorted(candidates)[0]
    raise ValueError('Native context ancestry exceeds the depth limit')


def _prefix_hash(path, end):
    digest = hashlib.sha256()
    for raw in _prefix_records(path, end):
        digest.update(raw)
    return digest.hexdigest()


def _projected_records(segments):
    if len(segments) == 1:
        yield from _prefix_records(segments[0]['path'], segments[0]['endByteOffset'])
        return
    metadata = copy.deepcopy(segments[-1]['first'])
    metadata['payload'].pop('history_base')
    metadata['ordinal'] = 0
    yield (json.dumps(metadata, ensure_ascii=False, separators=(',', ':')) + '\n').encode()
    ordinal = 1
    for segment in segments:
        for index, raw in enumerate(_prefix_records(segment['path'], segment['endByteOffset'])):
            if index == 0:
                continue
            record = json.loads(raw)
            if record.get('type') == 'session_meta':
                raise ValueError('Native context contains an unexpected session identity')
            record['ordinal'] = ordinal
            ordinal += 1
            yield (json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n').encode()


def sanitized_rollout(source, destination, thread_id, events, terminal_turn=None, *, segments=None,
                      inherited_empty=False, terminal_status=None, terminal_completed_at=None):
    """Change exact event prefixes only, with matching native turn provenance.

    User text, attachments, summaries and tool receipts retain their values.
    Exact versioned Studio role suffixes can be removed: the fork receives the
    current role in developer instructions. Unknown structures remain intact.
    """
    source, destination = Path(source), Path(destination)
    first = json.loads(next(_prefix_records(source, source.stat().st_size)))
    if first.get('type') != 'session_meta' or first['payload'].get('id') != thread_id:
        raise ValueError('The native rollout identity does not match the agent')
    inherited = bool(first['payload'].get('history_base'))
    if inherited and not segments:
        raise ValueError('Inherited native context requires verified ancestry')
    if inherited_empty:
        if not segments or len(segments) < 2:
            raise _waiting('Context repair needs a terminal native turn', 'native')
        for index, raw in enumerate(_prefix_records(source, segments[-1]['endByteOffset'])):
            record = json.loads(raw)
            if index and not (record.get('type') == 'event_msg' and record.get('payload', {}).get('type') == 'thread_settings_applied'):
                raise _waiting('Context repair needs a terminal native turn for the local history', 'native')
    # Native paginated threads reject alternate paths for their registered UUID.
    # The private import has its own UUID. The receipt retains the source UUID.
    import_id = str(uuid.uuid4())
    destination = destination.with_name(destination.name.replace(thread_id, import_id))
    by_turn = {}
    for event in events:
        if (event.get('kind') in KINDS or event.get('kind') == 'studio_role') and event.get('turn_id'):
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
            role_start = text.rfind('\n\n[Studio role skill: ')
            if role_start >= 0 and text.endswith('[End Studio role skill]'):
                from codex_efficiency import digest
                role = text[role_start + 2:]
                role_events = [event for event in by_turn.get(turn, []) if event.get('kind') == 'studio_role'
                               and digest(role) == event.get('roleVersion')
                               and text.startswith(event['prefix'] + '\n\n')
                               and len(event['prefix']) <= role_start]
                if len(role_events) == 1:
                    event = role_events[0]
                    matched_messages.setdefault((turn, event['id']), set()).add(item['id'])
                    saved += len(text[role_start:].encode())
                    changes.append(event['id'])
                    text = text[:role_start]
                    content['text'] = text
                    changed = True
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

    turn = None
    latest_started, latest_terminal, terminal_at = None, None, None
    source_hash, clean_hash = hashlib.sha256(), hashlib.sha256()
    source_bytes, clean_bytes = 0, 0
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    created = False
    try:
        with destination.open('xb') as outgoing:
            incoming = _projected_records(segments) if segments else _prefix_records(source, source.stat().st_size)
            created = True
            os.chmod(destination, 0o600)
            for index, raw in enumerate(incoming):
                if not raw.endswith(b'\n'):
                    raise _waiting('Context repair waits for the complete saved rollout tail', 'native')
                source_hash.update(raw)
                source_bytes += len(raw)
                record = json.loads(raw)
                payload = record.get('payload') or {}
                changed = False
                if index == 0:
                    if not inherited and record != first:
                        raise ValueError('The source context changed before its copy')
                    payload['id'] = import_id
                    changed = True
                elif record.get('type') == 'event_msg':
                    if payload.get('type') == 'task_started':
                        latest_started = payload.get('turn_id')
                    elif payload.get('type') in {'task_complete', 'turn_aborted'}:
                        latest_terminal = payload.get('turn_id')
                        terminal_at = payload.get('completed_at')
                        if not isinstance(terminal_at, (int, float)):
                            from datetime import datetime
                            try:
                                terminal_at = datetime.fromisoformat(record.get('timestamp', '').replace('Z', '+00:00')).timestamp()
                            except (ValueError, TypeError):
                                terminal_at = None
                elif record.get('type') == 'turn_context':
                    turn = payload.get('turn_id')
                    latest_started = turn
                elif record.get('type') == 'response_item':
                    changed = repair_item(payload, turn)
                elif record.get('type') == 'compacted':
                    for item in payload.get('replacement_history') or []:
                        changed = repair_item(item) or changed
                clean = (json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n').encode() if changed else raw
                outgoing.write(clean)
                clean_hash.update(clean)
                clean_bytes += len(clean)
            native_terminal_proof = (terminal_status == 'interrupted'
                or (terminal_status == 'completed' and isinstance(terminal_completed_at, (int, float))))
            if terminal_turn and latest_started == terminal_turn and latest_terminal != terminal_turn \
                    and native_terminal_proof:
                # A process can die after Codex persists the turn state but before
                # it appends the JSONL terminal event. Use the native terminal
                # status as proof and record the marker only in the immutable fork.
                marker_type = 'turn_aborted' if terminal_status == 'interrupted' else 'task_complete'
                marker = {'type': 'event_msg', 'payload': {'type': marker_type,
                    'turn_id': terminal_turn}}
                if isinstance(terminal_completed_at, (int, float)):
                    marker['payload']['completed_at'] = terminal_completed_at
                marker_raw = (json.dumps(marker, ensure_ascii=False, separators=(',', ':')) + '\n').encode()
                outgoing.write(marker_raw)
                clean_hash.update(marker_raw)
                clean_bytes += len(marker_raw)
                latest_terminal = terminal_turn
                terminal_at = terminal_completed_at
            if terminal_turn and (latest_started != terminal_turn or latest_terminal != terminal_turn):
                raise _waiting('Context repair waits for the saved terminal turn: ' + terminal_turn, 'native')
            if any(len(ids) > 1 for ids in matched_messages.values()):
                raise ValueError('The native event identity is ambiguous; original context is preserved')
            if inherited_empty and (not latest_terminal or latest_started != latest_terminal):
                raise _waiting('Context repair waits for the saved inherited terminal turn', 'native')
            outgoing.flush()
            os.fsync(outgoing.fileno())
    except Exception:
        # This UUID directory and file belong only to this repair attempt.
        if created:
            destination.unlink(missing_ok=True)
        raise
    report = {'sourcePath': str(source), 'sourceSha256': source_hash.hexdigest(),
              'sourceBytes': source_bytes, 'importThreadId': import_id,
              'savedBytes': saved, 'eventIds': sorted(set(changes)), 'terminalTurnId': latest_terminal,
              'terminalCompletedAt': terminal_at}
    if segments:
        report.update(sourceSha256=segments[-1]['sha256'], sourceBytes=segments[-1]['endByteOffset'])
        report['ancestry'] = [{k:v for k,v in segment.items() if k != 'first'} for segment in segments]
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


def _native_read(server, method, params, timeout=10):
    try:
        return server.call(method, params, timeout=timeout)
    except (RuntimeError, OSError) as cause:
        # A lost read cannot submit model input or a fork. Preserve the wait and
        # the exact accepted start, instead of treating it as a lost mutation.
        error = _waiting('Context repair waits for native history read (' + method + '): ' + str(cause), 'native')
        error.contextRepairWait['readOnly'] = True
        raise error from cause


def _terminal_native_item(item):
    kind = item.get('type')
    if kind in {'commandExecution', 'fileChange'}:
        return item.get('status') in {'completed', 'failed', 'declined'}
    if kind in {'dynamicToolCall', 'mcpToolCall', 'computerToolCall', 'collabAgentToolCall'}:
        return item.get('status') in {'completed', 'failed'}
    return True


def _callback_barrier(server):
    if not hasattr(server, 'after_events'):
        return
    drained = concurrent.futures.Future()
    server.after_events(lambda: drained.set_result(None))
    try:
        drained.result(WAIT_SECONDS)
    except concurrent.futures.TimeoutError as cause:
        raise _waiting('Context repair waits for native notification delivery', 'native') from cause


def _native_items(server, tid, turn_id, deadline=None):
    # Installed 0.154 ThreadItemsListParams uses cursor, not before/after IDs.
    cursor, seen, entries = None, set(), []
    deadline = deadline if deadline is not None else time.monotonic() + 20
    for _ in range(100):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        params = {'threadId': tid, 'turnId': turn_id, 'limit': 1000, 'sortDirection': 'asc'}
        if cursor is not None:
            params['cursor'] = cursor
        page = _native_read(server, 'thread/items/list', params, timeout=min(10, remaining))
        for entry in page.get('data', []):
            if entry.get('turnId', turn_id) != turn_id:
                raise ValueError('Native item history belongs to another turn')
            entries.append(entry)
        cursor = page.get('nextCursor')
        if not cursor:
            return entries
        if cursor in seen:
            raise ValueError('Native item history repeated its page cursor')
        seen.add(cursor)
    raise _waiting('Context repair waits for complete native history pages', 'native')


def _unresolved_tool_receipts(rt, a):
    with rt.lock, rt.db() as db:
        rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "AND json_extract(record,'$.outcome')='unknown'", (a['id'],)).fetchall()
        unresolved = []
        current_scope = (a.get('accountKey', 'default'), a.get('threadId'))
        history_scopes = {(h.get('accountKey', 'default'), h.get('threadId')) for h in a.get('accountHistory', [])}
        for row in rows:
            record = json.loads(row[0])
            # A terminal response ends execution even when its side effects remain
            # unknown. Preserve that outcome and its full response unchanged.
            if (record.get('stage') in {'completed', 'failed'} and record.get('finished') is not None
                    and isinstance(record.get('result'), dict) and type(record['result'].get('success')) is bool):
                continue
            cached = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (record['id'],)).fetchone()
            if cached and type(json.loads(cached[0]).get('success')) is bool:
                continue
            scope = (record.get('accountKey', 'default'), record.get('threadId'))
            if scope != current_scope:
                # Completed transfers/forks preserve these original unknown
                # receipts. Their old execution does not belong to this thread.
                if scope in history_scopes and record.get('stage') in {'completed', 'failed', 'interrupted'}:
                    continue
                raise _waiting('Context repair waits for the original account/thread receipt: ' + record['id'], 'native')
            unresolved.append(record)
        return unresolved


def _native_idle(server, tid, unresolved=(), *, inherited_empty=False):
    native = _native_read(server, 'thread/read', {'threadId': tid, 'includeTurns': False}, timeout=10)['thread']
    status = native.get('status', {}).get('type')
    if native.get('id') != tid:
        raise ValueError('Native thread identity changed before context repair')
    if status not in {'idle', 'notLoaded', 'systemError'}:
        raise _waiting('Context repair requires a confirmed idle native thread; native status: '
                       + json.dumps(native.get('status'), sort_keys=True), 'native')
    # Native terminals belong to a loaded session. Installed Codex rejects this
    # query for notLoaded threads, while their durable queue remains readable.
    methods = ('thread/queue/list',) if status == 'notLoaded' else (
        'thread/backgroundTerminals/list', 'thread/queue/list')
    for method in methods:
        result = _native_read(server, method, {'threadId': tid}, timeout=10)
        if result.get('data') or result.get('nextCursor'):
            raise _waiting('Context repair waits for native commands and queued input', 'native')
    turns = _native_read(server, 'thread/turns/list', {'threadId': tid, 'limit': 1,
                       'sortDirection': 'desc', 'itemsView': 'notLoaded'}, timeout=10)
    pages, deadline = {}, time.monotonic() + 20
    if not turns.get('data') and inherited_empty and not unresolved and status in {'idle', 'notLoaded'}:
        native['repairInheritedEmpty'] = True
    elif not turns.get('data'):
        raise _waiting('Context repair needs a terminal native turn; native status: ' + status, 'native')
    if turns.get('data'):
        turn = turns['data'][0]
        native['repairTerminalTurnId'] = turn['id']
        native['repairTerminalStatus'] = turn.get('status')
        native['repairTerminalStartedAt'] = turn.get('startedAt')
        native['repairTerminalCompletedAt'] = turn.get('completedAt')
        if turn.get('status') not in {'completed', 'failed', 'interrupted'}:
            raise _waiting('Context repair requires a terminal native turn', 'native')
        pages[turn['id']] = _native_items(server, tid, turn['id'], deadline)
        if not all(_terminal_native_item(e['item']) for e in pages[turn['id']]):
            raise _waiting('Context repair waits for complete native tool receipts', 'native')
    for receipt in unresolved:
        turn_id, call_id = receipt.get('turnId'), receipt.get('callId')
        if not turn_id or not call_id:
            raise _waiting('Context repair waits for the exact tool receipt: ' + receipt['id'], 'native')
        if turn_id not in pages:
            pages[turn_id] = _native_items(server, tid, turn_id, deadline)
        matches = [e['item'] for e in pages[turn_id] if e['item'].get('id') == call_id
                   and e['item'].get('type') == 'dynamicToolCall']
        if len(matches) != 1 or not _terminal_native_item(matches[0]):
            raise _waiting('Context repair waits for the exact tool receipt: ' + receipt['id'], 'native')
    if not native.get('path'):
        raise ValueError('The native context has no saved rollout path')
    return native


def _cleanup_source(rt, op, server):
    def record(details):
        with rt.lock, rt.db() as db:
            a = rt.agent(op['agent'], db)
            receipts = [a.get('contextRepair') or {}, *a.get('contextRepairHistory', [])]
            receipt = next((r for r in receipts if r.get('id') == op['id'] and r.get('source') == op['source']), None)
            if receipt:
                receipt.setdefault('sourceCleanup', {}).update(details, updatedAt=time.time())
                rt.put(db, 'agents', a)
    try:
        with rt.lock, rt.db() as db:
            a = rt.agent(op['agent'], db)
            receipt = a.get('contextRepair') or {}
            if (rt.closed or receipt.get('id') != op['id'] or receipt.get('phase') != 'completed'
                    or receipt.get('source') != op['source'] or receipt.get('newThreadId') != op['newThreadId']
                    or receipt.get('sourceCleanup', {}).get('phase') != 'planned'
                    or a.get('threadId') != op['newThreadId']
                    or not rt.connection_current(op['source']['accountKey'], op.get('connectionId'))):
                record({'phase':'skipped', 'error':'The original cleanup connection or operation changed'})
                return
            receipt['sourceCleanup'].update(phase='submitted', submittedAt=time.time())
            rt.put(db, 'agents', a)
        ticket = rt.submit_reserved(server, 'thread/unsubscribe', {'threadId':op['source']['threadId']})
        record({'requestId':ticket[0]})
        def received(future):
            try:
                record({'phase':'completed', 'result':future.result()})
            except Exception as error:
                record({'phase':'unknown', 'error':str(error)})
        server.on_result(ticket, received)
    except Exception as error:
        record({'phase':'unknown', 'error':str(error)})


def _settle(rt, op, future, server=None):
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
            if server is not None:
                stored['sourceCleanup'] = {'phase':'planned', 'threadId':stored['source']['threadId'],
                                          'accountKey':stored['source']['accountKey'], 'connectionId':stored.get('connectionId')}
            _save(rt, db, a, stored)
            rt.loaded.discard(a['id'])
            rt.preparations.pop(a['id'], None)
            rt.changed.set()
        if server is not None:
            rt.pool.submit(_cleanup_source, rt, copy.deepcopy(stored), server)
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
        if op.get('copyCleanup'):
            stored['copyCleanup'] = copy.deepcopy(op['copyCleanup'])
        stored.update(phase='unknown' if unknown else 'failed', error=str(error))
        _save(rt, db, a, stored)


def _unsubmitted(a, attempt_id):
    attempt = a.get('startAttempt') or {}
    return (attempt.get('id') == attempt_id and attempt.get('submitted') is False
            and not attempt.get('turnId') and not attempt.get('observedTurnId')
            and attempt.get('epoch') == a.get('epoch') and a.get('autoWake')
            and attempt.get('accountKey', a.get('accountKey', 'default')) == a.get('accountKey', 'default')
            and not a.get('deletedAt') and not a.get('nativeFailureHold'))


def _defer_context(rt, db, a, error, *, historical=False):
    detail = getattr(error, 'contextRepairWait', None)
    attempt = a.get('startAttempt') or {}
    if (not isinstance(detail, dict) or detail.get('source') != _identity(a)
            or not _unsubmitted(a, attempt.get('id'))
            or (a.get('contextRepair') or {}).get('phase') in ACTIVE):
        return False
    ids = attempt.get('events')
    if not isinstance(ids, list) or len(ids) > 32 or len(ids) != len(set(ids)):
        return False
    action = attempt.get('action')
    if (action and (ids or action not in {'review', 'compact', 'capacity'})) or (not action and not ids):
        return False
    rows = []
    for key in ids:
        row = db.execute('SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=?',
                         (key, a['id'], a['epoch'])).fetchone()
        statuses = {'failed'} if historical else {'pending', 'reserved', 'dispatching'}
        if not row or row['status'] not in statuses or row['turn_id'] or (historical and row['error'] != str(error)):
            return False
        rows.append(row)
    for row in rows:
        db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=?", (row['id'],))
    previous = a.get('contextRepairWait') or a.get('lastContextRepairWait') or {}
    checks = previous.get('checks', 0) + 1 if previous.get('source') == _identity(a) else 1
    delay = min(60, 2 ** min(checks - 1, 6)) if detail.get('scope') == 'native' else 1
    a['contextRepairWait'] = {'source': _identity(a), 'events': list(ids), 'action': action,
        'actionRequestId': attempt.get('actionRequestId'), 'actionIdentity': attempt.get('actionIdentity'),
        'error': str(error), 'scope': detail.get('scope', 'local'), 'at': time.time(),
        'nextCheckAt': time.time() + delay, 'checks': checks, 'historicalFailureRecovered': historical}
    a.update(status='queued', inFlight=False, error=str(error))
    if attempt.get('actionRequestId'):
        db.execute("UPDATE runtime_native_action_receipts SET outcome=? WHERE id=? AND json_extract(receipt,'$.attemptId')=?",
                   (json.dumps({'status':'pending', 'deferred':True, 'error':str(error)}), attempt['actionRequestId'], attempt['id']))
    rt.put(db, 'agents', a)
    return True


def defer_context_start(rt, agent_id, attempt_id, error, *, unknown=False):
    detail = getattr(error, 'contextRepairWait', None)
    if not isinstance(detail, dict) or (unknown and not detail.get('readOnly')):
        return False
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        if not _unsubmitted(a, attempt_id):
            return False
        deferred = _defer_context(rt, db, a, error)
    if deferred:
        rt.changed.set()
    return deferred


def recover_context_failures(rt, db, agents):
    # Repair only the exact pre-submission failures produced by the old helper.
    errors = {'Context repair waits for commands, monitors, and tool receipts',
              'Context repair waits for complete native tool receipts'}
    for a in agents:
        attempt = a.get('startAttempt') or {}
        receipt = a.get('contextRepair') or {}
        # A server can die after the repair record is saved but before a turn
        # reservation exists. That record cannot have submitted a native fork.
        # Retire it after a grace period so it cannot block the normal queue.
        if (receipt.get('phase') == 'preparing' and not a.get('inFlight')
                and not a.get('startAttempt')
                and time.time() - float(receipt.get('updated', receipt.get('created', 0))) >= 60):
            retired = copy.deepcopy(receipt)
            retired.update(
                status='superseded',
                finishedAt=time.time(),
                supersededReason='Stale pre-submission context repair after backend restart; no native fork was submitted.',
            )
            a['lastContextRepairCheck'] = retired
            a.pop('contextRepair', None)
            if a.get('error') == receipt.get('error'):
                a['error'] = None
            rt.put(db, 'agents', a)
            rt.changed.set()
            continue
        preparation_race = (a.get('error') == 'Thread preparation belongs to an earlier agent state'
            and receipt.get('phase') == 'unchanged' and receipt.get('source') == _identity(a)
            and receipt.get('settings') == rt.preparation_settings(a)
            and isinstance(receipt.get('snapshot'), dict))
        exact_error = isinstance(a.get('error'), str) and a['error'] in errors
        if (a.get('status') != 'failed' or (not exact_error and not preparation_race) or a.get('inFlight')
                or a.get('contextRepairWait') or not attempt.get('events')
                or not _unsubmitted(a, attempt.get('id'))):
            continue
        error = _waiting(a['error'])
        error.contextRepairWait['source'] = _identity(a)
        if not _defer_context(rt, db, a, error, historical=True):
            continue
        if a.get('parentId'):
            key = 'child:' + a['id'] + ':start-failed:' + attempt['events'][0]
            row = db.execute("SELECT text FROM runtime_events WHERE id=? AND agent=? AND status='pending'",
                             (key, a['parentId'])).fetchone()
            if row:
                try:
                    body = json.loads(row[0])
                except (ValueError, TypeError):
                    continue
                if body.get('agent_id') == a['id'] and body.get('result') == str(error) and body.get('status') == 'failed':
                    db.execute("UPDATE runtime_events SET status='stored_only',error=? WHERE id=? AND status='pending'",
                               ('The original unsubmitted context wait was restored', key))


def _optional_monitor_repair(rt, db, agent):
    """Keep normal input on its current thread while background work runs.

    Event shortening can wait. Submitted repairs, native actions, and input
    uncertainty cannot use this path. No history or command receipt changes.
    """
    attempt = agent.get('startAttempt') or {}
    if (rt.closed or not _unsubmitted(agent, attempt.get('id'))
            or not attempt.get('events') or attempt.get('action')
            or (agent.get('contextRepair') or {}).get('phase') in ACTIVE):
        return False
    monitor = db.execute("SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=? "
                         "AND json_extract(record,'$.status') IN ('starting','running','pending','stopping') LIMIT 1",
                         (agent['id'],)).fetchone()
    command = db.execute("SELECT 1 FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
                         "AND json_extract(record,'$.kind')='command' AND json_extract(record,'$.status')='running' "
                         "AND coalesce(cast(json_extract(record,'$.processId') AS TEXT),'')!='' LIMIT 1",
                         (agent['id'],)).fetchone()
    if not monitor and not command:
        return False
    try:
        return not _local_idle(rt, db, agent, attempt['id'], allow_background_work=True)
    except ValueError:
        return False


def claim_context_wait(rt, db, agent):
    wait = agent.get('contextRepairWait')
    if not isinstance(wait, dict):
        return None
    attempt = agent.get('startAttempt') or {}
    valid = (not rt.closed and _unsubmitted(agent, wait.get('source', {}).get('attemptId'))
             and wait.get('source') == _identity(agent) and wait.get('events') == attempt.get('events')
             and wait.get('action') == attempt.get('action')
             and wait.get('actionIdentity') == attempt.get('actionIdentity')
             and wait.get('actionRequestId') == attempt.get('actionRequestId')
             and not agent.get('inFlight'))
    rows = []
    if valid:
        for key in wait['events']:
            row = db.execute("SELECT * FROM runtime_events WHERE id=? AND agent=? AND epoch=? AND status='pending' AND turn_id IS NULL",
                             (key, agent['id'], agent['epoch'])).fetchone()
            if not row:
                valid = False
                break
            rows.append(dict(row))
    if not valid:
        agent.pop('contextRepairWait', None)
        agent['lastContextRepairWait'] = {**wait, 'status':'superseded', 'finishedAt':time.time()}
        if agent.get('error') == wait.get('error'):
            agent['error'] = None
        if (wait.get('actionRequestId') and attempt.get('id') == wait.get('source', {}).get('attemptId')
                and attempt.get('submitted') is False and not attempt.get('turnId') and not attempt.get('observedTurnId')):
            db.execute("UPDATE runtime_native_action_receipts SET outcome=? WHERE id=? AND json_extract(receipt,'$.attemptId')=? AND json_extract(outcome,'$.status')='pending'",
                       (json.dumps({'status':'failed','notSubmitted':True,'error':'The context wait belongs to an earlier agent state'}),
                        wait['actionRequestId'], attempt['id']))
        rt.put(db, 'agents', agent)
        return {'waiting':True}
    if time.time() < wait.get('nextCheckAt', 0):
        return {'waiting':True}
    try:
        optional_monitor = (wait.get('scope') == 'local'
                            and str(wait.get('error', '')).startswith(('Context repair waits for monitors: ',
                                                                          'Context repair waits for tasks: '))
                            and _optional_monitor_repair(rt, db, agent))
        if not optional_monitor:
            _local_idle(rt, db, agent, attempt['id'])
    except ValueError as error:
        wait.update(error=str(error), nextCheckAt=time.time() + 2)
        agent['error'] = str(error)
        rt.put(db, 'agents', agent)
        return {'waiting':True}
    for row in rows:
        db.execute("UPDATE runtime_events SET status='reserved' WHERE id=? AND status='pending'", (row['id'],))
    agent.pop('contextRepairWait', None)
    agent['lastContextRepairWait'] = {**wait, 'status':'resumed', 'finishedAt':time.time()}
    agent.update(status='starting', inFlight=True, error=None, turnEpoch=agent['epoch'])
    rt.put(db, 'agents', agent)
    return {'kind':'action' if wait.get('action') else 'turn', 'agent':agent,
            'attempt':dict(attempt), 'rows':rows}


def repair_before_start(rt, agent):
    try:
        with rt.lock, rt.db() as db:
            current = rt.agent(agent['id'], db)
            if _identity(current) == _identity(agent) and not current.get('contextRepairWait'):
                if _optional_monitor_repair(rt, db, current):
                    return current
        if (agent.get('contextRepair') or {}).get('phase') in {'unchanged', 'completed'}:
            _callback_barrier(rt.connect(agent.get('accountKey', 'default')))
        return _repair(rt, agent['id'], _attempt(agent))
    except ValueError as error:
        if isinstance(getattr(error, 'contextRepairWait', None), dict):
            error.contextRepairWait['source'] = _identity(agent)
        raise


def repair_idle(rt, key):
    return _repair(rt, key, None)


def _repair(rt, key, attempt_id):
    with rt.lock, rt.db() as db:
        a = rt.agent(key, db)
        old = a.get('contextRepair') or {}
        assert_context_available(a)
        if not a.get('threadId'):
            return a
        events = verified_events(db, a)
        has_uncertain = db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? AND status='uncertain' LIMIT 1",
                                   (a['id'], a['epoch'])).fetchone()
        if not events and not has_uncertain:
            return a
        historical = _local_idle(rt, db, a, attempt_id)
        checked_thread = old.get('newThreadId') if old.get('phase') == 'completed' else (old.get('source') or {}).get('threadId')
        previous_proof = old.get('historicalInputProof') or {}
        if (not events and old.get('phase') in {'completed', 'unchanged'} and checked_thread == a.get('threadId')
                and old.get('agent') == a['id'] and (old.get('source') or {}).get('accountKey') == a.get('accountKey')
                and old['source'].get('id') == a['id'] and old['source'].get('epoch') == a['epoch']
                and previous_proof.get('events') == historical and previous_proof.get('terminalTurnId')):
            return a
        checked = sorted(e['id'] for e in events)
        checked_thread = old.get('newThreadId') if old.get('phase') == 'completed' else (old.get('source') or {}).get('threadId')
        if checked_thread == a.get('threadId') and old.get('compactions') == a.get('compactions', 0):
            checked = sorted(set(checked) | set(old.get('checkedEventIds', [])))
        op = {'id': str(uuid.uuid4()), 'agent': key, 'source': _identity(a),
              'settings': rt.preparation_settings(a), 'phase': 'preparing', 'created': time.time(),
              'historicalInputs': historical,
              'checkedEventIds': checked, 'compactions': a.get('compactions', 0),
              'previousRepairedEventIds': old.get('repairedEventIds', []) if old.get('newThreadId') == a.get('threadId') else []}
        if old and (old.get('phase') == 'completed' or old.get('rpcMethod')):
            a.setdefault('contextRepairHistory', []).append(old)
        elif old:
            a['lastContextRepairCheck'] = old
        _save(rt, db, a, op)
    submitted, report = False, None
    try:
        server = rt.connect(a.get('accountKey', 'default'))
        op['connectionId'] = rt.connection_ids.get(a.get('accountKey', 'default'))
        _callback_barrier(server)
        native = _native_idle(server, a['threadId'], _unresolved_tool_receipts(rt, a), inherited_empty=True)
        # Native regular items flush before the terminal marker. Require that
        # exact saved marker below; unloading here could close a later resume.
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
        segments = _rollout_segments(home, source, a['threadId'])
        # Native fork can make its own history copy. Keep capacity for both copies.
        if shutil.disk_usage(home).free < sum(s['endByteOffset'] for s in segments) * 3 + 64 * 1024 * 1024:
            raise ValueError('Context repair needs free space for three copies of this rollout')
        report = sanitized_rollout(source, destination, a['threadId'], events, native.get('repairTerminalTurnId'),
                                   segments=segments, inherited_empty=native.get('repairInheritedEmpty', False),
                                   terminal_status=native.get('repairTerminalStatus'),
                                   terminal_completed_at=native.get('repairTerminalCompletedAt'))
        if any(_prefix_hash(s['path'], s['endByteOffset']) != s['sha256'] for s in segments):
            raise _waiting('Context repair waits for stable saved ancestry', 'native')
        source_hash = _hash_file(source)
        report['sourceFileIdentity'] = _file_identity(source)
        with rt.lock, rt.db() as db:
            current = _current(rt, db, op)
            _local_idle(rt, db, current, attempt_id)
            op['snapshot'] = report
            if historical:
                completed = report.get('terminalCompletedAt')
                if (not isinstance(completed, (int, float))
                        and native.get('repairTerminalStatus') == 'interrupted'
                        and isinstance(native.get('repairTerminalStartedAt'), (int, float))
                        and native['repairTerminalStartedAt'] > max(r['created'] for r in historical)):
                    # Codex may omit completedAt for a server-interrupted turn.
                    # Its startedAt still proves that old uncertain inputs came first.
                    completed = time.time()
                    report['terminalCompletedAt'] = completed
                if (report.get('terminalTurnId') != native.get('repairTerminalTurnId')
                        or not isinstance(completed, (int, float))
                        or completed <= max(r['created'] for r in historical)
                        or not db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                          (key + ':' + report['terminalTurnId'],)).fetchone()):
                    raise _waiting('Context repair waits for a later confirmed native terminal turn', 'native')
                op['historicalInputProof'] = {'events':historical, 'terminalTurnId':report['terminalTurnId'],
                    'terminalCompletedAt':completed, 'source':copy.deepcopy(op['source']),
                    'sourceSha256':report['sourceSha256'], 'deliveryOutcome':'unknown'}
            if source_hash != report['sourceSha256']:
                raise _waiting('Context repair waits for a stable saved context', 'native')
            if not report['eventIds']:
                op['phase'] = 'unchanged'
                _save(rt, db, current, op)
                return current
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
            _settle(rt, op, future, server)
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
        from codex_runtime import PreparationPending
        if not submitted and report and report.get('copyPath'):
            try:
                Path(report['copyPath']).unlink(missing_ok=True)
            except OSError as cleanup_error:
                op['copyCleanup'] = {'path':report['copyPath'], 'outcome':'failed', 'error':str(cleanup_error)}
        if not isinstance(error, PreparationPending):
            _fail(rt, op, error, unknown=submitted)
        raise
