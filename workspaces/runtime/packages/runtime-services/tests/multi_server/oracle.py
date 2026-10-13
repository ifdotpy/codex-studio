"""Real Python multi-server writers and decisions, with injected owner observations.

No Runtime is constructed. Keys are generated for this disposable fixture, and
all network calls are forbidden by the shared oracle guard.
"""
import base64
from contextlib import closing, contextmanager, redirect_stdout
import copy
import json
import os
from pathlib import Path
import sqlite3
import sys
import threading
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
guard = install_guard()
with redirect_stdout(sys.stderr):
    guard.self_test()
root = Path(sys.argv[1])
source = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(source))
import codex_multi_server as access
from codex_multi_server_crypto import CryptoProcess
from codex_server_discovery import ServerDiscovery
import codex_multi_server_orchestration as orchestration

NOW = int(os.environ.get("MULTI_SERVER_ORACLE_NOW", "1700000000"))
OWNER = "owner@example.test"
root.mkdir(parents=True, exist_ok=True)
catalog = json.loads((source.parent / "tests/fixtures/runtime-state-schema.json").read_text())
for name in ("canvas.sqlite3", "analytics.sqlite3"):
    with sqlite3.connect(":memory:") as db:
        entries = sorted(catalog[name]["sqlite_master"], key=lambda e: {"table": 0,"index": 1,"view": 2}.get(e["type"],3))
        for entry in entries:
            if entry.get("sql") and not entry["name"].startswith("sqlite_"):
                if not db.execute("SELECT 1 FROM sqlite_master WHERE name=?", (entry["name"],)).fetchone():
                    db.execute(entry["sql"])
        with closing(sqlite3.connect(root / name)) as destination:
            db.backup(destination)
            destination.execute("PRAGMA journal_mode=WAL")

class FixtureStore:
    def __init__(self):
        self.root = root
        self.lock = threading.RLock()
        self.changed = threading.Event()
    @contextmanager
    def db(self):
        with closing(sqlite3.connect(root / "canvas.sqlite3")) as db:
            db.row_factory = sqlite3.Row
            with db:
                yield db
    read_db = db

class Headers:
    def __init__(self, pairs): self.pairs = pairs
    def get_all(self, name, default=None):
        values = [v for k,v in self.pairs if k.lower()==name.lower()]
        return values or (default if default is not None else [])
    def get(self, name, default=None): return next(iter(self.get_all(name)),default)

runtime=FixtureStore()
crypto=CryptoProcess()
keys=crypto.call("generate")
service=access.MultiServerService(runtime)
service._identity={"serverId":"local-server","label":"Fixture server",**keys}
service.public_origin="https://local.fixture.ts.net"
service._login=lambda headers=None: OWNER
service.discovery()

with patch.object(access.time,"time",return_value=NOW):
    invitation=service.create_invite({"requestId":"fixture-invite"})["invitation"]
    peer_body={"protocol":1,"inviteId":invitation["inviteId"],"token":invitation["token"],"clientId":"peer-server","requestId":"pair-peer","label":"Peer","kind":"server","publicKey":keys["publicKey"],"origin":"https://peer.fixture.ts.net"}
    raw=access._json(peer_body).encode()
    service.reserve("peer-server","pair-peer","POST",access.PAIR_PATH,raw)
    peer_reply=service.pair({"pairing":True,"clientId":"peer-server","requestId":"pair-peer","tailscaleUser":OWNER},peer_body)
    invitation=service.create_invite({"requestId":"second-invite"})["invitation"]
    with runtime.db() as db:
        client_raw=db.execute("SELECT record FROM runtime_access_clients WHERE id='peer-server'").fetchone()[0]
        invite_raw=db.execute("SELECT record FROM runtime_access_invites WHERE id=?",(invitation["inviteId"],)).fetchone()[0]

status={"BackendState":"Running","Self":{"UserID":1,"DNSName":"local.fixture.ts.net.","Tags":[]},"User":{"1":{"LoginName":OWNER}},"Peer":{"peer":{"UserID":1,"DNSName":"peer.fixture.ts.net.","Online":True,"Tags":[]},"foreign":{"UserID":2,"DNSName":"foreign.fixture.ts.net.","Online":True}}}
whois={"Node":{"Name":"peer.fixture.ts.net.","Tags":[]},"UserProfile":{"LoginName":OWNER}}
def access_rows():
    with runtime.db() as db:
        return {table:[tuple(row) for row in db.execute("SELECT * FROM "+table+" ORDER BY rowid")] for table in ("runtime_access_clients","runtime_access_invites","runtime_access_nonces","runtime_access_receipts","runtime_access_audit")}

cases=[]
del service._login

def auth_case(name, *, path=access.PAIR_PATH, body=None, client=None, owner=OWNER, tamper=None, timestamp=NOW, target_server="local-server", at=float(NOW), expires=None, address="100.64.0.2", logins=None, peer_tags=None):
    if body is None:
        body={"protocol":1,"inviteId":invitation["inviteId"],"token":invitation["token"],"clientId":"new-client","requestId":"request-a","label":"New client","kind":"ui","publicKey":keys["publicKey"],"origin":None}
    method="POST" if body else "GET"
    raw=access._json(body).encode() if body else b""
    client_id=body.get("clientId",body.get("serverId","peer-server")) if body else "peer-server"
    request_id=body.get("requestId","request-a") if body else "request-a"
    nonce="a"*32
    headers=list(zip(access.SIGNATURE_HEADERS[:5],(client_id,target_server,str(timestamp),nonce,request_id)))
    signed=access.request_bytes(method,path,target_server,client_id,str(timestamp),nonce,request_id,raw)
    headers.append(("X-Studio-Signature",crypto.call("sign",privateKey=keys["privateKey"],data=base64.b64encode(signed).decode())["signature"]))
    if tamper: headers=tamper(headers)
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_access_nonces")
        db.execute("DELETE FROM runtime_access_clients")
        if client is not None: db.execute("INSERT INTO runtime_access_clients VALUES(?,?)",(client_id,access._json(client)))
    service.identity_cache.clear()
    peer_whois=copy.deepcopy(whois)
    peer_whois["UserProfile"]["LoginName"]=owner
    if peer_tags is not None: peer_whois["Node"]["Tags"]=peer_tags
    observed_headers=headers+[("X-Forwarded-For",address)]+[("Tailscale-User-Login",v) for v in (logins or [])]
    this_invite=json.loads(invite_raw)
    if expires is not None: this_invite["expires"]=expires
    with runtime.db() as db:
        db.execute("UPDATE runtime_access_invites SET record=? WHERE id=?",(access._json(this_invite),invitation["inviteId"]))
    case={"name":name,"method":method,"target":path,"raw":raw.decode(),"headers":headers,"clientId":client_id,"client":access._json(client) if client is not None else None,"inviteId":invitation["inviteId"],"invite":access._json(this_invite),"owner":owner,"now":at,"address":address,"logins":logins or [],"whois":peer_whois}
    before=access_rows()
    def observation(*args):
        if args[0]=="status": return status
        if args==("whois","--json",address): return peer_whois
        raise AssertionError("unexpected observation")
    with patch.object(access.time,"time",return_value=at), patch.object(access,"_tailscale_json",side_effect=observation):
        try:
            principal=service.authenticate(Headers(observed_headers),method,path,raw)
            with runtime.db() as db:
                row=db.execute("SELECT record FROM runtime_access_clients WHERE id=?",(client_id,)).fetchone()
            case["expected"]={"principal":principal,"clientWrite":json.loads(row[0]) if row and not principal["pairing"] and not principal["autoPairing"] else None}
            if name=="pair-ui":
                accepted=access_rows()
                try: service.authenticate(Headers(observed_headers),method,path,raw)
                except access.AccessError as replay:
                    case["replayRefusal"]={"status":replay.status,"code":replay.code,"error":str(replay)}
                else: raise AssertionError("nonce reuse was accepted")
                assert access_rows()==accepted, "Python replay refusal wrote state"
        except access.AccessError as error:
            case["refusal"]={"status":error.status,"code":error.code,"error":str(error)}
            assert access_rows()==before, name+": Python refusal wrote state"
            case["noWrite"]=True
    cases.append(case)

probe={"protocol":1,"serverId":"auto-server","label":"Auto peer","origin":"https://peer.fixture.ts.net","publicKey":keys["publicKey"],"tailscaleUser":OWNER,"autoPair":True}
service.discovery().probe=lambda origin: probe
body={"protocol":1,"serverId":"auto-server","label":"Auto peer","origin":"https://peer.fixture.ts.net","publicKey":keys["publicKey"],"requestId":"auto-request"}
auth_case("pair-ui")
for origin in ("https://PEER.fixture.ts.net:443/", "https://peer.fixture.ts.net:8443/"):
    auth_case("normalized-origin-"+origin,body={"protocol":1,"inviteId":invitation["inviteId"],"token":invitation["token"],"clientId":"new-client","requestId":"request-a","label":"New server","kind":"server","publicKey":keys["publicKey"],"origin":origin})
auth_case("status-server",path="/api/multi-server/v1/status",body={},client=json.loads(client_raw))
auth_case("status-query",path="/api/multi-server/v1/status?check=1",body={},client=json.loads(client_raw))
for fraction in (0.25,0.5,0.75):
    auth_case("invite-expiry-"+str(fraction),at=NOW+fraction,expires=NOW+0.5)
for name,kwargs in (("peer-tagged",dict(peer_tags=["tag:server"])),("outside-tailnet",dict(address="127.0.0.1")),("spoofed-login",dict(logins=["foreign@example.test"])),("duplicate-login",dict(logins=[OWNER,OWNER]))):
    auth_case(name,**kwargs)
auth_case("revoked",path="/api/multi-server/v1/status",body={},client={**json.loads(client_raw),"status":"revoked"})
auth_case("unknown-client",path="/api/multi-server/v1/status",body={})
auth_case("wrong-owner",owner="foreign@example.test")
auth_case("future-clock",timestamp=NOW+301)
auth_case("past-clock",timestamp=NOW-301)
auth_case("clock-boundary",timestamp=NOW-300)
auth_case("duplicate-header",tamper=lambda h:h+[h[0]])
auth_case("foreign-target",target_server="foreign")
auth_case("bad-signature",tamper=lambda h:[(k,"A"*88 if k=="X-Studio-Signature" else v) for k,v in h])
auth_case("bad-auto-signature",path="/api/multi-server/v1/auto-pair",body=body,tamper=lambda h:[(k,"A"*88 if k=="X-Studio-Signature" else v) for k,v in h])
auth_case("auto-pair",path="/api/multi-server/v1/auto-pair",body=body)
auth_case("auto-revoked",path="/api/multi-server/v1/auto-pair",body=body,client={**json.loads(client_raw),"id":"auto-server","clientId":"auto-server","status":"revoked"})
auth_case("auto-key-change",path="/api/multi-server/v1/auto-pair",body=body,client={**json.loads(client_raw),"id":"auto-server","clientId":"auto-server","publicKey":"different"})
for case in cases:
    if case["target"]=="/api/multi-server/v1/auto-pair": case["probe"]=probe
pair=cases[0]
fresh=copy.deepcopy(pair)
fresh["headers"]=[(k,"b"*32 if k=="X-Studio-Nonce" else v) for k,v in fresh["headers"]]
signed=access.request_bytes(pair["method"],pair["target"],"local-server","new-client",str(NOW),"b"*32,"request-a",pair["raw"].encode())
signature=crypto.call("sign",privateKey=keys["privateKey"],data=base64.b64encode(signed).decode())["signature"]
fresh["headers"]=[(k,signature if k=="X-Studio-Signature" else v) for k,v in fresh["headers"]]
with runtime.db() as db:
    db.execute("UPDATE runtime_access_invites SET record=? WHERE id=?",(invite_raw,invitation["inviteId"]))
    db.execute("DELETE FROM runtime_access_clients")
    db.execute("INSERT INTO runtime_access_clients VALUES('peer-server',?)",(client_raw,))
    db.execute("DELETE FROM runtime_access_nonces")
    db.execute("DELETE FROM runtime_access_receipts WHERE client!='peer-server'")
# Queue is exercised through the real Python producer, not a reconstructed row.
class Transport:
    local_server_id="local-server"
    def servers(self): return [{"id":"peer-server"}]
    def request(self,*a,**kw): raise AssertionError("network forbidden")
driver=orchestration.MultiServerService(runtime,Transport())
with patch.object(orchestration.time,"time",return_value=NOW),runtime.db() as db:
    driver.queue(db,"peer-server","browser_callback",{"actor":"fixture","url":"http://127.0.0.1/callback"},"python-queued")
    queued=dict(db.execute("SELECT * FROM runtime_server_outbox WHERE id='python-queued'").fetchone())
    # All three producers share this durable row writer. The storage contract
    # must accept location and orchestration bodies as well as browser bodies.
    driver.queue(db,"peer-server","add_location",{"project":"fixture","path":"/fixture","request":{"path":"/fixture"}},"python-location")
    driver.queue(db,"peer-server","input",{"actor":"fixture","text":"fixture"},"python-input")
# The real receive writer creates waiting/running/complete rows; retain each
# pre-restart record, then ask Python to resume the exact same envelope.
delivery_states=[]
for state in ("waiting","running","complete"):
    envelope={"requestId":"python-"+state,"action":"input","payload":{"actor":"fixture"}}
    def first(*args):
        if state=="running": raise RuntimeError("fixture uncertain effect")
        return {"waiting":True} if state=="waiting" else {"applied":True}
    driver._receive=first
    driver._unknown_diagnostic=lambda *args: None
    with patch.object(orchestration.time,"time",return_value=NOW):
        driver.receive("peer-server",envelope)
        with runtime.db() as db:
            saved=dict(db.execute("SELECT * FROM runtime_server_inbox WHERE id=?",(envelope["requestId"],)).fetchone())
        calls=[]
        driver._receive=lambda *args: calls.append(args) or {"applied":True}
        result=driver.receive("peer-server",envelope)
    delivery_states.append({"state":state,"row":saved,"envelope":envelope,"result":result,"calls":len(calls)})
    with runtime.db() as db:
        db.execute("UPDATE runtime_server_inbox SET state=?,result=?,completed_at=? WHERE id=?",(saved["state"],saved["result"],saved["completed_at"],saved["id"]))

candidates=[]
with patch.object(access,"_tailscale_json",return_value=status), patch.object(access.RemoteAccess,"origin",return_value=service.public_origin), patch.object(service.discovery(),"_candidate",side_effect=lambda origin,owner,deadline:candidates.append(origin)):
    service.discovery().discover()
discovery_probe={**probe,"serverId":"discovered-peer","autoPair":False}
with patch.object(access.time,"time",return_value=NOW), patch.object(service.discovery(),"probe",return_value=discovery_probe):
    service.discovery()._candidate("https://peer.fixture.ts.net",OWNER,float("inf"))
with runtime.db() as db:
    discovered=db.execute("SELECT record FROM runtime_access_discovered WHERE id='discovered-peer'").fetchone()[0]
with patch.object(access,"_tailscale_json",return_value=status):
    discovery_identity=service.discovery().identity()
output={"deliveryStates":delivery_states,"discoveryIdentity":discovery_identity,"discoveryProbe":discovery_probe,"discovered":json.loads(discovered),"freshPair":fresh,"candidates":sorted(candidates),"now":NOW,"identity":service.identity(),"invitation":invitation,"client":client_raw,"invite":invite_raw,"status":status,"whois":whois,"auth":cases,"queued":queued,"pairReply":peer_reply}
service.close();crypto.shutdown()
print(json.dumps(output,ensure_ascii=False))
