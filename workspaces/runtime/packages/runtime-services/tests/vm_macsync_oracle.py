import contextlib
import json
import sqlite3
import sys
import threading
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))

from codex_linux_vm import Client
from codex_runtime import Runtime
import codex_linux_workspaces
import codex_linux_vm_share
import codex_vm_mac_sync


class FakeClient:
    def __init__(self, state_dir):
        self.state_dir = state_dir
        self.calls = []

    def call(self, method, params, *, request_id=None, timeout=5):
        self.calls.append({"method": method, "params": params, "requestId": request_id})
        if method == "sync.mac.head":
            return {"mainStateId": "main-1"}
        if method == "sync.mac.read":
            return {"kind": "file", "mode": 420, "data": "Z3Vlc3QgdmVyc2lvbg==", "eof": True}
        if method == "sync.mac.handoff":
            return {"line": "mac-conflict", "stateId": "handoff-1", "mainStateId": "main-1"}
        return {"ok": True}


def make_runtime(state_dir, client):
    path = state_dir / "runtime.sqlite"
    with sqlite3.connect(path) as db:
        db.executescript("""
            CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
                created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
            CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        """)
    actor = {"id": "lead", "rootId": "lead", "epoch": 0, "autoWake": True,
             "deletedAt": None, "accountKey": "default", "status": "running"}
    runtime = Runtime.__new__(Runtime)
    runtime._linux_vm_client = client
    runtime.lock = threading.RLock()

    @contextlib.contextmanager
    def db_scope():
        db = sqlite3.connect(path)
        db.row_factory = sqlite3.Row
        try:
            yield db
            db.commit()
        except BaseException:
            db.rollback()
            raise
        finally:
            db.close()

    runtime.db = db_scope
    runtime.checked_actor = lambda _db, _key: actor
    runtime.agent = lambda _key, _db: actor
    runtime.records = lambda _db, _kind: [{"id": "lead", "isLead": True,
        "layrProjectId": "p1", "layrLine": "main", "status": "running", "created": 1}]
    runtime.workspace_blockers = lambda _db, _actor: []

    def enqueue(db, _actor, kind, text, key):
        db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                   (key, "lead", kind, text, "pending", 1.0, 0, None, None))
        return key

    runtime.enqueue = enqueue
    return runtime, path


def main(state_dir, choice):
    state_dir = Path(state_dir)
    vm_dir = state_dir / "linux-vm"
    vm = Client(state_dir=vm_dir)
    settings_read = vm.get_settings()
    settings_set = vm.set_settings({"cpus": 1, "memoryBytes": 4294967296,
                                    "systemDiskBytes": 17179869184,
                                    "dataDiskBytes": 137438953472})
    client = FakeClient(state_dir)
    runtime, database = make_runtime(state_dir, client)
    codex_linux_workspaces.client = lambda _runtime: client
    codex_linux_vm_share.upload_archive = lambda *_args, **_kwargs: None
    codex_vm_mac_sync._refresh = lambda _runtime: None
    configured = codex_vm_mac_sync.configure(runtime, "p1", enabled=False)
    resolved = codex_vm_mac_sync.resolve(runtime, "p1", "file.txt", choice)
    with sqlite3.connect(database) as db:
        events = [dict(zip(("id", "agent", "kind", "text", "status", "created", "epoch", "turnId", "error"), row))
                  for row in db.execute("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events")]
        metadata = {row[0]: json.loads(row[1]) for row in db.execute("SELECT id,record FROM runtime_event_meta")}
    return {"settingsRead": settings_read, "settingsSet": settings_set,
            "configured": configured, "resolved": resolved, "events": events,
            "eventMetadata": metadata, "guestCalls": client.calls}


if __name__ == "__main__":
    print(json.dumps(main(Path(sys.argv[1]), sys.argv[2]), sort_keys=True))
