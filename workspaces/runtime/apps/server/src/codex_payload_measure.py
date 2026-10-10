"""Sample payload field sizes from the state DB without a table scan."""

import argparse
import json
import math
from pathlib import Path
import sqlite3
import statistics

from codex_state import state_dir as default_state_dir


TABLES = {
    "runtime_items": "record",
    "runtime_tasks": "record",
    "runtime_checkpoints": "record",
    "runtime_tool_requests": "record",
    "runtime_tool_results": "result",
}


def percentile(values, q):
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, math.ceil(q * len(ordered)) - 1)]


def sample(database: Path, sample_rows: int = 500) -> dict:
    uri = database.resolve().as_uri() + "?mode=ro"
    db = sqlite3.connect(uri, uri=True, timeout=3)
    try:
        result = {"databaseBytes": database.stat().st_size, "tables": {}}
        for table, column in TABLES.items():
            max_rowid = db.execute(f"SELECT max(rowid) FROM {table}").fetchone()[0] or 0
            if max_rowid == 0:
                result["tables"][table] = {"maxRowid": 0, "samples": 0, "json": {}, "fields": {}}
                continue
            positions = sorted({max(1, min(max_rowid, int((i + 0.5) * max_rowid / sample_rows)))
                                for i in range(min(sample_rows, max_rowid))})
            json_sizes = []
            fields = {}
            for position in positions:
                row = db.execute(f"SELECT {column} FROM {table} WHERE rowid>=? LIMIT 1",
                                 (position,)).fetchone()
                if not row or not row[0]:
                    continue
                encoded = row[0]
                try:
                    record = json.loads(encoded)
                except (ValueError, TypeError):
                    continue
                json_sizes.append(len(encoded.encode("utf-8")))
                if isinstance(record, dict):
                    for name, value in record.items():
                        field_bytes = len(json.dumps(value, ensure_ascii=False,
                                                     separators=(",", ":")).encode("utf-8"))
                        fields.setdefault(name, []).append(field_bytes)
            result["tables"][table] = {
                "maxRowid": max_rowid,
                "samples": len(json_sizes),
                "json": {"p50": percentile(json_sizes, .5), "p95": percentile(json_sizes, .95),
                         "max": max(json_sizes, default=0)},
                "fields": {name: {"n": len(values), "p50": percentile(values, .5),
                                  "p95": percentile(values, .95), "max": max(values),
                                  "gt4KiBPercent": round(100 * sum(v > 4096 for v in values) / len(values), 1),
                                  "gt64KiBPercent": round(100 * sum(v > 65536 for v in values) / len(values), 1)}
                           for name, values in sorted(fields.items())},
            }
        return result
    finally:
        db.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, default=default_state_dir())
    parser.add_argument("--sample-rows", type=int, default=500)
    args = parser.parse_args()
    if args.sample_rows < 1:
        parser.error("sample-rows must be positive")
    print(json.dumps(sample(args.state_dir / "canvas.sqlite3", args.sample_rows), sort_keys=True))


if __name__ == "__main__":
    main()
