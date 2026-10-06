"""Minimal real HTTP server for the first-draft-push contract."""
from pathlib import Path
import json
import os
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from codex_canvas import Canvas, make_server
from codex_runtime import Runtime

canvas = Canvas(Path(sys.argv[1]))
server = make_server(canvas, 0, unix_socket=True)
canvas.runtime = Runtime(canvas.root)
print(json.dumps({"port": server.server_port, "token": server.context.token}), flush=True)
try:
    server.serve_forever()
finally:
    if server.unix_server:
        server.unix_server.shutdown()
    server.server_close()
    canvas.runtime.close()
