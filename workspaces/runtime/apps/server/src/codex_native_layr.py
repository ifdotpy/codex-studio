"""Native Layr commands and durable project attachment.

The caller supplies a trusted store and executable. A source locator cannot
choose an executable, socket, or privileged service store.
"""
from __future__ import annotations

from contextlib import contextmanager
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
from typing import Iterator

from codex_file_lock import LOCK_EX, LOCK_UN, flock


class Client:
    def __init__(self, state: Path, store: Path, binary: Path, socket: Path | None = None):
        self.state = state.resolve()
        self.store = store.resolve()
        self.binary = binary.resolve(strict=True)
        self.socket = socket

    def command(self, *args: str, cwd: Path | None = None, timeout: float = 300,
                session: str | None = None) -> str:
        environment = dict(os.environ)
        # Do not inherit a different chat's project, line, root, or socket.
        for key in ("LAYR_PROJECT", "LAYR_LINE", "LAYR_ROOT", "LAYR_SOCKET", "LAYR_DIRECT", "LAYR_SESSION"):
            environment.pop(key, None)
        environment["LAYR_ROOT"] = str(self.store)
        environment["LAYR_DIRECT"] = "0" if self.socket is not None else "1"
        if self.socket is not None:
            environment["LAYR_SOCKET"] = str(self.socket)
        if session is not None:
            environment["LAYR_SESSION"] = session
        result = subprocess.run([str(self.binary), *args], cwd=cwd, env=environment,
                                capture_output=True, text=True, timeout=timeout, check=False)
        if result.returncode:
            raise RuntimeError(result.stderr.strip() or "Layr command failed")
        return result.stdout.strip()

    @contextmanager
    def project_lock(self, project: str) -> Iterator[Path]:
        if not re.fullmatch(r"studio-[0-9a-f]{64}", project):
            raise ValueError("Invalid Studio native project identity")
        folder = self.state / "projects" / project
        folder.mkdir(parents=True, exist_ok=True, mode=0o700)
        with (folder / "lock").open("a+b") as lock:
            flock(lock, LOCK_EX)
            try:
                yield folder / "attachment.json"
            finally:
                flock(lock, LOCK_UN)

    @staticmethod
    def write(path: Path, record: dict[str, str]) -> None:
        from codex_workspace_images import _write_json
        _write_json(path, {key: value for key, value in record.items()})
        path.chmod(0o600)

    def ensure_project(self, project_id: str, source: Path) -> dict[str, str]:
        source = source.resolve(strict=True)
        if not source.is_dir():
            raise ValueError("The project path is not a directory")
        if source == self.store or source.is_relative_to(self.store) or self.store.is_relative_to(source):
            raise ValueError("The input folder and Layr store must be separate")
        project = "studio-" + hashlib.sha256(project_id.encode()).hexdigest()
        identity = {"project": project, "source": str(source), "store": str(self.store)}
        with self.project_lock(project) as receipt:
            record: dict[str, str] | None = json.loads(receipt.read_text()) if receipt.exists() else None
            if record is not None and any(record.get(key) != value for key, value in identity.items()):
                raise ValueError("This project already has a different native Layr attachment")
            if record is None:
                # The marker must only be published by Layr after the import.
                if (source / ".layr").exists() or (source / ".layr").is_symlink():
                    raise ValueError("This folder already has a Layr attachment")
                record = {**identity, "phase": "importing"}
                self.write(receipt, record)
                # A lost import response is not permission to repeat init. Keep
                # the receipt pending for explicit reconciliation.
                self.command("init", project, "--from", str(source), "-q", timeout=1800)
                self.command("--project", project, "protect", "main", "--remove")
                record["phase"] = "imported"
                self.write(receipt, record)
            if record.get("phase") == "importing":
                raise RuntimeError("The native Layr import outcome is unknown; inspect the import before retrying")
            if record.get("phase") not in {"imported", "ready"}:
                raise ValueError("Invalid native Layr attachment phase")
            locator = json.loads(self.command("workspace", "attach", project, "--path", str(source)))
            if locator.get("project") != project or Path(locator.get("store", "")) != self.store or locator.get("line") != "main":
                raise RuntimeError("Layr returned a different workspace attachment")
            if record.get("bindingId") and record["bindingId"] != locator.get("binding_id"):
                raise RuntimeError("The native Layr binding identity changed")
            record["phase"] = "ready"
            record["bindingId"] = str(locator["binding_id"])
            self.write(receipt, record)
            return {**record, "cwd": str(source), "line": "main"}

    def ensure_worker(self, project: str, agent_id: str, parent: Path,
                      state_id: str) -> dict[str, str]:
        """Create one owned line at an exact state. Never repeat an unknown branch."""
        parent = parent.resolve(strict=True)
        if not re.fullmatch(r"studio-[0-9a-f]{64}", project):
            raise ValueError("Invalid Studio native project identity")
        line = "agent-" + hashlib.sha256(agent_id.encode()).hexdigest()
        # Project and state are CLI arguments, not paths or shell text. Resolve
        # the state first so receipt identity does not depend on a moving ref.
        state = self.command("--project", project, "rev-parse", state_id, cwd=parent)
        identity = {"project": project, "agent": agent_id, "parent": str(parent),
                    "state": state, "line": line, "store": str(self.store)}
        with self.project_lock(project) as attachment:
            receipt = attachment.parent / (line + ".json")
            record: dict[str, str] | None = json.loads(receipt.read_text()) if receipt.exists() else None
            if record is not None and any(record.get(k) != v for k, v in identity.items()):
                raise ValueError("The native worker request changed")
            if record is None:
                record = {**identity, "phase": "creating"}
                self.write(receipt, record)
                path = self.command("--project", project, "branch", "--path", line, state, cwd=parent)
                info = json.loads(self.command("workspace", "inspect", project, "--line", line))
                if info.get("path") != path or info.get("state") != state:
                    raise RuntimeError("Layr created a different worker workspace")
                record.update(phase="ready", cwd=path, lineId=str(info["line_id"]),
                              projectId=str(info["project_id"]))
                self.write(receipt, record)
            if record.get("phase") != "ready":
                raise RuntimeError("The native worker creation outcome is unknown; inspect the line before retrying")
            worker_path = Path(record["cwd"])
            expected = self.store / "projects" / project / "lines" / line
            if worker_path != expected or worker_path.resolve(strict=True) != expected:
                raise RuntimeError("The native worker folder changed")
            info = json.loads(self.command("workspace", "inspect", project, "--line", line))
            if (info.get("line_id") != record["lineId"]
                    or info.get("project_id") != record["projectId"]
                    or info.get("path") != str(worker_path)):
                raise RuntimeError("The native worker line identity changed")
            return record

    def save_turn(self, project: str, line: str, folder: Path, agent_id: str, turn_id: str) -> str:
        token = hashlib.sha256((agent_id + "\0" + turn_id).encode()).hexdigest()
        identity = {"project": project, "line": line, "agent": agent_id, "turn": turn_id}
        message = "Studio turn " + turn_id
        with self.project_lock(project) as attachment:
            receipt = attachment.parent / ("turn-" + token + ".json")
            record: dict[str, str] | None = json.loads(receipt.read_text()) if receipt.exists() else None
            if record is not None and any(record.get(k) != v for k, v in identity.items()):
                raise ValueError("The native turn save identity changed")
            if record is not None:
                if record.get("phase") == "saved":
                    return str(record["state"])
                # A committed operation proves success even if its reply was lost.
                # An absent row does not prove failure of a service command.
                rows = self.command("--project", project, "--line", line,
                                    "op", "log", "--json", "-n", "500", cwd=folder)
                for raw in rows.splitlines():
                    row = json.loads(raw)
                    if row.get("op") != "save" or row.get("actor", {}).get("session") != agent_id:
                        continue
                    for state in row.get("states", []):
                        if state.get("message") == message:
                            value = self.command("--project", project, "rev-parse", state["id"], cwd=folder)
                            record.update(phase="saved", state=value)
                            self.write(receipt, record)
                            return value
                raise RuntimeError("The native turn save outcome is unknown; the save will not repeat")
            record = {**identity, "phase": "saving"}
            self.write(receipt, record)
            output = self.command("--project", project, "--line", line, "save", "--force", "-m", message,
                                  cwd=folder, session=agent_id)
            fields = output.split()
            if len(fields) != 2 or fields[0] != line:
                raise RuntimeError("Layr returned an invalid turn save result")
            record.update(phase="saved", state=fields[1])
            self.write(receipt, record)
            return fields[1]

    def merge_result(self, project: str, line: str, folder: Path,
                     revision: str, identity: str, line_id: str) -> dict[str, str]:
        """Merge the reviewed immutable state once, with a durable receipt."""
        token = hashlib.sha256(identity.encode()).hexdigest()
        session = "studio-accept-" + token
        message = "Studio accepted result " + identity
        requested = {"project": project, "line": line, "lineId": line_id,
                     "revision": revision, "result": identity}
        with self.project_lock(project) as attachment:
            receipt = attachment.parent / ("accept-" + token + ".json")
            record: dict[str, str] | None = json.loads(receipt.read_text()) if receipt.exists() else None
            if record is not None and any(record.get(k) != v for k, v in requested.items()):
                raise ValueError("The native result acceptance identity changed")
            if record is not None and record.get("phase") == "merged":
                return record
            info = json.loads(self.command("workspace", "inspect", project, "--line", line))
            if info.get("line_id") != line_id or info.get("path") != str(folder):
                raise ValueError("The native merge destination changed")
            if record is not None:
                rows = self.command("--project", project, "--line", line,
                                    "op", "log", "--json", "-n", "500", cwd=folder)
                for raw in rows.splitlines():
                    row = json.loads(raw)
                    if row.get("op") != "merge" or row.get("actor", {}).get("session") != session:
                        continue
                    if row.get("data", {}).get("conflicts", 0):
                        raise RuntimeError("The native merge has conflicts; resolve them before acceptance")
                    for state in row.get("states", []):
                        if state.get("message") == message and not state.get("conflicts"):
                            value = self.command("--project", project, "rev-parse", state["id"], cwd=folder)
                            record.update(phase="merged", state=value)
                            self.write(receipt, record)
                            return record
                raise RuntimeError("The native merge outcome is unknown; the merge will not repeat")
            # A full state ID prevents a moving source line from changing reviewed content.
            resolved = self.command("--project", project, "rev-parse", revision, cwd=folder)
            if resolved != revision:
                raise ValueError("Accept the exact Layr state ID")
            record = {**requested, "phase": "merging"}
            self.write(receipt, record)
            self.command("--project", project, "--line", line, "merge", revision,
                         "--expect", revision, "--no-ff", "-m", message,
                         cwd=folder, session=session)
            value = self.command("--project", project, "--line", line, "rev-parse", "HEAD", cwd=folder)
            record.update(phase="merged", state=value)
            self.write(receipt, record)
            return record
