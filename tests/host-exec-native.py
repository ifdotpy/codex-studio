#!/usr/bin/env python3
"""Opt-in proof through the native VM, real layr, and Mac command channel."""
from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_linux_vm import Client


def frames(client, method, params, identity):
    output = []
    result = None
    for frame in client.stream(method, params, request_id=identity, timeout=600):
        if frame.get("event") == "output":
            event = frame["data"]
            if "text" in event:
                output.append(event["text"])
            elif "data" in event:
                output.append(base64.b64decode(event["data"]).decode(errors="replace"))
        if "result" in frame:
            result = frame["result"]
    assert result is not None and result.get("exitCode") == 0, (method, result, "".join(output)[-2000:])
    return result, "".join(output)


def prove(client, agent, cwd):
    identity = "host-proof-" + uuid.uuid4().hex
    source = 'print("HOST_EXEC_SWIFT_OK")\n'
    setup = 'import pathlib;pathlib.Path("host-exec-proof.swift").write_text(' + repr(source) + ')'
    frames(client, "exec", {"agentId": agent, "cwd": cwd, "argv": ["python3", "-c", setup]}, identity + ":setup")
    def host(suffix, command):
        operation = identity + ":" + suffix
        return frames(client, "host.exec", {"action": "execute", "operationId": operation, "agentId": agent,
            "cwd": cwd, "argv": ["/bin/zsh", "-lc", command], "timeoutSeconds": 120}, operation)
    version, version_output = host("version", "xcodebuild -version")
    assert "Xcode " in version_output, version_output
    command = '\n'.join([
        'set -eu',
        'xcrun swiftc host-exec-proof.swift -o "$HOST_EXEC_DERIVED_DATA/host-exec-proof"',
        '"$HOST_EXEC_DERIVED_DATA/host-exec-proof"',
        'printf \'// Edit from macOS\\n\' >> host-exec-proof.swift',
        'printf \'artifact from macOS\\n\' > "$HOST_EXEC_ARTIFACTS/proof.txt"',
        'printf \'%s\\n\' "$PWD/host-exec-proof.swift"',
    ])
    build, build_output = host("build", command)
    assert "HOST_EXEC_SWIFT_OK" in build_output, build_output
    assert str(Path(cwd) / "host-exec-proof.swift") in build_output, build_output
    assert build["collected"] and build["changedPaths"] >= 1 and build["artifacts"], build
    artifact = build["artifacts"][0]["path"]
    verify = 'import pathlib;assert pathlib.Path("host-exec-proof.swift").read_text().endswith("// Edit from macOS\\n");' + \
        'assert pathlib.Path(' + repr(artifact) + ').read_text()=="artifact from macOS\\n";print("SOURCE_AND_ARTIFACT_OK")'
    _, checked = frames(client, "exec", {"agentId": agent, "cwd": cwd, "argv": ["python3", "-c", verify]}, identity + ":verify")
    warm, warm_output = host("warm", '"$HOST_EXEC_DERIVED_DATA/host-exec-proof"; printf \'%s\\n\' "$PWD"')
    assert warm["slotPath"] == build["slotPath"] == version["slotPath"], (version, build, warm)
    assert warm["derivedData"] == build["derivedData"]
    assert warm["transferredBytes"] == 0, warm
    assert "HOST_EXEC_SWIFT_OK" in warm_output, warm_output
    return {"agentId": agent, "linePath": cwd, "xcodeVersion": version_output.strip(),
        "swiftOutput": build_output.strip(), "sourceReturn": checked.strip(),
        "slotPath": build["slotPath"], "derivedData": build["derivedData"], "artifactPath": artifact,
        "secondRunTransferredBytes": warm["transferredBytes"], "operationIds": [version["operationId"], build["operationId"], warm["operationId"]]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--agent", required=True)
    parser.add_argument("--cwd", required=True)
    args = parser.parse_args()
    print(json.dumps(prove(Client(args.state), args.agent, args.cwd), indent=2))
