"""Restore one reviewed terminal tool receipt without repeating its operation."""
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
from types import MethodType

from codex_active_task_update import signature
from codex_tool_requests import RequestMixin

PLAN = {
    'id': 'route-terminal-receipt-20260914-v1',
    'evidence': 'evidence/limit-fixes-20260914/route-native-terminal-receipt.json',
    'sha256': '9d65ff9a9b260c06f194cbd5b1ebdc91e83c7fa7caf043670826d88a6f49a351',
    'agent': 'de69e6c8-8f00-4dca-be3a-7dda809a0ab4',
    'accountKey': 'login-33ec7874-e215-4955-adbd-a340fc4243e6',
    'threadId': '01a087b9-3381-7a81-8b49-92b344bd3f0b',
    'turnId': '01a096a2-b730-7f81-9f0b-3e1794d07bb0',
    'callId': 'exec-0d471cf3-4119-463a-a96e-cfbaff5832ba',
    'tool': 'orchestration_message',
    'epoch': 0,
}
FINISH_SIGNATURE = '55c39ff1b3d5da9c5a38316528632efeb8f6ebebce6e2a6a5ebd7bd0dd855a26'
JOURNAL_SQL = 'CREATE TABLE runtime_receipt_recoveries (id TEXT PRIMARY KEY, record TEXT NOT NULL)'
MAX_BYTES = 65536


def _require(condition, message):
    if not condition:
        raise RuntimeError('Terminal receipt recovery refused: ' + message)


def _encoded(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()


def _record(source, proof):
    offset = proof['byteOffset']
    _require(type(offset) is int and offset >= 0, 'invalid native offset')
    if offset:
        source.seek(offset - 1)
        _require(source.read(1) == b'\n', 'native offset is not a record boundary')
    source.seek(offset)
    raw = source.readline(MAX_BYTES + 1)
    _require(len(raw) <= MAX_BYTES and raw.endswith(b'\n'), 'native record exceeds its bound')
    _require(raw == proof['rawRecord'].encode() and hashlib.sha256(raw).hexdigest() == proof['recordSha256'],
             'native record bytes changed')
    record = json.loads(raw)
    _require(record == proof['record'] and type(record.get('ordinal')) is int and record['ordinal'] >= 0,
             'native record identity changed')
    return record


def _proof(root, plan=None, account_home=None):
    plan = PLAN if plan is None else plan
    evidence_path = (root / plan['evidence']).resolve(strict=True)
    _require(evidence_path.is_relative_to(root / 'evidence'), 'evidence escapes the runtime directory')
    with evidence_path.open('rb') as source:
        raw = source.read(MAX_BYTES + 1)
    _require(len(raw) <= MAX_BYTES and hashlib.sha256(raw).hexdigest() == plan['sha256'],
             'unreviewed evidence')
    evidence = json.loads(raw)
    saved = evidence['savedRequest']
    _require(all(saved.get(key) == plan[key] for key in
                 ('agent', 'accountKey', 'threadId', 'turnId', 'callId', 'tool', 'epoch')),
             'request differs from the explicit plan')
    prefix = '' if plan['accountKey'] == 'default' else plan['accountKey'] + ':'
    key = prefix + plan['threadId'] + ':' + plan['callId']
    _require(saved.get('id') == key and saved.get('stage') == 'queued' and saved.get('outcome') == 'pending'
             and saved.get('cancelRequested') is False and 'result' not in saved and 'finished' not in saved,
             'reviewed receipt is not an untouched queued request')
    account_home = root / 'accounts' / plan['accountKey'] if account_home is None else Path(account_home)
    path = Path(evidence['sourcePath']).resolve(strict=True)
    _require(path.is_relative_to(account_home / 'sessions')
             and path.name.endswith('-' + plan['threadId'] + '.jsonl'), 'native source escapes its account session')
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as source:
        before = os.fstat(source.fileno())
        observation = evidence['sourceObservation']
        _require(stat.S_ISREG(before.st_mode) and before.st_dev == observation['device']
                 and before.st_ino == observation['inode'] and before.st_size >= observation['size'],
                 'native source identity changed')
        header = source.readline(MAX_BYTES + 1)
        _require(len(header) <= MAX_BYTES, 'native header exceeds its bound')
        header = json.loads(header)
        _require(header.get('type') == 'session_meta' and header.get('payload', {}).get('id') == plan['threadId'],
                 'native session identity changed')
        receipt = _record(source, evidence['receipt'])
        terminal = _record(source, evidence['terminalTurn'])
        after = os.fstat(source.fileno())
        _require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                 (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), 'native source changed during proof read')
    payload, turn = receipt.get('payload', {}), terminal.get('payload', {})
    item = payload.get('item', {})
    _require(receipt.get('type') == 'event_msg' and payload.get('type') == 'item_completed'
             and payload.get('thread_id') == plan['threadId'] and payload.get('turn_id') == plan['turnId']
             and item.get('type') == 'DynamicToolCall' and item.get('id') == plan['callId']
             and item.get('tool') == plan['tool'] and item.get('status') == 'failed' and item.get('success') is False,
             'native tool receipt is not the exact terminal failure')
    _require(terminal.get('type') == 'event_msg' and turn.get('type') == 'task_complete'
             and turn.get('turn_id') == plan['turnId'] and terminal['ordinal'] > receipt['ordinal']
             and evidence['terminalTurn']['byteOffset'] > evidence['receipt']['byteOffset'],
             'native turn has no matching later completion')
    request_signature = hashlib.sha256(_encoded({'tool': item['tool'], 'arguments': item['arguments']})).hexdigest()
    _require(request_signature == saved.get('signature'), 'native arguments differ from the request')
    result = {'success': False, 'contentItems': item['content_items']}
    _require(result == evidence['nativeResult'], 'native response differs from the reviewed response')
    return evidence, saved, result


def _journal_exists(db):
    row = db.execute("SELECT type,sql FROM sqlite_master WHERE name='runtime_receipt_recoveries'").fetchone()
    if row is not None:
        _require(row[0] == 'table' and ' '.join(row[1].split()) == JOURNAL_SQL, 'unknown recovery journal schema')
        _require(not db.execute("SELECT 1 FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_receipt_recoveries'").fetchone(),
                 'unexpected recovery journal trigger')
    return row is not None


def apply(runtime):
    return apply_plan(runtime, PLAN)


def apply_plan(runtime, plan):
    import codex_runtime
    _require(sys.version_info[:2] == (3, 14) and type(runtime) is codex_runtime.Runtime, 'unknown runtime')
    method = runtime.finish_tool_request
    _require(isinstance(method, MethodType) and method.__self__ is runtime
             and method.__func__ is RequestMixin.finish_tool_request
             and signature(method.__func__) == FINISH_SIGNATURE, 'unknown receipt writer')
    root = Path(runtime.root).resolve(strict=True)
    policy = plan.get('accountHomePolicy', 'accountDirectory')
    _require(policy in {'accountDirectory', 'managed'}, 'unknown account home policy')
    account_home = Path(runtime.accounts.home(plan['accountKey'])).resolve(strict=True) if policy == 'managed' else None
    evidence, saved, result = _proof(root, plan, account_home)
    _require(runtime.lock.acquire(timeout=10), 'runtime remains busy')
    try:
        _require(not runtime.closed, 'runtime is closed')
        with runtime.db() as db:
            db.execute('BEGIN IMMEDIATE')
            exists = _journal_exists(db)
            journal = db.execute('SELECT record FROM runtime_receipt_recoveries WHERE id=?', (plan['id'],)).fetchone() if exists else None
            current = runtime.tool_request(saved['id'], db)
            if journal:
                journal = json.loads(journal[0])
                _require(journal.get('evidenceSha256') == plan['sha256'] and journal.get('proof') == evidence
                         and journal.get('before') == saved and journal.get('after') == current,
                         'receipt changed after its recorded recovery')
                return {'status': 'already_applied', 'receipt': saved['id'], 'journal': plan['id']}
            _require(current == saved, 'local receipt changed since review')
            actor_row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (saved['agent'],)).fetchone()
            actor = json.loads(actor_row[0]) if actor_row else {}
            _require(actor.get('id') == saved['agent'] and not actor.get('deletedAt')
                     and all(actor.get(key) == saved[key] for key in ('accountKey', 'threadId'))
                     and actor.get('epoch') == plan.get('currentEpoch', saved['epoch']),
                     'agent account, thread or epoch changed')
            if plan.get('requirePaused'):
                _require(actor.get('status') == 'paused' and actor.get('autoWake') is False
                         and actor.get('inFlight') is False, 'worker is no longer paused')
            _require(not db.execute('SELECT 1 FROM runtime_tool_results WHERE id=?', (saved['id'],)).fetchone(),
                     'another tool result is already saved')
            if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_operation_receipts'").fetchone():
                _require(not db.execute('SELECT 1 FROM runtime_operation_receipts WHERE id=?', (saved['id'],)).fetchone(),
                         'an operation receipt is already saved')
            if not exists:
                db.execute(JOURNAL_SQL)
            after = runtime.finish_tool_request(saved['id'], result, outcome='unknown', db=db)
            expected = {**saved, 'stage': 'failed', 'outcome': 'unknown', 'result': result,
                        'updated': after.get('updated'), 'finished': after.get('finished')}
            _require(after == expected, 'receipt writer produced an unexpected record')
            journal = {'id': plan['id'], 'evidenceSha256': plan['sha256'], 'proof': evidence,
                       'before': saved, 'after': after}
            db.execute('INSERT INTO runtime_receipt_recoveries(id,record) VALUES (?,?)', (plan['id'], _encoded(journal).decode()))
        return {'status': 'applied', 'receipt': saved['id'], 'journal': plan['id'], 'outcome': 'unknown'}
    finally:
        runtime.lock.release()
