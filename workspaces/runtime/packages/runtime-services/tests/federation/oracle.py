"""Real FederationService oracle with disposable SQLite and captured runtime ports."""
import json
from pathlib import Path
import sqlite3
import sys
import threading
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard()
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_federation import FederationService, _crypto, _json, _sign, _message_bytes

NOW = 1700000000.0
LOCAL = "11111111-1111-4111-8111-111111111111"
PEER = "22222222-2222-4222-8222-222222222222"
ROOM = "federated:33333333-3333-4333-8333-333333333333"

class Ports:
    def __init__(self):
        self.lock = threading.RLock()
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        FederationService.ensure_tables(self.connection)
        self.connection.executescript("""
        CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT);
        CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
        CREATE TABLE runtime_chat_messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE,room TEXT,sender TEXT,text TEXT,created REAL,deliveries TEXT);
        """)
        self.events = []
    def db(self): return self.connection
    def read_db(self): return self.connection
    def put(self, db, table, value):
        db.execute(f"INSERT INTO runtime_{table}(id,record) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record", (value["id"], _json(value)))
    def named_agents(self, db, ids):
        return {key: json.loads(row[0]) for key in ids if (row := db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone())}
    def agent(self, key, db): return self.named_agents(db, [key])[key]
    def enqueue(self, db, agent, kind, text, key):
        self.events.append({"id":key,"agent":agent["id"],"kind":kind,"text":json.loads(text)})
    def sync_room_entity(self, db, room, **kwargs): return True


def records(db, table):
    return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_federation_{table} ORDER BY rowid")]


def run(target):
    keys = [_crypto("generate"), _crypto("generate")]
    local = {"stateId":LOCAL,"label":"Local",**keys[0]}
    remote = {"stateId":PEER,"label":"Peer",**keys[1]}
    peer = {"stateId":PEER,"label":"Peer","origin":"https://127.0.0.1","publicKey":remote["publicKey"],"status":"approved","localApproved":True,"remoteApproved":True,"whoisUser":None}
    runtime = Ports()
    service = FederationService(runtime)
    service.start = lambda: None
    db = runtime.connection
    agents = [{"id":name,"rootId":"lead","epoch":0,"status":"idle","name":name,"autoWake":False,"isLead":name=="lead"} for name in ("lead","helper")]
    with db:
        db.execute("INSERT INTO runtime_federation_identity VALUES('local',?)", (_json(local),))
        db.execute("INSERT INTO runtime_federation_settings VALUES('global',?)", (_json({"enabled":True}),))
        db.execute("INSERT INTO runtime_federation_peers VALUES(?,?)", (PEER,_json(peer)))
        for agent in agents: runtime.put(db,"agents",agent)
    with patch("codex_federation.time.time", return_value=NOW), patch("codex_federation.uuid.uuid4", return_value=ROOM.removeprefix("federated:")):
        service.create_room({"peer_id":PEER,"local_members":["lead","helper"],"share_names":True})
    room = records(db,"rooms")[0]
    room.update(status="approved",remoteApproved=True,remoteMembers=[{"id":"remote-lead","role":"lead","name":"Remote"}])
    seeds = {"local":local,"remote":remote,"peer":peer,"agents":agents,"room":room,"now":NOW}
    cases=[]
    variations=[("chat",{}),("unicode",{"text":"Zażółć 🐈"}),("no-name",{"senderName":None}),("wrong-name",{"senderName":"spoof"}),("wrong-sender",{"sender":"other"}),("ordering-zero",{"sequence":0}),("empty-text",{"text":""}),("timestamp-zero",{"created":0}),("large-text",{"text":"a"*12001})]
    for name,fields in variations:
        envelope={"protocol":1,"senderServer":PEER,"id":"message-"+name,"kind":"chat-message","room":ROOM,"payload":{"sender":"remote-lead","senderName":"Remote","text":"hello","created":NOW,"sequence":1,**fields},"created":NOW,"timestamp":int(NOW),"nonce":"unique-nonce-"+name+"-12345"}
        envelope["signature"]=_sign(remote["privateKey"],_message_bytes(envelope))
        with db:
            for table in ("runtime_federation_inbox","runtime_federation_nonces","runtime_chat_messages"): db.execute("DELETE FROM "+table)
            db.execute("DELETE FROM sqlite_sequence WHERE name='runtime_chat_messages'")
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?",(_json(room),ROOM))
        runtime.events=[]
        try:
            with patch("codex_federation.time.time",return_value=NOW): receipt=service.receive_envelope(peer,envelope)
            expected={"receipt":receipt,"room":records(db,"rooms")[0],"inbox":records(db,"inbox"),"events":runtime.events,
                      "chat":[dict(row) for row in db.execute("SELECT * FROM runtime_chat_messages ORDER BY seq")]}
            for item in expected["chat"]: item["deliveries"]=json.loads(item["deliveries"])
        except (ValueError,PermissionError) as error:
            expected={"error":type(error).__name__}
        cases.append({"name":name,"envelope":envelope,"expected":expected})
    for kind in ("room-invite", "room-accept"):
        prior = None if kind == "room-invite" else {**room, "status":"pending", "remoteApproved":False}
        envelope={"protocol":1,"senderServer":PEER,"id":"message-"+kind,"kind":kind,"room":ROOM,
                  "payload":{"room":ROOM,"participants":[{"id":"remote-lead","role":"lead","name":"Remote"}],"shareNames":False,"shareStatus":False},
                  "created":NOW,"timestamp":int(NOW),"nonce":"unique-nonce-"+kind+"-12345"}
        envelope["signature"]=_sign(remote["privateKey"],_message_bytes(envelope))
        with db:
            for table in ("runtime_federation_rooms","runtime_federation_inbox","runtime_federation_nonces","runtime_chat_messages"): db.execute("DELETE FROM "+table)
            if prior: db.execute("INSERT INTO runtime_federation_rooms VALUES(?,?)",(ROOM,_json(prior)))
        runtime.events=[]
        with patch("codex_federation.time.time",return_value=NOW): receipt=service.receive_envelope(peer,envelope)
        expected={"receipt":receipt,"room":records(db,"rooms")[0],"inbox":records(db,"inbox"),"events":runtime.events,"chat":[]}
        cases.append({"name":kind,"envelope":envelope,"room":prior,"expected":expected})
    (target/"oracle.json").write_text(json.dumps({"seed":seeds,"cases":cases},ensure_ascii=False))
    # Python creates the established database catalog once. Rust clones this template.
    catalog=json.loads((ROOT/"workspaces/runtime/apps/server/tests/fixtures/runtime-state-schema.json").read_text())
    template=target/"template";template.mkdir()
    for filename in ("canvas.sqlite3","analytics.sqlite3"):
        conn=sqlite3.connect(template/filename)
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("BEGIN")
        entries=sorted(catalog[filename]["sqlite_master"],key=lambda row: {"table":0,"index":1,"view":2,"trigger":3}.get(row["type"],4))
        for entry in entries:
            if entry.get("sql") and not conn.execute("SELECT 1 FROM sqlite_master WHERE name=?",(entry["name"],)).fetchone(): conn.execute(entry["sql"])
        conn.commit()
        conn.close()

if __name__ == "__main__":
    run(Path(sys.argv[1]))
