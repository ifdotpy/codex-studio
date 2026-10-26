#!/usr/bin/env python3
"""Import projects and their Git worktrees into Studio's native Layr store.

The Rust command owns the store path, file rules, receipts, and migration logic.
Use --dry-run to inspect a migration without changing the store.
"""
from __future__ import annotations

import os
import shutil
import sys


def main() -> None:
    if "--help" in sys.argv[1:]:
        print("Usage: codex_layr_native_migration.py [--state DIR] [--dry-run] [--worktrees all|dirty] [--without-git PROJECT] [--skip-worktree PATH] PROJECT...")
        print("Wait for current WT turns to finish. Pause their chats in Studio before import.")
        print("Each selected Git worktree becomes a separate Layr line. Original folders remain intact.")
        print("Existing WT chats keep their Git paths. The script does not pause or convert chats.")
        print("Use --dry-run to print the worktree map and command list for each project.")
        print("--worktrees defaults to all. dirty selects changed files and HEAD commits absent from local branches and remote-tracking refs.")
        print("--without-git PROJECT omits that project's top-level .git. Nested repositories follow normal admission.")
        print("--skip-worktree PATH records an explicit worktree exclusion.")
        print("After import, start a new Layr chat for the same project folder.")
        print("Review each imported line before its printed merge command.")
        print("Keep old chats paused. Repeated imports replay receipts, without later Git edits.")
        return
    binary = os.environ.get("STUDIO_RUNTIME_SERVER") or shutil.which("studio-runtime-server")
    if not binary:
        raise SystemExit("studio-runtime-server is missing. Run just install first.")
    os.execv(binary, [binary, "native-layr", "migrate", *sys.argv[1:]])


if __name__ == "__main__":
    main()
