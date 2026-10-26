#!/usr/bin/env python3
"""Write state corpus records through Python's actual lifecycle writer."""

import json
import os
import sys
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).absolute().parent / "turn_lifecycle"))
from oracle_guard import install_guard

_GUARD = install_guard(block_processes=True)

server_source = Path(sys.argv[1])
root = Path(sys.argv[2])
sys.path.insert(0, str(server_source))

from codex_native_runtime import NativeRuntimeUpdates

if len(sys.argv) > 3 and sys.argv[3] == "--read":
    updates = NativeRuntimeUpdates(SimpleNamespace(root=root))
    updates._load_selected()
    print(json.dumps({
        "selected": updates.selected(),
        "accounts": updates.state["accounts"],
        "status": updates.state["status"],
    }, sort_keys=True, separators=(",", ":")))
    raise SystemExit(0)

cases = {
    "fresh": {
        "status": "checking",
        "checkedAt": None,
        "selected": None,
        "candidates": [],
        "accounts": {},
    },
    "rejected": {
        "status": "failed",
        "checkedAt": 9.25,
        "selected": None,
        "candidates": [
            {"path": "/fixture/codex", "status": "rejected", "error": "compatibility check failed"}
        ],
        "accounts": {},
    },
    "rotation": {
        "status": "ready",
        "checkedAt": 7.5,
        "selected": {
            "path": "/fixture/native-runtime/builds/hash/codex",
            "sourcePath": "/fixture/codex",
            "version": "1.2.3",
            "sha256": "a" * 64,
            "validatedAt": 4.5,
            "checks": {"schema": {"methods": ["initialize"]}, "smoke": ["ok"]},
            "bundleSha256": "b" * 64,
            "companions": {
                "codex-code-mode-host": {
                    "path": "/fixture/native-runtime/builds/hash/codex-code-mode-host",
                    "sourcePath": "/fixture/codex-code-mode-host",
                    "sha256": "c" * 64,
                    "sourceIdentity": {"path": "/fixture/codex-code-mode-host", "size": 8},
                }
            },
            "approvalRevision": 2,
            "sourceIdentity": {"path": "/fixture/codex", "size": 12, "mtimeNs": 123},
        },
        "candidates": [
            {"path": "/fixture/codex", "status": "approved", "version": "1.2.3",
             "identity": {"path": "/fixture/codex", "mtimeNs": 123},
             "companionIdentity": {"codex-code-mode-host": {"path": "/fixture/helper"}}}
        ],
        "accounts": {
            "default": {
                "status": "waiting",
                "version": None,
                "pid": None,
                "targetVersion": "2.0.0",
                "reason": "active session",
                "updatedAt": 4.0,
                "rotation": {
                    "id": "op-a",
                    "phase": "closing",
                    "old": {"pid": 42, "generation": 9},
                    "target": {"path": "/fixture/native-runtime/builds/hash/codex"},
                },
            }
        },
        "futurePythonField": {"preserve": [None, False, 8]},
    },
}
if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL") == "1":
    cases.update({
        "empty-accounts": {"status": "ready", "checkedAt": 0.0, "selected": None,
                           "candidates": [], "accounts": {}},
        "unknown-account-state": {
            "status": "ready",
            "checkedAt": 1.0,
            "selected": None,
            "candidates": [{"path": "", "status": "rejected", "error": ""}],
            "accounts": {"secondary": {"status": "failed", "rotation": None}},
            "extension": {"newer-python-field": {"absent": False, "null": None}},
        },
    })

written = {}
for name, record in cases.items():
    case_root = root / name
    updates = NativeRuntimeUpdates(SimpleNamespace(root=case_root))
    updates.state = record
    updates._save()
    written[name] = updates.state
print(json.dumps(written, sort_keys=True, separators=(",", ":")))
