"""Generate or check schemas for the persisted formats read by the runtime."""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from codex_layout import REPOSITORY_ROOT
from studio_api.persisted_models import STATE_SCHEMA_MODELS, state_schema_document


OUTPUT_ROOT = REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http"


def generated_schemas() -> dict[Path, object]:
    return {
        OUTPUT_ROOT / name: state_schema_document(model)
        for name, model in STATE_SCHEMA_MODELS.items()
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="write committed schemas")
    mode.add_argument("--check", action="store_true", help="fail when committed schemas are stale")
    args = parser.parse_args(argv)
    for path, expected in generated_schemas().items():
        try:
            current = json.loads(path.read_text(encoding="utf-8")) if path.exists() else None
        except (OSError, UnicodeError, json.JSONDecodeError):
            current = None
        if args.check:
            if current != expected:
                print(f"{path.relative_to(REPOSITORY_ROOT)} is stale; run `pnpm run state-schemas:generate`", file=sys.stderr)
                return 1
        else:
            path.write_text(
                json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
