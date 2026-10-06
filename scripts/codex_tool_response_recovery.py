"""Deliver saved Claude tool results to the original surviving native child."""
from contextlib import closing
import hashlib
import json
import math
import sqlite3
import threading
import time
import uuid
from typing import TYPE_CHECKING, Any

from codex_connection_recovery import supervisor_identity

if TYPE_CHECKING:
    from collections.abc import Iterable
    from codex_records import AgentRecord
    from codex_runtime import Runtime

FIELDS = ('id', 'epoch', 'accountKey', 'threadId', 'turnId', 'autoWake',
          'status', 'inFlight', 'deletedAt', 'activeTools')
MAX_JOBS = 8
MAX_RECEIPTS = 32


def eligible(agent: "AgentRecord") -> bool:
    return bool(agent.get('provider') == 'claude' and not agent.get('deletedAt')
                and agent.get('status') == 'running' and agent.get('autoWake')
                and agent.get('inFlight') and agent.get('threadId') and agent.get('turnId')
                and any(tool.get('type') == 'mcpToolCall'
                        and str(tool.get('name', '')).startswith('mcp__studio__')
                        for tool in agent.get('activeTools', [])))


def response_operation_id(runtime: "Runtime", account: str, rpc_id: Any) -> str | None:
    identity = supervisor_identity(runtime.servers.get(account))
    if identity is None:
        return None
    body = json.dumps([identity, rpc_id], sort_keys=True, separators=(',', ':'))
    return 'tool-response:' + hashlib.sha256(body.encode()).hexdigest()


def _rpc_id(value: object) -> bool:
    if not isinstance(value, str) or not value.startswith('claude:'):
        return False
    try:
        return str(uuid.UUID(value[7:])) == value[7:]
    except ValueError:
        return False


def _native_proof(runtime: "Runtime", server: Any, record: Any) -> Any:
    """An absent acceptance receipt permits a response, never operation replay."""
    identity = supervisor_identity(server)
    if (identity is None or identity['stateDir'] != str(runtime.root.resolve())
            or identity['handle'] != 'account:' + record.get('accountKey', 'default')):
        return None
    saved = record.get('supervisor')
    if saved is not None and saved != identity:
        return None
    uri = (runtime.root / 'supervisor.sqlite3').as_uri() + '?mode=ro'
    with closing(sqlite3.connect(uri, uri=True, timeout=.05)) as db:
        row = db.execute('SELECT h.pid,h.generation,h.closed_at,c.start_time '
                         'FROM handles h JOIN child_identities c ON c.handle=h.id AND c.pid=h.pid '
                         'WHERE h.id=?', (identity['handle'],)).fetchone()
        if not row or row[1] != identity['generation'] or row[2] is not None:
            return None
        # Older receipts have no generation. A child born before admission and
        # still alive cannot have been replaced between admission and recovery.
        if saved is None:
            try:
                started, created = float(row[3]), float(record['created'])
            except (TypeError, ValueError, KeyError):
                return None
            if not math.isfinite(started) or not math.isfinite(created) or started >= created:
                return None
        from codex_process_supervisor import process_start_matches
        if not process_start_matches(row[0], row[3]):
            return None
        accepted = db.execute('SELECT 1 FROM operations WHERE handle=? AND native_id=? LIMIT 1',
                              (identity['handle'], record['rpcId'])).fetchone()
        if accepted:
            # Acceptance precedes stdin write. A lost write outcome is not
            # evidence that delivery failed, so do not send another response.
            return None
    return identity


def recover(runtime: "Runtime", key: str) -> dict[str, Any]:
    with runtime.lock, runtime.read_db() as db:
        agent = runtime.agent(key, db)
        if runtime.closed or not eligible(agent):
            return {'status': 'superseded'}
        expected = {field: agent.get(field) for field in FIELDS}
        account = agent.get('accountKey', 'default')
        server = runtime.servers.get(account)
        connection = runtime.connection_ids.get(account)
        if (server is None or not runtime.connection_current(account, connection)
                or supervisor_identity(server) is None):
            return {'status': 'superseded'}
        rows = db.execute("SELECT record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                          "ORDER BY json_extract(record,'$.updated') DESC LIMIT ?",
                          (key, MAX_RECEIPTS)).fetchall()
    delivered = []
    for (raw,) in rows:
        record = json.loads(raw)
        if (record.get('stage') not in {'completed', 'failed', 'interrupted', 'cancelled'}
                or record.get('nativeDelivery') or not _rpc_id(record.get('rpcId'))
                or any(record.get(field) != agent.get(field)
                       for field in ('epoch', 'threadId', 'turnId'))
                or record.get('accountKey', 'default') != account
                or time.time() - record.get('updated', time.time()) < 15):
            continue
        names = {tool.get('name') for tool in agent.get('activeTools', [])}
        if 'mcp__studio__' + str(record.get('tool')) not in names:
            continue
        proof = _native_proof(runtime, server, record)
        if proof is None:
            continue
        from codex_payloads import resolve_record
        resolved = resolve_record(runtime.root, record=record)
        result = resolved.get('result')
        if not isinstance(result, dict):
            continue
        # The startup gate prevents replacement of this account handle. The
        # exact actor snapshot authorizes this response before native I/O.
        # A later Stop still revokes new operations and interrupts the turn.
        with runtime.start_lock:
            if not server.write_lock.acquire(blocking=False):  # type: ignore[union-attr]  # Server was checked for None before native I/O.
                continue
            try:
                # A legacy reply can use a random write identity. Recheck its
                # native RPC acceptance after all preceding writes finish.
                if _native_proof(runtime, server, record) != proof:
                    continue
                with runtime.lock:
                    with runtime.read_db() as db:
                        current = runtime.agent(key, db)
                        receipt = db.execute('SELECT record FROM runtime_tool_requests WHERE id=?',
                                             (record['id'],)).fetchone()
                    if (runtime.closed or any(current.get(field) != expected[field] for field in FIELDS)
                            or runtime.servers.get(account) is not server
                            or not runtime.connection_current(account, connection)
                            or supervisor_identity(server) != proof or not receipt or receipt[0] != raw):
                        continue
                    operation_id = response_operation_id(runtime, account, record['rpcId'])
                # Do not retain Runtime.lock across a pipe or supervisor socket wait.
                written = runtime.reply({'id': record['rpcId'], 'result': result}, account, connection,
                                        operation_id=operation_id)
            finally:
                server.write_lock.release()  # type: ignore[union-attr]  # Server was checked for None before native I/O.
        if (not isinstance(written, dict) or written.get('accepted') is not True
                or written.get('duplicate') is not False):
            # A duplicate proves only prior acceptance. Its stdin outcome can
            # remain unknown, so do not assert that the waiting tool received it.
            continue
        with runtime.lock, runtime.db() as db:
            current_receipt = db.execute('SELECT record FROM runtime_tool_requests WHERE id=?',
                                         (record['id'],)).fetchone()
            if current_receipt and current_receipt[0] == raw:
                resolved['nativeDelivery'] = {'at': time.time(), 'source': 'saved_tool_result',
                                              'stage': 'written', 'connectionId': connection,
                                              'supervisor': proof, 'rpcId': record['rpcId']}
                runtime.put(db, 'tool_requests', resolved)
        delivered.append(record['id'])
        break  # One native write per job; other exact waiters run on later ticks.
    return {'status': 'tool_response_delivered' if delivered else 'superseded', 'requests': delivered}


def tick(runtime: "Runtime", agents: "Iterable[AgentRecord]") -> None:
    """Bound retries by account and time without blocking the recovery timer."""
    with runtime.lock:
        if runtime.closed:
            return
        jobs = runtime.__dict__.setdefault('_tool_response_recovery_jobs', {})
        checks = runtime.__dict__.setdefault('_tool_response_recovery_checks', {})
        now = time.monotonic()
        for agent in agents:
            account = agent.get('accountKey', 'default')
            identity = tuple(agent.get(field) for field in ('id', 'epoch', 'accountKey', 'threadId', 'turnId'))
            if (not eligible(agent) or account in jobs or len(jobs) >= MAX_JOBS
                    or checks.get(identity, 0) > now):
                continue
            checks[identity] = now + 30
            worker = threading.Thread(target=_run, args=(runtime, agent['id'], account),
                                      name='studio-tool-response-' + account[:12], daemon=True)
            jobs[account] = worker
            try:
                worker.start()
            except Exception:
                jobs.pop(account, None)
                raise
        if len(checks) > 256:
            for old in sorted(checks, key=checks.get)[:-256]:
                checks.pop(old)


def _run(runtime: "Runtime", key: str, account: str) -> None:
    try:
        recover(runtime, key)
    except Exception as error:
        runtime._tool_response_recovery_error = {'at': time.time(), 'agent': key,  # type: ignore[attr-defined]  # Runtime diagnostic state is intentionally dynamic.
                                                 'errorType': type(error).__name__}
    finally:
        with runtime.lock:
            runtime.__dict__.get('_tool_response_recovery_jobs', {}).pop(account, None)
