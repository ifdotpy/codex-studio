"""Root broker for fixed guest operations. Only the studio UID can connect."""
from __future__ import annotations

import argparse
import asyncio
import fcntl
import importlib
import json
import os
from pathlib import Path
import pwd
import socket
import struct

from common import (GuestError, MAX_LINE, connect_db, digest, identifier,
                    private_dir, receipt, require, save_receipt)
from layr_projects import Projects
from layr_share import Share

ROOT = Path("/var/lib/codex-studio/layr")
STATE = Path("/var/lib/codex-studio/layr-admin")
SOCKET = Path("/run/codex-studio/layr-admin.sock")
METHODS = {"project.ensure", "project.import", "share.configure", "share.status", "layr.health"}
READ_METHODS = {"project.ensure", "share.status", "layr.health"}


class Broker:
    def __init__(self, state=STATE, root=ROOT, *, modules=("layr_agents", "host_exec_slot")):
        self.state, self.root = state, root
        private_dir(state)
        self.lease = (state / "broker.lock").open("a+")
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.db = connect_db(state / "receipts.sqlite3")
        self.active = {}
        self.methods, self.read_methods = set(METHODS), set(READ_METHODS)
        self.agents = None
        self.host_errors = ()
        self.handlers = {}
        for name in modules:
            try:
                module = importlib.import_module(name)
            except ModuleNotFoundError as exc:
                if exc.name != name:
                    raise
                continue
            if name == "host_exec_slot":
                require(self.agents is not None, "The host slot service needs the agent service")
                handler = module.HostSlotHandlers(state, root, self.agent_handler)
                from host_exec_protocol import HostExecError
                self.host_errors = (HostExecError,)
            else:
                handler = module.AgentHandlers(state, root)
                self.agent_handler = handler
            if name == "layr_agents":
                self.agents = module
            require(not self.methods.intersection(module.METHODS), "The admin method is registered twice")
            self.methods.update(module.METHODS)
            self.read_methods.update(module.READ_METHODS)
            self.handlers.update({method: handler for method in module.METHODS})
        self.share = Share(state, root)
        self.projects = Projects(state, root, self.share, self.agents)

    def close(self):
        self.db.close()
        self.lease.close()

    async def dispatch(self, request_id, method, params, emit):
        if method in {"project.ensure", "project.import"}:
            return await self.projects.dispatch(request_id, method, params)
        if method == "share.configure":
            return await self.share.configure(params)
        if method == "share.status":
            return await self.share.status()
        if method == "layr.health":
            return await self.projects.health()
        return await self.handlers[method].dispatch(request_id, method, params, emit)

    async def request(self, request, emit):
        request_id = request.get("id") if isinstance(request, dict) else None
        try:
            identifier(request_id)
            method, params = request.get("method"), request.get("params", {})
            require(method in self.methods and isinstance(params, dict), "The admin method or parameters are invalid")
            if method in self.read_methods:
                return {"id": request_id, "result": await self.dispatch(request_id, method, params, emit)}
            fingerprint = digest(method, params)
            if request_id in self.active:
                previous, task = self.active[request_id]
                if previous != fingerprint:
                    raise GuestError("id_conflict", "The request ID has different content")
                return await asyncio.shield(task)
            prior = receipt(self.db, request_id, fingerprint)
            if prior is not None:
                return prior

            async def operation():
                try:
                    result = await self.dispatch(request_id, method, params, emit)
                    response = {"id": request_id, "result": result}
                except GuestError as exc:
                    response = {"id": request_id, "error": exc.object()}
                except self.host_errors as exc:
                    response = {"id": request_id, "error": exc.object()}
                except Exception:
                    response = {"id": request_id, "error": {"code": "internal", "message": "The admin operation failed"}}
                save_receipt(self.db, request_id, response)
                return response

            task = asyncio.create_task(operation())
            self.active[request_id] = (fingerprint, task)
            try:
                return await asyncio.shield(task)
            finally:
                self.active.pop(request_id, None)
        except GuestError as exc:
            return {"id": request_id, "error": exc.object()}
        except self.host_errors as exc:
            return {"id": request_id, "error": exc.object()}
        except (TypeError, ValueError):
            return {"id": request_id, "error": {"code": "invalid_params", "message": "The admin parameters are invalid"}}

    async def client(self, reader, writer):
        try:
            peer = writer.get_extra_info("socket").getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
            _, uid, _ = struct.unpack("3i", peer)
            if uid != pwd.getpwnam("studio").pw_uid:
                return
            line = await asyncio.wait_for(reader.readline(), 10)
            require(0 < len(line) <= MAX_LINE and line.endswith(b"\n"), "The admin frame is invalid")
            request = json.loads(line)

            async def emit(event, data):
                writer.write(json.dumps({"id": request["id"], "event": event, "data": data}).encode() + b"\n")
                await asyncio.wait_for(writer.drain(), 5)

            response = await self.request(request, emit)
            writer.write(json.dumps(response).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), 5)
        except (OSError, ValueError, asyncio.TimeoutError, GuestError):
            pass
        finally:
            writer.close()


def socket_directory(path: Path) -> None:
    """The studio group reaches the socket. The unit's UMask=0077 masks mkdir's mode, so set it."""
    path.mkdir(mode=0o750, parents=True, exist_ok=True)
    path.chmod(0o750)
    os.chown(path, 0, pwd.getpwnam("studio").pw_gid)


async def serve():
    require(os.getuid() == 0, "The layr admin service requires root")
    broker = Broker()
    # Mounts disappear at reboot. Recreate only exports already registered.
    await broker.share.restore()
    socket_directory(SOCKET.parent)
    SOCKET.unlink(missing_ok=True)
    server = await asyncio.start_unix_server(broker.client, path=str(SOCKET), limit=MAX_LINE + 1)
    os.chown(SOCKET, 0, pwd.getpwnam("studio").pw_gid)
    SOCKET.chmod(0o660)
    try:
        async with server:
            await server.serve_forever()
    finally:
        broker.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["serve"])
    parser.parse_args()
    asyncio.run(serve())
