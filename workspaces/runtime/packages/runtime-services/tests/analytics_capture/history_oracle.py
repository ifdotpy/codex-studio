"""Run the real Python tail importer against a disposable database and rollout."""
from contextlib import contextmanager
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
from codex_analytics_history import AnalyticsHistoryMixin

root, rollout = Path(sys.argv[1]), Path(sys.argv[2])
with sqlite3.connect(root / "canvas.sqlite3") as canvas:
    agent = json.loads(canvas.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])

class Accounts:
    def home(self, _account):
        return str(rollout.parent)

class Importer(AnalyticsMixin, AnalyticsHistoryMixin):
    accounts = Accounts()
    def records(self, *_args):
        return []
    def agent(self, *_args):
        return agent
    def analytics_budget_capture(self, *_args, **_kwargs):
        return None
    def _analytics_rollout_path(self, *_args):
        return rollout, None
    @contextmanager
    def analytics_history_db(self):
        db = sqlite3.connect(root / "analytics.sqlite3")
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

state = {"id":"a:default:thread", "agent":"a", "accountKey":"default", "threadId":"thread", "deletedAt":agent.get("deletedAt"), "offset":0, "importedRecords":0, "malformedLines":0, "context":{"threadId":"thread"}}
with patch("time.time", return_value=200.0):
    Importer()._analytics_history_import(agent, state["id"], state, False, 1024*1024, 128)
with sqlite3.connect(root / "analytics.sqlite3") as db:
    db.row_factory = sqlite3.Row
    result = {}
    for table in ("analytics_agents", "analytics_turns", "analytics_items", "analytics_usage", "analytics_notifications", "analytics_limits", "analytics_meta", "analytics_usage_roots", "analytics_history"):
        result[table] = [dict(row) for row in db.execute(f"SELECT * FROM {table} ORDER BY rowid")]
print(json.dumps(result))
