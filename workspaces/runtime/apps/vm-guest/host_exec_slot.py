"""Root broker slot adapter.

A slot is a folder of the agent that holds one layr state of its line: `layr save` captures the
line, `layr export --all` writes the state into the folder (incrementally after the first time),
and the collect step brings the paths that the Mac command changed back into the line with a
three-way merge against that held state (host_exec_slot_io.py, as the agent).

layr keeps cache folders (`layr cache list`) outside states, so the export has none of their files.
The Mac receives them from the line folder itself, and changes inside them never return.
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import signal
import stat
import sys
from typing import Any

from host_exec_protocol import HostExecError, atomic, conflicts, inside, key, page, relative, require, under

EXPORT_MARKER = ".layr-export.json"
METHODS = {"host.slot.prepare", "host.slot.manifest", "host.slot.read", "host.slot.apply", "host.slot.collect", "host.slot.forget", "host.slot.artifact"}
READ_METHODS = {"host.slot.manifest", "host.slot.read"}


def traversable(path: Path) -> None:
    """A folder the line owner passes through to its own slot. The services run with
    UMask=0077, which would make mkdir's 0711 a 0700. The chmod must never follow a link."""
    path.mkdir(exist_ok=True, mode=0o711)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid(),
            "The slot parent must be a broker-owned directory", "forbidden")
    path.chmod(0o711)


class HostSlotHandlers:
    def __init__(self, state: Path, layr_root: Path, agents: Any):
        self.state, self.layr_root, self.agents = state, layr_root, agents
        self.root = layr_root.parent / "host-slots"
        traversable(self.root)
        # Sorted manifests during paging, by manifest file.
        self.pages: dict[str, list] = {}

    @staticmethod
    def slot_paths(parent: Path) -> dict[str, list[str]]:
        """The cache folders of the prepared line and the paths that stay on the Mac."""
        try:
            value = json.loads((parent / "paths.json").read_text())
        except FileNotFoundError:
            value = {}
        return {"caches": list(value.get("caches", [])), "macOnly": list(value.get("macOnly", []))}

    def layr_env(self, context: dict[str, Any]) -> dict[str, str]:
        return {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "LAYR_ROOT": str(self.layr_root),
                "LAYR_PROJECT": context["projectId"], "LAYR_LINE": context["line"], "HOME": context["home"],
                "LAYR_SESSION": context["agentId"]}

    async def layr(self, context: dict[str, Any], args: list[str], ok: tuple[int, ...] = (0,)) -> str:
        environment = self.layr_env(context)
        child = await asyncio.create_subprocess_exec("layr", *args, cwd=context["cwd"], env=environment,
            user=context["uid"], group=context["gid"], extra_groups=[], start_new_session=True,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            stdout, stderr = await asyncio.wait_for(child.communicate(), 1800)
        except asyncio.TimeoutError as exc:
            os.killpg(child.pid, signal.SIGKILL)
            await child.wait()
            raise HostExecError("outcome_unknown", "The layr slot operation exceeded its deadline") from exc
        output = stdout.decode(errors="replace")
        require(child.returncode in ok, "layr " + args[0] + " failed: " + (stderr.decode(errors="replace") + output)[-2000:], "layr_failed")
        return output

    async def io(self, context: dict[str, Any], params: dict[str, Any]) -> dict[str, Any]:
        child = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("host_exec_slot_io.py")),
            user=context["uid"], group=context["gid"], extra_groups=[], start_new_session=True,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        # A manifest hashes every file: a Chromium checkout takes minutes.
        deadline = 1800 if params["action"] == "manifest" else 300
        try:
            output, _ = await asyncio.wait_for(child.communicate(json.dumps(params).encode()), deadline)
        except asyncio.TimeoutError as exc:
            os.killpg(child.pid, signal.SIGKILL)
            await child.wait()
            raise HostExecError("outcome_unknown", "The slot file operation exceeded its deadline") from exc
        require(len(output) <= 1024**3, "The slot manifest exceeds its limit", "output_limit")
        result = json.loads(output)
        if "error" in result:
            raise HostExecError(result["error"]["code"], result["error"]["message"])
        require(child.returncode == 0, "The slot file helper failed", "outcome_unknown")
        return dict(result["result"])

    def hash_cache(self, context: dict[str, Any], root: Path) -> str:
        """The line owner's hash cache file for one scanned folder. The folder of the agent above it
        is root's, so the owner cannot replace the cache folder with a link."""
        folder = self.root / key(context["projectId"]) / key(context["agentId"]) / "hash-cache"
        folder.mkdir(exist_ok=True, mode=0o700)
        os.chown(folder, context["uid"], context["gid"], follow_symlinks=False)
        return str(folder / (key(str(root)) + ".json"))

    async def target(self, context: dict[str, Any], source: Path, parent: Path, manifest_path: Path) -> dict[str, Any]:
        """What the Mac slot must hold: the exported state and the line's cache folders, without the
        Mac-only paths. It is saved for the later pages."""
        paths = self.slot_paths(parent)
        caches, mac_only = paths["caches"], paths["macOnly"]
        entries = (await self.io(context, {"action": "manifest", "root": str(source), "skip": mac_only,
                                           "hashes": self.hash_cache(context, source)}))["entries"]
        entries.pop(EXPORT_MARKER, None)
        line = Path(context["path"])
        for cache in caches:
            # A cache inside another cache comes with the outer one.
            if under(cache, mac_only) or any(under(cache, [other]) for other in caches if other != cache):
                continue
            entries = {path: entry for path, entry in entries.items() if not under(path, [cache])}
            parts = cache.split("/")
            for size in range(1, len(parts) + 1):
                entries.setdefault("/".join(parts[:size]), {"kind": "directory"})
            skip = [item[len(cache) + 1:] for item in mac_only if item.startswith(cache + "/")]
            found = (await self.io(context, {"action": "manifest", "root": str(line / cache), "skip": skip,
                                             "hashes": self.hash_cache(context, line / cache)}))["entries"]
            entries.update({cache + "/" + path: entry for path, entry in found.items()})
        # The Mac keeps the folders above a Mac-only path, so the slot holds them for every line.
        for item in mac_only:
            parts = item.split("/")
            for size in range(1, len(parts)):
                entries.setdefault("/".join(parts[:size]), {"kind": "directory"})
        atomic(manifest_path, entries)
        return entries

    async def dispatch(self, request_id: str, method: str, params: dict[str, Any], emit: Any = None) -> dict[str, Any]:
        context = await self.agents.dispatch(request_id + ":context", "agent.context", {"agentId": params["agentId"]}, emit)
        require(not context.get("readOnly"), "A review line cannot accept host command changes", "forbidden")
        require(isinstance(params.get("slotId"), str) and params["slotId"].isdigit(), "Invalid host slot ID")
        require(isinstance(params.get("generation"), str) and len(params["generation"]) == 32
                and all(char in "0123456789abcdef" for char in params["generation"]), "Invalid host slot generation")
        parent = self.root
        for part in (key(context["projectId"]), key(context["agentId"]), params["slotId"], params["generation"]):
            parent /= part
            traversable(parent)
        source = parent / "source"
        source.mkdir(exist_ok=True, mode=0o700)
        os.chown(source, context["uid"], context["gid"])
        operation = params.get("operationId")
        require(isinstance(operation, str) and 0 < len(operation) <= 128, "Invalid slot operation ID")
        lease = parent / "lease.json"
        # The layr state the slot folder holds (root-owned, next to the agent's folder).
        held = parent / "held.json"
        if method == "host.slot.prepare":
            mac_only = params.get("macOnly", [])
            require(isinstance(mac_only, list) and len(mac_only) <= 256, "Invalid Mac-only paths")
            for item in mac_only:
                relative(item)
            if lease.exists():
                require(json.loads(lease.read_text())["operationId"] == operation,
                        "Another command retains the guest slot lease", "busy")
            atomic(lease, {"operationId": operation})
            await self.layr(context, ["save", "-q", "--force", "-m", "host_exec slot " + params["slotId"]])
            state = (await self.layr(context, ["rev-parse", "HEAD@{0}"])).strip()
            require(len(state) >= 7 and all(c in "0123456789abcdef" for c in state), "layr gave no state ID", "layr_failed")
            if not held.exists():
                # An unknown folder content: start from an empty folder, then a full copy.
                await self.io(context, {"action": "clear", "root": str(source)})
            # Exit code 1: names that differ only in case or Unicode form were not written.
            output = await self.layr(context, ["export", state, str(source), "--all", "--force"], ok=(0, 1))
            bad: list[str] = []
            marker = "they cannot coexist on APFS):"
            if marker in output:
                bad = [line.strip() for line in output.split(marker, 1)[1].splitlines() if line.strip()]
            # Lines "<path>\tcache"; a plain folder at a cache path is in the state and the export.
            caches = []
            for row in (await self.layr(context, ["cache", "list"])).splitlines():
                path, _, kind = row.partition("\t")
                if kind == "cache":
                    caches.append(relative(path))
            atomic(parent / "paths.json", {"caches": caches, "macOnly": mac_only})
            atomic(held, {"state": state})
            return {"path": str(source), "nameConflicts": bad, "syncOutput": output, "caches": caches,
                    "status": "slot holds state " + state}
        require(lease.exists() and json.loads(lease.read_text())["operationId"] == operation,
                "The command does not own its guest slot", "lease_lost")
        manifest_path = parent / (key(operation) + ".manifest.json")
        if method == "host.slot.forget":
            held.unlink(missing_ok=True)
            manifest_path.unlink(missing_ok=True)
            self.pages.pop(str(manifest_path), None)
            await self.io(context, {"action": "delete", "root": str(source), "path": EXPORT_MARKER})
            lease.unlink()
            return {"output": "slot forgotten"}
        if method == "host.slot.manifest":
            if not params.get("after"):
                self.pages[str(manifest_path)] = sorted((await self.target(context, source, parent, manifest_path)).items())
            items = self.pages.get(str(manifest_path))
            if items is None:
                items = sorted(json.loads(manifest_path.read_text()).items())
                self.pages[str(manifest_path)] = items
            result = page(items, params.get("after", ""))
            if result["next"] is None:
                self.pages.pop(str(manifest_path), None)
            return result
        if method == "host.slot.read":
            # A cache file comes from the line folder; the slot folder has no cache files.
            caches = self.slot_paths(parent)["caches"]
            if "paths" in params:
                names = params["paths"]
                require(isinstance(names, list) and 0 < len(names) <= 1024, "Invalid source batch")
                items = [{"root": context["path"] if under(relative(name), caches) else str(source), "path": name}
                         for name in names]
                return await self.io(context, {"action": "read-many", "items": items})
            root = context["path"] if under(params["path"], caches) else str(source)
            return await self.io(context, {**params, "root": root, "action": "read"})
        if method in {"host.slot.apply", "host.slot.artifact"}:
            if method == "host.slot.artifact":
                # Root parents protect the artifact folder inode. The line owner owns its content.
                source = parent / ("artifacts-" + key(operation))
                source.mkdir(exist_ok=True, mode=0o700)
                os.chown(source, context["uid"], context["gid"])
            entry_path = parent / (key(method + ":" + params["path"]) + ".entry.json")
            values = {**params, "root": str(source)}
            if params["action"] == "entry" and params["entry"]["kind"] == "file":
                atomic(entry_path, params["entry"])
            elif params["action"] in {"chunk", "commit"}:
                values["entry"] = json.loads(entry_path.read_text())
            return await self.io(context, values)
        if method == "host.slot.collect":
            paths = params.get("paths", [])
            require(isinstance(paths, list) and len(paths) <= 128, "The collect path count exceeds its limit")
            for path in paths:
                inside(source, path)
            path_list = parent / (key(operation) + ".collect.json")
            all_paths = json.loads(path_list.read_text()) if path_list.exists() else []
            all_paths = sorted(set(all_paths) | set(paths))
            atomic(path_list, all_paths)
            if not params.get("final"):
                return {"output": "", "paths": len(all_paths), "conflicts": []}
            # Cache folders and Mac-only paths never return to the line.
            paths_of = self.slot_paths(parent)
            all_paths = [path for path in all_paths if not under(path, paths_of["caches"] + paths_of["macOnly"])]
            entries = (await self.io(context, {"action": "manifest", "root": str(source),
                                               "hashes": self.hash_cache(context, source)}))["entries"]
            require(not conflicts(list(entries)), "The guest slot names conflict", "name_conflict")
            require(held.exists(), "The slot holds no known state", "lease_lost")
            report = await self.io(context, {"action": "collect", "root": str(source), "line": context["path"],
                                             "held": json.loads(held.read_text())["state"], "paths": all_paths,
                                             "env": self.layr_env(context)})
            output = "collected %d path(s) into line %s\n" % (len(report["copied"]), context["line"])
            if report["conflicts"]:
                output += "conflicts (the line also changed these paths):\n" + "".join(
                    "\t" + line + "\n" for line in report["conflicts"])
            lease.unlink()
            manifest_path.unlink(missing_ok=True)
            self.pages.pop(str(manifest_path), None)
            return {"output": output, "conflicts": report["conflicts"]}
        raise HostExecError("invalid_params", "Unknown host slot method")
