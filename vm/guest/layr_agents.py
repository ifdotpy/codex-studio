"""Root broker actions for owned layr lines and unprivileged provider children."""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import pwd
import stat
import subprocess
import sys
import uuid

from common import GuestError, atomic_json, identifier, require
from native import Native

READ_METHODS = {"agent.context", "line.status", "line.evidence", "layr.provider.list",
                "layr.provider.rpc", "agent.progress", "layr.file.stat", "layr.file.read"}
METHODS = READ_METHODS | {"line.bind", "line.branch", "line.save", "line.merge", "line.remove", "agent.release",
                          "layr.provider.start", "layr.provider.stop", "layr.credentials.sync", "layr.exec"}


def _user(name):
    try:
        user = pwd.getpwnam(name)
    except KeyError:
        subprocess.run(["/usr/sbin/useradd", "--create-home", "--user-group", "--shell", "/bin/bash", name],
                       check=True, capture_output=True, timeout=30)
        user = pwd.getpwnam(name)
    require(user.pw_uid != 0 and user.pw_dir == "/home/" + name,
            "The agent user identity differs from its fixed home")
    home = Path(user.pw_dir)
    require(not home.is_symlink() and home.is_dir(), "The agent home must be a real directory")
    home.chmod(0o700)
    return {"owner": name, "uid": user.pw_uid, "gid": user.pw_gid, "home": str(home)}


def ensure_project_owner(project_id):
    identifier(project_id)
    return _user("studio-p-" + hashlib.sha256(project_id.encode()).hexdigest()[:16])


class AgentHandlers:
    def __init__(self, state: Path, layr_root: Path):
        self.state, self.layr_root = state, layr_root
        self.associations = state / "agents"
        self.associations.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.native = Native(state)
        self.locks = {}

    def record_path(self, agent_id):
        require(str(uuid.UUID(agent_id)) == agent_id, "The agent ID must be a canonical UUID")
        return self.associations / (agent_id + ".json")

    def context(self, agent_id):
        path = self.record_path(agent_id)
        if not path.exists():
            raise GuestError("not_found", "The layr agent association does not exist")
        value = json.loads(path.read_text())
        root = self.project(value["projectId"]) / "lines" / value["line"]
        require(root.resolve() == root and root.is_dir(), "The layr line is unavailable")
        cwd = root / value.get("subpath", ".")
        require(cwd.resolve().is_relative_to(root) and cwd.is_dir(), "The layr directory is unavailable")
        return {**value, "path": str(root), "cwd": str(cwd)}

    def project(self, project_id):
        identifier(project_id)
        require(all(c.isascii() and (c.isalnum() or c in "-_") for c in project_id),
                "The project ID is invalid")
        path = self.layr_root / "projects" / project_id
        require(path.resolve() == path and path.is_dir(), "The layr project does not exist")
        return path

    async def command(self, cwd, args, *, user=None, timeout=120, env=None, stdin=None):
        command = list(args)
        environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8",
                       "LAYR_ROOT": str(self.layr_root), "HOME": "/root"}
        if user:
            environment.update(HOME=user["home"], USER=user["owner"], LOGNAME=user["owner"])
            command = ["/usr/sbin/runuser", "-u", user["owner"], "--", *command]
        environment.update(env or {})
        # The helper owns the deadline and output bound after a broker failure.
        command = [sys.executable, str(Path(__file__).with_name("deadline.py")), str(timeout), *command]
        process = await asyncio.create_subprocess_exec(*command, cwd=cwd, env=environment,
                    stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = await asyncio.wait_for(process.communicate(stdin), timeout)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            import signal
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await process.wait()
            raise GuestError("outcome_unknown", "The layr command has no proven result") from None
        require(len(stdout) + len(stderr) <= 4 * 1024 * 1024, "The layr output exceeds its limit")
        if process.returncode:
            raise GuestError("operation_failed", "The layr command failed: " + stderr.decode(errors="replace")[:1000])
        return stdout.decode(errors="replace").strip()

    async def layr(self, context, *args, root=False):
        return await self.command(context["path"], ["layr", *args], user=None if root else context)

    async def head(self, context):
        return await self.layr(context, "rev-parse", "HEAD", root=True)

    async def dispatch(self, request_id, method, params, emit):
        agent = params.get("agentId")
        lock = self.locks.setdefault(agent or params.get("handle") or "providers", asyncio.Lock())
        async with lock:
            return await self.dispatch_locked(request_id, method, params, emit)

    async def dispatch_locked(self, request_id, method, params, emit):
        if method == "layr.provider.list":
            providers = []
            for path in self.associations.glob("*.json"):
                saved = json.loads(path.read_text())
                handle = saved.get("providerHandle")
                if handle:
                    providers.append({**self.native.info(handle), "agentId": saved["agentId"], "transport": "native"})
            return {"providers": providers}
        if method.startswith("layr.provider.") and method != "layr.provider.start":
            handle = params.get("handle")
            matches = [json.loads(p.read_text()) for p in self.associations.glob("*.json")
                       if json.loads(p.read_text()).get("providerHandle") == handle]
            require(len(matches) == 1, "The VM provider association does not exist")
            if method == "layr.provider.rpc":
                return await self.native.rpc(params)
            require(method == "layr.provider.stop", "The provider operation is invalid")
            return await self.native.stop(handle)
        if method in {"line.bind", "line.branch"}:
            project = self.project(params["projectId"])
            record = self.record_path(params["agentId"])
            if record.exists():
                context = self.context(params["agentId"])
                require(context["projectId"] == params["projectId"], "The agent belongs to another project")
                if method == "line.branch":
                    require(context.get("parentAgentId") == params["parentAgentId"]
                            and context["readOnly"] is params.get("readOnly", False)
                            and context.get("subpath", ".") == params.get("subpath", ".")
                            and context.get("requestedState") == params.get("stateId"),
                            "The line request has different content")
                return {**context, "stateId": await self.head(context)}
            main = project / "lines" / "main"
            if method == "line.bind":
                require(params.get("line") == "main", "A lead must use the main line")
                user = ensure_project_owner(params["projectId"])
                require(main.stat().st_uid == user["uid"], "The main line has another owner")
                name, subpath, readonly = "main", ".", False
                extra = {}
            else:
                parent = self.context(params["parentAgentId"])
                require(parent["projectId"] == params["projectId"] and not parent["readOnly"],
                        "The parent must own a writable line in this project")
                user = _user("studio-a-" + uuid.UUID(params["agentId"]).hex[:22])
                name = params["agentId"]
                readonly = params.get("readOnly", False)
                require(type(readonly) is bool, "readOnly must be a boolean")
                subpath = params.get("subpath", ".")
                require(isinstance(subpath, str) and not PurePosixPath(subpath).is_absolute()
                        and ".." not in PurePosixPath(subpath).parts, "The line subpath is invalid")
                source = params.get("stateId") or await self.head(parent)
                require(isinstance(source, str) and source and not source.startswith("-"), "The state ID is invalid")
                target = project / "lines" / name
                # The broker's durable receipt reserves the request before this command.
                # A pre-existing line without our association is an unknown outcome.
                require(not target.exists(), "The line exists without a proven agent association")
                await self.layr(parent, "branch", name, source, "--owner", "0" if readonly else user["owner"], root=True)
                if readonly:
                    await self.command(str(project), ["setfacl", "-RP", "-m", "u:" + str(user["uid"]) + ":r-X", str(target)])
                    await self.command(str(project), ["btrfs", "property", "set", "-ts", str(target), "ro", "true"])
                extra = {"parentAgentId": parent["agentId"], "requestedState": params.get("stateId")}
            value = {"agentId": params["agentId"], "projectId": params["projectId"], "line": name,
                     "subpath": subpath, "readOnly": readonly, **user, **extra}
            atomic_json(record, value)
            context = self.context(params["agentId"])
            if method == "line.bind":
                setup = "from pathlib import Path; import sys; p=Path(sys.argv[1]); p.mkdir(parents=True,exist_ok=True,mode=0o700); f=p/'PROGRESS.md'; f.touch(mode=0o600,exist_ok=True)"
                await self.command(user["home"], ["python3", "-c", setup,
                    str(Path(user["home"]) / ".studio/progress" / params["agentId"])], user=user)
            return {**context, "stateId": await self.head(context)}
        context = self.context(params["agentId"])
        if method == "agent.context":
            return context
        if method == "agent.progress":
            path = Path(context["home"]) / ".studio/progress" / context["agentId"] / "PROGRESS.md"
            code = "import os,stat,sys,json; p=sys.argv[1]; f=os.open(p,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK); s=os.fstat(f); assert stat.S_ISREG(s.st_mode) and s.st_size<=131072; b=os.read(f,131073); assert len(b)<=131072; print(json.dumps(dict(markdown=b.decode(),revision=str(s.st_mtime_ns)+':'+str(s.st_size),updated=s.st_mtime,exists=True)))"
            output = await self.command(context["home"], ["python3", "-c", code, str(path)], user=context)
            return {"id": context["agentId"], "agent": context["agentId"], "format": "markdown",
                    "path": str(path), "error": None, **json.loads(output)}
        if method == "line.status":
            return {**context, "stateId": await self.head(context)}
        if method == "line.evidence":
            state = await self.head(context)
            require(state == params.get("stateId"), "The submitted state is not the line head")
            return {"projectId": context["projectId"], "line": context["line"], "stateId": state}
        if method == "line.save":
            turn = identifier(params.get("turnId"))
            await self.layr(context, "save", "--turn-end", "--force", "-m", "Studio turn " + turn, root=context["readOnly"])
            state = await self.layr(context, "rev-parse", "HEAD@{0}", root=True)
            return {"stateId": state, "turnId": turn}
        if method == "line.merge":
            require(context["line"] == "main" and not context["readOnly"], "Only the lead can merge results")
            source = self.context(params["sourceAgentId"])
            require(source["projectId"] == context["projectId"] and not source["readOnly"], "The source line is invalid")
            expected = params.get("expectedStateId")
            require(isinstance(expected, str) and expected == await self.head(source), "The reviewed state changed")
            output = await self.layr(context, "merge", source["line"], "--expect", expected)
            return {"stateId": await self.head(context), "reviewedStateId": expected, "output": output}
        if method == "agent.release":
            require(context["line"] == "main", "Only a lead releases the main association")
            if context.get("providerHandle"):
                await self.native.stop(context["providerHandle"])
            self.record_path(context["agentId"]).unlink()
            return {"state": "released", "line": "main"}
        if method == "line.remove":
            require(context["line"] != "main", "The main line cannot be removed with a worker")
            if context.get("providerHandle"):
                await self.native.stop(context["providerHandle"])
            main = {**context, "path": str(self.project(context["projectId"]) / "lines" / "main")}
            if context["readOnly"]:
                await self.command(main["path"], ["btrfs", "property", "set", "-ts", context["path"], "ro", "false"])
            await self.layr(main, "branch", "-D", context["line"], root=True)
            self.record_path(context["agentId"]).unlink()
            return {"state": "removed", "freedBytes": 0}
        if method in {"layr.provider.start", "layr.credentials.sync"}:
            provider = params.get("provider")
            require(provider in {"codex", "claude"}, "The provider is invalid")
            profile = params.get("profile")
            prefix = (".claude" if provider == "claude" else ".codex") + "/studio-accounts/"
            require(isinstance(profile, str) and profile.startswith(prefix)
                    and len(profile[len(prefix):]) == 32
                    and all(c in "0123456789abcdef" for c in profile[len(prefix):]), "The profile path is invalid")
            source = Path("/home/studio") / profile / (".credentials.json" if provider == "claude" else "auth.json")
            require(source.resolve() == source and stat.S_ISREG(source.stat().st_mode), "The credential profile is invalid")
            fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, "rb") as stream:
                require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "The credential profile must be a regular file")
                data = stream.read(1024 * 1024 + 1)
            require(len(data) <= 1024 * 1024, "The credential exceeds its limit")
            home = Path(context["home"])
            # Never write through a provider-controlled home path as root.
            # Drop privilege before creating the private profile and bridge directories.
            setup = "import os,sys; from pathlib import Path; p=Path(sys.argv[1]); p.mkdir(parents=True,exist_ok=True,mode=0o700); t=p/sys.argv[2]; t.write_bytes(sys.stdin.buffer.read()); t.chmod(0o600)"
            await self.command(str(home), ["python3", "-c", setup, str(home / profile), source.name], user=context, stdin=data)
            if method == "layr.credentials.sync":
                return {"synced": True}
            handle = params.get("handle")
            require(handle == "linux-worker:" + context["agentId"], "The provider handle differs from its agent")
            argv = params.get("argv")
            require(isinstance(argv, list) and all(isinstance(a, str) and "\0" not in a for a in argv), "The provider argv is invalid")
            expected = ["codex", "app-server", "--listen", "stdio://", "-c", 'cli_auth_credentials_store="file"']
            if provider == "claude":
                bridge = str(home / ".codex/studio-bridges" / context["agentId"])
                expected = ["node", "/opt/codex-studio/claude_bridge/bridge.mjs", bridge]
                await self.command(str(home), ["mkdir", "-p", str(home / ".codex/studio-bridges")], user=context)
            require(argv == expected, "The provider launch command is invalid")
            env = params.get("env", {})
            allowed = {"CODEX_HOME"} if provider == "codex" else {
                "CLAUDE_CONFIG_DIR", "STUDIO_CLAUDE_BIN", "STUDIO_CLAUDE_OPTIONS", "STUDIO_CLAUDE_ACCOUNT"}
            require(isinstance(env, dict) and set(env) <= allowed
                    and all(isinstance(v, str) and "\0" not in v for v in env.values()), "The provider environment is invalid")
            require(env.get("CODEX_HOME" if provider == "codex" else "CLAUDE_CONFIG_DIR") == str(home / profile),
                    "The provider home differs from the agent profile")
            require(params.get("cwd") == context["cwd"], "The provider cwd differs from its line")
            launch = {"argv": ["/usr/sbin/runuser", "-u", context["owner"], "--", *argv],
                      "cwd": context["cwd"], "env": {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8",
                      "HOME": str(home), "USER": context["owner"], "LOGNAME": context["owner"],
                      "LAYR_ROOT": str(self.layr_root), **env}, "agentId": context["agentId"]}
            context["providerHandle"] = handle
            atomic_json(self.record_path(context["agentId"]), {k: v for k, v in context.items() if k not in {"cwd", "path"}})
            directory = self.state / "providers" / uuid.uuid5(uuid.NAMESPACE_URL, handle).hex
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            return await self.native.start(handle, launch, directory)
        if method == "layr.exec":
            args = params.get("argv")
            require(isinstance(args, list) and args and all(isinstance(a, str) and "\0" not in a for a in args), "The command argv is invalid")
            cwd = Path(params.get("cwd", context["cwd"]))
            require(cwd.resolve().is_relative_to(Path(context["path"])), "The command cwd is outside its line")
            data = base64.b64decode(params.get("stdin", ""), validate=True)
            require(len(data) <= 1024 * 1024, "The command input exceeds its limit")
            # The detached supervisor owns the journal and deadline.
            # Its command runs as the line owner after runuser drops privilege.
            directory = self.state / "commands" / uuid.uuid5(uuid.NAMESPACE_URL, request_id).hex
            from common import private_dir
            private_dir(directory)
            config = {"argv": ["/usr/sbin/runuser", "-u", context["owner"], "--", *args],
                      "cwd": str(cwd), "env": {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": context["home"],
                      "USER": context["owner"], "LOGNAME": context["owner"], "LANG": "C.UTF-8",
                      "LAYR_ROOT": str(self.layr_root)}, "agentId": context["agentId"],
                      "stdin": params.get("stdin", ""), "timeoutSeconds": 300,
                      "outputLimitBytes": 512 * 1024}
            require(not (directory / "association.json").exists(), "The command already has an uncertain launch")
            atomic_json(directory / "association.json", {"agentId": context["agentId"], "requestId": request_id})
            atomic_json(directory / "launch.json", config)
            process = subprocess.Popen([sys.executable, str(Path(__file__).with_name("supervisor.py")), str(directory)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                start_new_session=True, close_fds=True)
            async def reap():
                await asyncio.to_thread(process.wait)
            asyncio.create_task(reap())
            status_path = directory / "status.json"
            deadline = asyncio.get_running_loop().time() + 310
            while True:
                status = json.loads(status_path.read_text()) if status_path.exists() else None
                if status and status["state"] == "exited":
                    break
                if process.poll() is not None or asyncio.get_running_loop().time() > deadline:
                    raise GuestError("outcome_unknown", "The command has no proven exit result")
                await asyncio.sleep(0.05)
            stdout, stderr = bytearray(), bytearray()
            from contextlib import closing
            import sqlite3
            with closing(sqlite3.connect(directory / "journal.sqlite3")) as db:
                rows = db.execute("SELECT event,data FROM events ORDER BY seq").fetchall()
            for row in rows:
                if row[0] == "output":
                    event = json.loads(row[1])
                    (stdout if event["stream"] == "stdout" else stderr).extend(base64.b64decode(event["data"]))
            result = {"stdout": base64.b64encode(stdout).decode(), "stderr": base64.b64encode(stderr).decode(),
                      "exitCode": status["exitCode"], "reason": status["reason"]}
            return result
        if method.startswith("layr.file."):
            path = Path(params["path"])
            root = Path(context["path"])
            home = Path(context["home"])
            allowed = root if path.is_relative_to(root) else home
            require(path.is_relative_to(allowed) and ".." not in path.parts, "The file path is outside the agent roots")
            values = {**params, "method": method.removeprefix("layr."), "allowedRoot": str(allowed)}
            script = Path(__file__).with_name("files.py")
            output = await self.command(context["home"], ["python3", str(script)], user=context,
                                        stdin=json.dumps(values).encode())
            result = json.loads(output)
            if result.get("error"):
                raise GuestError("operation_failed", "The guest file is unavailable")
            return result["result"]
        raise GuestError("invalid_request", "The layr agent method is not supported")
