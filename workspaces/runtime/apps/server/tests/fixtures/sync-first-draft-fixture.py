"""Minimal real HTTP server for the first-draft-push contract."""
import json
import os
import sys

from pathlib import Path

from codex_layout import SERVER_SOURCE_ROOT

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

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
