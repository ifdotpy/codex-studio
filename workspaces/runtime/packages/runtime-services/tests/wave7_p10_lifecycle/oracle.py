"""Real Python stop/delete methods, Python-written complete records, no provider."""
import copy
import json
from pathlib import Path
import sys
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "turn_lifecycle"))
import oracle_guard  # The imported lifecycle oracle installs this guard once.
sys.path.insert(0, str(HERE.parent / "agent_management"))
from oracle import MemoryRuntime, NOW, Runtime, seed
from codex_sync_entities import encoded, project


class LifecycleRuntime(MemoryRuntime):
    closed = False
    empty_lead = staticmethod(Runtime.empty_lead)
    image_workspace_summary = staticmethod(Runtime.image_workspace_summary)

    def __init__(self, records):
        super().__init__(records)
        for column, kind in (("kind", "TEXT"), ("text", "TEXT"), ("created", "REAL"),
                             ("turn_id", "TEXT"), ("error", "TEXT")):
            self.connection.execute(f"ALTER TABLE runtime_events ADD COLUMN {column} {kind}")
        self.connection.execute(
            "UPDATE runtime_events SET kind='user',text='input',created=1")
        self.connection.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT)")

    def interrupt(self, agent):
        # Only the provider boundary is injected; stop and every stored write
        # execute the actual Runtime implementation.
        self.interrupted.append(copy.deepcopy(agent))

    child_stopped_event = Runtime.child_stopped_event

    def enqueue_recovery_event(self, db, agent, kind, text, key):
        db.execute(
            "INSERT OR IGNORE INTO runtime_events "
            "(id,agent,kind,text,status,created,epoch,turn_id,error) "
            "VALUES(?,?,?,?,'pending',?, ?,NULL,NULL)",
            (key, agent["id"], kind, text, NOW, agent["epoch"]),
        )

    def latest_work_result_file(self, db, agent_id):
        return None

    def snapshot(self):
        result = {"tables": {"runtime_" + table: self.records(self.connection, table)
                             for table in ("agents", "work", "requests", "monitors", "tasks", "tool_requests")}}
        result["tables"]["runtime_events"] = [dict(row) for row in self.connection.execute(
            "SELECT id,agent,status,epoch FROM runtime_events ORDER BY rowid")]
        if self.has_voice:
            result["tables"]["voice_sessions"] = [dict(row) for row in self.connection.execute(
                "SELECT * FROM voice_sessions ORDER BY rowid")]
        return result


def case(name, records, action):
    runtime = LifecycleRuntime(records)
    runtime.interrupted = []
    before = runtime.snapshot()
    with patch("time.time", return_value=NOW):
        actions = action if isinstance(action, list) else [action]
        result = None
        for current in actions:
            if current["action"] == "delete":
                result = Runtime.delete_conversation(runtime, current["target"])
            else:
                result = Runtime.stop(runtime, current["target"], current["descendants"])
    notifications = [list(row) for row in runtime.connection.execute(
        "SELECT id,agent,status FROM runtime_events WHERE id LIKE 'child-stop:%' ORDER BY id")]
    notification_payloads = [list(row) for row in runtime.connection.execute(
        "SELECT id,text FROM runtime_events WHERE id LIKE 'child-stop:%' ORDER BY id")]
    sync_entities = []
    for table, collection in (("runtime_agents", "agent"), ("runtime_requests", "request"),
                              ("runtime_work", "work")):
        for record in runtime.records(runtime.connection, table.removeprefix("runtime_")):
            if collection == "agent":
                if record.get("deletedAt"):
                    continue
                entity = Runtime.agent_entity_view(runtime, runtime.connection, record)
            elif collection == "request" and record.get("status") != "pending":
                continue
            else:
                entity = record
            projected = project(collection, entity)
            if projected is not None:
                payload, _digest, deleted = encoded(collection, record["id"], projected)
                sync_entities.append([collection, record["id"], payload, int(deleted)])
    for row in runtime.connection.execute(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error "
            "FROM runtime_events ORDER BY rowid"):
        record = dict(row)
        projected = project("event", record)
        if projected is not None:
            payload, _digest, deleted = encoded("event", record["id"], projected)
            sync_entities.append(["event", record["id"], payload, int(deleted)])
    sync_entities.sort(key=lambda row: (row[0], row[1]))
    result = dict(name=name, records=before, action=action, now=NOW,
                  expected=dict(result=result, records=runtime.snapshot(),
                                notifications=notifications,
                                notificationPayloads=notification_payloads,
                                syncEntities=sync_entities),
                  interrupted=runtime.interrupted)
    if isinstance(action, list):
        result["actions"] = action
        result["action"] = action[0]
    runtime.connection.close()
    return result


def corpus():
    cases = []
    for phase in ("idle", "preparing", "submitted", "unknown", "running"):
        for descendants in (False, True):
            records = seed()
            lead, worker = records["tables"]["runtime_agents"]
            # A disconnected same-root node is not a descendant.
            records["tables"]["runtime_agents"].extend([
                dict(worker, id="unrelated", parentId="missing"),
                dict(worker, id="grandchild", parentId="worker", rootId="legacy-other-root"),
            ])
            if phase != "idle":
                lead.update(status="starting", inFlight=True,
                            startAttempt=dict(id="attempt", epoch=3, submitted=phase != "preparing"))
            if phase in {"submitted", "unknown", "running"}:
                lead.update(threadId="thread", accountKey="fixture-account")
                lead["startAttempt"].update(connectionGeneration=7, requestTicket=91)
            if phase == "unknown":
                lead["startAttempt"]["executionOutcome"] = "unknown"
                lead["error"] = "turn/start response timed out; outcome unknown"
            if phase == "running":
                lead.update(status="running", turnId="turn")
                lead["startAttempt"].update(turnId="turn")
            records["tables"]["runtime_events"] = [dict(id=status, agent="lead", epoch=3, status=status)
                for status in ("pending", "reserved", "dispatching", "uncertain")]
            records["tables"]["runtime_requests"] = [dict(id="question", agent="lead", status="pending", method="agent/asyncQuestion")]
            lead.update(parkedEvent="ready", parkAfterTurn=True,
                        parkReceipt=dict(event="ready", epoch=3, sequence=1), capacityRetryCount=2)
            cases.append(case(phase+"-"+str(descendants), records,
                              dict(action="stop", target="lead", descendants=descendants)))
    deleted = case("delete", seed(), dict(action="delete", target="lead"))
    cases.append(deleted)
    cases.append(case("delete-replay", deleted["expected"]["records"], dict(action="delete", target="lead")))
    cases.append(case("stop-worker-notification", seed(),
                      dict(action="stop", target="worker", descendants=False)))
    cases.append(case("stop-worker-indirect", seed(),
                      dict(action="stop", target="lead", descendants=True)))
    worker_stop = dict(action="stop", target="worker", descendants=False)
    cases.append(case("stop-worker-notification-replay", seed(), [worker_stop, worker_stop]))
    removed_workspace = seed()
    removed_workspace["tables"]["runtime_agents"][1].update(
        imageWorkspace=True, imageWorkspaceReady=True, imageWorkspacePhase="removed")
    cases.append(case("delete-removed-image-workspace", removed_workspace,
                      dict(action="delete", target="lead")))
    unrelated_work = seed()
    unrelated_work["tables"]["runtime_work"] = [dict(
        id="unrelated-work", owner="outside", rootId="outside", status="ready",
        version=1, title="Unrelated", releases=[])]
    cases.append(case("delete-unrelated-open-work", unrelated_work,
                      dict(action="delete", target="lead")))
    review_work = seed()
    review_work["tables"]["runtime_work"] = [dict(
        id="worker-review", owner="worker", rootId="lead", status="review",
        version=1, title="Under review", releases=[])]
    cases.append(case("delete-worker-review-work", review_work,
                      dict(action="delete", target="lead")))
    return cases


if __name__ == "__main__":
    print(json.dumps(corpus()))
