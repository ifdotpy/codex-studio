"""Compare the real bounded terminal-error repair on disposable state."""
import json
from pathlib import Path
import sqlite3
import sys

sys.path.insert(0, str(Path(__file__).parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).parents[4] / "apps/server/src"))
from codex_analytics_history import repair_terminal_errors

root = Path(sys.argv[1])
db = sqlite3.connect(root / "analytics.sqlite3")
db.execute("ATTACH DATABASE ? AS canvas", (str(root / "canvas.sqlite3"),))
# The real repair helper supports the legacy combined database. Clone the
# relevant Python-written runtime table into this disposable oracle database.
db.execute("CREATE TABLE runtime_items AS SELECT * FROM canvas.runtime_items")
repair_terminal_errors(db)
db.commit()
print(json.dumps({"turns": [json.loads(r[0]) for r in db.execute("SELECT record FROM analytics_turns ORDER BY id")], "state": json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'").fetchone()[0])}))
