"""Client-side error events for the Codex 0.153.4 app-server boundary.

Native Codex owns retry, authentication, compaction, and tool execution.
These handlers record its decisions; they never resubmit a model or tool call.
"""
import hashlib
import json
import time


class NativeRpcError(RuntimeError):
    """An explicit JSON-RPC rejection, distinct from a lost acknowledgement."""
    def __init__(self, error):
        self.error = error
        self.code = error.get('code') if isinstance(error, dict) else None
        self.data = error.get('data') if isinstance(error, dict) else None
        super().__init__(json.dumps(error, ensure_ascii=False))


SUPPORTED_REQUESTS = frozenset({
    'item/commandExecution/requestApproval', 'item/fileChange/requestApproval',
    'item/tool/requestUserInput', 'item/permissions/requestApproval',
    'mcpServer/elicitation/request',
})


def error_kind(error):
    if not isinstance(error, dict):
        return ''
    info = error.get('codexErrorInfo')
    return info if isinstance(info, str) else next(iter(info), '') if isinstance(info, dict) else ''



def error_message(error):
    if isinstance(error, dict):
        return str(error.get('message') or 'Codex reported an error.')
    return str(error or 'Codex reported an error.')


THREAD_BLOCK_MESSAGE = 'This chat is stopped as a precaution. Start or resume another chat.'


def preserve_thread_block(agent, error):
    """Keep the native precaution separate from temporary transport errors."""
    if (agent.get('threadId') and isinstance(error, dict)
            and error.get('codexErrorInfo') == 'misalignmentPolicyViolation'):
        agent['nativeThreadBlock'] = {'threadId': agent['threadId'], 'error': error}


def native_thread_block(agent):
    block = agent.get('nativeThreadBlock') or {}
    if block.get('threadId') and block['threadId'] == agent.get('threadId'):
        return block
    # Existing records can predate the separate persistent precaution field.
    error = agent.get('error')
    if (agent.get('threadId') and isinstance(error, dict)
            and error.get('codexErrorInfo') == 'misalignmentPolicyViolation'):
        return {'threadId': agent['threadId'], 'error': error}
    previous = agent.get('nativeTurnError') or {}
    if agent.get('threadId') and previous.get('turnId') and previous['turnId'] == agent.get('turnId'):
        error = previous.get('error')
        if isinstance(error, dict) and error.get('codexErrorInfo') == 'misalignmentPolicyViolation':
            return {'threadId': agent.get('threadId'), 'error': error}
    return None


def assert_native_thread_open(agent):
    if native_thread_block(agent):
        raise ValueError(THREAD_BLOCK_MESSAGE)


def refresh_native_limits(runtime, db, agent, error, turn_id, account_key, connection_id):
    """Read native limits once after this account's exact failed turn."""
    if (not isinstance(error, dict) or error.get('codexErrorInfo') not in
            ('usageLimitExceeded', 'rateLimitExceeded') or not turn_id or not agent.get('threadId')
            or runtime.closed):
        return
    connection_id = connection_id or runtime.connection_ids.get(account_key)
    if not connection_id or not runtime.connection_current(account_key, connection_id):
        return
    identity = [account_key, agent['threadId'], turn_id]
    key = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_limit_refreshes '
               '(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    inserted = db.execute('INSERT OR IGNORE INTO runtime_native_limit_refreshes VALUES (?,?)',
                         (key, json.dumps({'accountKey': account_key, 'threadId': agent['threadId'],
                                           'turnId': turn_id, 'connectionId': connection_id})))
    if not inserted.rowcount:
        return
    agent['nativeLimitErrorAt'] = time.time()
    def read_limits():
        try:
            runtime.limits(account_key, force=True, connection_id=connection_id)
        except Exception:
            # A failed read does not change the turn or authorize another request.
            return
    runtime.recovery_pool.submit(read_limits)
