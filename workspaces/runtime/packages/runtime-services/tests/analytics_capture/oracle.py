"""Real Python capture and parser oracle; no Runtime and no provider execution."""
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).parents[4] / "apps/server/src"))
from codex_analytics import AnalyticsMixin
from analytics.rollout_parser import rollout_actions

class Capture(AnalyticsMixin):
    def records(self, *_args):
        return []
    def analytics_budget_capture(self, *_args, **_kwargs):
        # Budget is independently captured by the notification/collector caller.
        return None

TABLES = ("analytics_agents", "analytics_turns", "analytics_items", "analytics_usage", "analytics_notifications", "analytics_limits", "analytics_meta", "analytics_usage_roots")

def rows(db):
    return {table: [dict(row) for row in db.execute(f"SELECT * FROM {table} ORDER BY rowid")] for table in TABLES}


def main():
    corpus = json.loads(Path(__file__).with_name("corpus.json").read_text())
    fixture = Capture()
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    with patch("time.time", return_value=200.0):
        fixture.analytics_init(db)
        db.commit()
        template = "\n".join(db.iterdump())
        cases = []
        for family in corpus["captures"]:
            connection = sqlite3.connect(":memory:")
            connection.row_factory = sqlite3.Row
            connection.executescript(template)
            fixture = Capture()
            for index, step in enumerate(family["steps"]):
                before = rows(connection)
                agent = {**corpus["agent"], **step.get("agent", {})}
                params = step["params"]
                source = step.get("source", "live")
                if step.get("model"):
                    fixture.analytics_model_payload(connection, agent, params, at=step["at"], turn_id=params.get("_analyticsTurnId"), source=source)
                else:
                    fixture.analytics_event(connection, agent, step["method"], params, at=step["at"], source=source)
                connection.commit()
                if step.get("seedOnly"):
                    continue
                canonical = hashlib.sha256(json.dumps([params.get("responseId"), params.get("requestUsage")], sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest() if params.get("usageSource") == "claudeResponse" else None
                cases.append({"canonicalClaudeFingerprint": canonical, "name": f'{family["name"]}/{index}', "step": step, "agent": agent, "before": before, "after": rows(connection)})
        parsers = []
        for family in corpus["rollouts"]:
            context = {"threadId": corpus["agent"]["threadId"]}
            for index, record in enumerate(family["records"]):
                before = json.loads(json.dumps(context))
                actions = rollout_actions(record, context, f"record-{index}", 50.0)
                parsers.append({"name": f'{family["name"]}/{index}', "record":record,"context":before,"identity":f"record-{index}","actions":actions,"after":json.loads(json.dumps(context))})
    print(json.dumps({"template":template,"capture":cases,"rollout":parsers}))

if __name__ == "__main__":
    main()
