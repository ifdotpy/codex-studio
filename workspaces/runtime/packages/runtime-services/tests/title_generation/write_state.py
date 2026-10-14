"""Write a small valid agent and transcript corpus with the Python models."""
import json
import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()
sys.path.insert(0, str(HERE.parents[3] / "apps/server/src"))
from studio_api.persisted_models import PersistedAgentRecord

database = Path(sys.argv[1])
agent = PersistedAgentRecord.model_validate({
    "id": "title-agent",
    "rootId": "title-agent",
    "epoch": 1,
    "status": "idle",
    "name": "Original conversation name",
    "provider": "codex",
    "model": "gpt-6",
    "prompt": "Start the title fixture",
    "cwd": "/fixture",
    "accountKey": "default",
    "isLead": False,
    "needsTitle": True,
    "manualName": False,
    "autoWake": False,
}).model_dump(mode="json", by_alias=True, exclude_none=True)
items = [
    {"id": "title-item-1", "role": "user", "title": "You", "text": "Plan a dashboard", "created": 1.0},
    {"id": "title-item-2", "role": "assistant", "title": "Agent", "text": "Use weekly trends", "created": 2.0},
]
with sqlite3.connect(database) as db:
    db.execute("INSERT INTO runtime_agents(id,record) VALUES(?,?)", (agent["id"], json.dumps(agent)))
    for item in items:
        db.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES(?,?,?,?)",
            (item["id"], agent["id"], json.dumps(item), item["created"]),
        )
