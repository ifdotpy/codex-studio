"""Prepare a Python-written runtime database template without starting services."""

import os
import pathlib
import sys
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from unittest.mock import patch

sys.argv.append("--isolated")
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "tests/turn_lifecycle"))
import oracle

import codex_accounts
import codex_runtime

root = pathlib.Path(sys.argv[1])
root.mkdir(parents=True, exist_ok=True)
codex_home = pathlib.Path(os.environ["CODEX_HOME"])
codex_home.mkdir(parents=True, exist_ok=True)

with (
    patch.object(codex_accounts, "codex_home", lambda: codex_home),
    patch.object(codex_accounts, "auth_metadata", lambda *_: {"status": "signedOut"}),
    patch.object(ThreadPoolExecutor, "submit", lambda *_args, **_kwargs: Future()),
    patch.object(threading.Thread, "start", lambda _self: None),
    patch.object(threading.Thread, "join", lambda _self, *_args, **_kwargs: None),
    patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *_args: None),
):
    runtime = oracle.QuietRuntime(root, lambda *_args, **_kwargs: None)
    runtime.close()
