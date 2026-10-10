"""User-session Mac command owner for the guest host_exec channel."""
from __future__ import annotations

import argparse
import asyncio
import base64
import codecs
import fcntl
import hashlib
import hmac
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import sys
import time
from typing import Any, TYPE_CHECKING

from codex_layout import VM_GUEST_ROOT

# The host side shares the frame protocol with the guest (mypy finds it through mypy_path).
sys.path.insert(0, str(VM_GUEST_ROOT))
from host_exec_protocol import (CHUNK, FRAME_LIMIT, HostExecError, aliases, atomic,  # noqa: E402
                                conflicts, inside, key, mapped, require, scan)


class HostService:
    def __init__(self, state: Path, *, slots: int = 4, budget: int = 20 * 1024**3):
        self.state = state.resolve()
        self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.pool = self.state / "host-slots"
        self.pool.mkdir(exist_ok=True, mode=0o700)
        self.slots, self.budget = slots, budget
        self.token = (self.state / "host-exec-token").read_text()
        self.lease = (self.state / "host-exec.lock").open("a+")
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.db = sqlite3.connect(self.state / "host-exec.sqlite3")
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute("CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, digest TEXT, response TEXT)")
        self.db.execute("CREATE TABLE IF NOT EXISTS events (operation TEXT, seq INTEGER, value TEXT, PRIMARY KEY(operation,seq))")
        self.db.commit()
        self.active: dict[str, asyncio.Task[dict[str, Any]]] = {}
        self.commands: dict[str, asyncio.Task[None]] = {}
        self.children: dict[str, asyncio.subprocess.Process] = {}
        self.locks: dict[str, asyncio.Lock] = {}
        self.clients = 0

    def close(self) -> None:
        self.db.close()
        self.lease.close()

    def opdir(self, identity: str) -> Path:
        return self.state / "host-operations" / key(identity)

    def load(self, identity: str) -> dict[str, Any]:
        path = self.opdir(identity) / "status.json"
        require(path.is_file(), "The host command does not exist", "not_found")
        value: dict[str, Any] = json.loads(path.read_text())
        if value["state"] == "running" and identity not in self.commands:
            value.update(state="unknown", reason="host_owner_lost")
        return value

    def save(self, identity: str, value: dict[str, Any]) -> None:
        atomic(self.opdir(identity) / "status.json", value)

    def event(self, identity: str, value: dict[str, Any]) -> None:
        seq = self.db.execute("SELECT coalesce(max(seq),0)+1 FROM events WHERE operation=?", (identity,)).fetchone()[0]
        self.db.execute("INSERT INTO events VALUES (?,?,?)", (identity, seq, json.dumps({"seq": seq, **value})))
        self.db.commit()

    @staticmethod
    def size(path: Path) -> int:
        total = 0
        for directory, _, files in os.walk(path, followlinks=False):
            for name in files:
                total += (Path(directory) / name).lstat().st_blocks * 512
        return total

    def evict(self, project: Path, *, reserve: int = 0, exclude: Path | None = None) -> None:
        require(0 <= reserve <= self.budget, "The sources exceed the project disk budget", "disk_budget")
        candidates = [path for path in project.iterdir() if path.is_dir() and path != exclude and not (path / "lease.json").exists()]
        candidates.sort(key=lambda path: json.loads((path / "slot.json").read_text()).get("usedAt", 0))
        used = self.size(project)
        while used + reserve > self.budget and candidates:
            victim = candidates.pop(0)
            used -= self.size(victim)
            shutil.rmtree(victim)
        require(used + reserve <= self.budget, "The project disk budget has no free slot capacity", "disk_budget")

    def slot(self, op: dict[str, Any]) -> Path:
        path = self.pool / key(op["projectId"]) / str(op["slotId"])
        lease = path / "lease.json"
        require(lease.is_file() and json.loads(lease.read_text())["operationId"] == op["operationId"],
                "The operation no longer owns its slot", "lease_lost")
        return path

    async def acquire(self, params: dict[str, Any]) -> dict[str, Any]:
        identity = params["operationId"]
        require(isinstance(identity, str) and 0 < len(identity) <= 128, "Invalid operation ID")
        for field in ("projectId", "agentId", "linePath"):
            require(isinstance(params.get(field), str) and 0 < len(params[field]) <= 4096, "Missing host command context: " + field)
        project = self.pool / key(params["projectId"])
        async with self.locks.setdefault(str(project), asyncio.Lock()):
            project.mkdir(exist_ok=True, mode=0o700)
            require(not self.opdir(identity).exists(), "The operation already exists", "outcome_unknown")
            self.evict(project)
            free = [path for path in project.iterdir() if path.is_dir() and not (path / "lease.json").exists()]
            free.sort(key=lambda path: json.loads((path / "slot.json").read_text()).get("usedAt", 0), reverse=True)
            if free:
                slot = free[0]
            else:
                require(len(list(project.iterdir())) < self.slots, "All project slots are leased", "busy")
                slot = next(project / str(index) for index in range(self.slots) if not (project / str(index)).exists())
                slot.mkdir(mode=0o700)
                atomic(slot / "slot.json", {"generation": os.urandom(16).hex(), "usedAt": 0})
            for name in ("source", "DerivedData", "packages"):
                (slot / name).mkdir(exist_ok=True, mode=0o700)
            directory = self.opdir(identity)
            directory.mkdir(parents=True, mode=0o700)
            artifact_dir = slot / "artifacts" / key(identity)
            artifact_dir.mkdir(parents=True, mode=0o700)
            (directory / "artifacts").symlink_to(artifact_dir, target_is_directory=True)
            meta = json.loads((slot / "slot.json").read_text())
            op = {"operationId": identity, **{field: params[field] for field in ("projectId", "agentId", "linePath")},
                  "slotId": slot.name, "generation": meta["generation"], "state": "leased", "createdAt": time.time(),
                  "slotPath": str(slot / "source"), "derivedData": str(slot / "DerivedData"),
                  "packageCache": str(slot / "packages"), "artifactsPath": str(directory / "artifacts")}
            self.save(identity, op)
            atomic(slot / "lease.json", {"operationId": identity})
            return op

    async def dispatch(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        if method == "acquire":
            return await self.acquire(params)
        identity = params["operationId"]
        op = self.load(identity)
        # The broker sets these fields; the host never accepts a slot path from the caller.
        require(params.get("agentId") == op["agentId"], "The command belongs to another agent", "forbidden")
        if method == "status":
            return op
        if method == "attach":
            after = params.get("afterSeq", 0)
            require(isinstance(after, int) and after >= 0, "Invalid event cursor")
            deadline = time.monotonic() + min(max(params.get("waitSeconds", 0), 0), 10)
            while time.monotonic() < deadline and identity in self.commands:
                if self.db.execute("SELECT 1 FROM events WHERE operation=? AND seq>?", (identity, after)).fetchone():
                    break
                await asyncio.sleep(0.025)
            events = [json.loads(row[0]) for row in self.db.execute("SELECT value FROM events WHERE operation=? AND seq>? ORDER BY seq LIMIT 16", (identity, after))]
            return {"events": events, "status": self.load(identity)}
        if method == "result.manifest":
            area: Any = params.get("area")
            require(area in {"changes", "artifacts"}, "Invalid result file area")
            require(op["state"] in {"exited", "released"}, "The command has no final files", "outcome_unknown")
            entries = json.loads((self.opdir(identity) / (area + ".json")).read_text())
            after = params.get("after", "")
            page = [(path, entries[path]) for path in sorted(entries) if path > after][:256]
            return {"entries": dict(page), "next": page[-1][0] if len(page) == 256 else None}
        if method == "read":
            require(op["state"] in {"exited", "released"}, "The command has no final files", "outcome_unknown")
            area = params.get("area")
            require(area in {"changes", "artifacts"}, "Invalid result file area")
            path = inside(self.opdir(identity) / area, params["path"], leaf=True)
            offset = params.get("offset", 0)
            require(isinstance(offset, int) and offset >= 0, "Invalid file offset")
            with path.open("rb") as stream:
                stream.seek(offset)
                data = stream.read(CHUNK)
            return {"data": base64.b64encode(data).decode(), "bytes": len(data)}
        slot = self.slot(op)
        if method == "manifest":
            manifest_path = self.opdir(identity) / "held-manifest.json"
            if not params.get("after"):
                atomic(manifest_path, scan(slot / "source"))
            entries = json.loads(manifest_path.read_text())
            after = params.get("after", "")
            page = [(path, entries[path]) for path in sorted(entries) if path > after][:256]
            return {"entries": dict(page), "next": page[-1][0] if len(page) == 256 else None}
        if method.startswith("sync."):
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            path = inside(slot / "source", params["path"])
            if method == "sync.delete":
                if path.is_symlink() or path.is_file():
                    path.unlink()
                elif path.exists():
                    shutil.rmtree(path)
            elif method == "sync.entry":
                entry = params["entry"]
                require(entry.get("kind") in {"directory", "link", "file"}, "Invalid source entry")
                path.parent.mkdir(parents=True, exist_ok=True)
                if path.is_symlink() or path.is_file():
                    path.unlink()
                elif path.exists() and entry["kind"] != "directory":
                    shutil.rmtree(path)
                if entry["kind"] == "directory":
                    path.mkdir(exist_ok=True)
                elif entry["kind"] == "link":
                    require(isinstance(entry.get("target"), str) and "\0" not in entry["target"], "Invalid link target")
                    path.symlink_to(entry["target"])
                else:
                    length = entry.get("bytes")
                    require(isinstance(length, int) and 0 <= length <= self.budget, "Invalid file size")
                    self.evict(slot.parent, reserve=length, exclude=slot)
                    path.touch()
                    atomic(self.opdir(identity) / (key(params["path"]) + ".json"), entry)
            elif method == "sync.chunk":
                require(path.is_file() and not path.is_symlink(), "The source file is not open")
                data = base64.b64decode(params["data"], validate=True)
                require(len(data) <= CHUNK, "The source chunk exceeds its limit")
                require(path.stat().st_size == params["offset"], "The source chunk offset differs")
                entry = json.loads((self.opdir(identity) / (key(params["path"]) + ".json")).read_text())
                require(path.stat().st_size + len(data) <= entry["bytes"], "The source exceeds its declared size")
                with path.open("ab") as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
            elif method == "sync.commit":
                require(not path.is_symlink(), "The source file is a symbolic link")
                entry = json.loads((self.opdir(identity) / (key(params["path"]) + ".json")).read_text())
                with path.open("rb") as stream:
                    checksum = hashlib.file_digest(stream, "sha256").hexdigest()
                require(path.stat().st_size == entry["bytes"] and checksum == entry["sha256"], "The source checksum differs")
                require(isinstance(entry["mode"], int) and 0 <= entry["mode"] <= 0o777, "Invalid source file mode")
                path.chmod(entry["mode"])
            else:
                raise HostExecError("invalid_params", "Unknown source transfer method")
            return {"path": params["path"]}
        if method == "run":
            require(op["state"] == "leased", "The command already started", "outcome_unknown")
            argv: Any = params.get("argv")
            require(isinstance(argv, list) and 0 < len(argv) <= 1024 and all(isinstance(item, str) and "\0" not in item for item in argv), "Invalid command arguments")
            timeout = params.get("timeoutSeconds", 300)
            require(isinstance(timeout, (int, float)) and not isinstance(timeout, bool) and 0 < timeout <= 3600, "Invalid command timeout")
            source = scan(slot / "source")
            require(not conflicts(list(source)), "The source names conflict on macOS", "name_conflict")
            require(key(json.dumps(source, sort_keys=True)) == params.get("manifestSha256"), "The Mac sources differ from the layr slot", "sync_conflict")
            cwd = slot / "source"
            if params.get("cwd"):
                cwd = inside(cwd, params["cwd"], leaf=True)
                require(cwd.is_dir(), "The command cwd is not a directory")
            self.evict(slot.parent, exclude=slot)
            op.update(state="running", startedAt=time.time())
            self.save(identity, op)
            self.commands[identity] = asyncio.create_task(self.command(identity, dict(op), argv, cwd, timeout, source))
            return op
        if method == "cancel":
            child = self.children.get(identity)
            if child is not None:
                op["cancelRequested"] = True
                self.save(identity, op)
                await self.terminate(child)
            elif op["state"] == "unknown":
                raise HostExecError("outcome_unknown", "The old host owner is lost; inspect the slot before recovery")
            else:
                op["cancelRequested"] = True
                self.save(identity, op)
            return self.load(identity)
        if method == "release":
            require(op["state"] in {"leased", "exited"}, "A running or unknown command retains its lease")
            op.update(state="released", collected=bool(params.get("collected")), finishedAt=time.time())
            self.save(identity, op)
            atomic(slot / "slot.json", {"generation": op["generation"], "usedAt": time.time()})
            (slot / "lease.json").unlink()
            if op["collected"]:
                shutil.rmtree(self.opdir(identity) / "changes", ignore_errors=True)
            self.evict(slot.parent)
            return op
        raise HostExecError("invalid_params", "Unknown host method")

    @staticmethod
    async def terminate(child: asyncio.subprocess.Process) -> None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            await asyncio.wait_for(child.wait(), 3)
        except asyncio.TimeoutError:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await child.wait()

    async def command(self, identity: str, op: dict[str, Any], argv: list[str], cwd: Path,
                      timeout: float, before: dict[str, dict[str, Any]]) -> None:
        reason, code = "exited", None
        output_bytes = 0
        child = None
        slot = self.slot(op)
        environment = {**os.environ, "HOST_EXEC_DERIVED_DATA": op["derivedData"],
                       "HOST_EXEC_PACKAGE_CACHE": op["packageCache"], "HOST_EXEC_ARTIFACTS": op["artifactsPath"]}
        async def drain(pipe: asyncio.StreamReader, stream: str) -> None:
            nonlocal output_bytes, reason
            decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
            tail = ""
            keep = max(map(len, aliases(op["slotPath"])))
            while block := await pipe.read(32768):
                output_bytes += len(block)
                if output_bytes > 64 * 1024**2:
                    reason = "output_limit"
                    assert child is not None
                    await self.terminate(child)
                    break
                tail += decoder.decode(block)
                # Preserve any path prefix split across pipe reads.
                end = max(0, len(tail) - keep)
                for path in aliases(op["slotPath"]):
                    start = tail.rfind(path, 0, end + len(path))
                    if 0 <= start < end < start + len(path):
                        end = start
                if end:
                    self.event(identity, {"stream": stream, "text": mapped(tail[:end], op["slotPath"], op["linePath"])})
                    tail = tail[end:]
            self.event(identity, {"stream": stream, "text": mapped(tail + decoder.decode(b"", final=True), op["slotPath"], op["linePath"])})
        try:
            if self.load(identity).get("cancelRequested"):
                reason = "cancelled"
            else:
                child = await asyncio.create_subprocess_exec(*argv, cwd=str(cwd), env=environment,
                    stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
                self.children[identity] = child
                op["pid"] = child.pid
                saved = self.load(identity)
                op.update(saved, pid=child.pid)
                self.save(identity, op)
                if op.get("cancelRequested"):
                    await self.terminate(child)
                assert child.stdout is not None and child.stderr is not None
                readers = [asyncio.create_task(drain(child.stdout, "stdout")), asyncio.create_task(drain(child.stderr, "stderr"))]
                async def budget_watch() -> None:
                    nonlocal reason
                    while child is not None and child.returncode is None:
                        try:
                            self.evict(slot.parent, exclude=slot)
                        except HostExecError:
                            reason = "disk_budget"
                            await self.terminate(child)
                            return
                        await asyncio.sleep(1)
                watchdog = asyncio.create_task(budget_watch())
                try:
                    await asyncio.wait_for(child.wait(), timeout)
                except asyncio.TimeoutError:
                    reason = "timeout"
                    await self.terminate(child)
                finally:
                    watchdog.cancel()
                    await asyncio.gather(watchdog, return_exceptions=True)
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                await asyncio.wait_for(asyncio.gather(*readers), 6)
                code = child.returncode
                if self.load(identity).get("cancelRequested"):
                    reason = "cancelled"
            after = scan(slot / "source")
            changed = {path: after.get(path) for path in before.keys() | after.keys() if before.get(path) != after.get(path)}
            result_dir = self.opdir(identity) / "changes"
            result_dir.mkdir()
            for path, entry in changed.items():
                if entry is None or entry["kind"] == "directory":
                    continue
                source, target = inside(slot / "source", path), inside(result_dir, path)
                target.parent.mkdir(parents=True, exist_ok=True)
                if entry["kind"] == "link":
                    target.symlink_to(entry["target"])
                else:
                    shutil.copyfile(source, target, follow_symlinks=False)
                    target.chmod(entry["mode"])
            artifact_manifest = scan(self.opdir(identity) / "artifacts")
            atomic(self.opdir(identity) / "changes.json", changed)
            atomic(self.opdir(identity) / "artifacts.json", artifact_manifest)
            op.update(state="exited", exitCode=code, reason=reason, exitedAt=time.time(), changedPaths=len(changed),
                      artifactFiles=len(artifact_manifest), nameConflicts=conflicts(list(after)))
            self.save(identity, op)
            self.event(identity, {"state": "exited", "exitCode": code, "reason": reason})
        except Exception as exc:
            if child is not None:
                await self.terminate(child)
            op.update(state="unknown", reason="host_command_failed", error=str(exc)[:1000])
            self.save(identity, op)
        finally:
            self.children.pop(identity, None)
            self.commands.pop(identity, None)

    async def request(self, request: dict[str, Any]) -> dict[str, Any]:
        identity: Any = request.get("id")
        try:
            require(isinstance(identity, str) and 0 < len(identity) <= 128, "Invalid request ID")
            token = request.get("token", "")
            require(isinstance(token, str) and hmac.compare_digest(token, self.token), "The guest host token differs", "forbidden")
            method, params = request["method"], request.get("params", {})
            require(isinstance(method, str) and isinstance(params, dict), "Invalid request")
            digest = hashlib.sha256(json.dumps([method, params], sort_keys=True, allow_nan=False).encode()).hexdigest()
            if method == "health":
                return {"id": identity, "result": {"protocol": 1}}
            readonly = method in {"status", "manifest", "attach", "read", "result.manifest"}
            if readonly:
                return {"id": identity, "result": await self.dispatch(method, params)}
            saved = self.db.execute("SELECT digest,response FROM receipts WHERE id=?", (identity,)).fetchone()
            if saved:
                require(saved[0] == digest, "The request ID has different content", "id_conflict")
                if identity in self.active:
                    return await asyncio.shield(self.active[identity])
                require(saved[1] is not None, "The request has no proven result; inspect its operation", "outcome_unknown")
                value: dict[str, Any] = json.loads(saved[1])
                return value
            self.db.execute("INSERT INTO receipts VALUES (?,?,NULL)", (identity, digest))
            self.db.commit()
            async def operation() -> dict[str, Any]:
                try:
                    result = {"id": identity, "result": await self.dispatch(method, params)}
                except HostExecError as exc:
                    result = {"id": identity, "error": exc.object()}
                except Exception as exc:
                    result = {"id": identity, "error": {"code": "outcome_unknown", "message": str(exc)[:1000]}}
                self.db.execute("UPDATE receipts SET response=? WHERE id=?", (json.dumps(result), identity))
                self.db.commit()
                return result
            task = asyncio.create_task(operation())
            self.active[identity] = task
            try:
                return await asyncio.shield(task)
            finally:
                self.active.pop(identity, None)
        except HostExecError as exc:
            return {"id": identity, "error": exc.object()}
        except Exception:
            return {"id": identity, "error": {"code": "invalid_request", "message": "Invalid host request"}}

    async def client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if self.clients >= 64:
            writer.close()
            return
        self.clients += 1
        try:
            line = await asyncio.wait_for(reader.readline(), 30)
            require(len(line) <= FRAME_LIMIT, "The request frame exceeds its limit")
            response = await self.request(json.loads(line))
            data = json.dumps(response).encode() + b"\n"
            require(len(data) <= FRAME_LIMIT, "The host result exceeds its frame limit", "output_limit")
            writer.write(data)
            await asyncio.wait_for(writer.drain(), 5)
        except Exception:
            pass
        finally:
            self.clients -= 1
            writer.close()


async def serve(state: Path) -> None:
    service = HostService(state)
    path = state / "host-exec.sock"
    path.unlink(missing_ok=True)
    server = await asyncio.start_unix_server(service.client, path=str(path), limit=FRAME_LIMIT + 1)
    path.chmod(0o600)
    try:
        async with server:
            await server.serve_forever()
    finally:
        for child in list(service.children.values()):
            await service.terminate(child)
        service.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("state", type=Path)
    options = parser.parse_args()
    os.umask(0o077)
    asyncio.run(serve(options.state))
