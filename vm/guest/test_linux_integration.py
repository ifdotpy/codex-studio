"""Opt-in Linux integration test against an installed, isolated guest service."""
import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


class Client:
    def __init__(self, path):
        self.path = path

    def call(self, method, params=None, identity=None):
        request = {"id": identity or uuid.uuid4().hex, "method": method, "params": params or {}}
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(1900)
            connection.connect(self.path)
            connection.sendall(json.dumps(request).encode() + b"\n")
            stream = connection.makefile("rb")
            events = []
            while True:
                line = stream.readline()
                if not line:
                    raise RuntimeError("The guest disconnected")
                response = json.loads(line)
                if "event" in response:
                    events.append(response)
                elif "error" in response:
                    raise RuntimeError(response["error"])
                else:
                    return response["result"], events

    def exec(self, argv, cwd, agent=None):
        result, events = self.call("exec", {"argv": argv, "cwd": cwd, **({"agentId": agent} if agent else {})})
        assert result["exitCode"] == 0, (result, events)
        return b"".join(base64.b64decode(event["data"]["data"]) for event in events if event["event"] == "output"
                        and event["data"]["stream"] == "stdout")


def main(path):
    client = Client(path)
    health, _ = client.call("health")
    assert health["uid"] != 0 and health["filesystem"] == "btrfs", health
    identity = uuid.uuid4().hex
    root = health["projects"] + "/test-" + identity
    with tempfile.TemporaryDirectory() as temporary:
        source = Path(temporary)
        subprocess.run(["git", "init", "-q", str(source)], check=True, timeout=10)
        (source / "hello").write_text("base\n")
        subprocess.run(["git", "-C", str(source), "add", "hello"], check=True, timeout=10)
        subprocess.run(["git", "-C", str(source), "-c", "user.name=Contract", "-c", "user.email=contract@example.invalid",
                        "commit", "-qm", "Base"], check=True, timeout=10)
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w") as archive:
            for item in source.iterdir():
                archive.add(item, arcname=item.name)
        data = data.getvalue()
    upload = {"uploadId": identity, "root": root, "totalBytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    client.call("upload.begin", upload)
    for sequence, offset in enumerate(range(0, len(data), 1024**2)):
        client.call("upload.chunk", {"uploadId": identity, "seq": sequence,
                                    "data": base64.b64encode(data[offset:offset + 1024**2]).decode()})
    client.call("upload.commit", {"uploadId": identity})
    marker = root + "/long-exec-proof"
    long_params = {"argv": ["python3", "-c", f"import time; time.sleep(65); open({marker!r},'a').write('once'); print('done')"],
                   "cwd": root, "timeoutSeconds": 90}
    started = time.monotonic()
    long_result, long_events = client.call("exec", long_params, "long-exec:" + identity)
    long_elapsed = time.monotonic() - started
    assert long_elapsed >= 65 and long_result["exitCode"] == 0, (long_elapsed, long_result)
    # A fresh connection retrieves the saved result without executing the command again.
    repeated, repeated_events = client.call("exec", long_params, "long-exec:" + identity)
    assert (repeated, repeated_events) == (long_result, long_events)
    assert client.exec(["cat", marker], root) == b"once"
    base, _ = client.call("workspace.startBase", {"root": root})
    assert base["state"] == "ready", base
    agent = "linux-" + identity
    workspace, _ = client.call("workspace.create", {"root": root, "agentId": agent})
    cwd = workspace["path"]
    client.exec(["python3", "-c", "open('hello','w').write('worker\\n')"], cwd, agent)
    client.exec(["git", "add", "hello"], cwd, agent)
    client.exec(["git", "-c", "user.name=Contract", "-c", "user.email=contract@example.invalid", "commit", "-qm", "Worker"], cwd, agent)
    commit = client.exec(["git", "rev-parse", "HEAD"], cwd, agent).decode().strip()
    client.exec(["git", "bundle", "create", "result.bundle", "HEAD"], cwd, agent)
    info, _ = client.call("file.stat", {"agentId": agent, "path": cwd + "/result.bundle"})
    bundle = bytearray()
    while len(bundle) < info["bytes"]:
        chunk, _ = client.call("file.read", {"agentId": agent, "path": cwd + "/result.bundle", "token": info["token"],
                                            "offset": len(bundle), "maxBytes": 65536})
        bundle.extend(base64.b64decode(chunk["data"]))
    assert hashlib.sha256(bundle).hexdigest() == info["sha256"]
    with tempfile.TemporaryDirectory() as temporary:
        fetched = Path(temporary)
        (fetched / "result.bundle").write_bytes(bundle)
        subprocess.run(["git", "init", "-q", str(fetched)], check=True, timeout=10)
        subprocess.run(["git", "-C", str(fetched), "fetch", "-q", str(fetched / "result.bundle"), "HEAD"], check=True, timeout=10)
        actual = subprocess.check_output(["git", "-C", str(fetched), "rev-parse", "FETCH_HEAD"], timeout=10).decode().strip()
        assert actual == commit
    source = "import sys,time; print('ready',flush=True); time.sleep(90)"
    provider, _ = client.call("provider.start", {"argv": ["python3", "-u", "-c", source], "cwd": cwd, "agentId": agent})
    handle = provider["handle"]
    before, events = client.call("provider.attach", {"handle": handle, "waitMs": 5000})
    assert any(event["event"] == "output" for event in events)
    subprocess.run(["sudo", "systemctl", "restart", "codex-studio-guest.service"], check=True, timeout=20)
    providers, _ = client.call("provider.list")
    after = next(row for row in providers["providers"] if row["handle"] == handle)
    assert after["state"] == "running" and after["pid"] == provider["pid"]
    _, events = client.call("provider.attach", {"handle": handle, "afterSeq": before["nextSeq"], "waitMs": 0})
    assert not events
    client.call("provider.stop", {"handle": handle})
    native_params = {"argv": ["python3", "-u", "-c", "import json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n if 'id' in r: print(json.dumps({'id':r['id'],'result':{'proof':'same'}}),flush=True)"],
                     "cwd": cwd, "agentId": agent, "transport": "native", "handle": "contract:" + identity}
    native, _ = client.call("provider.start", native_params)
    write = {"handle": native["handle"], "action": "write", "operationId": "initialize", "nativeId": 7,
             "message": {"id": 7, "method": "initialize", "params": {}}}
    written, _ = client.call("provider.rpc", write)
    assert written["remoteId"] == 1
    deadline = time.monotonic() + 10
    while True:
        event, _ = client.call("provider.rpc", {"handle": native["handle"], "action": "next", "cursor": 0})
        if event["event"]:
            break
        assert time.monotonic() < deadline, event
        time.sleep(0.05)
    client.call("provider.rpc", {"handle": native["handle"], "action": "ack", "sequence": event["event"]["sequence"]})
    subprocess.run(["sudo", "systemctl", "restart", "codex-studio-guest.service"], check=True, timeout=20)
    resumed, _ = client.call("provider.start", native_params)
    assert resumed["resumed"] and resumed["pid"] == native["pid"]
    assert resumed["initResult"] == {"proof": "same"} and resumed["acknowledged"] == 1
    duplicate, _ = client.call("provider.rpc", {**write, "nativeId": 19, "message": {**write["message"], "id": 19}})
    assert duplicate["duplicate"] and duplicate["remoteId"] == 1
    client.call("provider.stop", {"handle": native["handle"]})
    # A reboot leaves the same lease and socket files, with a dead owner.
    lease = Path(health["state"]) / "native" / "supervisor.lock"
    saved_owner = json.loads(lease.read_text())
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
    from codex_process_supervisor import process_start_time
    assert process_start_time(saved_owner["pid"]) == saved_owner["startTime"]
    os.kill(saved_owner["pid"], signal.SIGTERM)
    deadline = time.monotonic() + 10
    while process_start_time(saved_owner["pid"]) == saved_owner["startTime"]:
        assert time.monotonic() < deadline, "The test supervisor did not stop"
        time.sleep(0.05)
    assert lease.with_name("supervisor.sock").exists()
    subprocess.run(["sudo", "systemctl", "restart", "codex-studio-guest.service"], check=True, timeout=20)
    recovered, _ = client.call("provider.start", native_params)
    current_owner = json.loads(lease.read_text())
    assert current_owner["pid"] != saved_owner["pid"]
    assert process_start_time(current_owner["pid"]) == current_owner["startTime"]
    assert recovered["state"] == "running" and recovered["generation"] > native["generation"]
    client.call("provider.stop", {"handle": recovered["handle"]})
    client.call("workspace.archive", {"agentId": agent})
    client.call("workspace.remove", {"agentId": agent})
    print(json.dumps({"uid": health["uid"], "filesystem": health["filesystem"], "commit": commit,
                      "fetchMatched": True, "serviceRestartPreservedPid": True, "nativeRestartInitAckRemap": True,
                      "longExecSeconds": round(long_elapsed, 2), "longExecReconnectReplay": True,
                      "deadNativeSocketRecovered": True, "archiveRemoved": True}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    main(parser.parse_args().socket)
