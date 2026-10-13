"""Guarded workspace gate oracle and real Python state producer.

workspace_blockers is isolated on an in-memory database. prepare_locked and
prepare_worker_worktree cannot be pure oracles: they connect providers, mutate
Git, and start futures. Rust's explicit unsupported modes and evidence-based
provision recovery are intentional additions, covered by Rust decision tests.
Reservation overlap (ancestor/descendant, including the planned destination) is
intentionally stricter than Python's same-directory gate. Publication witnesses
execute Python's prepare_worker_worktree and capture_checkpoint with Git effects
stubbed at the boundary; Rust's actual Git effects are checked separately.
"""
import sys
from oracle_guard import install_guard
GUARD = install_guard(block_processes=True)
import contextlib
import concurrent.futures
import json
import os
import pathlib
import sqlite3
import threading
from unittest.mock import PropertyMock, patch
SERVER = pathlib.Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(SERVER))
import codex_runtime
import codex_accounts
from codex_multi_server import MultiServerService


class QuietRuntime(codex_runtime.Runtime):
    def schedule(self):
        pass

    def connect(self, *args, **kwargs):
        raise AssertionError("provider connection forbidden in workspace oracle")

    connect_agent = connect

    def start_review_archives(self):
        pass

    def resume_read_only_image_bases(self):
        pass



def blockers(cases):
    from codex_workspace import WorkspaceMixin
    runtime = WorkspaceMixin()
    results = []
    for case in cases:
        with sqlite3.connect(":memory:") as db:
            db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
            db.execute("CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY, record TEXT)")
            for actor in case["agents"]:
                db.execute("INSERT INTO runtime_agents VALUES(?,?)", (actor["id"], json.dumps(actor)))
            for operation in case["operations"]:
                db.execute("INSERT INTO runtime_workspace_operations VALUES(?,?)", (operation["id"], json.dumps(operation)))
            results.append(runtime.workspace_blockers(db, case["actor"]))
    return results


def seed(root, cwd):
    with patch.object(codex_accounts, "codex_home", lambda: pathlib.Path(os.environ["CODEX_HOME"])), \
         patch.object(codex_accounts, "auth_metadata", lambda *_: {"status":"signedOut","accountId":None,"email":None,"plan":None}), \
         patch.object(concurrent.futures.ThreadPoolExecutor, "submit", lambda *_a, **_kw: concurrent.futures.Future()), \
         patch.object(threading.Thread, "start", lambda self: None), \
         patch.object(threading.Thread, "join", lambda self, *a, **k: None), \
         patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None), \
         patch.object(codex_runtime, "git_toplevel", lambda *_a, **_kw: None), \
         patch.object(MultiServerService, "local_server_id", new_callable=PropertyMock, return_value="workspace-fixture-server"):
        runtime = QuietRuntime(pathlib.Path(root), lambda *_a, **_kw: None)
        try:
            actor = runtime.create({"name":"workspace fixture", "cwd":root, "prompt":"fixture"}, defer=True)
            actor.update(cwd=cwd, worktree=True, worktreeReady=False, workspaceMode="worktree", autoWake=True)
            with runtime.lock, runtime.db() as db:
                runtime.put(db, "agents", actor)
                runtime.enqueue(db, actor, "user", "queued fixture", key="workspace-input")
            return actor
        finally:
            runtime.close()


def publication(request):
    from codex_workspace import WorkspaceMixin
    import codex_worktree_creation

    class Witness(WorkspaceMixin):
        def __init__(self):
            self.root = pathlib.Path(request["repo"])
            self.lock = threading.RLock()
            self.connection = sqlite3.connect(":memory:")
            self.connection.execute("CREATE TABLE runtime_items(id TEXT, agent TEXT, record TEXT)")
            self.connection.execute("CREATE TABLE runtime_checkpoints(id TEXT, record TEXT)")
            self.current = dict(request["actor"])
            self.checkpoint = None
            for item in request["items"]:
                self.connection.execute("INSERT INTO runtime_items VALUES(?,?,?)",
                                        (item["id"], self.current["id"], json.dumps(item)))
            if request["parent"]:
                parent = request["parent"]
                self.connection.execute("INSERT INTO runtime_checkpoints VALUES(?,?)", (parent["id"], json.dumps(parent)))

        def db(self):
            return contextlib.nullcontext(self.connection)

        def agent(self, *_args):
            return dict(self.current)

        checked_actor_in_own_db = agent

        def put(self, db, kind, record):
            if kind == "agents":
                self.current = dict(record)
            else:
                assert kind == "checkpoints"
                self.checkpoint = dict(record)

        def git(self, actor, args, *_args):
            if args[0] == "commit-tree":
                return request["commit"].encode()
            if args[0] == "update-ref":
                return b""
            if args == ["rev-parse", "HEAD^{tree}"]:
                return request["tree"].encode()
            if args == ["rev-parse", "--git-path", "hooks/post-checkout"]:
                return str(self.root / "absent-hook").encode()
            raise AssertionError(args)

    runtime = Witness()
    try:
        with patch.object(codex_runtime.subprocess, "check_output", return_value=b""), \
             patch.object(codex_worktree_creation, "create_worker_worktree", return_value=False):
            codex_runtime.Runtime.prepare_worker_worktree(runtime, dict(runtime.current), request["repo"])
        return {"actor": runtime.current, "checkpoint": runtime.checkpoint}
    finally:
        runtime.connection.close()


if __name__ == "__main__":
    request = json.load(sys.stdin)
    if request["command"] == "seed":
        result = seed(request["root"], request["cwd"])
    elif request["command"] == "publication":
        result = publication(request)
    else:
        result = blockers(request["cases"])
    print(json.dumps(result))
