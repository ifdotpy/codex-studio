#!/usr/bin/env python3
"""Manual benchmark of the host_exec source transfer (docs/host-exec.md).

It runs the guest caller, the root broker and the Mac service in one process, and the slot file
helper as a real subprocess, as in the VM. The fixture layr copies the line, and requests go
to the Mac service without the VM socket, so the numbers leave out the socket and real layr.

    python3 -B workspaces/runtime/apps/server/tests/host-exec-transfer-bench.py --files 50000

It prints one JSON line per command: a cold slot, a warm slot without changes, and a warm slot
after a change of --changed files. With --cache the files are in a layr cache folder, which the
guest reads from the line folder itself, as for a Chromium checkout.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
from types import SimpleNamespace
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import VM_GUEST_ROOT  # noqa: E402

sys.path.insert(0, str(VM_GUEST_ROOT))
from codex_host_exec_server import HostService  # noqa: E402
from host_exec import HostExec  # noqa: E402
from host_exec_protocol import HostExecError, key  # noqa: E402
from host_exec_slot import HostSlotHandlers  # noqa: E402


class Agents:
    def __init__(self, context: dict[str, Any]):
        self.context = context

    async def dispatch(self, request_id: str, method: str, params: dict[str, Any], emit: Any = None) -> dict[str, Any]:
        return self.context


class Slots(HostSlotHandlers):
    export_seconds = 0.0
    helper_runs = 0
    caches: list[str] = []

    async def io(self, context: dict[str, Any], params: dict[str, Any]) -> dict[str, Any]:
        # The real helper process, without the change of user that needs root.
        Slots.helper_runs += 1
        child = await asyncio.create_subprocess_exec(
            sys.executable, str(VM_GUEST_ROOT / "host_exec_slot_io.py"),
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
        output, _ = await child.communicate(json.dumps(params).encode())
        result = json.loads(output)
        if "error" in result:
            raise HostExecError(result["error"]["code"], result["error"]["message"])
        return dict(result["result"])

    async def layr(self, context: dict[str, Any], args: list[str], ok: tuple[int, ...] = (0,)) -> str:
        line = Path(context["path"])
        if args[0] == "rev-parse":
            return "0123456789abcdef0123456789abcdef"
        if args[:2] == ["cache", "list"]:
            return "".join(path + "\tcache\n" for path in Slots.caches)
        if args[0] == "export":
            started = time.monotonic()
            source = Path(args[2])
            shutil.rmtree(source)
            # layr keeps cache folders outside states.
            shutil.copytree(line, source, symlinks=True,
                            ignore=lambda folder, names: [name for name in names if Path(folder) == line and name in Slots.caches])
            (source / ".layr-export.json").write_text('{"state": "%s"}' % args[1])
            Slots.export_seconds += time.monotonic() - started
        return ""


async def main(options: argparse.Namespace) -> None:
    with tempfile.TemporaryDirectory(prefix="host-exec-bench-") as name:
        root = Path(name).resolve()
        state, line, home, layr = root / "state", root / "line", root / "home", root / "layr"
        for folder in (state, line, home, layr):
            folder.mkdir()
        (state / "host-exec-token").write_text("a" * 64)
        data = os.urandom(options.size)
        files = line / "chromium" if options.cache else line
        Slots.caches = ["chromium"] if options.cache else []
        (line / "main.swift").write_text('print("Hello")\n')
        for index in range(options.files):
            folder = files / ("d%03d" % (index % 500))
            folder.mkdir(parents=True, exist_ok=True)
            (folder / ("f%06d.txt" % index)).write_bytes(data + str(index).encode())
        host = HostService(state, budget=64 * 1024**3)
        context = {"agentId": "agent", "projectId": "project", "line": "worker", "path": str(line),
                   "cwd": str(line), "uid": os.getuid(), "gid": os.getgid(), "home": str(home), "readOnly": False}
        slots = Slots(state, layr, Agents(context))

        async def host_context(agent: str) -> dict[str, Any]:
            return context

        guest = HostExec(SimpleNamespace(state=state, host_context=host_context))
        guest.configure({"token": "a" * 64})
        counts = {"mac": 0, "broker": 0}

        async def call(method: str, params: dict[str, Any], phase: str, timeout: float = 300) -> dict[str, Any]:
            counts["mac"] += 1
            response = await host.request({"id": key(params["operationId"] + ":" + phase), "method": method,
                                           "params": params, "token": "a" * 64})
            if "error" in response:
                raise HostExecError(response["error"]["code"], response["error"]["message"])
            return dict(response["result"])

        async def broker(method: str, params: dict[str, Any], phase: str) -> dict[str, Any]:
            counts["broker"] += 1
            return await slots.dispatch(key(params["operationId"] + phase), method, params)

        guest.call = call  # type: ignore[method-assign]
        guest.broker = broker  # type: ignore[method-assign]

        async def emit(event: str, value: Any) -> None:
            pass

        try:
            for label in ("cold", "warm", "changed"):
                if label == "changed":
                    for index in range(options.changed):
                        path = files / ("d%03d" % (index % 500)) / ("f%06d.txt" % index)
                        path.write_bytes(b"changed" + data + str(index).encode())
                counts.update(mac=0, broker=0)
                Slots.export_seconds, Slots.helper_runs = 0.0, 0
                started = time.monotonic()
                result = await guest.handle(label, {"agentId": "agent", "operationId": label, "argv": ["true"],
                                                    "cwd": str(line), "timeoutSeconds": 60}, emit)
                total = time.monotonic() - started
                print(json.dumps({"command": label, "files": options.files, "seconds": round(total, 2),
                                  "secondsWithoutExport": round(total - Slots.export_seconds, 2),
                                  "transferredBytes": result["transferredBytes"], "macRequests": counts["mac"],
                                  "brokerRequests": counts["broker"], "helperProcesses": Slots.helper_runs}), flush=True)
        finally:
            for child in list(host.children.values()):
                await host.terminate(child)
            host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--files", type=int, default=50_000)
    parser.add_argument("--size", type=int, default=1024, help="bytes of each file")
    parser.add_argument("--changed", type=int, default=100)
    parser.add_argument("--cache", action="store_true", help="put the files in a layr cache folder")
    asyncio.run(main(parser.parse_args()))
