#!/usr/bin/env python3
"""Import projects and their Git worktrees into Studio's native Layr store.

The Rust command owns the store path, code rules, receipts, and migration logic.
Use --dry-run to inspect a migration without changing the store.
"""
from __future__ import annotations

import os
import shutil
import sys


def main() -> None:
    binary = os.environ.get("STUDIO_RUNTIME_SERVER") or shutil.which("studio-runtime-server")
    if not binary:
        raise SystemExit("studio-runtime-server is missing. Run just install first.")
    os.execv(binary, [binary, "native-layr", "migrate", *sys.argv[1:]])


if __name__ == "__main__":
    main()
