"""Restore unanswered Claude questions on the same retained native child."""
import json
import math
import sqlite3
from typing import TYPE_CHECKING, Any

from codex_connection_recovery import supervisor_identity
from codex_context_repair import blocked
from codex_tool_response_recovery import _native_proof, _rpc_id

if TYPE_CHECKING:
    from codex_runtime import Runtime

OWNER_FIELDS = ('id', 'accountKey', 'epoch', 'threadId', 'turnId', 'turnEpoch',
                'autoWake', 'inFlight', 'activeTools', 'isLead', 'provider', 'deletedAt',
                'restartRecovery', 'disconnectRecovery', 'supervisorRestore',
                'nativeFailureHold', 'nativeThreadBlock', 'accountTransferId',
                'workspaceOperation', 'contextRepair', 'contextRepairWait')


def _object(value: object) -> dict[str, Any] | None:
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except (ValueError, TypeError):
            return None
    return value if isinstance(value, dict) else None


def _time(value: object) -> float | None:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return None
    try:
        return float(value) if math.isfinite(value) else None
    except OverflowError:
        return None


def _question_items(owner: dict[str, Any]) -> set[str]:
    tools = owner.get('activeTools')
    if not isinstance(tools, list):
        return set()
    return {tool['id'] for tool in tools if isinstance(tool, dict)
            and isinstance(tool.get('id'), str) and tool['id']
            and tool.get('name') == 'AskUserQuestion' and tool.get('type') == 'mcpToolCall'}


def _question_requests(db: sqlite3.Connection, owner_id: str, item_id: str) -> list[tuple[str, str]]:
    return [(row[0], row[1]) for row in db.execute(
        "SELECT id,record FROM runtime_requests WHERE json_valid(record) "
        "AND json_extract(record,'$.method')='item/tool/requestUserInput' "
        "AND json_extract(record,'$.agent')=? "
        "AND json_extract(record,'$.params.itemId')=? LIMIT 2", (owner_id, item_id))]


def _scope(db: "sqlite3.Connection", request: dict[str, Any], owner: dict[str, Any]) -> bool:
    params = request.get('params')
    if not isinstance(params, dict) or not isinstance(params.get('questions'), list) or not params['questions']:
        return False
    account = request.get('accountKey', 'default')
    created = _time(request.get('createdAt'))
    item_id = params.get('itemId')
    if (request.get('method') != 'item/tool/requestUserInput'
            or request.get('status') != 'expired' or request.get('answerSignature')
            or request.get('agent') != owner.get('id')
            or not _rpc_id(request.get('rpcId'))
            or created is None
            or owner.get('provider') != 'claude' or not owner.get('isLead')
            or owner.get('deletedAt') or not owner.get('autoWake') or not owner.get('inFlight')
            or owner.get('nativeFailureHold') or owner.get('nativeThreadBlock')
            or owner.get('accountTransferId') or owner.get('workspaceOperation')
            or (owner.get('contextRepair') is not None and not isinstance(owner.get('contextRepair'), dict))
            or blocked(owner)
            or owner.get('status') not in {'running', 'approval'}
            or owner.get('accountKey', 'default') != account
            or not params.get('turnId') or params.get('turnId') != owner.get('turnId')
            or not params.get('threadId') or params.get('threadId') != owner.get('threadId')
            or not isinstance(item_id, str) or not item_id
            or request.get('epoch', owner.get('epoch')) != owner.get('epoch')
            or owner.get('turnEpoch', owner.get('epoch')) != owner.get('epoch')
            or item_id not in _question_items(owner)):
        return False
    # A manual stop changes the epoch. Legacy requests need the original run,
    # rather than assigning the current epoch to an old question.
    run = db.execute('SELECT 1 FROM runtime_execution_runs WHERE agent=? AND account=? '
                     'AND thread=? AND turn=? AND epoch=? AND created<=? LIMIT 1',
                     (owner['id'], account, params['threadId'], params['turnId'],
                      owner['epoch'], created)).fetchone()
    if not run:
        return False
    restored = _object(owner.get('supervisorRestore')) or {}
    if (restored.get('status') != 'restored' or restored.get('reason') != 'live_handle_resumed'
            or restored.get('accountKey') != account
            or any(restored.get(field) != owner.get(field) for field in ('epoch', 'threadId', 'turnId'))):
        return False
    marker = _object(owner.get('restartRecovery')) or {}
    if marker.get('stage') != 'reattached':
        marker = _object(owner.get('disconnectRecovery')) or {}
    when = _time(marker.get('at'))
    if (not marker.get('autoWake') or when is None or when < created
            or marker.get('accountKey', 'default') != account
            or any(marker.get(field) != owner.get(field) for field in ('epoch', 'threadId', 'turnId'))):
        return False
    item = db.execute('SELECT record,agent FROM runtime_items WHERE id=?',
                      (owner['id'] + ':' + item_id,)).fetchone()
    if not item:
        return False
    saved = _object(item[0])
    native = _object(saved.get('text')) if saved is not None else None
    if saved is None or native is None:
        return False
    return bool(item[1] == owner['id'] and saved.get('turnId') == params['turnId']
                and native.get('id') == item_id and native.get('type') == 'mcpToolCall'
                and native.get('tool') == 'AskUserQuestion' and native.get('status') == 'inProgress')


def recover_native_questions(runtime: "Runtime", account: str, connection: str,
                             *, agent_id: str | None = None) -> int:
    """Rebind a saved question, without sending an answer or model input."""
    candidates: list[tuple[str, str, str, dict[str, Any]]] = []
    with runtime.lock, runtime.read_db() as db:
        server = runtime.server_for(account, connection)
        identity = supervisor_identity(server)
        if (runtime.closed or server is None or identity is None
                or not runtime.connection_current(account, connection)):
            return 0
        owners = db.execute("SELECT id,record FROM runtime_agents WHERE "
                            "CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
                            "ELSE json_extract(record,'$.accountKey') END=? "
                            "AND json_extract(record,'$.inFlight')=1 "
                            "AND json_extract(record,'$.status') IN ('running','approval')", (account,))
        for owner_id, owner_raw in owners:
            if agent_id is not None and owner_id != agent_id:
                continue
            owner = _object(owner_raw)
            if owner is None or runtime.agent_connection(owner) != connection:
                continue
            for item_id in _question_items(owner):
                rows = _question_requests(db, owner_id, item_id)
                if len(rows) != 1:
                    continue
                key, raw = rows[0]
                request = _object(raw)
                if request is not None and request.get('id') == key and _scope(db, request, owner):
                    candidates.append((key, raw, owner_raw, request))
    restored = 0
    for key, raw, owner_raw, request in candidates:
        # The supervisor database and process identity reads happen outside the
        # runtime writer. An accepted answer remains unknown, never retryable.
        proof_record = {**request, 'created': request['createdAt']}
        try:
            proof = _native_proof(runtime, server, proof_record)
        except (OSError, ValueError, TypeError, sqlite3.Error):
            continue
        if proof != identity:
            continue
        with runtime.lock, runtime.notification_db() as db:
            if (runtime.closed or not runtime.connection_current(account, connection)
                    or runtime.server_for(account, connection) is not server
                    or supervisor_identity(server) != identity):
                break
            current = db.execute('SELECT record FROM runtime_requests WHERE id=?', (key,)).fetchone()
            owner_id = request['agent']
            owner_row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (owner_id,)).fetchone()
            if not current or current[0] != raw or not owner_row:
                continue
            if _question_requests(db, owner_id, request['params']['itemId']) != [(key, raw)]:
                continue
            owner = json.loads(owner_row[0])
            original_owner = json.loads(owner_raw)
            if any(owner.get(field) != original_owner.get(field) for field in OWNER_FIELDS):
                continue
            if runtime.agent_connection(owner) != connection or not _scope(db, request, owner):
                continue
            request.update(status='pending', connectionId=connection, epoch=owner['epoch'], supervisor=identity)
            runtime.put(db, 'requests', request)
            owner['status'] = 'approval'
            runtime.put(db, 'agents', owner)
            restored += 1
    return restored
