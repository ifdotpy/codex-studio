#!/usr/bin/python3 -I
"""Ask the person, through Studio, to open a link in the browser of the Mac.

The VM installs this file as /usr/local/bin/studio-browser and links /usr/local/bin/xdg-open
to it. Agent processes have BROWSER=/usr/local/bin/studio-browser. A sign-in command such as
`kubectl oidc-login` or `aws sso login` runs it with one URL. It sends the URL to the Studio
guest service and exits: 0 when Studio shows the person an approval card, 1 when the guest
refuses the link. The person approves in Studio; nothing opens without that approval.
(docs/vm-browser-bridge.md)
"""
from __future__ import annotations

import json
import os
import socket
import sys

SOCKET = "/run/codex-studio-browser/open.sock"


def main(arguments: list[str]) -> int:
    if len(arguments) != 1:
        print("studio-browser: give exactly one URL", file=sys.stderr)
        return 2
    path = os.environ.get("STUDIO_BROWSER_SOCKET") or SOCKET
    request = {"v": 1, "url": arguments[0], "session": os.environ.get("LAYR_SESSION")}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(15)
            connection.connect(path)
            connection.sendall(json.dumps(request).encode() + b"\n")
            data = b""
            while not data.endswith(b"\n") and len(data) <= 65536:
                block = connection.recv(65536)
                if not block:
                    break
                data += block
        reply = json.loads(data)
    except (OSError, ValueError) as error:
        print(f"studio-browser: Studio is not available in this VM ({type(error).__name__})", file=sys.stderr)
        return 1
    if not isinstance(reply, dict) or reply.get("ok") is not True:
        message = reply.get("message") if isinstance(reply, dict) else None
        print(f"studio-browser: {message or 'Studio refused the link'}", file=sys.stderr)
        return 1
    print(f"studio-browser: Studio asks the person to open {reply.get('host')}. "
          "The sign-in continues in the browser on the Mac after the approval.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
