"""Python-written fresh and frozen submission fixtures; no runtime or provider."""
import contextlib
import copy
import io
import json
from pathlib import Path
import runpy
import sqlite3
import sys

# The shared Claude oracle installs oracle_guard before application imports.
root = Path(sys.argv[1])
sys.argv = [__file__, "--template", str(root / "template.sqlite3")]
with contextlib.redirect_stdout(io.StringIO()):
    corpus = runpy.run_path(str(Path(__file__).parent / "claude_runtime/oracle.py"))
base = corpus["cases"][0]
followup = corpus["case"]("input_same", followup=("input", lambda a, t, i, c: None))
fixtures = []
for retry in (False, True):
    source = copy.deepcopy(followup["input"] if retry else base["input"])
    agent = source["agent"]
    current = copy.deepcopy(agent["startAttempt"])
    current.update(submitted=False, threadId="thread", connectionId="connection", created=130.0)
    current.pop("nativeOperationId", None)
    current.pop("claudeInputRequest", None)
    agent["startAttempt"] = current
    agent["status"] = "starting"
    agent["inFlight"] = True
    frozen = followup["expected"] if retry else source["attempt"]["claudeInputRequest"]
    params = dict(frozen["configuration"], input=frozen["input"],
                  clientUserMessageId=frozen["clientUserMessageId"], dynamicTools=[{"name": "refreshed"}])
    text = "compiled original"
    path = root / ("retry.sqlite3" if retry else "fresh.sqlite3")
    with sqlite3.connect(root / "template.sqlite3") as template, sqlite3.connect(path) as db:
        template.backup(db)
        db.execute("UPDATE runtime_agents SET record=? WHERE id='agent'", (json.dumps(agent),))
        if retry:
            db.execute("UPDATE runtime_execution_attempts SET record=? WHERE id='attempt'", (json.dumps(source["attempt"]),))
            db.execute("INSERT INTO runtime_execution_attempts VALUES (?,'run',?)", (current["id"], json.dumps(current)))
        else:
            db.execute("UPDATE runtime_execution_attempts SET record=? WHERE id='attempt'", (json.dumps(current),))
        for event in source["events"]:
            db.execute("UPDATE runtime_events SET status='reserved',turn_id=NULL,error=NULL WHERE id=?", (event["id"],))
            db.execute("UPDATE runtime_event_meta SET record=? WHERE id=?", (json.dumps(event["metadata"]), event["id"]))
        db.execute("INSERT INTO runtime_items VALUES ('agent:one','agent',100,?)", (json.dumps(dict(id="agent:one",role="user",text=text)),))
        db.row_factory = sqlite3.Row
        rows = list(db.execute("SELECT * FROM runtime_events ORDER BY created"))
        expected = corpus["oracle"].capture_input(db, agent, rows, params, text)
        db.commit()
    fixtures.append(dict(retry=retry, path=str(path), agent=agent, attempt=current,
                         params=params, text=text, frozen=expected, transport=source["transport"]))
print(json.dumps(fixtures))
