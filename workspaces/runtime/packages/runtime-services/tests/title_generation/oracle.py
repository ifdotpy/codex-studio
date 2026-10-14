"""Call the real Python title helpers against Python-written transcript rows."""
import json
import sqlite3
import sys
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(HERE.parents[3] / "apps/server/src"))
import codex_rename


class Accounts:
    def get(self, _key):
        return object()

    def home(self, _key):
        return "/tmp/title-oracle-home"


class Runtime:
    factory = object()
    accounts = Accounts()

    def __init__(self, catalog):
        self._catalog = catalog

    def catalog(self, _account_key):
        return self._catalog


def run(value):
    agent = value["agent"]
    catalog = value["catalog"]
    db = sqlite3.connect(":memory:")
    db.execute("CREATE TABLE runtime_items(id TEXT,agent TEXT,record TEXT,created REAL)")
    for record in value["items"]:
        db.execute(
            "INSERT INTO runtime_items VALUES(?,?,?,?)",
            (record["id"], agent["id"], json.dumps(record), record["created"]),
        )
    first, recent = codex_rename._messages(db, agent)
    model, effort = codex_rename._title_model(catalog, agent)
    captured = {}

    def fake_run(command, **kwargs):
        captured["prompt"] = kwargs.get("input") or command[-1]
        output = command[command.index("--output-last-message") + 1]
        Path(output).write_text(value.get("providerOutput", "A short chat title"))
        return type("Completed", (), {"returncode": 0, "stdout": ""})()

    runtime = Runtime(catalog)
    with patch.dict("os.environ", {"CODEX_BIN": "injected-title-provider"}), patch.object(
        codex_rename.subprocess, "run", fake_run
    ):
        title = codex_rename._generate(runtime, agent, first, recent)
    return {
        "model": model,
        "effort": effort,
        "first": first,
        "recent": recent,
        "prompt": captured["prompt"],
        "title": title,
    }


for line in sys.stdin:
    case = json.loads(line)
    if case is None:
        break
    print(json.dumps(run(case), ensure_ascii=False), flush=True)
