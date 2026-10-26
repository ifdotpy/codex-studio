#!/usr/bin/env python3
"""Differential HTTP contract for work writes and action-specific 501s."""

from __future__ import annotations

import importlib.util
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import closing
from pathlib import Path

HERE = Path(os.path.abspath(os.path.dirname(__file__)))
# Fields advanced by the accepted-archive tick between differential snapshots.
TICK_OWNED_WORK_FIELDS = {
    "archive": {"tasks"},
    "archiveIntent": {"attempts", "nextAttemptAt", "lastOutcome"},
}


def without_accepted_archive_tick_fields(snapshot):
    """Normalize only the retry fields owned by the accepted-archive tick."""
    normalized = []
    for database_name, tables in snapshot:
        normalized_tables = []
        for table, columns, rows in tables:
            if table == "runtime_work" and "record" in columns:
                record_index = columns.index("record")
                stable_rows = []
                for row in rows:
                    values = list(row)
                    record = json.loads(values[record_index])
                    for owner, fields in TICK_OWNED_WORK_FIELDS.items():
                        section = record.get(owner)
                        if isinstance(section, dict):
                            record[owner] = {
                                key: value for key, value in section.items() if key not in fields
                            }
                    values[record_index] = json.dumps(record, sort_keys=True, separators=(",", ":"))
                    stable_rows.append(tuple(values))
                rows = tuple(stable_rows)
            normalized_tables.append((table, columns, rows))
        normalized.append((database_name, tuple(normalized_tables)))
    return tuple(normalized)


sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parents[4] / "workspaces/runtime/packages/runtime-services/tests/turn_lifecycle"))
from oracle_guard import install_guard

_oracle_guard = install_guard(allow_loopback=True)

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import BackendProcess, TimedHTTPContractTest, _report_contract_backend_timing, no_write_snapshot
from server_test_waits import HANG_GUARD_SECONDS
from codex_sync_entities import register_functions

_fixture_spec = importlib.util.spec_from_file_location("work_write_runtime_fixture", HERE / "runtime-contract.py")
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)
_oracle_guard.self_test()


def quiet_runtime(state):
    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

        def _queue_accepted_archive(self, task_id, intent_id):
            # The Rust route returns before its delayed maintenance retry. Keep
            # Python's WorkMixin intent at the same post-write boundary.
            pass
    return make_runtime(QuietRuntime, state, _fixture.FakeServer)


def fixture_connection(path: Path):
    database = sqlite3.connect(path)
    register_functions(database)
    return database


class FakeLayrGuest:
    """Python WorkMixin oracle for the two VM work guest requests."""

    def __init__(self):
        self.calls = []

    def request(self, method, params, request_id=None, timeout=None):
        self.calls.append((method, params, request_id, timeout))
        if method == "line.evidence":
            return {
                "projectId": "work-write-project",
                "line": "work-write-worker-line",
                "stateId": params["stateId"],
            }
        if method == "line.merge":
            return {"stateId": params["expectedStateId"], "reviewedStateId": params["expectedStateId"]}
        if method in {"line.remove", "agent.release"}:
            return {"state": "removed"}
        raise AssertionError(f"Unexpected fake Layr method: {method}")


class FakeLayrSocket:
    """Disposable Unix JSON-RPC endpoint kept outside the guarded oracle process."""

    SERVER = r'''import json, socket, sqlite3, sys
path, config_path, calls_path, database_path, ready_path = sys.argv[1:]
try: __import__('os').unlink(path)
except FileNotFoundError: pass
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(path)
listener.listen()
open(ready_path, 'w', encoding='utf-8').write('ready')
while True:
    connection, _ = listener.accept()
    with connection:
        frame = bytearray()
        while b'\n' not in frame:
            part = connection.recv(65536)
            if not part: break
            frame.extend(part)
        if b'\n' not in frame: continue
        request = json.loads(bytes(frame).split(b'\n', 1)[0])
        method, params = request['method'], request['params']
        kind = 'submission' if method == 'line.evidence' else 'merge'
        with sqlite3.connect(database_path) as db:
            row = db.execute('SELECT result FROM runtime_operation_receipts WHERE id LIKE ? AND json_extract(result,\'$.requestId\') = ?',
                             ('work-write-vm-intent:%s:%%' % kind, request['id'])).fetchone()
        intent_status = json.loads(row[0])['status'] if row else None
        with open(calls_path, 'a', encoding='utf-8') as output:
            output.write(json.dumps({**request, 'intentStatus': intent_status}) + '\n')
        try:
            config = json.load(open(config_path, encoding='utf-8'))
        except FileNotFoundError: config = {}
        if config.get('dropNext') == method:
            json.dump({}, open(config_path, 'w', encoding='utf-8'))
            continue
        if method == 'line.evidence':
            result = {'projectId':'work-write-project', 'line':'work-write-worker-line',
                      'stateId':params['stateId']}
        elif method == 'line.merge':
            state_id = params['expectedStateId']
            result = {'stateId':state_id, 'reviewedStateId':state_id}
        else: raise AssertionError('unexpected method: ' + method)
        connection.sendall(json.dumps({'id':request['id'], 'result':result}).encode() + b'\n')
'''

    def __init__(self, path: Path, database_path: Path):
        self.path = path
        self.config_path = path.with_name("fake-guest-config.json")
        self.calls_path = path.with_name("fake-guest-calls.jsonl")
        self.ready_path = path.with_name("fake-guest-ready")
        self.stderr_path = path.with_name("fake-guest-stderr.log")
        self.database_path = database_path
        path.parent.mkdir(parents=True, exist_ok=True)
        self.calls_path.unlink(missing_ok=True)
        self.ready_path.unlink(missing_ok=True)
        self.stderr_path.unlink(missing_ok=True)
        self.config_path.write_text("{}", encoding="utf-8")
        server_script = path.with_name("fake-guest-server.py")
        server_script.write_text(self.SERVER, encoding="utf-8")
        stderr = self.stderr_path.open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [os.environ.get("CODEX_AGENTS_PYTHON", sys.executable), "-B", str(server_script), str(path), str(self.config_path),
             str(self.calls_path), str(self.database_path), str(self.ready_path)],
            cwd=str(path.parent), stdout=subprocess.DEVNULL, stderr=stderr,
        )
        stderr.close()
        # The fake guest must bind its socket; this deadline only guards against a hang.
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while not self.ready_path.exists():
            if self.process.poll() is not None:
                detail = self.stderr_path.read_text(encoding="utf-8", errors="replace")
                raise RuntimeError(
                    "fake guest exited before binding its Unix socket: " + detail[-2000:]
                )
            if time.monotonic() >= deadline:
                self.process.terminate()
                self.process.wait(timeout=2)
                raise RuntimeError("fake guest did not bind its Unix socket")
            time.sleep(0.01)

    @property
    def calls(self):
        if not self.calls_path.exists():
            return []
        return [json.loads(line) for line in self.calls_path.read_text(encoding="utf-8").splitlines()]

    def drop_next(self, method):
        self.config_path.write_text(json.dumps({"dropNext": method}), encoding="utf-8")

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            # A killed process must be reaped; this wait only guards against a hang.
            self.process.wait(timeout=HANG_GUARD_SECONDS)
        self.path.unlink(missing_ok=True)


def pause_agent(backend, agent_id: str) -> None:
    with closing(fixture_connection(backend.state / "canvas.sqlite3")) as database, database:
        row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
        agent = json.loads(row[0])
        agent.update(status="paused", autoWake=False, moveImportPending=True)
        agent.pop("restartRecovery", None)
        database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), agent_id))


def seed_agents(backend) -> tuple[str, str]:
    runtime = quiet_runtime(backend.state)
    try:
        root = runtime.create({"name": "Work write lead", "cwd": str(backend.cwd), "prompt": "Work write contracts", "autoWake": True, "workspaceMode": "worktree"}, defer=True)
        root.update(status="idle", autoWake=True, executionMode="native", workspaceMode="worktree", environment="host")
        with runtime.db() as database:
            runtime.put(database, "agents", root)
        worker = runtime.create({"name": "Work write worker", "cwd": str(backend.cwd), "prompt": "Receive decisions", "autoWake": True}, parent=root["id"], defer=True)
        root.update(status="paused", autoWake=False, concurrency=32, subagentConcurrencyVersion=2, agentMode="multi", moveImportPending=True, executionMode="native", workspaceMode="worktree", environment="host")
        worker.update(status="paused", autoWake=False, worktree=False, moveImportPending=True, executionMode="native", workspaceMode="worktree", environment="host")
        with runtime.db() as database:
            runtime.put(database, "agents", root)
            runtime.put(database, "agents", worker)
        return root["id"], worker["id"]
    finally:
        runtime.close()


def start_backend_with_seeded_agents(test_case):
    scratch_root = Path(os.environ["TMPDIR"])
    backend = BackendProcess(scratch_root)
    test_case.addClassCleanup(_report_contract_backend_timing, backend)
    test_case.addClassCleanup(backend.close)
    backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
    agent_ids = seed_agents(backend)
    ready = backend.start()
    backend.ready_samples.append(ready)
    return backend, agent_ids


def _normal(value, work_ids=None, aliases=None, field=None):
    aliases = aliases or {}
    if isinstance(value, str):
        if field in {"id", "task", "task_id", "resultId", "dependencies"}:
            return aliases.get(value, (work_ids or {}).get(value, value))
        if field == "resultFile":
            return aliases.get(value, value)
        return value
    if isinstance(value, (list, tuple)):
        normalized = [_normal(item, work_ids, aliases, field) for item in value]
        if field == "tasks" and all(isinstance(item, str) for item in normalized):
            normalized.sort()
        return normalized if isinstance(value, list) else tuple(normalized)
    if isinstance(value, dict):
        result = {}
        for key, item in value.items():
            if key in {"created", "updated", "enqueuedAt"}:
                continue
            if key == "nextAttemptAt" and isinstance(item, (int, float)):
                updated = value.get("updated")
                if isinstance(updated, (int, float)):
                    result[key] = round(item - updated, 1)
                    continue
            result[key] = _normal(item, work_ids, aliases, key)
        return result
    return value


def snapshot(state: Path):
    """Normalize work-write state for the Python/Rust parity assertion."""
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        works = database.execute("SELECT id,record FROM runtime_work ORDER BY rowid").fetchall()
        work_ids = {identity: f"work-{index}" for index, (identity, _) in enumerate(works)}
        normalized_work = [
            (work_ids[identity], _normal(json.loads(record), work_ids))
            for identity, record in works
        ]
        agents = []
        for identity, raw_record in database.execute("SELECT id,record FROM runtime_agents ORDER BY id"):
            record = json.loads(raw_record)
            # Parity ignores fields that the Python dispatcher can advance between reads.
            for field in ("status", "inFlight", "startAttempt", "turnId", "threadId", "error",
                          "restartRecovery", "connectionRecovery", "worktreePreparation"):
                record.pop(field, None)
            agents.append((identity, _normal(record)))
        receipts = []
        archive_receipts = []
        for identity, signature, result in database.execute(
            "SELECT id,signature,result FROM runtime_operation_receipts ORDER BY id"
        ):
            record = _normal(json.loads(result), work_ids)
            if signature == "accepted-archive-v1":
                archive_receipts.append((identity, signature, record))
            elif not identity.startswith("work-write-vm-intent:"):
                receipts.append((identity, signature, record))
        events = []
        for row in database.execute("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events"):
            identity, agent, kind, body, status, created, epoch, turn_id, error = row
            try:
                body = _normal(json.loads(body), work_ids)
            except (TypeError, ValueError):
                pass
            # Durable event parity ignores the dispatch-owned status column.
            events.append((identity, agent, kind, body, epoch, turn_id, error))
        event_meta = [
            (identity, _normal(json.loads(record), work_ids))
            for identity, record in database.execute(
                "SELECT id,record FROM runtime_event_meta ORDER BY id"
            )
        ]
        return {
            "runtime_work": normalized_work,
            "runtime_agents": agents,
            "runtime_operation_receipts": receipts,
            # Rust's durable retry receipt has no Python-side table analogue;
            # assert its contract separately while comparing all Python-visible
            # rows, events, responses, and the complete work records.
            "accepted_archive_receipts": archive_receipts,
            "runtime_events": events,
            "runtime_event_meta": event_meta,
            "_work_ids": work_ids,
        }


def normalize_event_id(identity, work_ids, aliases):
    parts = identity.split(":")
    if len(parts) == 2 and parts[0] == "work-result":
        parts[1] = aliases.get(parts[1], parts[1])
    elif len(parts) >= 3 and parts[0] in {"work-decision", "work-released"}:
        parts[1] = work_ids.get(parts[1], parts[1])
    elif len(parts) >= 3 and parts[0] == "work-ready":
        parts[1] = work_ids.get(parts[1], parts[1])
        parts[2] = work_ids.get(parts[2], parts[2])
    return ":".join(parts)


def strip_tick_owned_work_fields(state):
    state.pop("accepted_archive_receipts", None)
    for identity, record in state.get("runtime_work", []):
        for owner, fields in TICK_OWNED_WORK_FIELDS.items():
            section = record.get(owner)
            if isinstance(section, dict):
                record[owner] = {key: value for key, value in section.items() if key not in fields}
    return state


def compare_snapshots(expected_state, actual_state, aliases):
    actual_work_ids = actual_state.pop("_work_ids")
    expected_work_ids = expected_state.pop("_work_ids")
    # The reference Python oracle keeps its dispatcher quiet, so the two
    # backends can reach different retries after the same request.
    strip_tick_owned_work_fields(actual_state)
    strip_tick_owned_work_fields(expected_state)
    actual_state = _normal(actual_state, actual_work_ids, aliases)
    expected_state = _normal(expected_state, expected_work_ids, aliases)
    for state, work_ids in ((actual_state, actual_work_ids), (expected_state, expected_work_ids)):
        state["runtime_events"] = sorted(
            [
                (normalize_event_id(row[0], work_ids, aliases), *row[1:])
                for row in state["runtime_events"]
            ],
            key=lambda row: (row[0], json.dumps(row[1:], ensure_ascii=False, sort_keys=True, separators=(",", ":"))),
        )
        state["runtime_event_meta"] = sorted(
            (normalize_event_id(identity, work_ids, aliases), record)
            for identity, record in state["runtime_event_meta"]
        )
    return actual_state, expected_state


def reference_action(backend, agent_id: str, body: dict, *, guest=None):
    """Run the Python writer on a byte-for-byte database copy of the input state."""
    with tempfile.TemporaryDirectory(prefix="work-write-python-ref-") as directory:
        state = Path(directory) / "state"
        state.mkdir()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source, closing(sqlite3.connect(state / "canvas.sqlite3")) as target:
            source.backup(target)
        runtime = quiet_runtime(state)
        try:
            if guest is not None:
                runtime._linux_vm_client = guest
            result = runtime.work_action(agent_id, {"agent": agent_id, **body}, body.get("id"))
        finally:
            runtime.close()
        expected = snapshot(state)
        return result, expected


def reference_organization(backend, body: dict):
    """Run Python's organization effect on an independent copy of the input."""
    with tempfile.TemporaryDirectory(prefix="organization-python-ref-") as directory:
        state = Path(directory) / "state"
        state.mkdir()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source, closing(
            sqlite3.connect(state / "canvas.sqlite3")
        ) as target:
            source.backup(target)
        runtime = quiet_runtime(state)
        try:
            if body.get("archived") is True:
                with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source:
                    monitor = source.execute(
                        "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.status') IN ('running','approval','starting') LIMIT 1",
                        (body["id"],),
                    ).fetchone()
                if monitor is None:
                    raise AssertionError("Python organization oracle fixture lost its active monitor")
                with runtime.db() as database:
                    # Runtime startup may classify stale monitors before the
                    # organization effect reads them; restore this fixture row
                    # after startup in the isolated Python oracle.
                    database.execute(
                        "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES (?,?)",
                        monitor,
                    )
            result = runtime.chat_organization(body["id"], body)
        finally:
            runtime.close()
        return result, snapshot(state)


def reference_actions(backend, agent_id: str, bodies: list[dict]):
    """Apply a complete independent Python action chain to a DB copy."""
    with tempfile.TemporaryDirectory(prefix="work-write-python-chain-") as directory:
        state = Path(directory) / "state"
        state.mkdir()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source, closing(
            sqlite3.connect(state / "canvas.sqlite3")
        ) as target:
            source.backup(target)
        runtime = quiet_runtime(state)
        try:
            results = [
                runtime.work_action(agent_id, {"agent": agent_id, **body}, body.get("id"))
                for body in bodies
            ]
        finally:
            runtime.close()
        return results, snapshot(state)


def normal_result(value, aliases=None):
    if isinstance(value, dict):
        value = {key: item for key, item in value.items() if key not in {"_syncEntities", "_syncEntitiesAfter"}}
    return _normal(value, aliases=aliases)


def result_id_aliases(expected_state, actual_state):
    aliases = {}
    expected_work_ids = expected_state["_work_ids"]
    actual_work_ids = actual_state["_work_ids"]
    for identity in expected_work_ids.keys() - actual_work_ids.keys():
        aliases[identity] = expected_work_ids[identity]
    for identity in actual_work_ids.keys() - expected_work_ids.keys():
        aliases[identity] = actual_work_ids[identity]
    expected_work = dict(expected_state["runtime_work"])
    actual_work = dict(actual_state["runtime_work"])
    result_index = 0
    for work_id in expected_work.keys() & actual_work.keys():
        before_results = expected_work[work_id].get("results", [])
        after_results = actual_work[work_id].get("results", [])
        for before, after in zip(before_results, after_results):
            if isinstance(before.get("id"), str) and isinstance(after.get("id"), str):
                alias = f"<result-{result_index}>"
                aliases[before["id"]] = alias
                aliases[after["id"]] = alias
                if isinstance(before.get("resultFile"), str) and isinstance(after.get("resultFile"), str):
                    file_alias = f"<result-file-{result_index}>"
                    aliases[before["resultFile"]] = file_alias
                    aliases[after["resultFile"]] = file_alias
                result_index += 1
    return aliases


def python_action(backend, agent_id, body):
    """Run the Python writer on a clone, then install its rows as live fixtures."""
    table_names = (
        "runtime_work",
        "runtime_events",
        "runtime_event_meta",
        "runtime_operation_receipts",
    )
    with tempfile.TemporaryDirectory(prefix="work-write-python-action-") as directory:
        cloned_db = Path(directory) / "canvas.sqlite3"
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source, closing(
            sqlite3.connect(cloned_db)
        ) as target:
            source.backup(target)

        runtime = quiet_runtime(Path(directory))
        try:
            result = runtime.work_action(agent_id, {"agent": agent_id, **body}, body.get("id"))
        finally:
            runtime.close()

        with closing(sqlite3.connect(cloned_db)) as reference, closing(
            fixture_connection(backend.state / "canvas.sqlite3")
        ) as live, live:
            for table in table_names:
                columns = [row[1] for row in reference.execute(f'PRAGMA table_info("{table}")')]
                keys = [row[1] for row in reference.execute(f'PRAGMA table_info("{table}")') if row[5]]
                if not columns or not keys:
                    continue
                selected = ",".join(f'"{column}"' for column in columns)
                key_indexes = [columns.index(key) for key in keys]
                before_rows = {
                    tuple(row[index] for index in key_indexes): tuple(row)
                    for row in live.execute(f'SELECT {selected} FROM "{table}"')
                }
                changed = []
                for row in reference.execute(f'SELECT {selected} FROM "{table}"'):
                    identity = tuple(row[index] for index in key_indexes)
                    if before_rows.get(identity) != tuple(row):
                        changed.append(tuple(row))
                if changed:
                    placeholders = ",".join("?" for _ in columns)
                    live.executemany(
                        f'INSERT OR REPLACE INTO "{table}" ({selected}) VALUES ({placeholders})',
                        changed,
                    )
    return result


def python_error(backend, agent_id, body):
    try:
        reference_action(backend, agent_id, body)
    except ValueError as error:
        return str(error)
    raise AssertionError("Python accepted a refusal case")


class WorkWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    def assert_snapshots_equal(self, expected, actual, message=None):
        self.assertEqual(
            strip_tick_owned_work_fields(actual),
            strip_tick_owned_work_fields(expected),
            message,
        )

    def assert_snapshots_differ(self, expected, actual, message=None):
        self.assertNotEqual(
            strip_tick_owned_work_fields(actual),
            strip_tick_owned_work_fields(expected),
            message,
        )

    @classmethod
    def setUpClass(cls) -> None:
        cls.backend, (cls.agent_id, cls.worker_id) = start_backend_with_seeded_agents(cls)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def setUp(self) -> None:
        super().setUp()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.worker_id,)).fetchone()
            worker = json.loads(row[0])
            worker.update(executionMode="native", workspaceMode="worktree", environment="host")
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(worker), self.worker_id))

    def configure_vm_agents(self):
        originals = []
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            for agent_id, line in ((self.agent_id, "main"), (self.worker_id, "work-write-worker-line")):
                row = database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
                ).fetchone()
                original = json.loads(row[0])
                originals.append((agent_id, original))
                agent = dict(original)
                agent.update(executionMode="vm", workspaceMode="layr", environment="linux",
                             layrProjectId="work-write-project", layrLine=line, layrReady=True)
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), agent_id),
                )
        self.addCleanup(self.restore_agent_records, originals)

    def restore_agent_records(self, records):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            for agent_id, record in records:
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), agent_id),
                )

    def headers(self):
        return {"Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, body):
        return self.backend.request("POST", "/api/work", body={"agent": self.agent_id, **body}, headers=self.headers())

    def post_as(self, agent_id, body):
        return self.backend.request("POST", "/api/work", body={"agent": agent_id, **body}, headers=self.headers())

    def organization(self, body):
        return self.backend.request("POST", "/api/organization", body=body, headers=self.headers())

    def assert_organization_matches_python(self, body):
        expected_result, expected_state = reference_organization(self.backend, body)
        response = self.organization(body)
        self.assertEqual(response.status, 200, response.body[:1000])
        actual_state = snapshot(self.backend.state)
        aliases = result_id_aliases(expected_state, actual_state)
        self.assertEqual(normal_result(response.json(), aliases), normal_result(expected_result, aliases))
        actual_state, expected_state = compare_snapshots(expected_state, actual_state, aliases)
        self.assertEqual(actual_state, expected_state)
        return response.json()

    def test_organization_effects_and_refusals(self):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            original = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()[0]
        self.addCleanup(self.restore_agent_record, original)
        organized = self.organization({"id": self.agent_id, "pinned": True, "project": "P16"})
        self.assertEqual(organized.status, 200, organized.body[:1000])
        self.assertTrue(organized.json()["pinned"])
        self.assertEqual(organized.json()["project"], "P16")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0])
            agent.update(threadId="thread-p16", lastCompletedTurn="turn-p16", lastCompletedTurnStatus="completed")
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        state = {"thread_id": "thread-p16", "turn_id": "turn-p16", "read": True, "expected_revision": 0}
        read = self.organization({"id": self.agent_id, "read_state": state})
        self.assertEqual(read.status, 200, read.body[:1000])
        self.assertEqual(read.json()["readState"], {"threadId": "thread-p16", "turnId": "turn-p16", "read": True, "revision": 1})
        self.assertEqual(self.organization({"id": self.agent_id, "read_state": state}).status, 200)
        stale = self.organization({"id": self.agent_id, "read_state": {**state, "read": False}})
        self.assertEqual(stale.status, 400, stale.body[:1000])
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent["inFlight"] = True
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        archive = self.organization({"id": self.agent_id, "archived": True})
        self.assertEqual(archive.status, 400, archive.body[:1000])

    def test_organization_folder_revision_parity_and_projection(self):
        original = self.agent_record()
        self.addCleanup(self.restore_agent_record, original)
        project_id = str(self.backend.cwd)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            project_row = database.execute(
                "SELECT record FROM runtime_projects WHERE id=?", (project_id,)
            ).fetchone()
        self.addCleanup(self.restore_project_record, project_id, project_row[0] if project_row else None)
        folders = [
            {"id": "p16-folder-a", "name": "Folder A"},
            {"id": "p16-folder-b", "name": "Folder B"},
        ]
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute(
                "INSERT OR REPLACE INTO runtime_projects(id,record) VALUES (?,?)",
                (
                    project_id,
                    json.dumps({
                        "id": project_id,
                        "path": project_id,
                        "name": "P16 HTTP fixture",
                        "created": 1.0,
                        "accountKey": "default",
                        "accountRevision": 1,
                        "folders": folders,
                    }),
                ),
            )
            agent = json.loads(original)
            agent.pop("projectFolder", None)
            agent["projectFolderRevision"] = 0
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), self.agent_id),
            )
        moved = self.assert_organization_matches_python({
            "id": self.agent_id,
            "project_path": str(self.backend.cwd),
            "project_folder": "p16-folder-a",
            "expected_revision": 0,
            "expected_folder": None,
        })
        self.assertEqual(moved["projectFolder"], "p16-folder-a")
        self.assertEqual(moved["projectFolderRevision"], 1)
        moved_again = self.assert_organization_matches_python({
            "id": self.agent_id,
            "project_path": str(self.backend.cwd),
            "project_folder": "p16-folder-b",
            "expected_revision": 1,
            "expected_folder": "p16-folder-a",
        })
        self.assertEqual(moved_again["projectFolder"], "p16-folder-b")
        self.assertEqual(moved_again["projectFolderRevision"], 2)
        replay = self.assert_organization_matches_python({
            "id": self.agent_id,
            "project_path": str(self.backend.cwd),
            "project_folder": "p16-folder-b",
            "expected_revision": 1,
            "expected_folder": "p16-folder-a",
        })
        self.assertEqual(replay["projectFolderRevision"], 2)
        before = no_write_snapshot(self.backend.state)
        stale = self.organization({
            "id": self.agent_id,
            "project_path": str(self.backend.cwd),
            "project_folder": "p16-folder-a",
            "expected_revision": 0,
            "expected_folder": None,
        })
        self.assertEqual(stale.status, 400, stale.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_organization_archive_refuses_active_monitor_without_writes(self):
        original = self.agent_record()
        self.addCleanup(self.restore_agent_record, original)
        self.addCleanup(self.remove_monitor, "p16-active-monitor")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            agent = json.loads(original)
            agent.pop("inFlight", None)
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), self.agent_id),
            )
            database.execute(
                "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES (?,?)",
                ("p16-active-monitor", json.dumps({"id": "p16-active-monitor", "agent": self.agent_id, "status": "running", "created": 1.0})),
            )
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            self.assertTrue(database.execute(
                "SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=? AND json_extract(record,'$.status') IN ('running','approval','starting'))",
                (self.agent_id,),
            ).fetchone()[0])
        expected_state = no_write_snapshot(self.backend.state)
        with self.assertRaisesRegex(ValueError, "Stop active work before archiving"):
            reference_organization(self.backend, {"id": self.agent_id, "archived": True})
        response = self.organization({"id": self.agent_id, "archived": True})
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertIn("Stop active work before archiving", response.body.decode())
        actual_state = no_write_snapshot(self.backend.state)
        self.assertEqual(
            without_accepted_archive_tick_fields(actual_state),
            without_accepted_archive_tick_fields(expected_state),
        )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "strict persisted-row validation is Rust-specific")
    def test_organization_corrupt_revisions_are_located_and_no_write(self):
        original = self.agent_record()
        self.addCleanup(self.restore_agent_record, original)
        cases = (
            ("projectFolderRevision", "bad"),
            ("readState", []),
            ("readState", {"threadId": "thread-p16", "turnId": "turn-p16", "read": True, "revision": "bad"}),
        )
        for field, corrupt in cases:
            with self.subTest(field=field, corrupt=corrupt):
                with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                    agent = json.loads(original)
                    agent.update(threadId="thread-p16", lastCompletedTurn="turn-p16", lastCompletedTurnStatus="completed")
                    agent[field] = corrupt
                    database.execute(
                        "UPDATE runtime_agents SET record=? WHERE id=?",
                        (json.dumps(agent), self.agent_id),
                    )
                before = no_write_snapshot(self.backend.state)
                body = (
                    {"id": self.agent_id, "project_folder": None, "project_path": str(self.backend.cwd), "expected_revision": 0, "expected_folder": None}
                    if field == "projectFolderRevision"
                    else {"id": self.agent_id, "read_state": {"thread_id": "thread-p16", "turn_id": "turn-p16", "read": True, "expected_revision": 0}}
                )
                response = self.organization(body)
                self.assertEqual(response.status, 503, response.body[:1000])
                self.assertIn("runtime_agents", response.body.decode())
                self.assertIn(self.agent_id, response.body.decode())
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def agent_record(self):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            return database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0]

    def restore_agent_record(self, record):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (record, self.agent_id))

    def restore_project_record(self, project_id, record):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            if record is None:
                database.execute("DELETE FROM runtime_projects WHERE id=?", (project_id,))
            else:
                database.execute(
                    "INSERT OR REPLACE INTO runtime_projects(id,record) VALUES (?,?)",
                    (project_id, record),
                )

    def remove_monitor(self, monitor_id):
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("DELETE FROM runtime_monitors WHERE id=?", (monitor_id,))

    def assert_matches_python(self, body):
        return self.assert_matches_python_as(self.agent_id, body)

    def assert_matches_python_as(self, agent_id, body):
        expected_result, expected_state = reference_action(self.backend, agent_id, body)
        response = self.post_as(agent_id, body)
        self.assertEqual(response.status, 200, response.body[:1000])
        actual_state = snapshot(self.backend.state)
        aliases = result_id_aliases(expected_state, actual_state)
        self.assertEqual(normal_result(response.json(), aliases), normal_result(expected_result, aliases))
        actual_state, expected_state = compare_snapshots(expected_state, actual_state, aliases)
        for table, actual_rows in actual_state.items():
            if actual_rows != expected_state[table]:
                self.fail(f"{table} differs (result ID aliases={aliases!r}): actual={actual_rows!r}; expected={expected_state[table]!r}")
        return response.json()

    def test_http_lead_and_unassigned_submit_create_work_review(self):
        lead_owned = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Lead-owned submit", "owner": self.agent_id,
        })
        self.assert_matches_python({
            "action": "submit", "task_id": lead_owned["id"], "version": lead_owned["version"],
            "result": "lead evidence", "checks": "checked", "revision": "abc1234",
            "id": "http-lead-submit-review-event",
        })
        unassigned = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Unassigned submit",
        })
        self.assert_matches_python({
            "action": "submit", "task_id": unassigned["id"], "version": unassigned["version"],
            "result": "unassigned evidence", "checks": "checked", "revision": "abc1234",
            "id": "http-unassigned-submit-review-event",
        })

    def test_rust_submit_accept_chain_matches_independent_python_chain(self):
        task = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Independent action chain", "owner": self.agent_id,
        })
        submit_body = {
            "action": "submit", "task_id": task["id"], "version": task["version"],
            "result": "chain result", "checks": "chain checks", "revision": "abc1234",
            "files": ["src/chain.rs"], "id": "independent-chain-submit",
        }
        expected_results, expected_state = reference_actions(
            self.backend,
            self.agent_id,
            [submit_body, {
                "action": "accept", "task_id": task["id"],
                "version": task["version"] + 1, "result": "chain accepted",
                "id": "independent-chain-accept",
            }],
        )
        submitted = self.post(submit_body)
        self.assertEqual(submitted.status, 200, submitted.body[:1000])
        accepted = self.post({
            "action": "accept", "task_id": task["id"], "version": submitted.json()["version"],
            "result": "chain accepted", "id": "independent-chain-accept",
        })
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        actual_state = snapshot(self.backend.state)
        aliases = result_id_aliases(expected_state, actual_state)
        self.assertEqual(normal_result(submitted.json(), aliases), normal_result(expected_results[0], aliases))
        self.assertEqual(normal_result(accepted.json(), aliases), normal_result(expected_results[1], aliases))
        actual_state, expected_state = compare_snapshots(expected_state, actual_state, aliases)
        self.assertEqual(actual_state, expected_state)

    def test_accept_remote_dependent_is_501_and_atomic(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python backend routes remote work through federation")
        self.configure_vm_agents()
        review_response = self.post({
            "action": "create", "title": "Remote dependent review", "owner": self.worker_id,
        })
        self.assertEqual(review_response.status, 200, review_response.body[:1000])
        review = review_response.json()
        guest = FakeLayrSocket(self.backend.vm_state / "guest.sock", self.backend.state / "canvas.sqlite3")
        self.addCleanup(guest.close)
        submission = self.post_as(self.worker_id, {
            "action": "submit", "task_id": review["id"], "version": review["version"],
            "result": "review me", "checks": "checked", "revision": "vm-remote-dependent",
            "id": "vm-remote-dependent-submit",
        })
        self.assertEqual(submission.status, 200, submission.body[:1000])
        dependent_response = self.post({
            "action": "create", "title": "Remote dependent", "owner": self.worker_id,
            "status": "blocked", "dependencies": [review["id"]],
        })
        self.assertEqual(dependent_response.status, 200, dependent_response.body[:1000])
        dependent = dependent_response.json()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.worker_id,)).fetchone()
            worker = json.loads(row[0])
            worker["remoteWorker"] = {"server": "remote-server", "link": "remote-link"}
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(worker), self.worker_id))
        body = {
            "action": "accept", "task_id": review["id"], "version": submission.json()["version"],
            "result": "verified", "id": "remote-dependent-accept",
        }
        before = snapshot(self.backend.state)
        calls_before_accept = len(guest.calls)
        refused = self.post(body)
        self.assertEqual(refused.status, 501, refused.body[:1000])
        self.assertEqual(refused.json().get("error"), "Not implemented: requires cross-server/remote-events")
        self.assert_snapshots_equal(before, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), calls_before_accept)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.worker_id,)).fetchone()
            worker = json.loads(row[0])
            worker.pop("remoteWorker", None)
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(worker), self.worker_id))
        accepted = self.post(body)
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        self.assertEqual(sum(call["method"] == "line.merge" for call in guest.calls), 1)
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(
            normal_result(replay.json()),
            normal_result(accepted.json()),
        )
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            rows = database.execute(
                "SELECT id FROM runtime_events WHERE kind='work_ready' AND json_extract(text,'$.task')=?",
                (dependent["id"],),
            ).fetchall()
        self.assertEqual(len(rows), 1)

    def test_claim_submit_accept_differential_python_written_states(self):
        claimed = python_action(self.backend, self.agent_id, {"action": "create", "title": "Claim oracle"})
        self.assert_matches_python_as(self.agent_id, {
            "action": "claim", "task_id": claimed["id"], "owner": self.worker_id,
        })

        submitted = python_action(self.backend, self.agent_id, {"action": "create", "title": "Submit oracle"})
        worker_claim = python_action(self.backend, self.agent_id, {
            "action": "claim", "task_id": submitted["id"], "owner": self.worker_id,
        })
        self.assert_matches_python_as(self.agent_id, {
            "action": "submit", "task_id": submitted["id"], "version": worker_claim["version"],
            "result": "worker evidence", "checks": "checks passed", "revision": "abc1234",
            "files": ["src/lib.rs"], "id": "work-diff-submit",
        })

        reviewed = python_action(self.backend, self.agent_id, {"action": "create", "title": "Accept oracle"})
        lead_claim = python_action(self.backend, self.agent_id, {
            "action": "claim", "task_id": reviewed["id"], "owner": self.agent_id,
        })
        lead_submit = python_action(self.backend, self.agent_id, {
            "action": "submit", "task_id": reviewed["id"], "version": lead_claim["version"],
            "result": "lead evidence", "checks": "checks passed", "revision": "abc1234",
        })
        python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Wake after acceptance", "owner": self.worker_id,
            "status": "blocked", "dependencies": [reviewed["id"]],
        })
        accept_body = {
            "action": "accept", "task_id": reviewed["id"], "version": lead_submit["version"],
            "result": "reviewed", "id": "work-diff-accept",
        }
        self.assert_matches_python_as(self.worker_id, accept_body)
        self.assert_matches_python_as(self.worker_id, accept_body)
        conflict = {**accept_body, "result": "changed decision"}
        expected_error = python_error(self.backend, self.worker_id, conflict)
        before = snapshot(self.backend.state)
        response = self.post_as(self.worker_id, conflict)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), expected_error)
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

    def test_vm_submission_accept_parity_and_unknown_intents_survive_restart(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python backend uses the in-process fake guest contract")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            for agent_id, line in ((self.agent_id, "main"), (self.worker_id, "work-write-worker-line")):
                row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
                agent = json.loads(row[0])
                agent.update(executionMode="vm", workspaceMode="layr", environment="linux",
                             layrProjectId="work-write-project", layrLine=line, layrReady=True)
                database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), agent_id))

        task = self.assert_matches_python({"action": "create", "title": "VM evidence oracle",
                                           "owner": self.worker_id, "id": "vm-evidence-create"})
        guest = FakeLayrSocket(self.backend.vm_state / "guest.sock", self.backend.state / "canvas.sqlite3")
        self.addCleanup(guest.close)
        submit = {"action": "submit", "task_id": task["id"], "version": task["version"],
                  "result": "exact VM result", "checks": "fake guest passed", "revision": "vm-state-1",
                  "files": [], "id": "vm-evidence-submit"}
        expected_submit_guest = FakeLayrGuest()
        expected_submit, expected_state = reference_action(
            self.backend, self.worker_id, submit, guest=expected_submit_guest)
        actual_submit = self.post_as(self.worker_id, submit)
        self.assertEqual(actual_submit.status, 200, actual_submit.body[:1000])
        actual_state = snapshot(self.backend.state)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            intent = json.loads(database.execute(
                "SELECT result FROM runtime_operation_receipts WHERE id=?",
                (f"work-write-vm-intent:submission:{task['id']}",),
            ).fetchone()[0])
        self.assertEqual(intent["status"], "completed")
        self.assertEqual(intent["requestId"],
                         "layr-evidence:" + self.worker_id + ":vm-state-1")
        aliases = result_id_aliases(expected_state, actual_state)
        self.assertEqual(normal_result(actual_submit.json(), aliases),
                         normal_result(expected_submit, aliases))
        self.assertEqual([call[0] for call in expected_submit_guest.calls], ["line.evidence"])
        actual_state, expected_state = compare_snapshots(expected_state, actual_state, aliases)
        self.assertEqual(actual_state, expected_state)

        accept = {"action": "accept", "task_id": task["id"], "version": actual_submit.json()["version"],
                  "result": "verified on VM", "id": "vm-merge-accept"}
        expected_guest = FakeLayrGuest()
        expected_accept, expected_state = reference_action(
            self.backend, self.agent_id, accept, guest=expected_guest)
        actual_accept = self.post(accept)
        self.assertEqual(actual_accept.status, 200, actual_accept.body[:1000])
        actual_accept_fields = set(actual_accept.json()) - {"_syncEntities", "_syncEntitiesAfter"}
        self.assertEqual(actual_accept_fields, set(expected_accept),
                         f"accept result fields differ: actual={sorted(actual_accept_fields)}, expected={sorted(expected_accept)}")
        actual_state = snapshot(self.backend.state)
        rust_work = actual_state["runtime_work"][-1][1]
        expected_work = expected_state["runtime_work"][-1][1]
        self.assert_snapshots_equal(
            {"runtime_work": [(task["id"], expected_work)]},
            {"runtime_work": [(task["id"], rust_work)]},
        )
        archive_receipts = [
            item for item in actual_state["accepted_archive_receipts"]
            if item[2].get("taskId") == task["id"]
        ]
        self.assertEqual(len(archive_receipts), 1)
        archive_receipt = archive_receipts[0][2]
        self.assertEqual(archive_receipt["taskId"], task["id"])
        self.assertEqual(archive_receipt["intentId"], task["id"] + ":" + actual_accept.json()["results"][-1]["id"])
        self.assertEqual(archive_receipt["actor"], self.agent_id)
        self.assertEqual(archive_receipt["target"], self.worker_id)
        self.assertIn(archive_receipt["status"], {"unknown", "pending", "retryable", "terminal", "complete"})
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            intent = json.loads(database.execute(
                "SELECT result FROM runtime_operation_receipts WHERE id=?",
                (f"work-write-vm-intent:merge:{task['id']}",),
            ).fetchone()[0])
        self.assertEqual(intent["status"], "completed")
        self.assertEqual(intent["requestId"],
                         "layr-accept:" + actual_accept.json()["results"][-1]["id"])
        aliases = result_id_aliases(expected_state, actual_state)
        self.maxDiff = None
        self.assertEqual(normal_result(actual_accept.json(), aliases),
                         normal_result(expected_accept, aliases))
        actual_state, expected_state = compare_snapshots(expected_state, actual_state, aliases)
        self.assertEqual(actual_state, expected_state)
        self.assertEqual([call["intentStatus"] for call in guest.calls], ["pending", "pending"])
        self.assertEqual([call["method"] for call in guest.calls], ["line.evidence", "line.merge"])
        self.assertEqual(guest.calls[0]["id"], "layr-evidence:" + self.worker_id + ":vm-state-1")
        self.assertEqual(guest.calls[1]["id"], intent["requestId"])
        self.assertEqual([call[0] for call in expected_guest.calls], ["line.merge"])
        self.assertEqual(expected_guest.calls[0][2],
                         "layr-accept:" + expected_accept["results"][-1]["id"])
        completed_call_count = len(guest.calls)
        replay_submit = self.post_as(self.worker_id, submit)
        replay_accept = self.post(accept)
        self.assertEqual(replay_submit.status, 200, replay_submit.body[:1000])
        self.assertEqual(replay_accept.status, 200, replay_accept.body[:1000])
        self.assertEqual(len(guest.calls), completed_call_count)

        before_intent = self.assert_matches_python({"action": "create", "title": "Before intent oracle",
                                                   "owner": self.worker_id, "id": "vm-before-intent-create"})
        before_intent_body = {"action": "submit", "task_id": before_intent["id"],
                              "version": before_intent["version"], "result": "blocked before intent",
                              "checks": "injected", "revision": "vm-state-before-intent",
                              "id": "vm-before-intent-submit"}
        before_intent_snapshot = snapshot(self.backend.state)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("""CREATE TRIGGER fail_vm_intent BEFORE INSERT ON runtime_operation_receipts
                WHEN NEW.id LIKE 'work-write-vm-intent:submission:%'
                BEGIN SELECT RAISE(ABORT,'injected VM intent failure'); END""")
        call_count = len(guest.calls)
        refused_before_intent = self.post_as(self.worker_id, before_intent_body)
        self.assertNotEqual(refused_before_intent.status, 200, refused_before_intent.body[:1000])
        self.assert_snapshots_equal(before_intent_snapshot, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("DROP TRIGGER fail_vm_intent")

        after_guest = self.assert_matches_python({"action": "create", "title": "After guest oracle",
                                                  "owner": self.worker_id, "id": "vm-after-guest-create"})
        after_guest_body = {"action": "submit", "task_id": after_guest["id"],
                            "version": after_guest["version"], "result": "guest completed",
                            "checks": "fail completion write", "revision": "vm-state-after-guest",
                            "id": "vm-after-guest-submit"}
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("""CREATE TRIGGER fail_vm_completion BEFORE UPDATE ON runtime_operation_receipts
                WHEN NEW.id LIKE 'work-write-vm-intent:submission:%'
                 AND json_extract(NEW.result,'$.status')='completed'
                BEGIN SELECT RAISE(ABORT,'injected VM completion failure'); END""")
        call_count = len(guest.calls)
        failed_completion = self.post_as(self.worker_id, after_guest_body)
        self.assertNotEqual(failed_completion.status, 200, failed_completion.body[:1000])
        self.assertEqual(guest.calls[call_count]["method"], "line.evidence")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            row = database.execute("SELECT result FROM runtime_operation_receipts WHERE id=?",
                                   (f"work-write-vm-intent:submission:{after_guest['id']}",)).fetchone()
        self.assertEqual(json.loads(row[0])["status"], "pending")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("DROP TRIGGER fail_vm_completion")
        pending_snapshot = snapshot(self.backend.state)
        self.assertIn(self.post_as(self.worker_id, after_guest_body).status, (400, 409))
        self.assert_snapshots_equal(pending_snapshot, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 1)
        self.backend.stop()
        self.backend.start()
        self.assertIn(self.post_as(self.worker_id, after_guest_body).status, (400, 409))
        self.assert_snapshots_equal(pending_snapshot, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 1)

        guest.drop_next("line.evidence")
        unknown = self.assert_matches_python({"action": "create", "title": "Unknown evidence oracle",
                                             "owner": self.worker_id, "id": "vm-unknown-create"})
        unknown_submit = {"action": "submit", "task_id": unknown["id"], "version": unknown["version"],
                          "result": "unknown evidence", "checks": "lost reply", "revision": "vm-state-unknown",
                          "id": "vm-unknown-submit"}
        call_count = len(guest.calls)
        failed = self.post_as(self.worker_id, unknown_submit)
        self.assertEqual(failed.status, 502, failed.body[:1000])
        self.assertEqual(guest.calls[call_count]["method"], "line.evidence")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            row = database.execute("SELECT result FROM runtime_operation_receipts WHERE id=?",
                                   (f"work-write-vm-intent:submission:{unknown['id']}",)).fetchone()
        self.assertEqual(json.loads(row[0])["status"], "unknown")
        before_retry = snapshot(self.backend.state)
        retry = self.post_as(self.worker_id, unknown_submit)
        self.assertIn(retry.status, (400, 409))
        self.assert_snapshots_equal(before_retry, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 1)
        self.backend.stop()
        self.backend.start()
        after_restart = self.post_as(self.worker_id, unknown_submit)
        self.assertIn(after_restart.status, (400, 409))
        self.assert_snapshots_equal(before_retry, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 1)

        merge_task = self.assert_matches_python({"action": "create", "title": "Unknown merge oracle",
                                                "owner": self.worker_id, "id": "vm-merge-unknown-create"})
        merge_submit = self.post_as(self.worker_id, {
            "action": "submit", "task_id": merge_task["id"], "version": merge_task["version"],
            "result": "merge unknown", "checks": "evidence ok", "revision": "vm-state-merge-unknown",
            "id": "vm-merge-unknown-submit",
        })
        self.assertEqual(merge_submit.status, 200, merge_submit.body[:1000])
        guest.drop_next("line.merge")
        merge_accept = {"action": "accept", "task_id": merge_task["id"],
                        "version": merge_submit.json()["version"], "result": "merge test",
                        "id": "vm-merge-unknown-accept"}
        call_count = len(guest.calls)
        failed_merge = self.post(merge_accept)
        self.assertEqual(failed_merge.status, 502, failed_merge.body[:1000])
        self.assertEqual(guest.calls[call_count]["method"], "line.merge")
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            row = database.execute("SELECT result FROM runtime_operation_receipts WHERE id=?",
                                   (f"work-write-vm-intent:merge:{merge_task['id']}",)).fetchone()
        self.assertEqual(json.loads(row[0])["status"], "unknown")
        before_merge_retry = snapshot(self.backend.state)
        self.assertIn(self.post(merge_accept).status, (400, 409))
        self.assert_snapshots_equal(before_merge_retry, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 1)
        replacement = self.post_as(self.worker_id, {
            "action": "submit", "task_id": merge_task["id"],
            "version": merge_submit.json()["version"], "result": "replacement result",
            "checks": "replacement evidence", "revision": "vm-state-merge-replacement",
            "id": "vm-merge-unknown-replacement-submit",
        })
        self.assertEqual(replacement.status, 200, replacement.body[:1000])
        before_replacement_accept = snapshot(self.backend.state)
        replacement_accept_body = {
            "action": "accept", "task_id": merge_task["id"],
            "version": replacement.json()["version"], "result": "new acceptance",
            "id": "vm-merge-unknown-replacement-accept",
        }
        replacement_accept = self.post(replacement_accept_body)
        self.assertIn(replacement_accept.status, (400, 409))
        self.assert_snapshots_equal(before_replacement_accept, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 2)  # one fresh evidence request, no second merge
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            row = database.execute(
                "SELECT result FROM runtime_operation_receipts WHERE id=?",
                (f"work-write-vm-intent:merge:{merge_task['id']}",),
            ).fetchone()
        self.assertEqual(json.loads(row[0])["status"], "unknown")
        self.backend.stop()
        self.backend.start()
        self.assertIn(self.post(replacement_accept_body).status, (400, 409))
        self.assert_snapshots_equal(before_replacement_accept, snapshot(self.backend.state))
        self.assertEqual(len(guest.calls), call_count + 2)


    def test_vm_blank_accept_refuses_before_guest_merge(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python backend uses the in-process fake guest contract")
        self.configure_vm_agents()

        task = self.post({"action": "create", "title": "Blank VM accept", "owner": self.worker_id})
        self.assertEqual(task.status, 200, task.body[:1000])
        guest = FakeLayrSocket(self.backend.vm_state / "guest.sock", self.backend.state / "canvas.sqlite3")
        self.addCleanup(guest.close)
        submission = self.post_as(self.worker_id, {
            "action": "submit", "task_id": task.json()["id"], "version": task.json()["version"],
            "result": "verified result", "checks": "checks passed", "revision": "vm-blank-state",
            "id": "vm-blank-submit",
        })
        self.assertEqual(submission.status, 200, submission.body[:1000])
        self.assertEqual([call["method"] for call in guest.calls], ["line.evidence"])

        # Python's codex_work.WorkMixin validates text_field(result) before merge_result.
        blank_accept = {
            "action": "accept", "task_id": task.json()["id"],
            "version": submission.json()["version"], "result": "\u001c", "id": "vm-blank-accept",
        }
        before = snapshot(self.backend.state)
        response = self.post(blank_accept)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), "Field must not be empty")
        self.assert_snapshots_equal(before, snapshot(self.backend.state))
        self.assertEqual([call["method"] for call in guest.calls], ["line.evidence"])

    def test_vm_nonlead_accept_does_not_reserve_or_send_a_merge(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python backend uses the in-process fake guest contract")
        self.configure_vm_agents()

        task = self.post({"action": "create", "title": "Non-lead VM accept", "owner": self.worker_id})
        self.assertEqual(task.status, 200, task.body[:1000])
        guest = FakeLayrSocket(self.backend.vm_state / "guest.sock", self.backend.state / "canvas.sqlite3")
        self.addCleanup(guest.close)
        submission = self.post_as(self.worker_id, {
            "action": "submit", "task_id": task.json()["id"], "version": task.json()["version"],
            "result": "verified result", "checks": "checks passed", "revision": "vm-nonlead-state",
            "id": "vm-nonlead-submit",
        })
        self.assertEqual(submission.status, 200, submission.body[:1000])

        # Python codex_work.WorkMixin checks isLead and rootId before merge_result.
        accept = {
            "action": "accept", "task_id": task.json()["id"],
            "version": submission.json()["version"], "result": "reviewed", "id": "vm-nonlead-accept",
        }
        before = snapshot(self.backend.state)
        rejected = self.post_as(self.worker_id, accept)
        self.assertEqual(rejected.status, 400, rejected.body[:1000])
        self.assert_snapshots_equal(before, snapshot(self.backend.state))
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
            reservation = database.execute(
                "SELECT 1 FROM runtime_operation_receipts WHERE id=?", (accept["id"],)
            ).fetchone()
        self.assertIsNone(reservation)
        self.assertEqual([call["method"] for call in guest.calls], ["line.evidence"])

        # Reusing the unreserved canonical identity as the actual lead succeeds.
        accepted = self.post(accept)
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        self.assertEqual([call["method"] for call in guest.calls], ["line.evidence", "line.merge"])

    def test_vm_accept_request_identity_is_reserved_before_guest_merge(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python backend uses the in-process fake guest contract")
        self.configure_vm_agents()

        tasks = []
        guest = FakeLayrSocket(self.backend.vm_state / "guest.sock", self.backend.state / "canvas.sqlite3")
        self.addCleanup(guest.close)
        for index in range(2):
            created = self.post({"action": "create", "title": f"VM identity {index}",
                                 "owner": self.worker_id})
            self.assertEqual(created.status, 200, created.body[:1000])
            submitted = self.post_as(self.worker_id, {
                "action": "submit", "task_id": created.json()["id"],
                "version": created.json()["version"], "result": f"result {index}",
                "checks": "checks", "revision": f"vm-identity-{index}",
                "id": f"vm-identity-submit-{index}",
            })
            self.assertEqual(submitted.status, 200, submitted.body[:1000])
            tasks.append((created.json()["id"], submitted.json()["version"]))

        request_id = "vm-accept-shared-request-id"
        first = self.post({"action": "accept", "task_id": tasks[0][0],
                           "version": tasks[0][1], "result": "accept first", "id": request_id})
        self.assertEqual(first.status, 200, first.body[:1000])
        merge_count = sum(call["method"] == "line.merge" for call in guest.calls)
        self.assertEqual(merge_count, 1)

        # Simulate the interval after the durable VM reservation is committed but
        # before the HTTP result receipt is published. The canonical receipt must
        # fence every work mutation that tries to reuse this HTTP id.
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            reservation = {
                "vmAcceptReservation": True, "taskId": tasks[0][0],
                "agentId": self.agent_id, "status": "reserved",
            }
            database.execute(
                "UPDATE runtime_operation_receipts SET result=? WHERE id=?",
                (json.dumps(reservation, separators=(",", ":")), request_id),
            )
        before_reuse = snapshot(self.backend.state)
        reused = self.post({"action": "create", "title": "Request id collision",
                            "owner": self.worker_id, "id": request_id})
        self.assertEqual(reused.status, 400, reused.body[:1000])
        self.assertEqual(reused.json().get("error"), "This request id has different content")
        self.assert_snapshots_equal(before_reuse, snapshot(self.backend.state))
        conflict = self.post({"action": "accept", "task_id": tasks[1][0],
                              "version": tasks[1][1], "result": "accept second", "id": request_id})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(conflict.json().get("error"), "This request id has different content")
        self.assertEqual(sum(call["method"] == "line.merge" for call in guest.calls), merge_count)

    def test_claim_submit_accept_validation_edges_are_no_write(self):
        item = python_action(self.backend, self.agent_id, {"action": "create", "title": "Validation oracle"})
        claim = python_action(self.backend, self.agent_id, {"action": "claim", "task_id": item["id"], "owner": self.worker_id})
        bad_submissions = (
            {"result": "\u001c", "checks": "checks", "revision": "abc1234"},
            {"result": "result", "checks": "\u001c", "revision": "abc1234"},
            {"result": "result", "checks": "checks", "revision": "abc1234", "files": [" " * 4096 + "x"]},
        )
        for index, fields in enumerate(bad_submissions):
            body = {"action": "submit", "task_id": item["id"], "version": claim["version"], "id": f"invalid-submit-{index}", **fields}
            before = snapshot(self.backend.state)
            response = self.post(body)
            self.assertEqual(response.status, 400, response.body[:1000])
            self.assert_snapshots_equal(before, snapshot(self.backend.state))

        review = python_action(self.backend, self.agent_id, {"action": "create", "title": "Blank accept"})
        claim = python_action(self.backend, self.agent_id, {"action": "claim", "task_id": review["id"], "owner": self.agent_id})
        submit = python_action(self.backend, self.agent_id, {
            "action": "submit", "task_id": review["id"], "version": claim["version"],
            "result": "result", "checks": "checks", "revision": "abc1234",
        })
        before = snapshot(self.backend.state)
        response = self.post_as(self.worker_id, {
            "action": "accept", "task_id": review["id"], "version": submit["version"],
            "result": "\u001c", "id": "blank-accept",
        })
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

    def test_claim_submit_accept_stale_versions_match_python_and_replay_conflicts(self):
        task = python_action(self.backend, self.agent_id, {"action": "create", "title": "Stale claim"})
        python_action(self.backend, self.agent_id, {
            "action": "claim", "task_id": task["id"], "owner": self.worker_id,
        })
        body = {"action": "claim", "task_id": task["id"], "version": task["version"],
                "owner": self.worker_id, "id": "stale-claim"}
        expected_error = python_error(self.backend, self.agent_id, body)
        before = snapshot(self.backend.state)
        response = self.post(body)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), expected_error)
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

        submitted = python_action(self.backend, self.agent_id, {"action": "create", "title": "Stale submit"})
        python_action(self.backend, self.agent_id, {
            "action": "claim", "task_id": submitted["id"], "owner": self.worker_id,
        })
        body = {"action": "submit", "task_id": submitted["id"], "version": submitted["version"],
                "result": "result", "checks": "checks", "revision": "abc1234", "id": "stale-submit"}
        expected_error = python_error(self.backend, self.agent_id, body)
        before = snapshot(self.backend.state)
        response = self.post(body)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), expected_error)
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

        reviewed = python_action(self.backend, self.agent_id, {"action": "create", "title": "Stale accept"})
        claim = python_action(self.backend, self.agent_id, {
            "action": "claim", "task_id": reviewed["id"], "owner": self.agent_id,
        })
        submit = python_action(self.backend, self.agent_id, {
            "action": "submit", "task_id": reviewed["id"], "version": claim["version"],
            "result": "result", "checks": "checks", "revision": "abc1234",
        })
        body = {"action": "accept", "task_id": reviewed["id"], "version": submit["version"] - 1,
                "result": "reviewed", "id": "stale-accept"}
        expected_error = python_error(self.backend, self.worker_id, body)
        before = snapshot(self.backend.state)
        response = self.post_as(self.worker_id, body)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), expected_error)
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

    def test_claim_submit_accept_blocked_and_terminal_transitions_match_python(self):
        blocker = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Unaccepted prerequisite", "status": "blocked",
        })
        dependent = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Blocked dependent", "owner": self.worker_id,
            "dependencies": [blocker["id"]],
        })
        claim = {
            "action": "claim", "task_id": dependent["id"], "version": dependent["version"],
            "owner": self.worker_id, "id": "blocked-claim",
        }
        self.assert_python_refusal_without_write(self.agent_id, claim)

        ready = python_action(self.backend, self.agent_id, {"action": "create", "title": "No result yet"})
        self.assert_python_refusal_without_write(self.agent_id, {
            "action": "accept", "task_id": ready["id"], "version": ready["version"],
            "result": "premature", "id": "accept-before-submit",
        })

        owned = python_action(self.backend, self.agent_id, {
            "action": "create", "title": "Terminal submit", "owner": self.agent_id,
        })
        submitted = python_action(self.backend, self.agent_id, {
            "action": "submit", "task_id": owned["id"], "version": owned["version"],
            "result": "result", "checks": "checked", "revision": "abc1234",
        })
        accepted = python_action(self.backend, self.agent_id, {
            "action": "accept", "task_id": owned["id"], "version": submitted["version"],
            "result": "verified",
        })
        self.assert_python_refusal_without_write(self.agent_id, {
            "action": "submit", "task_id": owned["id"], "version": accepted["version"],
            "result": "late result", "checks": "checked", "revision": "abc1234",
            "id": "submit-after-accept",
        })
        self.assert_python_refusal_without_write(self.agent_id, {
            "action": "accept", "task_id": owned["id"], "version": accepted["version"],
            "result": "second decision", "id": "accept-after-accept",
        })

    def assert_python_refusal_without_write(self, agent_id, body):
        expected_error = python_error(self.backend, agent_id, body)
        before = snapshot(self.backend.state)
        response = self.post_as(agent_id, body)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json().get("error"), expected_error)
        self.assert_snapshots_equal(before, snapshot(self.backend.state))

    def test_submit_accept_store_failpoints_and_artifact_replay(self):
        metadata_failpoint = (
            ("event metadata", "CREATE TRIGGER fail_point BEFORE INSERT ON runtime_event_meta BEGIN SELECT RAISE(ABORT,'injected'); END"),
        ) if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else ()
        if metadata_failpoint:
            # Recovery-pending agents intentionally persist queue events without
            # timing metadata; clear that fixture flag so this case reaches the
            # runtime_event_meta store call it is meant to fail.
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                row = database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
                ).fetchone()
                lead = json.loads(row[0])
                lead.pop("moveImportPending", None)
                lead.update(status="running", autoWake=True)
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?",
                    (json.dumps(lead), self.agent_id),
                )
        submit_failures = (
            ("work_review event", "CREATE TRIGGER fail_point BEFORE INSERT ON runtime_events WHEN NEW.kind='work_review' BEGIN SELECT RAISE(ABORT,'injected'); END"),
            *metadata_failpoint,
            ("work update", "CREATE TRIGGER fail_point BEFORE UPDATE ON runtime_work BEGIN SELECT RAISE(ABORT,'injected'); END"),
            ("operation receipt", "CREATE TRIGGER fail_point BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'injected'); END"),
        )
        for label, trigger in submit_failures:
            task = python_action(self.backend, self.agent_id, {"action": "create", "title": f"Submit failpoint {label}"})
            claim = python_action(self.backend, self.agent_id, {"action": "claim", "task_id": task["id"], "owner": self.worker_id})
            body = {"action": "submit", "task_id": task["id"], "version": claim["version"],
                    "result": "result", "checks": "checks", "revision": "abc1234", "id": f"fail-submit-{label}"}
            before = snapshot(self.backend.state)
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                database.execute(trigger)
            failed = self.post(body)
            self.assertNotEqual(failed.status, 200, f"{label}: {failed.body[:1000]}")
            self.assert_snapshots_equal(before, snapshot(self.backend.state), label)
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                database.execute("DROP TRIGGER fail_point")

        accept_failures = (
            ("review event close", "CREATE TRIGGER fail_point BEFORE UPDATE ON runtime_events WHEN OLD.kind='work_review' BEGIN SELECT RAISE(ABORT,'injected'); END"),
            ("decision event", "CREATE TRIGGER fail_point BEFORE INSERT ON runtime_events WHEN NEW.kind='work_decision' BEGIN SELECT RAISE(ABORT,'injected'); END"),
            *metadata_failpoint,
            ("work update", "CREATE TRIGGER fail_point BEFORE UPDATE ON runtime_work BEGIN SELECT RAISE(ABORT,'injected'); END"),
            ("operation receipt", "CREATE TRIGGER fail_point BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'injected'); END"),
        )
        for label, trigger in accept_failures:
            task = python_action(self.backend, self.agent_id, {"action": "create", "title": f"Accept failpoint {label}"})
            claim = python_action(self.backend, self.agent_id, {"action": "claim", "task_id": task["id"], "owner": self.agent_id})
            submit = python_action(self.backend, self.agent_id, {
                "action": "submit", "task_id": task["id"], "version": claim["version"],
                "result": "result", "checks": "checks", "revision": "abc1234",
            })
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                review_id = f"work-result:{submit['results'][-1]['id']}"
                if metadata_failpoint:
                    # Python enqueue marks a paused recovery fixture cancelled;
                    # this failpoint specifically exercises closing a pending review.
                    database.execute(
                        "UPDATE runtime_events SET status='pending' WHERE id=?",
                        (review_id,),
                    )
                review_event = database.execute(
                    "SELECT agent,status FROM runtime_events WHERE id=?",
                    (review_id,),
                ).fetchone()
            if metadata_failpoint:
                self.assertEqual(review_event, (self.agent_id, "pending"))
            body = {"action": "accept", "task_id": task["id"], "version": submit["version"], "result": "verified", "id": f"fail-accept-{label}"}
            before = snapshot(self.backend.state)
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                database.execute(trigger)
            failed = self.post(body)
            self.assertNotEqual(failed.status, 200, f"{label}: {failed.body[:1000]}")
            self.assert_snapshots_equal(before, snapshot(self.backend.state), label)
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                database.execute("DROP TRIGGER fail_point")

        task = python_action(self.backend, self.agent_id, {"action": "create", "title": "Artifact replay"})
        claim = python_action(self.backend, self.agent_id, {"action": "claim", "task_id": task["id"], "owner": self.agent_id})
        task_dir = self.backend.state / "results" / task["id"]
        task_dir.parent.mkdir(parents=True, exist_ok=True)
        task_dir.write_text("block directory")
        body = {"action": "submit", "task_id": task["id"], "version": claim["version"],
                "result": "committed result", "checks": "checks", "revision": "abc1234", "id": "artifact-crash-replay"}
        failed = self.post(body)
        self.assertNotEqual(failed.status, 200, failed.body[:1000])
        committed = snapshot(self.backend.state)
        task_dir.unlink()
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        artifact = Path(replay.json()["results"][-1]["resultFile"])
        self.assertTrue(artifact.is_file())
        self.assert_snapshots_equal(committed, snapshot(self.backend.state))
        conflict = self.post({**body, "result": "changed committed result"})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assert_snapshots_equal(committed, snapshot(self.backend.state))

    def test_create_matches_python_fields_receipt_replay_and_dependency_state(self):
        dependency = self.post({"action": "create", "title": "Accepted dependency"})
        self.assertEqual(dependency.status, 200, dependency.body[:1000])
        dependency_id = dependency.json()["id"]
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (dependency_id,)).fetchone()
            record = json.loads(row[0]); record["status"] = "accepted"
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(record), dependency_id))
        body = {"action": "create", "title": "Blocked create", "description": "Stored once", "status": "blocked", "dependencies": [dependency_id], "id": "work-create-replay"}
        result = self.assert_matches_python(body)
        self.assertEqual(result["dependencies"], [dependency_id])
        self.assertEqual(result["status"], "blocked")
        before = no_write_snapshot(self.backend.state)
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(normal_result(replay.json()), normal_result(result))
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        conflict = self.post({**body, "title": "Changed"})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_claim_submit_accept_and_host_result_artifact(self):
        worker_task = self.post({"action": "create", "title": "Worker submission"})
        self.assertEqual(worker_task.status, 200, worker_task.body[:1000])
        worker_id = worker_task.json()["id"]
        claimed = self.assert_matches_python({
            "action": "claim", "task_id": worker_id, "owner": self.worker_id,
        })
        self.assertEqual(claimed["owner"], self.worker_id)
        submit = {
            "action": "submit", "task_id": worker_id, "version": claimed["version"],
            "result": "Worker result", "checks": "Contract passed", "revision": "abc1234",
            "files": ["src/example.rs"], "id": "work-submit-replay",
        }
        response = self.post_as(self.worker_id, submit)
        self.assertEqual(response.status, 200, response.body[:1000])
        result = response.json()["results"][-1]
        self.assertEqual(result["agent"], self.worker_id)
        self.assertEqual(result["text"], "Worker result")
        self.assertEqual(result["checks"], "Contract passed")
        result_path = Path(result["resultFile"])
        self.assertTrue(result_path.is_file())
        self.assertIn("Worker result", result_path.read_text())
        after_submit = snapshot(self.backend.state)
        replay = self.post_as(self.worker_id, submit)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(normal_result(replay.json()), normal_result(response.json()))
        self.assert_snapshots_equal(after_submit, snapshot(self.backend.state))
        before_archive = snapshot(self.backend.state)
        worker_accept = self.post({"action": "accept", "task_id": worker_id, "result": "Verified"})
        self.assertEqual(worker_accept.status, 200, worker_accept.body[:1000])
        self.assertEqual(worker_accept.json()["status"], "accepted")
        self.assertEqual(worker_accept.json()["archive"]["status"], "kept")
        self.assert_snapshots_differ(before_archive, snapshot(self.backend.state))

        lead_task = self.post({"action": "create", "title": "Lead submission"})
        self.assertEqual(lead_task.status, 200, lead_task.body[:1000])
        lead_id = lead_task.json()["id"]
        lead_claim = self.post({"action": "claim", "task_id": lead_id, "owner": self.agent_id})
        self.assertEqual(lead_claim.status, 200, lead_claim.body[:1000])
        lead_submit = self.post({
            "action": "submit", "task_id": lead_id, "version": lead_claim.json()["version"],
            "result": "Lead result", "checks": "Contract passed", "revision": "abc1234",
            "id": "lead-submit-replay",
        })
        self.assertEqual(lead_submit.status, 200, lead_submit.body[:1000])
        accepted = self.post({
            "action": "accept", "task_id": lead_id, "version": lead_submit.json()["version"],
            "result": "Verified on host", "id": "lead-accept-replay",
        })
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        self.assertEqual(accepted.json()["status"], "accepted")
        self.assertEqual(accepted.json()["archive"]["status"], "skipped")
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(accepted.json()["archivePending"], False)
            vm_task = self.post({"action": "create", "title": "VM submission"})
            self.assertEqual(vm_task.status, 200, vm_task.body[:1000])
            vm_id = vm_task.json()["id"]
            vm_claim = self.post({"action": "claim", "task_id": vm_id, "owner": self.worker_id})
            self.assertEqual(vm_claim.status, 200, vm_claim.body[:1000])
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.worker_id,)).fetchone()
                worker = json.loads(row[0]); worker["executionMode"] = "vm"
                database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(worker), self.worker_id))
            vm_submit = self.post_as(self.worker_id, {
                "action": "submit", "task_id": vm_id, "result": "VM result",
                "checks": "Contract passed", "revision": "abc1234",
            })
            self.assertEqual(vm_submit.status, 502, vm_submit.body[:1000])
            self.assertIn("Layr evidence request failed", vm_submit.json().get("error", ""))
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database:
                row = database.execute(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?",
                    (f"work-write-vm-intent:submission:{vm_id}",),
                ).fetchone()
            self.assertIsNotNone(row)
            self.assertIn(json.loads(row[0]).get("status"), {"not_sent", "unknown"})

    def test_refusals_stale_versions_deleted_agent_and_failure_roll_back_all_tables(self):
        created = self.post({"action": "create", "title": "Refusal target"})
        self.assertEqual(created.status, 200, created.body[:1000])
        body = {"action": "update", "task_id": created.json()["id"], "version": 0.0, "title": "Stale"}
        before = no_write_snapshot(self.backend.state)
        stale = self.post(body)
        self.assertEqual(stale.status, 400, stale.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        assigned = self.assert_matches_python({
            "action": "create", "title": "Assigned", "owner": self.worker_id,
            "id": "assigned-work-create",
        })
        self.assertEqual(assigned["owner"], self.worker_id)
        self.assertEqual(
            self.assert_matches_python({
                "action": "update", "task_id": created.json()["id"],
                "version": 1, "owner": self.worker_id, "id": "assigned-work-update",
            })["owner"],
            self.worker_id,
        )
        cancel = {"action": "cancel", "task_id": created.json()["id"], "reason": " "}
        before = no_write_snapshot(self.backend.state)
        refused = self.post(cancel)
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        cancelled = self.assert_matches_python({"action": "cancel", "task_id": created.json()["id"], "reason": "Retire before replay"})
        self.assertEqual(cancelled["status"], "cancelled")
        recancel = {"action": "cancel", "task_id": created.json()["id"], "reason": " ", "id": "invalid-recancel-reason"}
        before = no_write_snapshot(self.backend.state)
        refused = self.post(recancel)
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (created.json()["id"],)).fetchone()
            work = json.loads(row[0]); work["owner"] = self.worker_id
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(work), created.json()["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-ready-before-failure", self.worker_id, "work_ready", json.dumps({"task":created.json()["id"]}), "pending", 1.0, 0, None, None))
            database.execute("CREATE TRIGGER fail_work_receipt BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'injected'); END")
        failure_body = {"action": "cancel", "task_id": created.json()["id"], "reason": "Roll back", "id": "work-injected-failure"}
        before = no_write_snapshot(self.backend.state)
        failed = self.post(failure_body)
        self.assertNotEqual(failed.status, 200, failed.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("DROP TRIGGER fail_work_receipt")
        deleted_body = {"action": "update", "task_id": created.json()["id"], "title": "No", "id": "deleted-selected-agent"}
        self.assert_matches_python(deleted_body)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent["deletedAt"] = True
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        before = no_write_snapshot(self.backend.state)
        deleted = self.post(deleted_body)
        self.assertEqual(deleted.status, 400, deleted.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent.pop("deletedAt", None); agent.update(concurrency=0, agentMode="single", subagentConcurrencyVersion=2)
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        before = no_write_snapshot(self.backend.state)
        single = self.post({"action": "create", "title": "Blocked worker", "owner": self.worker_id, "status": "blocked", "id": "single-agent-delegation"})
        self.assertEqual(single.status, 400, single.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent.update(concurrency=32, agentMode="multi", subagentConcurrencyVersion=2)
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))

    def test_update_cancel_reject_match_python_and_preserve_durable_events(self):
        dependency = self.assert_matches_python({"action": "create", "title": "Ready dependency", "id": "update-dep"})
        created = self.assert_matches_python({"action": "create", "title": "Before update", "id": "update-target"})
        update = {"action": "update", "task_id": created["id"], "version": 1, "title": "After update", "description": "Edited", "owner": None, "status": "blocked", "dependencies": [dependency["id"]], "id": "work-update-state"}
        updated = self.assert_matches_python(update)
        self.assertEqual((updated["title"], updated["status"], updated["dependencies"]), ("After update", "blocked", [dependency["id"]]))
        replay_body = {"action": "update", "task_id": created["id"], "id": "work-update-replay", "title": "Replay edit"}
        self.assert_matches_python(replay_body)
        before = no_write_snapshot(self.backend.state)
        changed = self.post({**replay_body, "title": "Different edit"})
        self.assertEqual(changed.status, 400, changed.body[:1000]); self.assertEqual(no_write_snapshot(self.backend.state), before)
        task_response = self.post({"action": "create", "title": "Cancel task"})
        review_response = self.post({"action": "create", "title": "Reject task"})
        self.assertEqual(task_response.status, 200, task_response.body[:1000])
        self.assertEqual(review_response.status, 200, review_response.body[:1000])
        task, review = task_response.json(), review_response.json()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            task_row = database.execute("SELECT record FROM runtime_work WHERE id=?", (task["id"],)).fetchone()
            task_record = json.loads(task_row[0]); task_record["owner"] = self.worker_id
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(task_record), task["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("old-work-ready", self.worker_id, "work_ready", json.dumps({"task": task["id"]}), "pending", 1.0, 0, None, None))
            review_row = database.execute("SELECT record FROM runtime_work WHERE id=?", (review["id"],)).fetchone()
            review_record = json.loads(review_row[0]); review_record.update(owner=self.worker_id, status="review", results=[{"id":"fixture-result","agent":self.worker_id,"text":"done","checks":"checked","revision":"abc1234","files":[],"created":1.0,"resultFile":str(self.backend.cwd / "result.md")}])
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(review_record), review["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-result:fixture-result", self.agent_id, "work_review", json.dumps({"task":review["id"],"result":review_record["results"][-1]}), "pending", 1.0, 0, None, None))
        cancel_body = {"action":"cancel","task_id":task["id"],"reason":"No longer needed","id":"work-cancel-replay"}
        cancelled = self.assert_matches_python(cancel_body)
        self.assertEqual(cancelled["status"], "cancelled")
        self.assert_matches_python(cancel_body)
        changed_key = {**cancel_body,"reason":"Different reason"}
        before = no_write_snapshot(self.backend.state)
        conflict = self.post(changed_key)
        self.assertEqual(conflict.status, 400, conflict.body[:1000]); self.assertEqual(no_write_snapshot(self.backend.state), before)
        reject_body = {"action":"reject","task_id":review["id"],"result":"Add more checks","id":"work-reject-replay"}
        rejected = self.assert_matches_python(reject_body)
        self.assertEqual(rejected["status"], "ready")
        self.assert_matches_python(reject_body)
        pause_agent(self.backend, self.agent_id)
        root_response = self.post({"action": "create", "title": "Lead-owned result"})
        self.assertEqual(root_response.status, 200, root_response.body[:1000])
        root_review = root_response.json()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (root_review["id"],)).fetchone()
            record = json.loads(row[0]); record.update(owner=self.agent_id, status="review", results=[{"id":"lead-result","agent":self.agent_id,"text":"done","checks":"checked","revision":"abc1234","files":[],"created":1.0,"resultFile":str(self.backend.cwd / "lead-result.md")}])
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(record), root_review["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-result:lead-result", self.agent_id, "work_review", json.dumps({"task":root_review["id"],"result":record["results"][-1]}), "pending", 1.0, 0, None, None))
        lead_reject = {"action": "reject", "task_id": root_review["id"], "result": "Add lead details", "id": "reject-lead-owned"}
        self.assert_matches_python(lead_reject)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            self.assertIsNotNone(database.execute("SELECT 1 FROM runtime_events WHERE id=?", (f"work-decision:{root_review['id']}:1",)).fetchone())
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            events = {row[0]: row[1:] for row in database.execute("SELECT id,agent,kind,text,status FROM runtime_events WHERE id IN (?,?,?)", ("old-work-ready", f"work-decision:{task['id']}:cancel", f"work-decision:{review['id']}:1"))}
        self.assertEqual(events["old-work-ready"][3], "stored_only")
        self.assertEqual(events[f"work-decision:{task['id']}:cancel"][3], "pending")
        self.assertEqual(events[f"work-decision:{review['id']}:1"][3], "pending")


if __name__ == "__main__":
    unittest.main()
