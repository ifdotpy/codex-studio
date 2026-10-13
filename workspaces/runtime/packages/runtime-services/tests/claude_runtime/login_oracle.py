"""Native-free receipt and validation oracle, using LoginManager's real writers."""
import sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import copy
import json
import tempfile
import sqlite3
import threading
from types import SimpleNamespace
sys.path.insert(0,str(Path(__file__).resolve().parents[6]/"workspaces/runtime/apps/server/src"))
from codex_claude_login import LoginManager, verification_url

rid="e0166868-592a-4dca-8994-04d34a730af5"
rows=[]
for state in ("starting","pending","ready","error","cancelled"):
    for submitted in (False,True):
        for ready in (False,True):
            with tempfile.TemporaryDirectory() as root:
                manager=LoginManager(SimpleNamespace(root=root))
                writes=[]
                process=SimpleNamespace(poll=lambda:None,stdin=SimpleNamespace(write=writes.append,flush=lambda:None))
                job=dict(receipt=dict(requestId=rid,accountKey="account",status=state,email="fixture@example.test"),codeSubmitted=submitted)
                if ready:job.update(process=process);job["receipt"]["verificationUrl"]="https://claude.com/cai/oauth/authorize?test=1"
                manager.jobs[rid]=job
                manager._save(job)
                before=json.loads((manager.directory/(rid+".json")).read_text())
                error=None
                try:manager.code(rid,"fixture-code")
                except ValueError as e:error=str(e)
                after=json.loads((manager.directory/(rid+".json")).read_text())
                rows.append(dict(name=f"{state}-{submitted}-{ready}",input=before,ready=ready,error=error,expected=after,writes=len(writes)))
urls=["https://claude.com/cai/oauth/authorize", "https://claude.com/cai/oauth/authorize?code=x", "http://claude.com/cai/oauth/authorize", "https://CLAUDE.com/cai/oauth/authorize", "https://claude.com:443/cai/oauth/authorize", "https://claude.com/cai/oauth/authorize#x", "https://claude.com.evil/cai/oauth/authorize", "https://user@claude.com/cai/oauth/authorize", "https://claude.com/cai/oauth/authorize/", "https://claude.com/a/../cai/oauth/authorize"]
codes=["", "abc", "a b", "\t", "\n", "\u200b", "\u00a0", "café😀", "\u2028", "x"*4096,"x"*4097]
urls.extend(["https://claude.com/cai/oauth/authorize#", "HTTPS://claude.com/cai/oauth/authorize", " https://claude.com/cai/oauth/authorize", "https://claude.com/cai/oauth/authorize;fixture=1", "https://claude.com/cai/\nauthorize"])
chats=[]
for change in ({},{"provider":"codex"},{"accountKey":"foreign"},{"deletedAt":12},{"inFlight":True},{"activeTools":["tool"]},{"workspaceOperation":"branch"},{"status":"paused"},{"error":{"message":"different error"}},{"error":"Failed to authenticate"},{"error":{"message":"profile’s account changed"}}):
    agent=dict(id="chat",accountKey="account",provider="claude",status="failed",error={"message":"OAuth session expired"},nativeStatus={"type":"error"},extra={"preserved":True});agent.update(change)
    before=copy.deepcopy(agent)
    with tempfile.TemporaryDirectory() as root:
        db=sqlite3.connect(":memory:")
        def put(db,table,value):agent.clear();agent.update(copy.deepcopy(value))
        runtime=SimpleNamespace(root=root,lock=threading.RLock(),db=lambda:db,account_agents=lambda db,key:[copy.deepcopy(agent)],put=put)
        manager=LoginManager(runtime)
        manager._refresh_chats(dict(receipt=dict(requestId=rid,accountKey="account",status="ready",email="fixture@example.test")))
        chats.append(dict(input=before,expected=copy.deepcopy(agent)));db.close()
print(json.dumps(dict(rows=rows,chats=chats,urls=[dict(value=u,expected=verification_url(u)) for u in urls],codes=[dict(value=c,expected=bool(1<=len(c)<=4096 and c.isprintable())) for c in codes])))
