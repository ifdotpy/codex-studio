"""Report environment key names after the real Python terminal launch path."""
import base64
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

source, scratch, private_home = map(Path, sys.argv[1:4])
sys.path.insert(0, str(source))

from codex_runtime import app_server_launch_environment
from codex_process_supervisor import native_launch_environment
from codex_terminals import terminal_environment_overlay

server_environment = app_server_launch_environment(home=private_home, isolated=True)
if os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1":
    with sqlite3.connect(scratch / "supervisor.sqlite3") as db:
        db.executescript(
            "CREATE TABLE IF NOT EXISTS handles(id TEXT PRIMARY KEY, signature TEXT, pid INTEGER, closed_at REAL);"
            "CREATE TABLE IF NOT EXISTS child_identities(handle TEXT PRIMARY KEY, pid INTEGER, start_time TEXT);"
        )
    server_environment = native_launch_environment(
        scratch, "terminal-environment-oracle", ["codex"], server_environment, None
    )

for key, value in terminal_environment_overlay().items():
    if value is None:
        server_environment.pop(key, None)
    else:
        server_environment[key] = value

result = subprocess.run(
    ["/usr/bin/env", "-0"], env=server_environment, check=True, capture_output=True
)
keys = sorted(
    base64.b64encode(entry.partition(b"=")[0]).decode("ascii")
    for entry in result.stdout.split(b"\0")
    if entry
)
print("\n".join(keys))
