"""Validate a Rust-written archive with the Python portable-history reader."""

import json
import sys
from pathlib import Path

SERVER = Path(sys.argv[1])
ROOT = Path(sys.argv[2])
sys.path.insert(0, str(SERVER))

from codex_portable_history import history_context  # noqa: E402


class Runtime:
    def __init__(self, root):
        self.root = Path(root)


descriptor = json.load(sys.stdin)
context = history_context(Runtime(ROOT), descriptor)
print(json.dumps({"context": context}, ensure_ascii=False, separators=(",", ":")))
