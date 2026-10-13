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


class LifecycleRuntime(MemoryRuntime):
    def interrupt(self, agent):
        # Only the provider boundary is injected; stop and every stored write
        # execute the actual Runtime implementation.
        self.interrupted.append(copy.deepcopy(agent))


def case(name, records, action):
    runtime = LifecycleRuntime(records)
    runtime.interrupted = []
    before = runtime.snapshot()
    with patch("time.time", return_value=NOW):
        if action["action"] == "delete":
            result = Runtime.delete_conversation(runtime, action["target"])
        else:
            result = Runtime.stop(runtime, action["target"], action["descendants"])
    result = dict(name=name, records=before, action=action, now=NOW,
                  expected=dict(result=result, records=runtime.snapshot()),
                  interrupted=runtime.interrupted)
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
    return cases


if __name__ == "__main__":
    print(json.dumps(corpus()))
