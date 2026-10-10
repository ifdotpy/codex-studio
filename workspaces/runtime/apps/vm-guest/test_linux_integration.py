"""Opt-in Linux integration test against an installed, isolated guest service."""
import argparse
import asyncio
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


async def disk_floor_proof(state):
    """Use a separate fixture state on the real btrfs disk, without a second server."""
    sys.path.insert(0, str(Path(__file__).parent))
    import service as guest
    from upload import tree_space
    old_floor = os.environ.get("CODEX_WORKSPACE_MIN_FREE_BYTES")
    original_space = guest.tree_space
    checks = []

    def record_space(path, required=0):
        checks.append(required)
        return tree_space(path, required)

    with tempfile.TemporaryDirectory(prefix="floor-proof-", dir=state) as temporary:
        path = Path(temporary)
        home = path / "home"
        home.mkdir()
        os.environ["CODEX_WORKSPACE_MIN_FREE_BYTES"] = str(shutil.disk_usage(path).free - 40 * 1024**2)
        fixture = guest.Service(path / "state", path / "store", path / "projects", home)
        guest.tree_space = record_space
        try:
            data = io.BytesIO()
            size = 32 * 1024**2
            with tarfile.open(fileobj=data, mode="w:gz") as archive:
                item = tarfile.TarInfo("large")
                item.size = size
                archive.addfile(item, io.BytesIO(b"0" * size))
            data = data.getvalue()
            root = fixture.projects / "source"

            async def emit(*unused):
                pass

            async def call(method, params):
                return await fixture.request({"id": uuid.uuid4().hex, "method": method, "params": params}, emit)

            begun = await call("upload.begin", {"uploadId": "floor", "root": str(root), "totalBytes": len(data),
                                               "sha256": hashlib.sha256(data).hexdigest()})
            assert "result" in begun, begun
            chunk = await call("upload.chunk", {"uploadId": "floor", "seq": 0, "data": base64.b64encode(data).decode()})
            assert "result" in chunk, chunk
            result = await call("upload.commit", {"uploadId": "floor"})
            assert result.get("error", {}).get("code") == "busy", result
            assert checks == [size], checks
            assert not root.exists()
            assert not (fixture.uploads.path / "floor" / "expanded").exists()
            assert fixture.uploads.row("floor")[5] == "receiving"
        finally:
            fixture.close()
            guest.tree_space = original_space
            if old_floor is None:
                os.environ.pop("CODEX_WORKSPACE_MIN_FREE_BYTES", None)
            else:
                os.environ["CODEX_WORKSPACE_MIN_FREE_BYTES"] = old_floor
    return True


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
        (source / "absolute-link").symlink_to("/tmp")
        (source / "outside-link").symlink_to("../../guest")
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
    actual_links = json.loads(client.exec(["python3", "-c",
        "import json,os; print(json.dumps([os.readlink('absolute-link'),os.readlink('outside-link')]))"], root))
    assert actual_links == ["/tmp", "../../guest"], actual_links
    escape = "link-escape-" + identity
    for link in ("absolute-link", "outside-link"):
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w") as archive:
            item = tarfile.TarInfo(link + "/" + escape)
            item.size = 2
            archive.addfile(item, io.BytesIO(b"no"))
        data = data.getvalue()
        upload_id = identity + "-" + link
        client.call("upload.begin", {"uploadId": upload_id, "root": root, "totalBytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(), "mode": "delta"})
        client.call("upload.chunk", {"uploadId": upload_id, "seq": 0, "data": base64.b64encode(data).decode()})
        try:
            client.call("upload.commit", {"uploadId": upload_id})
            raise AssertionError("The guest accepted a write below a link")
        except RuntimeError as exc:
            assert "invalid_params" in str(exc), exc
    assert not Path("/tmp", escape).exists()
    assert not Path(health["state"], escape).exists()
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
    cwd, agent = root, None
    source = "import sys,time; print('ready',flush=True); time.sleep(90)"
    provider, _ = client.call("provider.start", {"argv": ["python3", "-u", "-c", source], "cwd": cwd})
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
                     "cwd": cwd, "transport": "native", "handle": "contract:" + identity}
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
    from common import runtime_source_dir
    sys.path.insert(0, str(runtime_source_dir()))
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
    large_params = {**native_params, "handle": "large:" + identity,
                    "argv": ["python3", "-u", "-c", "import json,time; print(json.dumps({'method':'fixture/large','params':{'text':'é🙂'+'x'*2200000}},ensure_ascii=False),flush=True); time.sleep(90)"]}
    large, _ = client.call("provider.start", large_params)
    handle = large["handle"]
    deadline = time.monotonic() + 10
    while True:
        replay, _ = client.call("provider.rpc", {"handle": handle, "action": "replay", "cursor": 0})
        if replay["events"]:
            break
        assert time.monotonic() < deadline, replay
        time.sleep(0.05)
    descriptor = replay["events"][0]
    assert descriptor["payload"] is None and descriptor["payloadBytes"] > 2 * 1024**2
    next_event, _ = client.call("provider.rpc", {"handle": handle, "action": "next", "cursor": 0})
    assert next_event["event"] == descriptor
    subprocess.run(["sudo", "systemctl", "restart", "codex-studio-guest.service"], check=True, timeout=20)
    attached, _ = client.call("provider.start", large_params)
    assert attached["resumed"] and attached["acknowledged"] == 0 and attached["pid"] == large["pid"]
    payload = bytearray()
    while len(payload) < descriptor["payloadBytes"]:
        chunk, _ = client.call("provider.rpc", {"handle": handle, "action": "eventRead", "sequence": descriptor["sequence"],
                                                "offset": len(payload), "maxBytes": 1024**2})
        assert (chunk["sequence"], chunk["generation"], chunk["offset"], chunk["bytes"]) == (
            descriptor["sequence"], descriptor["generation"], len(payload), descriptor["payloadBytes"])
        payload.extend(base64.b64decode(chunk["data"], validate=True))
        assert chunk["nextOffset"] == len(payload)
    decoded = json.loads(payload)
    assert decoded["method"] == "fixture/large" and decoded["params"]["text"] == "é🙂" + "x" * 2_200_000
    client.call("provider.rpc", {"handle": handle, "action": "ack", "sequence": descriptor["sequence"]})
    next_event, _ = client.call("provider.rpc", {"handle": handle, "action": "next", "cursor": descriptor["sequence"]})
    assert next_event["event"] is None
    client.call("provider.stop", {"handle": handle})
    floor_proved = asyncio.run(disk_floor_proof(health["state"]))
    print(json.dumps({"uid": health["uid"], "filesystem": health["filesystem"],  "serviceRestartPreservedPid": True, "nativeRestartInitAckRemap": True,
                      "longExecSeconds": round(long_elapsed, 2), "longExecReconnectReplay": True,
                      "deadNativeSocketRecovered": True, "largeNativeEventBytes": len(payload),
                      "largeNativeReattachAck": True, "compressedUploadFloor": floor_proved,
                      "uploadLinkObjects": True, "symlinkParentWritesRejected": True}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    main(parser.parse_args().socket)
