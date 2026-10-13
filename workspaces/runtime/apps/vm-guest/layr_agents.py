"""Root broker actions for owned layr lines and unprivileged provider children."""
from __future__ import annotations

import asyncio
import base64
import grp
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
from project_env import wrap

READ_METHODS = {"agent.context", "line.status", "line.evidence", "layr.provider.list",
                "layr.provider.rpc", "agent.progress", "layr.file.stat", "layr.file.read"}
METHODS = READ_METHODS | {"line.bind", "line.branch", "line.save", "line.merge", "line.remove", "agent.release",
                          "layr.provider.start", "layr.provider.stop", "layr.credentials.sync", "layr.exec"}


# Every agent user is in this group: the project's layr deny rules for it keep agents away
# from roles, replication, remotes, records and backups whatever role an agent gets.
AGENTS_GROUP = "studio-agents"

# The names that the launch of an agent process sets (AgentHandlers.agent_env; runuser adds
# SHELL), and the names that each provider adds. A project secret never takes one of them
# (project_secrets.py).
LAUNCH_ENV_NAMES = frozenset({"PATH", "LANG", "HOME", "USER", "LOGNAME", "SHELL", "LAYR_ROOT", "LAYR_SESSION",
                              "BROWSER"})
# A sign-in command opens its link with this shim: Studio asks the person to approve the link
# and opens it in the browser of the Mac (browser_bridge.py, docs/vm-browser-bridge.md).
BROWSER = "/usr/local/bin/studio-browser"
PROVIDER_ENV = {"codex": frozenset({"CODEX_HOME"}),
                "claude": frozenset({"CLAUDE_CONFIG_DIR", "STUDIO_CLAUDE_BIN", "STUDIO_CLAUDE_OPTIONS",
                                     "STUDIO_CLAUDE_ACCOUNT"})}


def ensure_group(name):
    try:
        grp.getgrnam(name)
    except KeyError:
        subprocess.run(["/usr/sbin/groupadd", "--system", name], check=True, capture_output=True, timeout=30)


def ensure_agents_group():
    ensure_group(AGENTS_GROUP)


def project_group(project_id):
    """The group of every agent user of one project: a layr principal (`@studio-g-...`) for the
    readers of an overlay that all agents of the project, and no other project, may read."""
    identifier(project_id)
    return "studio-g-" + hashlib.sha256(project_id.encode()).hexdigest()[:16]


def join_project_group(user, project_id):
    group = project_group(project_id)
    ensure_group(group)
    if user["owner"] not in grp.getgrnam(group).gr_mem:
        subprocess.run(["/usr/sbin/usermod", "-aG", group, user["owner"]], check=True, capture_output=True, timeout=30)
    return user


def _user(name):
    ensure_agents_group()
    try:
        user = pwd.getpwnam(name)
    except KeyError:
        subprocess.run(["/usr/sbin/useradd", "--create-home", "--user-group", "--groups", AGENTS_GROUP,
                        "--shell", "/bin/bash", name], check=True, capture_output=True, timeout=30)
        user = pwd.getpwnam(name)
    if name not in grp.getgrnam(AGENTS_GROUP).gr_mem:
        subprocess.run(["/usr/sbin/usermod", "-aG", AGENTS_GROUP, name], check=True, capture_output=True, timeout=30)
    require(user.pw_uid != 0 and user.pw_dir == "/home/" + name,
            "The agent user identity differs from its fixed home")
    home = Path(user.pw_dir)
    require(not home.is_symlink() and home.is_dir(), "The agent home must be a real directory")
    home.chmod(0o700)
    return {"owner": name, "uid": user.pw_uid, "gid": user.pw_gid, "home": str(home)}


def ensure_project_owner(project_id):
    identifier(project_id)
    return join_project_group(_user("studio-p-" + hashlib.sha256(project_id.encode()).hexdigest()[:16]), project_id)


class AgentHandlers:
    def __init__(self, state: Path, layr_root: Path):
        self.state, self.layr_root = state, layr_root
        self.associations = state / "agents"
        self.associations.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.native = Native(state)
        self.locks = {}
        # The studio user's account profiles, the source of each agent's credential copy.
        self.studio_home = Path("/home/studio")
        self.join_project_groups()

    def join_project_groups(self):
        """Users made before project groups existed join them at service start: each project's
        main owner and each recorded agent. A user whose record is unreadable stays as it is."""
        members = [(path.name, None) for path in (self.layr_root / "projects").glob("*")
                   if (path / "lines" / "main").is_dir()]
        members += [(None, path) for path in sorted(self.associations.glob("*.json"))]
        for project_id, record in members:
            try:
                if record is None:
                    ensure_project_owner(project_id)
                else:
                    value = json.loads(record.read_text())
                    require(str(value["owner"]).startswith(("studio-a-", "studio-p-")),
                            "The record owner is not an agent user")
                    join_project_group({"owner": value["owner"]}, value["projectId"])
            except Exception as exc:
                print(f"project group: {project_id or record.name}: {exc}", file=sys.stderr, flush=True)

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
            if user.get("agentId"):
                # The agent's operations carry its ID (layr op log, and undo finds "my last").
                environment["LAYR_SESSION"] = user["agentId"]
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

    def agent_env(self, context, extra=None):
        """The environment of an agent process. Its project's secrets come at the start of the
        process from the wrapper (project_env.py), never from this launch record."""
        return {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": context["home"],
                "USER": context["owner"], "LOGNAME": context["owner"], "LAYR_ROOT": str(self.layr_root),
                "LAYR_SESSION": context["agentId"], "BROWSER": BROWSER, **(extra or {})}

    def agent_argv(self, context, command):
        """The command as the line owner, with the project's secrets (project_env.py)."""
        return wrap(self.state, context["projectId"], context["owner"], command)

    async def layr(self, context, *args, root=False):
        return await self.command(context["path"], ["layr", *args], user=None if root else context)

    async def head(self, context):
        return await self.layr(context, "rev-parse", "HEAD", root=True)

    async def branch_line(self, params, project, record):
        parent = self.context(params["parentAgentId"])
        require(parent["projectId"] == params["projectId"] and not parent["readOnly"],
                "The parent must own a writable line in this project")
        readonly = params.get("readOnly", False)
        require(type(readonly) is bool, "readOnly must be a boolean")
        subpath = params.get("subpath", ".")
        require(isinstance(subpath, str) and not PurePosixPath(subpath).is_absolute()
                and ".." not in PurePosixPath(subpath).parts, "The line subpath is invalid")
        name = params["agentId"]
        target = project / "lines" / name
        # The broker's durable receipt reserves the request before this command.
        # A pre-existing line without our association is an unknown outcome.
        require(not target.exists(), "The line exists without a proven agent association")
        if readonly:
            # A reviewer line needs setfacl; find a missing tool before the line exists.
            await self.command(str(project), ["setfacl", "--version"])
        user = join_project_group(_user("studio-a-" + uuid.UUID(name).hex[:22]), params["projectId"])
        source = params.get("stateId") or await self.head(parent)
        require(isinstance(source, str) and source and not source.startswith("-"), "The state ID is invalid")
        created = False
        try:
            await self.layr(parent, "branch", name, source, "--owner", "0" if readonly else user["owner"], root=True)
            created = True
            if readonly:
                await self.command(str(project), ["setfacl", "-RP", "-m", "u:" + str(user["uid"]) + ":r-X", str(target)])
                await self.command(str(project), ["btrfs", "property", "set", "-ts", str(target), "ro", "true"])
            value = {"agentId": name, "projectId": params["projectId"], "line": name,
                     "subpath": subpath, "readOnly": readonly, **user,
                     "parentAgentId": parent["agentId"], "requestedState": params.get("stateId")}
            atomic_json(record, value)
            context = self.context(name)
            return {**context, "stateId": await self.head(context)}
        except BaseException:
            try:
                # Never expose an association for a line whose setup failed.
                record.unlink(missing_ok=True)
                if created or target.exists():
                    # layr deletes read-only subvolumes without changing their flag.
                    await self.layr(parent, "branch", "-D", name, root=True)
                require(not target.exists(), "The failed line still exists after rollback")
            except BaseException as rollback_error:
                raise GuestError("outcome_unknown",
                                 "The line setup failed and its rollback has no proven result") from rollback_error
            raise

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
                return await self.branch_line(params, project, record)
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
            # The end of a turn is a quiet point: cache folders that a build tool made again
            # become caches, then the line's content is saved.
            await self.layr(context, "cache", "repair", root=context["readOnly"])
            await self.layr(context, "save", "--force", "-m", "Studio turn " + turn, root=context["readOnly"])
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
            source = self.studio_home / profile / (".credentials.json" if provider == "claude" else "auth.json")
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
            require(isinstance(env, dict) and set(env) <= PROVIDER_ENV[provider]
                    and all(isinstance(v, str) and "\0" not in v for v in env.values()), "The provider environment is invalid")
            require(env.get("CODEX_HOME" if provider == "codex" else "CLAUDE_CONFIG_DIR") == str(home / profile),
                    "The provider home differs from the agent profile")
            require(params.get("cwd") == context["cwd"], "The provider cwd differs from its line")
            launch = {"argv": self.agent_argv(context, argv), "cwd": context["cwd"],
                      "env": self.agent_env(context, env), "agentId": context["agentId"]}
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
            # Its command runs as the line owner after runuser drops privilege.
            config = {"argv": self.agent_argv(context, args), "cwd": str(cwd), "env": self.agent_env(context),
                      "agentId": context["agentId"],
                      "stdin": params.get("stdin", ""), "timeoutSeconds": 300,
                      "outputLimitBytes": 512 * 1024}
            from common import supervised
            return await supervised(self.state / "commands" / uuid.uuid5(uuid.NAMESPACE_URL, request_id).hex, config,
                                    {"agentId": context["agentId"], "requestId": request_id}, 310)
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
