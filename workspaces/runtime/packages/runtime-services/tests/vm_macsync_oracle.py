import contextlib
import json
import sqlite3
import sys
import threading
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))

from codex_linux_vm import Client, LinuxVMError
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
             "deletedAt": None, "accountKey": "default", "status": "running", "inFlight": True}
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
    runtime._stage_event_resources = lambda *_args, **_kwargs: None
    runtime.rule_event = lambda *_args, **_kwargs: None
    runtime.changed = threading.Event()
    runtime.closed = True
    return runtime, path


def main(state_dir, choice):
    state_dir = Path(state_dir)
    vm_dir = state_dir / "linux-vm"
    vm = Client(state_dir=vm_dir)
    if choice == "memory-ceiling":
        before = (vm_dir / "settings.json").read_text()
        try:
            with patch("codex_linux_vm.platform.system", return_value="Darwin"), \
                    patch("codex_linux_vm._run", return_value=str(16 * 1024 ** 3)):
                vm.set_settings({"cpus": 1, "memoryBytes": 9 * 1024 ** 3,
                                 "systemDiskBytes": 16 * 1024 ** 3,
                                 "dataDiskBytes": 128 * 1024 ** 3})
        except LinuxVMError as error:
            return {"error": str(error), "settingsBefore": json.loads(before),
                    "settingsAfter": json.loads((vm_dir / "settings.json").read_text())}
        raise AssertionError("Python accepted settings above its Darwin memory ceiling")
    if choice == "write-space-preflight":
        preflights = []
        for method in ("project.import", "line.branch", "upload.begin", "upload.commit", "sync.push"):
            with patch("codex_linux_vm.shutil.disk_usage", return_value=SimpleNamespace(free=0)):
                with patch.object(vm, "_channel") as channel:
                    try:
                        list(vm.stream(method, {"agentId": "agent-1"}, request_id=f"request-{method}"))
                    except LinuxVMError as error:
                        preflights.append({"method": method, "error": str(error),
                                           "channelOpened": channel.called})
                    else:
                        raise AssertionError(f"Python accepted {method} below its free-space floor")
        return {"preflights": preflights}
    settings_read = vm.get_settings()
    # Host memory is an input, not a subprocess allowed by the guarded oracle.
    with patch("codex_linux_vm._run", return_value=str(16 * 1024 ** 3)):
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
