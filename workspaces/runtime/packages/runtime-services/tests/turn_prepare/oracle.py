"""Real Python preparation functions on a minimal host and in-memory SQLite.

Imports the existing lifecycle audit guard, but never constructs Runtime, starts
threads, executes Runtime.start, or runs a provider. Every file effect belongs to
this process's disposable directory. The receipt adapter uses the real manifest
writer; it is never allowed to promote an unknown/rejected event.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

GUARD = install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
import copy
import hashlib
import json
import os
import sqlite3
import tempfile
from unittest.mock import patch
from contextlib import nullcontext

from codex_runtime import Runtime
import codex_runtime
from codex_efficiency import EfficiencyMixin, remember_context_manifest
import codex_efficiency as efficiency
from codex_workspace import WorkspaceMixin
from codex_time import message_clock, append_message_clocks
import codex_vm_agents

for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
    assert Path(os.environ[key]).resolve().is_relative_to(Path(os.environ["TMPDIR"]).parent), key

TOKENS = {}
ORIGINAL_DIGEST = efficiency.digest

def digest(value):
    token = ORIGINAL_DIGEST(value)
    canonical = json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    TOKENS[token] = hashlib.sha256(canonical.encode()).hexdigest()[:24]
    return token


def convert_tokens(value):
    if isinstance(value, dict):
        return {k: convert_tokens(v) for k, v in value.items()}
    if isinstance(value, list):
        return [convert_tokens(v) for v in value]
    if isinstance(value, str):
        return TOKENS.get(value, value)
    return value


def manifest_tokens(manifest):
    if manifest:
        manifest["versions"] = {k:TOKENS.get(v,v) for k,v in manifest["versions"].items()}
    return manifest


def normalize_text(text, segments):
    """Only explicitly observed generated objects may differ in JSON key order.

    Literal spans (including user/follow-up, guidance, excerpts, and JSON strings)
    are never parsed or rewritten. A generated token remains its own identity.
    """
    output=[]
    at=0
    for segment in segments:
        kind, expected = next(iter(segment.items()))
        if kind == "json":
            value, end = json.JSONDecoder().raw_decode(text, at)
            output.append({"json":value})
            at=end
        elif kind == "token":
            token=text[at:at+24]
            output.append({"token":TOKENS.get(token,token)})
            at+=24
        else:
            actual=text[at:at+len(expected)]
            assert actual == expected, f"literal span differs at {at}: {actual[:80]!r} != {expected[:80]!r}"
            output.append({"literal":actual})
            at+=len(expected)
    assert at == len(text), "unexpected trailing model text"
    return output


def normalize_output(value, segments):
    value=copy.deepcopy(value)
    if not value.get("ok"): return value
    manifest_tokens(value["manifest"])
    for metadata in value["metadata_patches"].values(): manifest_tokens(metadata.get("contextManifest"))
    delivery=value["delivery"]
    manifest_tokens(delivery["manifest_upsert"])
    delivery["reminder_insert"]=TOKENS.get(delivery["reminder_insert"],delivery["reminder_insert"])
    for manifest in value["stored"]["manifests"]: manifest_tokens(manifest)
    value["stored"]["reminders"]=sorted(TOKENS.get(v,v) for v in value["stored"]["reminders"])
    # Check the original excerpt before replacing the parent with its semantic view.
    full=value["fulltext"]
    parent=value["transcript"]
    assert parent["text"] == (full if full is not None else parent["text"])[:20000]
    assert parent["truncated"] == (full is not None)
    if full is not None:
        assert len(full)>20000
        value["fulltext"]=normalize_text(full,segments)
        parent["text"]={"prefixOf":value["fulltext"],"characters":20000}
    else: parent["text"]=normalize_text(parent["text"],segments)
    provider=value["provider_inputs"][0]
    provider["text"]=normalize_text(provider["text"],segments+value.pop("clock_segments",[]))
    return value


def literal(text): return {"literal":text}


def event_segments(rows, projections):
    result=[]
    for row in rows:
        bypass=row["kind"] in {"user","followup","work_decision","radio_turn"}
        if not bypass and row["id"] not in projections: continue
        if result: result.append(literal("\n\n"))
        if row["kind"] != "user": result.append(literal("[Orchestration event: "+row["kind"]+"]\n"))
        text=row["text"] if bypass else projections[row["id"]]
        generated=json.loads(text) if text != row["text"] else None
        # Rewritten producer payloads now compare byte for byte, even after edits.
        summary=isinstance(generated,dict) and generated.get("truncated") is True and "textBytes" in generated and "read" in generated
        result.append({"json":generated} if summary else literal(text))
    return result


def context_segments(text, manifest, known):
    # Context-generated JSON occurs after guidance. Find only blocks that the real
    # version decision selected; guidance may itself contain marker-looking text.
    spans=[]
    versions=manifest["versions"]
    if "plan" in versions and versions["plan"] != known.get("plan"):
        header="[Agent plan version "+versions["plan"]+"]\n"
        pos=text.rindex(header)
        start=pos+len("[Agent plan version ")
        spans.append((start,start+24,{"token":versions["plan"]}))
        start=pos+len(header)
        if text[start:start+1] == "{":
            value,end=json.JSONDecoder().raw_decode(text,start)
            spans.append((start,end,{"json":value}))
    if any(k.startswith("complaint:") and v != known.get(k) for k,v in versions.items()):
        start=text.rindex("[Complaint messages requiring a response]\n")+len("[Complaint messages requiring a response]\n")
        value,end=json.JSONDecoder().raw_decode(text,start)
        spans.append((start,end,{"json":value}))
    result=[];at=0
    for start,end,segment in sorted(spans):
        if at<start: result.append(literal(text[at:start]))
        result.append(segment);at=end
    if at<len(text): result.append(literal(text[at:]))
    return result


def stored(host):
    db=host.connection
    return dict(manifests=[json.loads(r[0]) for r in db.execute("SELECT record FROM runtime_context_manifests ORDER BY thread_id,compactions")],
                reminders=[r[0] for r in db.execute("SELECT version FROM runtime_context_reminders ORDER BY version")],
                transcript_turn_id=json.loads(db.execute("SELECT record FROM runtime_items WHERE id=?",(host.observed_item,)).fetchone()[0]).get("turnId"))


def settle(host, expected, receipt, current):
    transition=dict(status="unconfirmed",manifest_upsert=None,reminder_insert=None,transcript_turn_id=None)
    valid=all(receipt[k]==expected[k] for k in ("operation_id","actor_id","epoch","event_ids")) and receipt["outcome"]["kind"]=="accepted" and bool(receipt["outcome"].get("turn_id"))
    if not valid: return transition
    # Authorized pure compare-and-set rule, enforced before Python's receipt writer.
    if current and current["epoch"] == expected["epoch"] and current["sequence"]>expected["base_sequence"]:
        transition["status"]="stale"
        return transition
    attempt=dict(id=receipt["operation_id"],submitted=True,events=receipt["event_ids"],epoch=0)
    assert Runtime.bind_start(host,host.connection,host.actor,attempt["id"],receipt["outcome"]["turn_id"],historical=attempt)
    rows=stored(host)
    transition.update(status="accepted",manifest_upsert=next(m for m in rows["manifests"] if m["epoch"]==expected["epoch"]),transcript_turn_id=rows["transcript_turn_id"])
    reminder=expected["manifest"]["versions"].get("worktreeReminder")
    if reminder:
        assert reminder in rows["reminders"]
        transition["reminder_insert"]=reminder
    return transition


def merge(target, patch):
    for key, value in patch.items():
        if isinstance(value, dict) and isinstance(target.get(key), dict): merge(target[key], value)
        else: target[key] = copy.deepcopy(value)
    return target


def snapshot():
    return dict(actor=dict(id="lead",root_id="lead",epoch=["thread",0],is_lead=True,forked_from=None,prepared_context=None,delivered_mode=None),
                root=dict(concurrency=32,mode_revision=0),delivered_manifests=[],plan=None,complaints=[],workers=[],delivered_reminders=[],assets=[],
                guidance=dict(role="[Studio role skill: fixture]\nSource: fixture\nComplete assigned work.\n[End Studio role skill]",progress="Update the fixture progress file when facts change."),
                artifact_paths={},prepared_at=1790000000.125,operation_id="operation")


def event(patch=None, index=0):
    result = dict(id=f"event-{index}",kind="user",text="A complete user request.",created=1789999999.25,
                  metadata=dict(assets=[],accepted_at=None,extra={"retained":"metadata"}),preserve_progress=False,preserve_complaint=False)
    return merge(result,patch or {})


def expand(spec):
    """Compact data syntax for large boundary strings, not behavior expectations."""
    if isinstance(spec, dict):
        if set(spec) == {"repeat", "count"}: return spec["repeat"] * spec["count"]
        if set(spec) == {"json"}: return json.dumps(expand(spec["json"]),ensure_ascii=False,separators=(",",":"))
        return {k:expand(v) for k,v in spec.items()}
    if isinstance(spec,list): return [expand(v) for v in spec]
    return spec


class Host(EfficiencyMixin):
    unanswered_complaints = staticmethod(Runtime.unanswered_complaints)
    complaint_message = Runtime.complaint_message
    asset_view = staticmethod(WorkspaceMixin.asset_view)
    asset_record = WorkspaceMixin.asset_record
    message_inputs = WorkspaceMixin.message_inputs
    item = Runtime.item

    def __init__(self, root, s):
        self.root, self.s = root, s
        self.lock = nullcontext()
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript("""
          CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_plans(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_assets(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, status TEXT, epoch INTEGER, turn_id TEXT, error TEXT, kind TEXT, text TEXT);
          CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
          CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY, body TEXT);
          CREATE TABLE runtime_context_manifests(agent TEXT,thread_id TEXT,compactions INTEGER,sequence INTEGER,event_id TEXT,record TEXT,PRIMARY KEY(agent,thread_id,compactions));
          CREATE TABLE runtime_context_reminders(agent TEXT,version TEXT,event_id TEXT,PRIMARY KEY(agent,version));
        """)
        self.reset(s)

    def reset(self, s):
        self.s = s
        db = self.connection
        for table in ("agents","complaints","plans","assets"): db.execute("DELETE FROM runtime_"+table)
        a=s["actor"]
        self.actor=dict(id=a["id"],rootId=a["root_id"],threadId=a["epoch"][0],compactions=a["epoch"][1],isLead=a["is_lead"],forkedFrom=a["forked_from"],preparedContext=a["prepared_context"],deliveredMode=a["delivered_mode"])
        root=dict(id=a["root_id"],rootId=a["root_id"],concurrency=s["root"]["concurrency"],subagentConcurrencyVersion=2,agentModeRevision=s["root"]["mode_revision"])
        self.agents={root["id"]:root,a["id"]:{**root,**self.actor}}
        for w in s["workers"]:
            self.agents[w["id"]]=dict(id=w["id"],rootId=w["root_id"],worktreeReady=w["worktree_ready"],deletedAt=1 if w["deleted"] else None,status=w["status"],autoWake=w["auto_wake"])
        for asset in s["assets"]:
            self.agents.setdefault(asset["owner_id"],dict(id=asset["owner_id"],rootId=asset["owner_root_id"],deletedAt=1 if asset["owner_deleted"] else None))
            record={**asset["view"],"id":asset["id"],"agent":asset["owner_id"],"image":asset["image"],"path":asset["path"],"name":asset["name"],"size":asset["size"]}
            db.execute("INSERT INTO runtime_assets VALUES (?,?)",(asset["id"],json.dumps(record)))
        for c in s["complaints"]:
            record=dict(id=c["id"],leadId=c["lead_id"],recipient=c["recipient"],status=c["status"],sourceType="user_task" if c["legacy_user_task"] else "agent",responses=[dict(author=a) for a in c["response_authors"]],author=c["author"],text=c["text"],version=c["version"],created=c["created"])
            db.execute("INSERT INTO runtime_complaints VALUES (?,?)",(c["id"],json.dumps(record)))
            if c["author_name"] is not None: self.agents[c["author"]]=dict(id=c["author"],name=c["author_name"])
        for key, agent in self.agents.items(): db.execute("INSERT INTO runtime_agents VALUES (?,?)",(key,json.dumps(agent)))
        if s["plan"] is not None:
            p=s["plan"]
            db.execute("INSERT INTO runtime_plans VALUES (?,?)",(p["id"],json.dumps(dict(id=p["id"],rootId=p["rootId"],native=dict(plan=p["steps"],explanation=p["explanation"])))))

    def agent(self,key,db=None): return self.agents[key]
    checked_actor = Runtime.checked_actor
    def named_agents(self,db,keys): return {k:self.agents[k] for k in keys if k in self.agents}
    def role_guidance(self,a): return self.s["guidance"]["role"]
    def progress_file(self,a): pass
    def role_update(self,previous,text):
        projected = super().role_update(previous,text)
        assert projected == text or projected.startswith("[Studio role skill update:")
        self.role_was_diff = projected != text
        return text  # Approved presentation substitution; selection stays real Python.
    def db(self): return self.connection
    def index_item(self,*args): pass
    def touch_ui(self,*args): pass
    def _stage_event_resources(self,*args): pass
    def sync_chat_delivery(self,*args): pass

    def seed_manifest(self,manifest,event_id="previous"):
        db=self.connection
        db.execute("INSERT OR REPLACE INTO runtime_context_manifests VALUES (?,?,?,?,?,?)",(self.actor["id"],manifest["epoch"][0] or "",manifest["epoch"][1],manifest["sequence"],event_id,json.dumps(manifest)))


def run(case, root):
    TOKENS.clear()
    case=expand(case)
    s=merge(snapshot(),case.get("snapshot",{}))
    rows=[event(p,i) for i,p in enumerate(case.get("events",[{}]))]
    prior=merge(copy.deepcopy(s),case.get("prior",{}))
    host=Host(root,prior)
    db=host.connection
    with patch.object(efficiency,"digest",digest), patch.object(codex_vm_agents,"progress_context",lambda host,a:host.s["guidance"]["progress"]):
        if "prior" in case:
            host.model_turn_context(db,host.actor,"previous")
            old=json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id='previous'").fetchone()[0])["contextManifest"]
            if case.get("history","delivered") == "delivered":
                host.seed_manifest(old)
                s["delivered_manifests"].append(old)
            if case.get("developer"):
                s["actor"]["prepared_context"]={"epoch":old["epoch"],"versions":{k:v for k,v in old["versions"].items() if k in ("roleSkill","progressFile")}}
            if case.get("deliveredMode"):
                s["actor"]["delivered_mode"]={"epoch":old["epoch"],"version":old["versions"]["agentMode"]}
            if case.get("reminderDelivered"):
                version=old["versions"]["worktreeReminder"]
                s["delivered_reminders"].append(version)
        if case.get("roleSnapshot"):
            for path in (root/"context-snapshots").glob("*.txt"):
                if case["roleSnapshot"] == "missing": path.unlink()
                else: path.write_text("corrupt snapshot")
        host.reset(s)
        if "rawNativePlan" in case:
            db.execute("INSERT INTO runtime_plans VALUES (?,?)",(s["actor"]["root_id"],json.dumps(dict(id=s["actor"]["root_id"],rootId=s["actor"]["root_id"],native=case["rawNativePlan"]))))
        for m in s["delivered_manifests"]: host.seed_manifest(m)
        for v in s["delivered_reminders"]: db.execute("INSERT OR IGNORE INTO runtime_context_reminders VALUES (?,?,?)",(host.actor["id"],v,"old"))
        pyrows=[]
        ids=[]
        try:
            if not rows: return dict(snapshot=convert_tokens(s),events=rows,expected=dict(ok=False,error="EmptyBatch"),tokens={})
            if len({r["id"] for r in rows}) != len(rows): return dict(snapshot=convert_tokens(s),events=rows,expected=dict(ok=False,error="DuplicateEvent"),tokens={})
            for r in rows:
                meta={**r["metadata"]["extra"],"assets":r["metadata"]["assets"]}
                if r["metadata"]["accepted_at"] is not None: meta["acceptedAt"]=r["metadata"]["accepted_at"]
                db.execute("INSERT INTO runtime_events VALUES (?,?,?,0,NULL,NULL,?,?)",(r["id"],host.actor["id"],"dispatching",r["kind"],r["text"]))
                db.execute("INSERT INTO runtime_event_meta VALUES (?,?)",(r["id"],json.dumps(meta)))
                ids.extend(meta["assets"])
                pyrows.append({k:r[k] for k in ("id","kind","text","created")}|dict(preserveProgress=r["preserve_progress"],preserveComplaint=r["preserve_complaint"]))
            # Match Python's per-input assets and provider ownership checks.
            host.message_inputs(host.actor["id"],"",ids)
            try:
                clocks=[message_clock(r["id"],r["metadata"]["accepted_at"]) for r in rows if r["kind"] in {"user","followup"} and r["metadata"]["accepted_at"] is not None]
            except (ValueError, OverflowError, OSError):
                return dict(snapshot=convert_tokens(s),events=rows,expected=dict(ok=False,error="InvalidClock"),tokens=TOKENS)
            for p,r in zip(pyrows,rows): p["assets"]=[host.asset_view(host.asset_record(i)) for i in r["metadata"]["assets"]]
            for row in rows:
                try: value=json.loads(row["text"])
                except ValueError: continue
                if not isinstance(value,dict): continue
                result=value.get("result")
                invalid=row["kind"]=="work_review" and isinstance(result,dict) and (("checks" in result and not isinstance(result["checks"],str)) or (isinstance(result.get("files"),list) and any(not isinstance(v,str) for v in result["files"])))
                invalid |= row["kind"]=="child_result" and any(isinstance(value.get(k),(list,dict)) for k in ("agent_id","name"))
                if invalid: return dict(snapshot=convert_tokens(s),events=rows,expected=dict(ok=False,error="InvalidEvidence"),tokens=TOKENS,segments=[])
            projections={}
            bounded=EfficiencyMixin.bounded_event
            def observe(row,text,limit):
                value=bounded(row,text,limit);projections[row["id"]]=value;return value
            projected_rows=copy.deepcopy(pyrows)
            if case.get("revisionDeviation"):
                # Approved u64 boundary: record Python's different coalescing result,
                # then preserve exactly the unsupported member for the Rust contract.
                python_projection=host.model_event_text(pyrows)
                for row in projected_rows:
                    version=json.loads(row["text"])["progress_version"]
                    if version>2**64-1 or '"progress_version":-0' in row["text"]: row["preserveProgress"]=True
                assert python_projection != host.model_event_text(projected_rows)
            with patch.object(EfficiencyMixin,"bounded_event",staticmethod(observe)):
                text=host.model_event_text(projected_rows)
            segments=event_segments(rows,projections)
            normalize_text(text,segments)

            artifacts=[]
            for p in pyrows:
                # Observe only actual archive outputs, never infer success from a fixture.
                if p["kind"] == "child_result":
                    for path in (root / "results").glob("**/*.md") if (root/"results").exists() else []:
                        if hashlib.sha256(p["id"].encode()).hexdigest() == path.stem:
                            artifacts.append(dict(event_id=p["id"],relative_path=str(path.relative_to(root)),body=path.read_text()))
                            s["artifact_paths"][p["id"]]=str(path)
                            original = json.loads(p["text"])
                            assert host._archive_child_result_event(p, original, original["result"]) == str(path)
                            before = path.read_bytes()
                            try:
                                host._archive_child_result_event(p, original, original["result"] + "conflict")
                            except ValueError:
                                pass
                            else:
                                raise AssertionError("archive conflict accepted")
                            assert path.read_bytes() == before
                            assert not list(path.parent.glob(".result-*"))
            _,_,known=host.model_known_context(db,host.actor)
            context=host.model_turn_context(db,host.actor,rows[0]["id"])
            text+=context
            if case.get("expectGuidance"):
                key=case["expectGuidance"]
                assert prior["guidance"][key] != s["guidance"][key]
                assert s["guidance"][key] in context and context, "changed guidance must be sent"

            if case.get("expectRoleDiff"): assert host.role_was_diff, "fixture must reach real Python diff branch"
            if not text:
                text="[Complaint update] No complaints require a response."
                segments=[literal(text)]
            for row in rows:
                metadata=json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?",(row["id"],)).fetchone()[0]);metadata["modelEventProjection"]=1
                db.execute("UPDATE runtime_event_meta SET record=? WHERE id=?",(json.dumps(metadata),row["id"]))
            with patch.object(codex_runtime.time,"time",lambda:s["prepared_at"]):
                host.item(db,host.actor["id"],rows[0]["id"],"user",text,inputs=pyrows,assets=[host.asset_view(host.asset_record(i)) for i in ids])
            item=json.loads(db.execute("SELECT record FROM runtime_items").fetchone()[0])
            full=db.execute("SELECT body FROM runtime_item_fulltext").fetchone()
            patches={}
            for r in rows:
                m=json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?",(r["id"],)).fetchone()[0])
                patches[r["id"]]={k:v for k,v in m.items() if k in ("contextManifest","modelEventProjection","transcriptItemId")}
            manifest=patches[rows[0]["id"]]["contextManifest"]
            segments+=context_segments(context,manifest,known)
            clock_segments=[literal(append_message_clocks("",clocks))] if clocks else []
            expected=dict(base_sequence=max((m["sequence"] for m in s["delivered_manifests"] if m["epoch"]==s["actor"]["epoch"]),default=0),ok=True,operation_id=s["operation_id"],actor_id=s["actor"]["id"],epoch=s["actor"]["epoch"],event_ids=[r["id"] for r in rows],provider_inputs=host.message_inputs(host.actor["id"],append_message_clocks(text,clocks),ids),transcript=item,fulltext=full[0] if full else None,metadata_patches=patches,agent_patch=dict(complaintsPresented=[c["id"] for c in host.unanswered_complaints(db,host.actor["id"])]),manifest=manifest,artifact_intents=artifacts)
            receipt=merge(dict(operation_id=s["operation_id"],actor_id=s["actor"]["id"],epoch=s["actor"]["epoch"],event_ids=[r["id"] for r in rows],outcome=dict(kind="accepted",turn_id="turn")),case.get("receipt",{}))
            host.observed_item=item["id"]
            current=copy.deepcopy(manifest) if "current" in case else next((m for m in s["delivered_manifests"] if m["epoch"]==manifest["epoch"]),None)
            if "current" in case:
                current["sequence"]+=case["current"]
                host.seed_manifest(current)
            before=stored(host)
            expected["delivery"]=settle(host,expected,receipt,current)
            expected["stored"]=stored(host)
            expected["clock_segments"]=clock_segments
            return dict(snapshot=convert_tokens(s),events=rows,receipt=receipt,current=convert_tokens(current),before=convert_tokens(before),expected=normalize_output(expected,segments),prepared=convert_tokens({k:v for k,v in expected.items() if k not in ("delivery","stored","clock_segments","ok")}),tokens=TOKENS,segments=segments,clock_segments=clock_segments)

        except (ValueError,KeyError,OverflowError,OSError) as exc:
            known={"Attach up to eight files":"TooManyAttachments","Unknown attachment":"UnknownAttachment","This agent was deleted":"UnknownAttachment","Attachment belongs to another conversation":"AttachmentOwnership"}
            error=known.get(str(exc))
            if error is None: raise
            return dict(snapshot=convert_tokens(s),events=rows,expected=dict(ok=False,error=error),tokens=TOKENS)
        finally: db.close()


def oracle_invariants():
    TOKENS.clear();TOKENS.update({"python-a":"canonical-a","python-b":"canonical-b"})
    mapped=manifest_tokens(dict(versions={"role":"python-a","plan":"python-b"}))
    assert mapped["versions"] == {"role":"canonical-a","plan":"canonical-b"}, "token identities collapsed"
    # JSON-looking caller text and strings embedded inside generated JSON stay exact.
    original='{"z": 1, "a": 2}'
    changed='{"a": 2, "z": 1}'
    try: normalize_text(changed,[literal(original)])
    except AssertionError: pass
    else: raise AssertionError("caller JSON was normalized")
    embedded={"json":{"message":original}}
    assert normalize_text(json.dumps(embedded["json"]),[embedded]) != normalize_text(json.dumps({"message":changed}),[embedded])
    with tempfile.TemporaryDirectory(prefix="prepare-branches-") as directory:
        host=Host(Path(directory),snapshot());db=host.connection
        db.execute("INSERT INTO runtime_events VALUES ('bad','lead','delivered',0,NULL,NULL,'user','')")
        for metadata in [{}, {"contextManifest":None}, {"contextManifest":{"epoch":[]}}, {"contextManifest":{"epoch":["t",0],"sequence":"invalid"}}]:
            db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES ('bad',?)",(json.dumps(metadata),))
            assert remember_context_manifest(db,"lead","bad") is False
            assert db.execute("SELECT count(*) FROM runtime_context_manifests").fetchone()[0] == 0
        text=host.s["guidance"]["role"]
        host.remember_role_text(text)
        assert host.role_update(ORIGINAL_DIGEST(text),text)==text and not host.role_was_diff
        assert host.role_update(ORIGINAL_DIGEST(text),"changed "*10000)=="changed "*10000 and not host.role_was_diff
        db.close()
    return True


def forbidden_runtime(*args, **kwargs):
    raise AssertionError("the preparation oracle must not construct Runtime")
Runtime.__init__ = forbidden_runtime


def main():
    for line in sys.stdin:
        request=json.loads(line)
        if request["command"]=="close": break
        if request["command"]=="oracle_invariants":
            result=oracle_invariants()
        elif request["command"]=="receipt_sequence":
            with tempfile.TemporaryDirectory(prefix="prepare-sequence-") as directory:
                host=Host(Path(directory),snapshot()); db=host.connection
                prepared=request["prepared"]
                for value in prepared:
                    for event_id in value["event_ids"]:
                        db.execute("INSERT INTO runtime_events VALUES (?,?,?,0,NULL,NULL,'user','request')",(event_id,value["actor_id"],"dispatching"))
                        db.execute("INSERT INTO runtime_event_meta VALUES (?,?)",(event_id,json.dumps(dict(contextManifest=value["manifest"]))))
                    db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",(value["transcript"]["id"],value["actor_id"],json.dumps(value["transcript"]),value["transcript"]["at"]))
                result=[]; current=None
                for index in request["order"]:
                    value=prepared[index];host.observed_item=value["transcript"]["id"]
                    receipt={k:value[k] for k in ("operation_id","actor_id","epoch","event_ids")}
                    receipt["outcome"]=dict(kind="accepted",turn_id="turn-"+str(index))
                    transition=settle(host,value,receipt,current)
                    result.append(dict(delivery=transition,stored=stored(host)))
                    if transition["manifest_upsert"]: current=transition["manifest_upsert"]
                db.close()
        elif request["command"]=="normalize":
            TOKENS.clear();TOKENS.update(request["tokens"])
            result=normalize_output(request["value"],request["segments"])
        else:
            with tempfile.TemporaryDirectory(prefix="prepare-oracle-") as directory:
                # Child result paths remain literal observed facts after the fixture closes.
                case=request["case"]
                if case.get("id"):
                    case=next(c for c in json.loads(Path(__file__).with_name("cases.json").read_text()) if c["id"]==case["id"])
                result=run(case,Path(directory))
        print(json.dumps(result,ensure_ascii=False),flush=True)

if __name__=="__main__": main()
