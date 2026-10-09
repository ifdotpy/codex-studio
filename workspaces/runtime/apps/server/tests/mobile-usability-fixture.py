"""Add a recorded transcript to the existing isolated mobile UI fixture."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

import json
import os
from pathlib import Path
import runpy
import sys

sys.dont_write_bytecode = True
repo = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_TESTS_ROOT))
from test_isolation import isolate_api_schema_cache
isolate_api_schema_cache()
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_canvas

if os.environ.get("MOBILE_UI_DIST"):
    codex_canvas.WEB = Path(os.environ["MOBILE_UI_DIST"])

make_server = codex_canvas.make_server
items = json.loads(Path(sys.argv.pop(2)).read_text())


def audit_server(canvas, *args, **kwargs):
    runtime = canvas.runtime
    with runtime.lock, runtime.db() as db:
        lead = next(agent for agent in runtime.records(db, "agents")
                    if agent["name"] == "Release lead")
        db.execute("DELETE FROM runtime_items WHERE agent=?", (lead["id"],))
        for value in items:
            item = dict(value)
            runtime.item(db, lead["id"], item.pop("id"), item.pop("role"),
                         item.pop("text"), **item)
        worker = next(agent for agent in runtime.records(db, "agents")
                      if agent["name"] == "Worker 07")
        worker["error"] = "The release command exited with code 1. " + "/workspace/release/reports/mobile/" * 6
        runtime.put(db, "agents", worker)
    return make_server(canvas, *args, **kwargs)


codex_canvas.make_server = audit_server
runpy.run_path(str(SERVER_TESTS_ROOT / "simple-ui-fixture.py"), run_name="__main__")
