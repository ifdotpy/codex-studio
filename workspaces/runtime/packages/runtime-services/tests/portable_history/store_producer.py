"""Create a disposable Python runtime state for the Rust archive writer."""

import json
import os
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()

SERVER = pathlib.Path(sys.argv[1])
ROOT = pathlib.Path(sys.argv[2])
for variable, name in (("HOME", "home"), ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude")):
    directory = ROOT / name
    directory.mkdir()
    os.environ[variable] = str(directory)
sys.path.insert(0, str(SERVER))

from codex_canvas import Canvas  # noqa: E402
from codex_account_transfer import AccountTransfers  # noqa: E402
from codex_runtime import Runtime  # noqa: E402
from codex_terminals import TerminalManager  # noqa: E402


class QuietRuntime(Runtime):
    def schedule(self):
        return


Canvas(ROOT)
TerminalManager(ROOT)
runtime = QuietRuntime(ROOT, lambda *_args, **_kwargs: None)
assert runtime.accounts.base_home.is_relative_to(ROOT), "fixture owns its provider home"
agent = runtime.create(
    {"name": "portable history fixture", "cwd": str(ROOT), "prompt": "synthetic"},
    defer=True,
)
agent["threadId"] = "native-thread"
agent["provider"] = "claude"
agent["workerDefaults"] = {"model": "fixture-model"}
agent["claudeOptions"] = {"thinking": "enabled"}
item = {
    "id": "fixture-item",
    "role": "assistant",
    "text": "short stub",
    "truncated": True,
    "inputs": [{"id": "fixture-event", "text": "omitted", "truncated": True}],
    "assets": [{"path": '/tmp/archive-fixture "日本語.png'}],
}
full_text = "Python-written full transcript 😀\n" * 700
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    db.execute(
        "INSERT INTO runtime_items VALUES(?,?,?,?)",
        (item["id"], agent["id"], json.dumps(item, ensure_ascii=False), 1.25),
    )
    restored = {
        "id": "fixture-restored-item",
        "role": "assistant",
        "text": "restored content must stay out of context",
        "afterRestore": True,
    }
    db.execute(
        "INSERT INTO runtime_items VALUES(?,?,?,?)",
        (restored["id"], agent["id"], json.dumps(restored), 1.5),
    )
    for index, after_restore in enumerate(([], {}), start=1):
        legacy_restored = {
            "id": f"fixture-restored-empty-{index}",
            "role": "assistant",
            "text": f"empty afterRestore fixture {index}",
            "afterRestore": after_restore,
        }
        db.execute(
            "INSERT INTO runtime_items VALUES(?,?,?,?)",
            (legacy_restored["id"], agent["id"], json.dumps(legacy_restored), 1.5 + index),
        )
    db.execute("INSERT INTO runtime_item_fulltext VALUES(?,?)", (item["id"], full_text))
    db.execute(
        "INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
        ("fixture-event", agent["id"], "user", "Python-written original input", "delivered",
         2.5, agent["epoch"], None, None),
    )
transfer = AccountTransfers.__new__(AccountTransfers)
transfer.rt = runtime
print(json.dumps({
    "agent": agent["id"],
    "epoch": agent["epoch"],
    "settings": transfer.settings_snapshot(agent),
}))
runtime.close()
