"""Sign and verify one request through Python's multi-server wire owner."""
from __future__ import annotations

import base64
import json
import sys
from pathlib import Path

source = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(source))
from codex_multi_server import request_bytes
from codex_multi_server_crypto import CryptoProcess

case = json.loads(sys.argv[1])
proof = request_bytes(
    case["method"], case["target"], case["serverId"], case["clientId"],
    case["timestamp"], case["nonce"], case["requestId"],
    base64.b64decode(case["body"]),
)
seed = base64.b64decode(case["seed"])
if len(seed) != 32:
    raise SystemExit("The Ed25519 fixture seed is invalid")
private_der = bytes.fromhex("302e020100300506032b657004220420") + seed
private_key = (
    "-----BEGIN PRIVATE KEY-----\n"
    + base64.encodebytes(private_der).decode("ascii")
    + "-----END PRIVATE KEY-----\n"
)
crypto = CryptoProcess()
try:
    signature = crypto.call(
        "sign", privateKey=private_key, data=base64.b64encode(proof).decode("ascii")
    )
    result = crypto.call(
        "verify", publicKey=case["publicKey"], signature=case["rustSignature"],
        data=base64.b64encode(proof).decode("ascii"),
    )
    if result.get("valid") is not True:
        raise SystemExit("Python rejected the Rust request signature")
finally:
    crypto.shutdown()
print(json.dumps({"pythonSignature": signature["signature"], "verified": True}))
