"""Real Python `orchestration_servers` calls over a recorded paired transport.

The Python Runtime handles each call through `Runtime.dynamic`, the production
tool path. A fake transport records every envelope and answers it with the
reply rules in `REPLIES`; the Rust test answers with the same rules. No
provider, network or command runs.
"""
import os
from pathlib import Path
import sys

HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parent.parent / "turn_lifecycle"))
from oracle_guard import install_guard
_GUARD = install_guard()
SERVER = HERE.parents[4] / "apps/server"
if "--isolated" not in sys.argv:
    sys.path.insert(0, str(SERVER / "tests"))
    from http_backend_harness import BackendProcess
    scratch = Path(sys.argv[1]).resolve(strict=True)
    owner = BackendProcess(scratch)
    binary = owner._directory("disabled-provider") / "codex"
    binary.write_text("#!/bin/sh\nexit 99\n")
    binary.chmod(0o700)
    owner._provider_executable = binary
    os.execve(sys.executable, [sys.executable, "-B", str(HERE), "--isolated", str(scratch)], owner._environment())

import concurrent.futures
import copy
import json
import shutil
import sqlite3
import subprocess
import threading
from contextlib import ExitStack
from unittest.mock import patch

sys.path.insert(0, str(SERVER / "src"))
import codex_accounts
import codex_runtime
from codex_runtime import Runtime
from codex_multi_server_orchestration import MultiServerService

PEER = "peer-server"


class Quiet(Runtime):
    def schedule(self):
        pass
    def connect(self, *args, **kwargs):
        raise AssertionError("provider forbidden")
    connect_agent = connect
    def server_for(self, *args, **kwargs):
        return None
    def connection_current(self, *args, **kwargs):
        return True
    def reply(self, message, *args, **kwargs):
        self.replies.append(message)
    def start_review_archives(self):
        pass
    def resume_read_only_image_bases(self):
        pass


stack = ExitStack()
_connect = codex_runtime.sqlite_connect
def fixture_connect(*args, **kwargs):
    connection = _connect(*args, **kwargs)
    connection.execute("PRAGMA synchronous=OFF")
    return connection
stack.enter_context(patch.object(codex_runtime, "sqlite_connect", fixture_connect))
for substitution in (
    patch.object(codex_accounts, "codex_home", lambda: Path(os.environ["CODEX_HOME"])),
    patch.object(codex_accounts, "auth_metadata", lambda *_: {"status": "signedOut", "accountId": None, "email": None, "plan": None}),
    patch.object(concurrent.futures.ThreadPoolExecutor, "submit", lambda *_a, **_kw: concurrent.futures.Future()),
    patch.object(threading.Thread, "start", lambda self: None),
    patch.object(threading.Thread, "join", lambda self, *a, **k: None),
    patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None),
):
    stack.enter_context(substitution)

scratch = Path(sys.argv[-1])
root = scratch / "template"
root.mkdir()
project_path = scratch / "project"
project_path.mkdir()
git_environment = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
subprocess.run(["git", "-C", str(project_path), "init", "-q"], check=True, env=git_environment)
subprocess.run(["git", "-C", str(project_path), "remote", "add", "origin", "https://Example.test/team/repo.git"],
               check=True, env=git_environment)

runtime = Quiet(root, lambda *_args, **_kwargs: None)
runtime.replies = []
lead = runtime.create({"name": "Servers lead", "cwd": str(root), "prompt": "fixture"}, defer=True)
worker = runtime.create({"name": "Servers worker", "cwd": str(root), "prompt": "fixture"}, defer=True)
worker.update(parentId=lead["id"], rootId=lead["id"], isLead=False)
for index, actor in enumerate([lead, worker]):
    actor.update(autoWake=True, status="running", accountKey="default", threadId=f"thread-{index}",
                 turnId=f"turn-{index}", epoch=3, turnEpoch=3)
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", actor)
project = runtime.projects({"action": "register", "path": str(project_path), "name": "Fixture project"})
project_id = project.get("id") or str(project_path.resolve())
peer_record = {"id": PEER, "clientId": PEER, "serverId": PEER, "label": "Peer", "kind": "server",
               "origin": "https://peer.fixture.ts.net", "publicKey": "fixture-public-key",
               "tailscaleUser": "owner@example.test", "status": "paired", "created": 10,
               "lastAccess": None, "revoked": None}
with runtime.lock, runtime.db() as db:
    db.execute("INSERT OR REPLACE INTO runtime_access_clients(id,record) VALUES(?,?)", (PEER, json.dumps(peer_record)))
local_server = runtime.paired_access().local_server_id
# Settle the server aliases that the server list writes on its first read.
runtime.paired_access().servers()
runtime.close()

python_root = scratch / "python"
shutil.copytree(root, python_root)
managed = Quiet(python_root, lambda *_args, **_kwargs: None)
managed.replies = []
with managed.lock, managed.db() as db:
    for actor in [lead, worker]:
        managed.put(db, "agents", actor)
baseline = scratch / "baseline.sqlite3"
with managed.db() as db:
    copy_target = sqlite3.connect(baseline)
    db.backup(copy_target)
    copy_target.close()

FAR = 4102444800.0


def reply(envelope):
    """The answer of the paired fixture server. Rust mirrors these rules."""
    key, action, payload = envelope["requestId"], envelope["action"], envelope["payload"]
    if action == "projects":
        return {"requestId": key, "outcome": "applied", "value": {"items": [
            {"id": "/peer/project", "path": "/peer/project", "name": "Peer project", "created": 1.0}]}}
    if action == "git":
        return {"requestId": key, "outcome": "applied", "value": {"exitCode": 0, "output": " M a.txt\n", "truncated": False}}
    if action == "folders":
        return {"requestId": key, "outcome": "not_applied", "error": "This directory is unavailable"}
    if action == "exec":
        return {"requestId": key, "outcome": "applied", "value": {
            "handle": key, "serverId": PEER, "status": "running", "startedAt": 10.0, "finishedAt": None,
            "exitCode": None, "signal": None, "duration": None, "timedOut": False,
            "outputLimit": 262144, "outputExpiresAt": FAR}}
    if action == "exec_read":
        return {"requestId": key, "outcome": "applied", "value": {
            "handle": payload["handle"], "status": "completed", "exitCode": 0, "stdout": "ok\n",
            "stdoutOffset": 0, "stdoutNextOffset": 3, "outputExpiresAt": FAR}}
    if action == "add_location":
        return {"requestId": key, "outcome": "applied", "value": {
            "serverId": PEER, "path": payload["path"], "canonicalPath": payload["path"],
            "projectId": payload["path"], "gitOrigin": None, "homeProjectId": payload["project"],
            "requestId": key}}
    if action == "remove_location":
        return {"requestId": key, "outcome": "applied", "value": {"removed": True, "serverId": PEER}}
    if action == "location_matches":
        return {"requestId": key, "outcome": "applied", "value": {"matches": [
            {"projectId": "/peer/project", "path": "/peer/project", "name": "project"}]}}
    raise AssertionError("unexpected fixture action " + action)


class RecordingTransport:
    def __init__(self):
        self.sent = []
    @property
    def local_server_id(self):
        return local_server
    def servers(self):
        return [row for row in managed.paired_access().servers() if row.get("status") == "paired"]
    def request(self, server, envelope, *, timeout):
        assert server == PEER, server
        self.sent.append({"server": server, "envelope": copy.deepcopy(envelope)})
        return reply(copy.deepcopy(envelope))


transport = RecordingTransport()
managed._cross_server_service = MultiServerService(managed, transport)

cases = []


def call(name, actor, arguments):
    message = {"id": f"rpc-{name}", "method": "item/tool/call", "params": {
        "threadId": actor["threadId"], "turnId": actor["turnId"], "callId": f"call-{name}",
        "tool": "orchestration_servers", "arguments": arguments}}
    before = len(transport.sent)
    with patch("time.time", return_value=20.0):
        managed.dynamic(message, "default", "connection")
    result = managed.replies[-1]["result"]
    text = result["contentItems"][0]["text"]
    try:
        value = json.loads(text)
    except ValueError:
        value = text
    cases.append({"name": name, "actor": actor["id"], "callId": message["params"]["callId"],
                  "arguments": arguments, "success": result["success"], "value": value,
                  "serverOutputExpiresAt": result.get("serverOutputExpiresAt"),
                  "sent": transport.sent[before:]})
    return value


call("list", lead, {"action": "list"})
call("worker-refused", worker, {"action": "list"})
call("unknown-action", lead, {"action": "nope", "server": PEER})
call("unpaired-server", lead, {"action": "git", "server": "other-server", "cwd": "/r", "argv": ["status", "--porcelain=v1"]})
call("projects", lead, {"action": "projects", "server": PEER})
git = call("git", lead, {"action": "git", "server": PEER, "cwd": "/peer/repo",
                         "argv": ["status", "--porcelain=v1"], "request_id": "git-read-1"})
call("folders-refused", lead, {"action": "folders", "server": PEER, "cwd": "/missing"})
call("receipt", lead, {"action": "receipt", "server": PEER, "request_id": git["requestId"]})
call("receipt-unknown", lead, {"action": "receipt", "server": PEER, "request_id": "no-such-request"})
started = call("exec", lead, {"action": "exec", "server": PEER, "cwd": "/peer/repo", "command": ["true"], "timeout": 30})
call("exec-read", lead, {"action": "exec_read", "server": PEER, "handle": started["requestId"]})
call("exec-foreign-handle", lead, {"action": "exec_read", "server": PEER, "handle": "not-a-handle"})
call("exec-relative-cwd", lead, {"action": "exec", "server": PEER, "cwd": "relative", "command": ["true"]})
call("exec-bad-timeout", lead, {"action": "exec", "server": PEER, "cwd": "/peer/repo", "command": ["true"], "timeout": 0})
call("location-matches", lead, {"action": "location_matches", "project": project_id, "server": PEER})
call("add-location", lead, {"action": "add_location", "project": project_id, "server": PEER, "path": "/peer/project"})
with managed.read_db() as db:
    added = json.loads(db.execute("SELECT record FROM runtime_projects WHERE id=?", (project_id,)).fetchone()[0])
call("add-location-again", lead, {"action": "add_location", "project": project_id, "server": PEER, "path": "/peer/other"})
call("remove-location", lead, {"action": "remove_location", "project": project_id, "server": PEER})
with managed.read_db() as db:
    removed = json.loads(db.execute("SELECT record FROM runtime_projects WHERE id=?", (project_id,)).fetchone()[0])
managed.close()
shutil.copy2(baseline, root / "canvas.sqlite3")
for suffix in ("-wal", "-shm"):
    (root / ("canvas.sqlite3" + suffix)).unlink(missing_ok=True)
print(json.dumps({"template": str(root), "localServer": local_server, "peer": PEER,
                  "lead": lead, "worker": worker, "projectId": project_id,
                  "projectAfterAdd": added, "projectAfterRemove": removed, "cases": cases},
                 ensure_ascii=False))
