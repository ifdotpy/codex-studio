"""Actual Runtime.set_account with injected account and store ports, no Runtime instance."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import threading
from contextlib import nullcontext
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'turn_lifecycle'))
from oracle_guard import install_guard
install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / 'workspaces/runtime/apps/server/src'))
from codex_runtime import Runtime

class Accounts:
    def __init__(self, value): self.value = value
    def get(self, key): return self.value
class Ports:
    worker_defaults = staticmethod(Runtime.worker_defaults)
    def __init__(self, agent, account, empty):
        self.value = agent
        self.accounts = Accounts(account)
        self.empty = empty
        self.lock = threading.RLock()
    def db(self): return nullcontext(None)
    def agent(self, key, db): return self.value
    def empty_lead(self, db, agent): return self.empty
    def usage_resume_cancel(self, db, agent, reason):
        assert not agent.get('usageResume'), 'this corpus has no resume dependency'
    def ensure_project(self, directory, account, db): pass
    def put(self, db, table, agent): self.value = agent

cases=[]
with tempfile.TemporaryDirectory(prefix='selection-oracle-') as root:
    new = Path(root) / 'new'; new.mkdir()
    for name, target, changes, account, empty, directory in [
        ('same', 'source', {}, {}, False, root),
        ('switch', 'target', {}, {}, True, root),
        ('folder', 'source', {'projectFolder':'old','projectFolderRevision':3}, {}, True, str(new)),
        ('claude', 'target', {'pendingSettings':{'model':'old'},'pendingSettingsAccountKey':'source'}, {'provider':'claude'}, True, root),
        ('codex', 'target', {'provider':'claude','model':'opus'}, {'provider':'codex'}, True, root),
        ('fixed', 'target', {}, {}, False, root),
        ('inflight', 'target', {'inFlight':True}, {}, True, root),
        ('deleted', 'target', {}, {'deleted':True}, True, root),
        ('disconnected', 'target', {}, {'disconnected':True}, True, root),
        ('same-deleted', 'source', {}, {'deleted':True}, False, root),
        ('preserve-execution', 'target', {'executionSettingsAccountKey':'earlier','workerDefaults':{'model':'chosen','daybreakEnabled':True}}, {}, True, root),
    ]:
        agent=dict(id='lead',rootId='lead',epoch=0,isLead=True,status='idle',accountKey='source',cwd=root,model='old',provider='codex')
        agent.update(changes)
        raw=json.dumps(agent)
        ports=Ports(copy.deepcopy(agent), account, empty)
        try:
            result=Runtime.set_account(ports,'lead',target,directory)
            error=None
        except ValueError as exc:
            result=None; error=str(exc)
        cases.append(dict(name=name,raw=raw,target=target,account=account,empty=empty,directory=directory,result=result,error=error))
print(json.dumps(cases))
