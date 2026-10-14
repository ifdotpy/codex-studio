"""Emit complete terminal read results from the Python implementation."""

import json
import os
from pathlib import Path
import sqlite3
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()

SERVER_SOURCE = Path(__file__).resolve().parents[3] / "apps/server/src"
sys.path.insert(0, str(SERVER_SOURCE))
from codex_canvas import Canvas
from codex_terminals import HISTORY_LIMIT, TerminalManager


def bootstrap_schema(root: Path) -> None:
    snapshot = json.loads((SERVER_SOURCE.parent / "tests/fixtures/runtime-state-schema.json").read_text())
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        connection = sqlite3.connect(root / name)
        try:
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("PRAGMA synchronous=FULL")
            statements = sorted(
                [
                    (entry["type"], entry["name"], entry["sql"])
                    for entry in snapshot[name]["sqlite_master"]
                ],
                key=lambda item: {"table": 0, "index": 1, "view": 2}.get(item[0], 3),
            )
            for _, object_name, statement in statements:
                exists = connection.execute(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?)", (object_name,)
                ).fetchone()[0]
                if not exists:
                    connection.execute(statement)
            connection.commit()
        finally:
            connection.close()


def main(root: Path) -> None:
    Canvas(root)
    bootstrap_schema(root)
    manager = TerminalManager(root)
    terminal_id = "terminal-corpus"
    record = {
        "id": terminal_id,
        "agent": "python-agent",
        "title": "Python written terminal",
        "cwd": str(root),
        "status": "exited",
        "created": 1.0,
        "updated": 2.0,
        "exitCode": 7,
    }
    closed = {**record, "id": "terminal-closed", "status": "closed", "created": 3.0}
    with manager.db() as db:
        db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (terminal_id, json.dumps(record), "", 0))
        db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (closed["id"], json.dumps(closed), "", 0))
    published = []
    manager._publish = lambda *resources: published.extend(
        {"kind": ref.root.kind, "terminalId": getattr(ref.root, "terminalId", None)}
        for ref in resources
    )
    manager.append(terminal_id, "alpha-Ж🙂-omega")

    output_cases = []
    for offset in (0, 6, 15, 100):
        output_cases.append({"offset": offset, "value": manager.output(terminal_id, offset)})
    history_cases = []
    for offset, limit in ((0, 7), (6, 4), (100, 3)):
        history_cases.append({"offset": offset, "limit": limit, "value": manager.history_output(terminal_id, offset, limit)})
    sizes = []
    for name, data in (
        ("defaults", {}), ("minimum", {"cols": 2, "rows": 1}),
        ("maximum", {"cols": 1000, "rows": 500}), ("bool", {"cols": True}),
        ("columns-low", {"cols": 1}), ("columns-high", {"cols": 1001}),
        ("rows-low", {"rows": 0}), ("rows-high", {"rows": 501}),
    ):
        try:
            value = {"cols": TerminalManager.dimensions(data)[0], "rows": TerminalManager.dimensions(data)[1]}
            sizes.append({"name": name, "value": value})
        except ValueError as error:
            sizes.append({"name": name, "error": str(error)})

    restart_id = "terminal-restart"
    restart_record = {**record, "id": restart_id, "status": "running", "created": 4.0}
    with manager.db() as db:
        db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (restart_id, json.dumps(restart_record), "pending", 0))
    import unittest.mock

    with unittest.mock.patch("codex_terminals.time.time", return_value=9.0):
        restarted = TerminalManager(root)
    restart_row = restarted.output(restart_id)

    large = []
    if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL") == "1":
        large_id = "terminal-large"
        large_record = {**record, "id": large_id, "created": 5.0}
        with manager.db() as db:
            db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)", (large_id, json.dumps(large_record), "", 0))
        manager.append(large_id, "x" * (HISTORY_LIMIT + 4))
        large = [manager.output(large_id, 0), manager.history_output(large_id, 0, 7), manager.history_output(large_id, HISTORY_LIMIT, 7)]

    tables = {}
    with manager.db() as db:
        for table in (
            "user_terminals", "user_terminal_receipts", "user_terminal_output",
            "user_terminal_event_cursor", "user_terminal_decoder_state",
        ):
            tables[table] = [list(row) for row in db.execute(f'SELECT * FROM "{table}" ORDER BY rowid')]
    print(json.dumps({
        "listing": manager.listing(),
        "output": output_cases,
        "history": history_cases,
        "sizes": sizes,
        "restart": restart_row,
        "events": published,
        "tables": tables,
        "large": large,
    }, ensure_ascii=False, sort_keys=True))
    restarted.close()
    manager.close()


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve())
