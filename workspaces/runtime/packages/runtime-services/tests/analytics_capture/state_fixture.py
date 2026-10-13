"""Create a disposable Python-written schema and analytics state without Runtime."""
import json
from pathlib import Path
import sqlite3
import sys

sys.path.insert(0, str(Path(__file__).parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).parents[4] / "apps/server/src"))
from codex_analytics import AnalyticsMixin
from codex_analytics_history import AnalyticsHistoryMixin

root = Path(sys.argv[1])
fixture_path = Path(__file__).parents[3] / "runtime-store/tests/fixtures/live-state-schema.json"
schema = json.loads(fixture_path.read_text())
agent = json.loads(Path(__file__).with_name("corpus.json").read_text())["agent"]
for database in ("canvas.sqlite3", "analytics.sqlite3"):
    db = sqlite3.connect(root / database)
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("PRAGMA synchronous=FULL")
    objects = sorted(schema[database]["sqlite_master"], key=lambda obj: (dict(table=0, index=1, trigger=2, view=3).get(obj["type"], 4), obj["name"]))
    for obj in objects:
        if obj["sql"] and obj["name"] not in {"runtime_search_next_config", "runtime_search_next_data", "runtime_search_next_docsize", "runtime_search_next_idx"}:
            db.execute(obj["sql"])
    if database == "canvas.sqlite3":
        # Seed the synthetic runtime row with projections temporarily disabled;
        # restore the exact Python schema before opening the Rust store.
        for name, in db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_agents'").fetchall():
            db.execute('DROP TRIGGER "' + name.replace('"', '""') + '"')
        db.execute("INSERT INTO runtime_agents(id,record) VALUES (?,?)", (agent["id"], json.dumps(agent)))
        for obj in objects:
            if obj["type"] == "trigger" and obj["table"] == "runtime_agents":
                db.execute(obj["sql"])
    else:
        class Writer(AnalyticsMixin, AnalyticsHistoryMixin):
            def records(self, *_args):
                return []
        writer = Writer()
        writer.analytics_init(db)
        writer.analytics_history_init(db)
        writer.analytics_agent(db, agent)
    db.commit()
    db.close()
print(json.dumps(agent))
