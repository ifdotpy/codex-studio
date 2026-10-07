"""Detached process owner. The service can restart without closing child stdio."""
from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
from pathlib import Path
import signal

from common import (GuestError, MAX_INPUT, MAX_LINE, MAX_OUTPUT, atomic_json,
                    connect_db, decode, digest, identifier, process_identity,
                    receipt, require, save_receipt)


class Supervisor:
    def __init__(self, directory: Path, config):
        self.directory, self.config = directory, config
        self.db = connect_db(directory / "journal.sqlite3")
        self.db.execute("CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, event TEXT, data TEXT)")
        self.db.commit()
        self.sequence = 0
        self.output_bytes = 0
        self.output_limit = config.get("outputLimitBytes", MAX_OUTPUT)
        self.child = None
        self.reason = "exited"
        self.lock = asyncio.Lock()
        self.complete = asyncio.Event()
        self.meta = {"handle": directory.name, "agentId": config.get("agentId"),
                     "pid": None, "supervisorPid": os.getpid(),
                     "supervisorStart": process_identity(os.getpid()), "state": "starting",
                     "exitCode": None, "reason": None, "lastSeq": 0}

    def metadata(self):
        self.meta["lastSeq"] = self.sequence
        atomic_json(self.directory / "status.json", self.meta)

    def event(self, event, data):
        self.sequence += 1
        value = {"handle": self.directory.name, "seq": self.sequence, **data}
        self.db.execute("INSERT INTO events VALUES (?, ?, ?)", (self.sequence, event, json.dumps(value)))
        self.db.commit()

    async def terminate(self, reason):
        if self.child is None or self.child.returncode is not None:
            return
        self.reason = reason
        try:
            os.killpg(self.child.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            await asyncio.wait_for(self.child.wait(), 3)
        except asyncio.TimeoutError:
            try:
                os.killpg(self.child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await asyncio.wait_for(self.child.wait(), 3)

    async def drain(self, pipe, stream):
        while True:
            block = await pipe.read(32768)
            if not block:
                break
            self.output_bytes += len(block)
            if self.output_bytes > self.output_limit:
                await self.terminate("output_limit")
                break
            self.event("output", {"stream": stream, "data": base64.b64encode(block).decode("ascii")})

    async def input(self, data):
        if self.child.returncode is not None or self.child.stdin.is_closing():
            raise GuestError("not_found", "The process stdin is closed")
        self.child.stdin.write(data)
        try:
            await asyncio.wait_for(self.child.stdin.drain(), 5)
        except (BrokenPipeError, ConnectionResetError) as exc:
            raise GuestError("outcome_unknown", "The process closed stdin during the write") from exc
        except asyncio.TimeoutError as exc:
            # A pipe may already contain part of this input. Never resend it.
            await self.terminate("stdin_timeout")
            raise GuestError("outcome_unknown", "The stdin write exceeded its deadline") from exc

    async def control(self, reader, writer):
        response = None
        try:
            line = await asyncio.wait_for(reader.readline(), 5)
            require(len(line) <= MAX_LINE, "The request line exceeds the limit")
            request = json.loads(line)
            request_id = identifier(request.get("id"))
            method, params = request.get("method"), request.get("params", {})
            require(isinstance(params, dict), "The parameters must be an object")
            async with self.lock:
                response = receipt(self.db, request_id, digest(method, params))
                if response is None:
                    try:
                        if method == "write":
                            data = decode(params.get("data", ""), MAX_INPUT)
                            require(isinstance(params.get("close", False), bool), "close must be a boolean")
                            await self.input(data)
                            if params.get("close"):
                                self.child.stdin.close()
                            result = {"written": len(data), "closed": bool(params.get("close"))}
                        elif method == "stop":
                            await self.terminate("stopped")
                            await asyncio.wait_for(self.complete.wait(), 5)
                            result = self.meta
                        else:
                            raise GuestError("invalid_params", "Unknown supervisor method")
                        response = {"result": result}
                    except GuestError as exc:
                        response = {"error": exc.object()}
                    save_receipt(self.db, request_id, response)
        except GuestError as exc:
            response = {"error": exc.object()}
        except (ValueError, TypeError, asyncio.TimeoutError):
            response = {"error": {"code": "invalid_request", "message": "Invalid supervisor request"}}
        except Exception:
            response = {"error": {"code": "internal", "message": "Supervisor request failed"}}
        try:
            writer.write(json.dumps(response).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), 5)
        except (OSError, asyncio.TimeoutError):
            pass
        finally:
            writer.close()

    async def run(self):
        self.metadata()
        try:
            self.child = await asyncio.create_subprocess_exec(
                *self.config["argv"], cwd=self.config["cwd"], env=self.config["env"],
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE, start_new_session=True)
        except (OSError, ValueError):
            self.event("exit", {"exitCode": None, "reason": "start_failed"})
            self.meta.update(state="exited", reason="start_failed")
            self.metadata()
            return
        self.meta.update(pid=self.child.pid, state="running")
        self.metadata()
        socket_path = self.directory / "control.sock"
        server = await asyncio.start_unix_server(self.control, path=str(socket_path), limit=MAX_LINE + 1)
        os.chmod(socket_path, 0o600)
        readers = [asyncio.create_task(self.drain(self.child.stdout, "stdout")),
                   asyncio.create_task(self.drain(self.child.stderr, "stderr"))]
        if "stdin" in self.config:
            try:
                await self.input(decode(self.config["stdin"]))
                self.child.stdin.close()
            except GuestError:
                await self.terminate("stdin_failed")
        timeout = self.config.get("timeoutSeconds")
        try:
            if timeout:
                await asyncio.wait_for(self.child.wait(), timeout)
            else:
                await self.child.wait()
        except asyncio.TimeoutError:
            await self.terminate("timeout")
        # Descendants must not keep the journal or pipes open after the leader exits.
        try:
            os.killpg(self.child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            await asyncio.wait_for(asyncio.gather(*readers), 5)
        except asyncio.TimeoutError:
            for task in readers:
                task.cancel()
            self.reason = "pipe_timeout"
        self.event("exit", {"exitCode": self.child.returncode, "reason": self.reason})
        self.meta.update(state="exited", exitCode=self.child.returncode, reason=self.reason)
        self.metadata()
        self.complete.set()
        # Keep the control endpoint briefly for stop callers that await the exit record.
        await asyncio.sleep(0.1)
        server.close()
        await server.wait_closed()
        socket_path.unlink(missing_ok=True)
        self.db.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    config_path = args.directory / "launch.json"
    config = json.loads(config_path.read_text())
    config_path.unlink()
    os.umask(0o077)
    asyncio.run(Supervisor(args.directory, config).run())


if __name__ == "__main__":
    main()
