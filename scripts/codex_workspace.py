"""Workspace files, checkpoints, conversation forks, and capability discovery."""

import base64
import codecs
import hashlib
import json
import mimetypes
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import uuid

from codex_native_errors import NativeRpcError
from codex_safety_buffering import active as safety_retry_active
from codex_work import text_field


def active_task_records(db, statuses=("running",), *, agent=None):
    """Use the status index before loading task payloads under the runtime lock."""
    if not statuses:
        return []
    values = tuple(statuses)
    scope = " AND json_extract(record,'$.agent')=?" if agent is not None else ""
    rows = db.execute(
        "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status') IN ("
        + ",".join("?" for _ in values) + ")" + scope,
        (*values, agent) if agent is not None else values,
    )
    return [json.loads(row[0]) for row in rows]


class WorkspaceMixin:
    WORKSPACE_OPERATION_ACTIVE = {
        "provider_pending",
        "provider_ready",
        "local_mutation",
        "recovery_required",
        "capture_pending",
        "capture_running",
    }

    def setup_workspace(self, db):
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_workspace_migrations (id TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
        """)
        self.migrate_project_accounts(db)
        # Capture uses a separate Git index and never restores workspace files or
        # changes the native thread. A dead process cannot retain its reservation.
        for operation in self._workspace_operations(db):
            if operation.get("kind") in {"checkpoint", "capture"}:
                operation.update(phase="failed", updated=time.time(),
                                 error="Server restarted before checkpoint capture finished")
                self.put(db, "workspace_operations", operation)
        active = {
            operation["id"]: operation
            for operation in self._workspace_operations(db)
        }
        for a in self.records(db, "agents"):
            operations = [operation for operation in active.values() if operation.get("agent") == a["id"]]
            if operations:
                operation = operations[0]
                operation["restartHold"] = True
                if operation.get("kind") == "restore" or operation.get("phase") == "provider_pending":
                    operation["phase"] = "recovery_required"
                    operation["error"] = (
                        "Server restarted during a workspace operation. "
                        "Inspect the workspace before retrying."
                    )
                    operation["updated"] = time.time()
                self.put(db, "workspace_operations", operation)
                a.update(
                    workspaceOperation=(
                        "restore_recovery"
                        if operation.get("kind") == "restore"
                        else "branch"
                    ),
                    autoWake=False,
                    status="interrupted",
                    error=operation.get("error")
                    or "Workspace operation interrupted. Inspect files before continuing.",
                )
                self.put(db, "agents", a)
            elif a.get("workspaceOperation") in {"checkpoint", "capture"}:
                a.update(workspaceOperation=None,
                         checkpointError="Server restarted before checkpoint capture finished")
                a.pop("workspaceReservationId", None)
                self.put(db, "agents", a)
            elif a.get("workspaceOperation"):
                a.update(
                    workspaceOperation=None,
                    autoWake=False,
                    status="interrupted",
                    error="Workspace operation interrupted. Inspect files before continuing.",
                )
                self.put(db, "agents", a)
        self.capability_cache = {}

    @staticmethod
    def _workspace_operation_id(kind, agent_id, data):
        if kind == "branch" and data.get("id"):
            return "branch:" + str(data["id"])
        value = data.get("checkpoint_id") or data.get("checkpoint")
        if kind == "restore":
            expected = str(data.get("expectedTree"))
            digest = hashlib.sha256(expected.encode()).hexdigest()[:32]
            return "restore:" + str(agent_id) + ":" + str(value) + ":" + digest
        return "branch:" + str(agent_id) + ":" + str(data.get("message_id"))

    @staticmethod
    def _workspace_operation_signature(body):
        return hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()

    @classmethod
    def _workspace_restore_signature(cls, agent_id, data):
        return cls._workspace_operation_signature(
            {
                "agent": agent_id,
                "checkpoint": data.get("checkpoint_id") or data.get("checkpoint"),
                "expectedTree": data.get("expectedTree"),
            }
        )

    @staticmethod
    def _workspace_provider_result(response):
        thread = response.get("thread") if isinstance(response, dict) else None
        thread_id = thread.get("id") if isinstance(thread, dict) else None
        if not isinstance(thread_id, str) or not thread_id:
            raise ValueError("The native workspace response has no thread id")
        result = {"thread": {"id": thread_id}}
        for field in ("sandbox", "approvalPolicy", "activePermissionProfile", "model"):
            if field in response:
                result[field] = response[field]
        return result

    @staticmethod
    def _workspace_source(a):
        return {
            "accountKey": a.get("accountKey", "default"),
            "threadId": a.get("threadId"),
            "cwd": a["cwd"],
        }

    def _assert_workspace_source(self, operation, agent):
        expected = operation.get("source")
        if expected and expected != self._workspace_source(agent):
            raise ValueError(
                "Workspace operation source changed. Inspect the operation before retrying"
            )

    def _workspace_operation(self, db, operation_id):
        row = db.execute(
            "SELECT record FROM runtime_workspace_operations WHERE id=?",
            (operation_id,),
        ).fetchone()
        return json.loads(row[0]) if row else None

    def _workspace_operations(self, db, agent_id=None):
        phases = tuple(sorted(self.WORKSPACE_OPERATION_ACTIVE))
        query = ("SELECT record FROM runtime_workspace_operations "
                 "WHERE json_extract(record,'$.phase') IN ("
                 + ",".join("?" for _ in phases) + ")")
        params = phases
        if agent_id is not None:
            query += " AND json_extract(record,'$.agent')=?"
            params += (agent_id,)
        return [json.loads(row[0]) for row in db.execute(query, params)]

    def _put_workspace_operation(self, db, operation):
        self.put(db, "workspace_operations", operation)

    def _update_workspace_operation(self, operation_id, **changes):
        with self.lock, self.db() as db:
            operation = self._workspace_operation(db, operation_id)
            if operation is None:
                raise ValueError("Workspace operation record is missing")
            operation.update(changes, updated=time.time())
            self._put_workspace_operation(db, operation)
            return operation

    def _workspace_provider_rejected(self, error):
        return isinstance(error, NativeRpcError)

    def _finish_workspace_operation(self, operation_id, agent_id, *, result=None, error=None):
        with self.lock, self.db() as db:
            operation = self._workspace_operation(db, operation_id)
            if operation is not None:
                operation.update(
                    phase="completed" if error is None else "failed",
                    result=result,
                    error=str(error) if error is not None else None,
                    updated=time.time(),
                )
                self._put_workspace_operation(db, operation)
            agent = self.agent(agent_id, db)
            if agent.get("workspaceOperation") in {
                "restore",
                "restore_recovery",
                "branch",
                "branch_recovery",
            }:
                agent["workspaceOperation"] = None
                # Native completion and user stop own the current source status.
                # Finishing a fork must not restore an older running state.
                self.put(db, "agents", agent)

    def _require_workspace_recovery(self, operation_id, agent_id, error):
        message = "Workspace operation outcome is unknown. Inspect the workspace before retrying."
        try:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
                if operation is not None:
                    operation.update(
                        phase="recovery_required",
                        error=str(error),
                        updated=time.time(),
                    )
                    self._put_workspace_operation(db, operation)
                agent = self.agent(agent_id, db)
                agent.update(
                    workspaceOperation=(
                        "restore_recovery"
                        if operation and operation.get("kind") == "restore"
                        else "branch_recovery"
                    ),
                    autoWake=False,
                    status="interrupted",
                    error=message,
                )
                self.put(db, "agents", agent)
        except Exception:
            # The original failure remains visible. The prepared operation and
            # agent marker already provide a durable hold if this write fails.
            pass

    def _workspace_operation_busy(self, db, cwd, exclude_operation=None):
        return any(
            Path(agent["cwd"]).resolve() == Path(cwd).resolve()
            for operation in self._workspace_operations(db)
            if operation["id"] != exclude_operation
            for agent in self.records(db, "agents")
            if operation.get("agent") == agent["id"]
        )

    @staticmethod
    def project_directory(value, require_existing=True):
        if isinstance(value, Path):
            value = str(value)
        text_field(value, "a project path", 4096)
        path = Path(value).expanduser().resolve()
        if require_existing and not path.is_dir():
            raise ValueError("Select an existing project directory")
        return str(path)

    def migrate_project_accounts(self, db):
        marker = "project-account-defaults-v1"
        if db.execute("SELECT 1 FROM runtime_workspace_migrations WHERE id=?", (marker,)).fetchone():
            return
        legacy = self.accounts.legacy_project_defaults()
        leads = {}
        for agent in self.records(db, "agents"):
            if not agent.get("isLead") or agent.get("deletedAt") or not agent.get("cwd"):
                continue
            key = agent.get("accountKey", "default")
            try:
                self.accounts.get(key)
            except ValueError:
                continue
            path = self.project_directory(agent["cwd"], require_existing=False)
            previous = leads.get(path)
            if previous is None or (agent.get("created", 0), agent["id"]) > (previous.get("created", 0), previous["id"]):
                leads[path] = agent
        projects = self.records(db, "projects")
        registered = {self.project_directory(p["path"], require_existing=False) for p in projects}
        for path in sorted((set(leads) | set(legacy)) - registered):
            projects.append({"id": path, "path": path, "name": Path(path).name or path,
                             "created": leads.get(path, {}).get("created", time.time())})
        for project in projects:
            if not project.get("accountKey"):
                path = self.project_directory(project["path"], require_existing=False)
                project["accountKey"] = leads[path].get("accountKey", "default") if path in leads else legacy.get(path, self.accounts.default())
                project["accountRevision"] = 1
            else:
                project.setdefault("accountRevision", 1)
            self.put(db, "projects", project)
        db.execute("INSERT INTO runtime_workspace_migrations(id) VALUES (?)", (marker,))

    def project_account(self, cwd, db=None):
        if db is None:
            with self.lock, self.db() as connection:
                return self.project_account(cwd, db=connection)
        directory = Path(self.project_directory(cwd, require_existing=False))
        matches = [p for p in self.records(db, "projects")
                   if p.get("accountKey") and directory.is_relative_to(Path(p["path"]).expanduser().resolve())]
        if matches:
            return max(matches, key=lambda p: len(Path(p["path"]).parts))["accountKey"]
        return self.accounts.default()

    def ensure_project(self, path, account_key, db):
        """Register a chat project in its transaction; preserve an existing choice."""
        path = self.project_directory(path, require_existing=False)
        existing = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
        if existing:
            return json.loads(existing[0])
        self.accounts.get(account_key)
        project = {"id": path, "path": path, "name": Path(path).name or path,
                   "created": time.time(), "accountKey": account_key, "accountRevision": 1}
        self.put(db, "projects", project)
        return project

    def projects(self, data=None, db=None):
        if data is None:
            if db is None:
                with self.lock, self.db() as connection:
                    return self.projects(db=connection)
            return {"items": sorted(self.records(db, "projects"), key=lambda p: (p["created"], p["id"]))}
        action = data.get("action", "register")
        if action in ('rename', 'add_folder', 'rename_folder', 'remove_folder'):
            from codex_project_folders import organize_project
            return organize_project(self, data)
        if action == "set_accounts":
            from codex_project_accounts import set_project_accounts
            return set_project_accounts(self, data)
        if action not in ("register", "remove", "set_account"):
            raise ValueError("Unknown project action")
        path = self.project_directory(data.get("path"), require_existing=action == "register")
        name = text_field(data["name"], "a project name", 255) if "name" in data else Path(path).name or path
        if action == "set_account":
            revision = data.get("expected_revision")
            if type(revision) is not int or revision < 0:
                raise ValueError("Supply the current project account revision")
        with self.lock, self.db() as connection:
            connection.execute("BEGIN IMMEDIATE")
            if action == "remove":
                removed = connection.execute("DELETE FROM runtime_projects WHERE id=?", (path,)).rowcount
                return {"id": path, "removed": bool(removed)}
            existing = connection.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
            project = json.loads(existing[0]) if existing else None
            if action == "register" and project:
                return project
            if action == "set_account":
                current = project.get("accountRevision", 0) if project else 0
                desired = data["account_key"]
                if project and project.get("accountKey") == desired and revision in (current, current - 1):
                    return project
                if revision != current:
                    raise ValueError("Project account changed. Reload it before saving")
                if self.accounts.get(desired).get("disconnected"):
                    raise ValueError("Reconnect this account before selecting it")
            else:
                current = 0
                desired = data.get("account_key") or self.project_account(path, db=connection)
                self.accounts.get(desired)
            if project is None:
                project = {"id": path, "path": path, "name": name, "created": time.time()}
            project.update(accountKey=desired, accountRevision=current + 1)
            if project.get("accountKeys") and desired not in project["accountKeys"]:
                project["accountKeys"] = sorted([*project["accountKeys"], desired])
            self.put(connection, "projects", project)
            return project

    def workspace_path(self, agent_id, path):
        a = self.agent(agent_id)
        root = Path(a["cwd"]).resolve()
        supplied = Path(text_field(path, "a path", 4096)).expanduser()
        resolved = (supplied if supplied.is_absolute() else root / supplied).resolve()
        return resolved

    def upload_asset(self, data):
        agent = self.checked_actor_in_own_db(data.get("agent"))
        name = Path(text_field(data.get("name"), "a filename", 255)).name
        if name in {".", ".."}:
            raise ValueError("Invalid filename")
        try:
            content = base64.b64decode(
                data.get("base64", data.get("data", "")), validate=True
            )
        except (ValueError, TypeError):
            raise ValueError("Invalid file data")
        if not content or len(content) > 20 * 1024 * 1024:
            raise ValueError("Files must contain 1 byte to 20 MiB")
        key = str(uuid.UUID(data["id"])) if data.get("id") else str(uuid.uuid4())
        digest = hashlib.sha256(content).hexdigest()
        with self.lock, self.db() as db:
            old = db.execute(
                "SELECT record FROM runtime_assets WHERE id=?", (key,)
            ).fetchone()
            if old:
                asset = json.loads(old[0])
                if (asset["agent"], asset["hash"], asset["name"]) != (
                    agent["id"],
                    digest,
                    name,
                ):
                    raise ValueError("This upload id has different content")
                return self.asset_view(asset)
            directory = self.root / "uploads" / key
            directory.mkdir(parents=True, exist_ok=True)
            path = directory / name
            path.write_bytes(content)
            path.chmod(0o600)
            mime = mimetypes.guess_type(name)[0] or "application/octet-stream"
            image = (
                content.startswith(b"\x89PNG\r\n\x1a\n")
                or content.startswith(b"\xff\xd8\xff")
                or content.startswith((b"GIF87a", b"GIF89a"))
                or (content[:4] == b"RIFF" and content[8:12] == b"WEBP")
            )
            if mime.startswith("image/") and not image:
                mime = "application/octet-stream"
            asset = {
                "id": key,
                "agent": agent["id"],
                "name": name,
                "path": str(path),
                "mime": mime,
                "image": image,
                "size": len(content),
                "hash": digest,
                "created": time.time(),
            }
            self.put(db, "assets", asset)
            return self.asset_view(asset)

    def checked_actor_in_own_db(self, key, actor=None):
        with self.lock, self.db() as db:
            return self.checked_actor(db, key, actor)

    @staticmethod
    def asset_view(asset):
        return {k: v for k, v in asset.items() if k != "path"}

    def asset_record(self, key):
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_assets WHERE id=?", (key,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown attachment")
            asset = json.loads(row[0])
            self.checked_actor(db, asset["agent"])
            return asset

    def message_inputs(self, agent_id, text, asset_ids):
        inputs = [{"type": "text", "text": text}]
        if not isinstance(asset_ids, list) or len(asset_ids) > 8:
            raise ValueError("Attach up to eight files")
        for key in asset_ids:
            asset = self.asset_record(key)
            # Explicit user attachments can be reused by a fork in the same project.
            a = self.agent(agent_id)
            owner = self.agent(asset["agent"])
            if a["rootId"] != owner["rootId"] and a.get("forkedFrom") != owner["id"]:
                raise ValueError("Attachment belongs to another conversation")
            if asset["image"]:
                inputs.append({"type": "localImage", "path": asset["path"]})
            else:
                inputs.append(
                    {
                        "type": "text",
                        "text": f"Attached file: {asset['name']}\nRead the file at {asset['path']} ({asset['size']} bytes).",
                    }
                )
        return inputs

    def file_info(self, agent_id=None, path=None, asset_id=None):
        if asset_id:
            asset = self.asset_record(asset_id)
            file = Path(asset["path"]).resolve()
            mime = asset["mime"]
        else:
            self.checked_actor_in_own_db(agent_id)
            file = self.workspace_path(agent_id, path)
            mime = mimetypes.guess_type(file.name)[0] or "application/octet-stream"
        if not file.is_file():
            raise ValueError("This file does not exist")
        return {"path": str(file), "name": file.name, "mime": mime, "size": file.stat().st_size}

    def file_content(
        self, agent_id=None, path=None, asset_id=None, limit=20 * 1024 * 1024
    ):
        if asset_id:
            asset = self.asset_record(asset_id)
            file = Path(asset["path"])
            mime = asset["mime"]
        else:
            self.checked_actor_in_own_db(agent_id)
            file = self.workspace_path(agent_id, path)
            mime = mimetypes.guess_type(file.name)[0] or "application/octet-stream"
        if not file.is_file():
            raise ValueError("This file does not exist")
        if file.stat().st_size > limit:
            raise ValueError("This file exceeds the 20 MiB preview limit")
        return file.read_bytes(), mime, file.name

    def git(self, a, args, env=None, input=None):
        result = subprocess.run(
            ["git", "-C", a["cwd"], *args],
            input=input,
            capture_output=True,
            env=env,
            timeout=30,
        )
        if result.returncode:
            raise ValueError(
                result.stderr.decode(errors="replace").strip()[:1200]
                or "Git operation failed"
            )
        return result.stdout

    @staticmethod
    def reported_change_files(patch):
        def header_path(value):
            if value.startswith('"'):
                if not re.fullmatch(r'"(?:[^"\\]|\\(?:[abfnrtv\\"]|[0-3][0-7]{2}))*"', value):
                    return None
                try:
                    return codecs.escape_decode(value[1:-1].encode("utf-8"))[0].decode("utf-8")
                except (ValueError, UnicodeError):
                    return None
            return value.split("\t", 1)[0] or None

        files = {}
        previous = None
        old_lines = new_lines = 0
        for line in patch.splitlines():
            hunk = re.match(r"^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@", line)
            if hunk:
                old_lines = int(hunk[1] or 1)
                new_lines = int(hunk[2] or 1)
                previous = None
                continue
            if old_lines or new_lines:
                if line.startswith(("-", " ")):
                    old_lines = max(0, old_lines - 1)
                if line.startswith(("+", " ")):
                    new_lines = max(0, new_lines - 1)
                continue
            if line.startswith("--- "):
                previous = header_path(line[4:])
            elif line.startswith("+++ ") and previous is not None:
                current = header_path(line[4:])
                if current is not None:
                    path = previous if current == "/dev/null" else current
                    prefix = "a/" if current == "/dev/null" else "b/"
                    if path.startswith(prefix):
                        path = path[2:]
                    if path and path != "/dev/null":
                        status = "D" if current == "/dev/null" else "A" if previous == "/dev/null" else "M"
                        files[path] = {"path": path, "status": status}
                previous = None
            else:
                previous = None
        return list(files.values())

    def reported_changes(self, agent_id):
        with self.lock, self.db() as db:
            self.checked_actor(db, agent_id)
            row = db.execute(
                "SELECT record FROM runtime_items WHERE agent=? AND (id=? OR id LIKE ?) ORDER BY created DESC LIMIT 1",
                (agent_id, agent_id + ":turn/diff/updated", agent_id + ":turn/diff/updated:%"),
            ).fetchone()
            record = json.loads(row[0]) if row else None
            result = {
                "scope": "chat", "git": True, "files": [], "diff": "", "patch": "",
                "truncated": False, "turnId": None, "reportedAt": None,
            }
            if record is None or record.get("afterRestore"):
                return result
            full = db.execute("SELECT body FROM runtime_search WHERE id=?", (record["id"],)).fetchone()
            if full is None and record.get("truncated"):
                raise ValueError("The complete reported changes are unavailable")
            try:
                payload = json.loads(full[0] if full else record["text"])
                patch = payload["diff"]
                if not isinstance(patch, str):
                    raise ValueError("Invalid diff")
            except (ValueError, TypeError, KeyError):
                raise ValueError("The recorded changes are invalid") from None
            result.update(
                files=self.reported_change_files(patch), diff=patch[:300000], patch=patch[:300000],
                truncated=len(patch) > 300000, turnId=payload.get("turnId"), reportedAt=record.get("at"),
            )
            return result

    def changes(self, agent_id, scope=None):
        if scope == "chat":
            return self.reported_changes(agent_id)
        if scope is not None:
            raise ValueError("Unknown changes scope")
        a = self.checked_actor_in_own_db(agent_id)
        try:
            self.git(a, ["rev-parse", "--show-toplevel"])
            raw = self.git(a, ["status", "--porcelain=v1", "-z"])
            entries = raw.decode(errors="replace").split("\0")
            files = []
            i = 0
            while i < len(entries):
                entry = entries[i]
                i += 1
                if not entry:
                    continue
                state, path = entry[:2], entry[3:]
                if "R" in state or "C" in state:
                    i += 1
                files.append({"path": path, "status": state.strip()})
            patch = self.git(
                a, ["diff", "HEAD", "--no-ext-diff", "--no-color", "--unified=3"]
            ).decode(errors="replace")
            revision = self.git(a, ["rev-parse", "HEAD"]).decode().strip()
            return {
                "files": files,
                "diff": patch[:300000],
                "patch": patch[:300000],
                "truncated": len(patch) > 300000,
                "revision": revision,
                "git": True,
            }
        except ValueError as error:
            return {"files": [], "patch": "", "git": False, "error": str(error)}

    def snapshot_tree(self, a):
        # An independent index preserves the user's staging area.
        with tempfile.TemporaryDirectory(
            prefix="checkpoint-", dir=self.root
        ) as directory:
            env = {**os.environ, "GIT_INDEX_FILE": str(Path(directory) / "index")}
            self.git(a, ["read-tree", "HEAD"], env)
            self.git(a, ["add", "-A", "--", "."], env)
            return self.git(a, ["write-tree"], env).decode().strip()

    def _reserve_checkpoint(self, db, agent, kind, turn_id=None):
        operation = {
            "id": kind + ":" + str(uuid.uuid4()),
            "kind": kind, "agent": agent["id"], "cwd": agent["cwd"],
            "epoch": agent["epoch"], "turnId": turn_id,
            "phase": "capture_pending", "created": time.time(),
        }
        self._put_workspace_operation(db, operation)
        agent.update(workspaceOperation=kind, workspaceReservationId=operation["id"])
        self.put(db, "agents", agent)
        return operation["id"]

    def queue_checkpoint_after_turn(self, db, agent, turn_id):
        # A native turn can finish while a conversation fork holds this directory.
        # Its completion must not replace the fork's reservation.
        try:
            self._assert_workspace_idle(db, agent, current_state=True)
        except ValueError as error:
            agent["checkpointError"] = "Checkpoint skipped: " + str(error)
            return
        operation_id = self._reserve_checkpoint(db, agent, "checkpoint", turn_id)
        try:
            self.pool.submit(self.checkpoint_after_turn, agent["id"], turn_id, operation_id)
        except Exception as error:
            self._settle_checkpoint(db, agent, operation_id, error)

    def _settle_checkpoint(self, db, agent, operation_id, error=None):
        operation = self._workspace_operation(db, operation_id)
        if not operation or operation.get("kind") not in {"checkpoint", "capture"}:
            return
        if operation.get("phase") not in {"capture_pending", "capture_running"}:
            return
        operation.update(phase="failed" if error else "completed", updated=time.time(),
                         error=str(error) if error else None)
        self._put_workspace_operation(db, operation)
        if (agent.get("workspaceReservationId") == operation_id
                and agent.get("workspaceOperation") == operation["kind"]):
            agent.update(workspaceOperation=None, checkpointError=str(error) if error else None)
            agent.pop("workspaceReservationId", None)
            self.put(db, "agents", agent)

    def _capture_reserved_checkpoint(self, key, label, turn_id, operation_id):
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            operation = self._workspace_operation(db, operation_id)
            if not operation or operation.get("phase") != "capture_pending":
                return None
            if (agent.get("workspaceReservationId") != operation_id
                    or agent.get("workspaceOperation") != operation["kind"]
                    or agent["cwd"] != operation["cwd"]):
                self._settle_checkpoint(db, agent, operation_id,
                                        "Checkpoint reservation changed before capture")
                return None
            try:
                self._assert_workspace_idle(db, agent, operation_id)
            except ValueError as error:
                self._settle_checkpoint(db, agent, operation_id, "Checkpoint skipped: " + str(error))
                return None
            operation.update(phase="capture_running", updated=time.time())
            self._put_workspace_operation(db, operation)
        error = None
        try:
            return self.capture_checkpoint(key, label, turn_id)
        except Exception as cause:
            error = cause
            raise
        finally:
            with self.lock, self.db() as db:
                self._settle_checkpoint(db, self.agent(key, db), operation_id, error)
            self.changed.set()

    def checkpoint_capture(
        self, agent_id, label="Checkpoint", turn_id=None, internal=False
    ):
        if internal:
            return self.capture_checkpoint(agent_id, label, turn_id)
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            self.assert_workspace_idle(a)
            operation_id = self._reserve_checkpoint(db, a, "capture", turn_id)
        return self._capture_reserved_checkpoint(agent_id, label, turn_id, operation_id)

    def capture_checkpoint(self, agent_id, label="Checkpoint", turn_id=None):
        a = self.checked_actor_in_own_db(agent_id)
        tree = self.snapshot_tree(a)
        key = str(uuid.uuid4())
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "Codex Agents",
            "GIT_AUTHOR_EMAIL": "local@codex-agents.invalid",
            "GIT_COMMITTER_NAME": "Codex Agents",
            "GIT_COMMITTER_EMAIL": "local@codex-agents.invalid",
        }
        commit = (
            self.git(
                a,
                ["commit-tree", tree, "-p", "HEAD"],
                env,
                b"Codex Agents workspace checkpoint\n",
            )
            .decode()
            .strip()
        )
        ref = f"refs/codex-agents/checkpoints/{agent_id}/{key}"
        self.git(a, ["update-ref", ref, commit])
        with self.lock, self.db() as db:
            visible = [
                row[0]
                for row in db.execute(
                    "SELECT id FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created",
                    (agent_id,),
                )
            ]
            record = {
                "items": visible,
                "id": key,
                "agent": agent_id,
                "rootId": a["rootId"],
                "label": label,
                "tree": tree,
                "commit": commit,
                "ref": ref,
                "threadId": a.get("threadId"),
                "turnId": turn_id or a.get("lastCompletedTurn"),
                "created": time.time(),
                "cwd": a["cwd"],
            }
            self.put(db, "checkpoints", record)
            return record

    def checkpoint_after_turn(self, key, turn_id, operation_id):
        try:
            self._capture_reserved_checkpoint(key, "After turn", turn_id, operation_id)
        except Exception:
            # The capture helper persists the exact failure and releases only its
            # own reservation. Automatic capture must not fail the completed turn.
            pass

    def checkpoint_preview(self, key, checkpoint_id):
        a = self.checked_actor_in_own_db(key)
        recovery_expected = None
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_checkpoints WHERE id=?", (checkpoint_id,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown checkpoint")
            checkpoint = json.loads(row[0])
            if checkpoint["agent"] != key:
                raise ValueError("Checkpoint belongs to another agent")
            for operation in self._workspace_operations(db, key):
                if (
                    operation.get("kind") == "restore"
                    and operation.get("checkpoint") == checkpoint_id
                ):
                    recovery_expected = operation.get("expectedTree")
                    break
        tree = self.snapshot_tree(a)
        expected_tree = recovery_expected or tree
        patch = self.git(
            a, ["diff", "--no-ext-diff", "--no-color", expected_tree, checkpoint["tree"]]
        ).decode(errors="replace")
        can_restore = bool(a.get("worktreeReady") and not a.get("inFlight"))
        if recovery_expected is not None:
            can_restore = can_restore and tree in {recovery_expected, checkpoint["tree"]}
        return {
            "checkpoint": checkpoint,
            "expectedTree": expected_tree,
            "diff": patch[:300000],
            "patch": patch[:300000],
            "truncated": len(patch) > 300000,
            "canRestore": can_restore,
        }

    def restore_checkpoint(self, key, data):
        with self.lock:
            guard = self.prepare_locks.setdefault(
                "restore:" + key, __import__("threading").Lock()
            )
        with guard:
            return self._restore_checkpoint_locked(key, data)

    def _restore_checkpoint_locked(self, key, data):
        operation_id = self._workspace_operation_id("restore", key, data)
        resume_operation = None
        files_already_restored = False
        with self.lock:
            a = self.checked_actor_in_own_db(key)
            if not a.get("worktreeReady"):
                raise ValueError(
                    "Restore is available only in an isolated worker worktree"
                )
            checkpoint_id = data.get("checkpoint_id") or data.get("checkpoint")
            with self.db() as db:
                active = self._workspace_operations(db, key)
                existing = self._workspace_operation(db, operation_id)
                if existing and existing.get("phase") == "completed":
                    if existing.get("signature") != self._workspace_restore_signature(key, data):
                        raise ValueError("This restore request has different content")
                    row = db.execute(
                        "SELECT record FROM runtime_checkpoints WHERE id=?",
                        (existing.get("checkpoint"),),
                    ).fetchone()
                    if not row or self.snapshot_tree(a) != json.loads(row[0])["tree"]:
                        raise ValueError("Files changed after the completed restore. Preview again")
                    return existing.get("result") or {
                        "status": "restored",
                        "checkpoint": checkpoint_id,
                    }
                if active:
                    restore_ops = [
                        operation
                        for operation in active
                        if operation.get("kind") == "restore"
                        and operation.get("id") == operation_id
                    ]
                    if not restore_ops:
                        raise ValueError(
                            "Workspace recovery is required before retrying this operation"
                        )
                    resume_operation = restore_ops[0]
                    self._assert_workspace_source(resume_operation, a)
                    if resume_operation.get("signature") != self._workspace_restore_signature(key, data):
                        raise ValueError("This restore request has different content")
                    row = db.execute(
                        "SELECT record FROM runtime_checkpoints WHERE id=?",
                        (resume_operation.get("checkpoint"),),
                    ).fetchone()
                    if not row:
                        raise ValueError("Unknown checkpoint")
                    checkpoint = json.loads(row[0])
                    expected_tree = resume_operation.get("expectedTree")
                    if data.get("checkpoint_id") not in {None, checkpoint["id"]}:
                        raise ValueError("This restore request has different content")
                    if data.get("expectedTree") != expected_tree:
                        raise ValueError("This restore request has different content")
                    current_tree = self.snapshot_tree(a)
                    if current_tree == checkpoint["tree"]:
                        files_already_restored = True
                    elif current_tree != expected_tree:
                        raise ValueError(
                            "Workspace recovery is required. Restore files to the saved checkpoint or preview tree before retrying"
                        )
                    preview = {"checkpoint": checkpoint, "expectedTree": expected_tree}
                    operation = resume_operation
                else:
                    self.assert_workspace_idle(a)
                    preview = self.checkpoint_preview(key, checkpoint_id)
                    if data.get("expectedTree") != preview["expectedTree"]:
                        raise ValueError("Files changed after the preview. Preview again")
                    checkpoint = preview["checkpoint"]
                    operation = {
                        "id": operation_id,
                        "kind": "restore",
                        "agent": key,
                        "checkpoint": checkpoint["id"],
                        "expectedTree": preview["expectedTree"],
                        "signature": self._workspace_restore_signature(key, data),
                        "source": self._workspace_source(a),
                        "phase": "provider_pending",
                        "created": time.time(),
                    }
                    self._put_workspace_operation(db, operation)
                    a["workspaceOperation"] = "restore"
                    self.put(db, "agents", a)
        try:
            # Create the matching conversation before changing files. A provider rejection leaves files intact.
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            if resume_operation:
                response = resume_operation.get("provider")
                if not response:
                    raise ValueError(
                        "Workspace recovery is required because the native result is unknown"
                    )
            else:
                try:
                    if checkpoint.get("turnId"):
                        response = self.connect(a.get("accountKey", "default")).call(
                            "thread/fork",
                            {
                                "threadId": checkpoint["threadId"],
                                "lastTurnId": checkpoint["turnId"],
                                "cwd": a["cwd"],
                                "config": self.thread_config(),
                                **{k: v for k, v in self.new_thread_params(a).items() if k in {"approvalPolicy", "sandbox"}},
                                "model": a["model"],
                            },
                        )
                    else:
                        response = self.connect(a.get("accountKey", "default")).call(
                            "thread/start", self.new_thread_params(a)
                        )
                except Exception as error:
                    if self._workspace_provider_rejected(error):
                        self._finish_workspace_operation(operation_id, key, error=error)
                    else:
                        self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            response = self._workspace_provider_result(response)
            try:
                if not resume_operation:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        provider=response,
                    )
                with self.lock, self.db() as db:
                    self._assert_workspace_source(operation, self.agent(key, db))
                current_tree = self.snapshot_tree(a)
                if current_tree != preview["expectedTree"] and not files_already_restored:
                    raise ValueError("Files changed while preparing restore. Preview again")
                # Mark the local phase before any Git mutation. A restart now must hold the workspace.
                if not files_already_restored:
                    self._update_workspace_operation(operation_id, phase="local_mutation")
                    # Only this isolated, idle worktree is changed; its old state remains a saved checkpoint.
                    if not resume_operation or not resume_operation.get("beforeCheckpoint"):
                        before = self.checkpoint_capture(key, "Before restore", internal=True)
                        self._update_workspace_operation(
                            operation_id,
                            phase="local_mutation",
                            beforeCheckpoint=before["id"],
                        )
                    # Record the preview tree in this isolated index so added files are removed on restore too.
                    self.git(a, ["read-tree", preview["expectedTree"]])
                    self.git(a, ["read-tree", "--reset", "-u", checkpoint["tree"]])
                if self.snapshot_tree(a) != checkpoint["tree"]:
                    raise ValueError(
                        "Workspace files changed before restore metadata was saved"
                    )
                with self.lock, self.db() as db:
                    current = self.agent(key, db)
                    self._assert_workspace_source(operation, current)
                    result = {"status": "restored", "checkpoint": checkpoint["id"]}
                    current.update(
                        threadId=response["thread"]["id"],
                        sandbox=response.get("sandbox"),
                        approvalPolicy=response.get("approvalPolicy"),
                        profile=response.get("activePermissionProfile"),
                        turnId=None,
                        inFlight=False,
                        status="paused",
                        autoWake=False,
                        workspaceOperation=None,
                        restoredCheckpoint=checkpoint["id"],
                        lastCompletedTurn=checkpoint.get("turnId"),
                    )
                    self.put(db, "agents", current)
                    operation = self._workspace_operation(db, operation_id)
                    operation.update(phase="completed", result=result, updated=time.time())
                    self._put_workspace_operation(db, operation)
                    db.execute(
                        "UPDATE runtime_events SET status='cancelled' WHERE agent=? AND status='pending'",
                        (key,),
                    )
                    for item_row in db.execute(
                        "SELECT id,record,created FROM runtime_items WHERE agent=?", (key,)
                    ).fetchall():
                        record = json.loads(item_row["record"])
                        visible = (
                            item_row["id"] in checkpoint["items"]
                            if "items" in checkpoint
                            else item_row["created"] <= checkpoint["created"]
                        )
                        if visible:
                            record.pop("afterRestore", None)
                            if not db.execute(
                                "SELECT 1 FROM runtime_search WHERE id=?", (record["id"],)
                            ).fetchone():
                                self.index_item(
                                    db,
                                    record["id"],
                                    key,
                                    record.get("title", "message"),
                                    record.get("text", ""),
                                )
                        else:
                            record["afterRestore"] = checkpoint["id"]
                        db.execute(
                            "UPDATE runtime_items SET record=? WHERE id=?",
                            (json.dumps(record), item_row["id"]),
                        )
                    self.item(
                        db,
                        key,
                        "restore:" + checkpoint["id"],
                        "system",
                        "Restored workspace and conversation to " + checkpoint["label"],
                        "Checkpoint",
                    )
                self.loaded.discard(current["id"])
                return result
            except Exception as error:
                self._require_workspace_recovery(operation_id, key, error)
                raise
        except Exception as error:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
            if not operation or operation.get("phase") not in {"failed", "completed"}:
                self._require_workspace_recovery(operation_id, key, error)
            raise

    def assert_workspace_idle(self, a):
        with self.db() as db:
            self._assert_workspace_idle(db, a)

    def _assert_workspace_idle(self, db, a, reservation_id=None, *, current_state=False):
        if self._workspace_operation_busy(db, a["cwd"], reservation_id):
            raise ValueError("Workspace recovery is required before using this workspace")
        # Notification handlers have not yet written this turn's terminal state.
        # Use that exact current agent rather than its older in-flight DB row.
        agents = [a if current_state and other["id"] == a["id"] else other
                  for other in self.records(db, "agents")]
        cwd = Path(a["cwd"]).resolve()
        peers = [other for other in agents if Path(other["cwd"]).resolve() == cwd]
        for other in peers:
            own_reservation = (reservation_id is not None and other["id"] == a["id"]
                               and other.get("workspaceReservationId") == reservation_id)
            if other.get("inFlight") or (other.get("workspaceOperation") and not own_reservation):
                raise ValueError("An agent is using this workspace")
        if any(Path(m["cwd"]).resolve() == cwd and m["status"] in {"running", "starting", "approval"}
               for m in self.records(db, "monitors")):
            raise ValueError("A monitor is using this workspace")
        peer_ids = {other["id"] for other in peers}
        if any(t["agent"] in peer_ids for t in active_task_records(db)):
            raise ValueError("A command or tool is still active")

    def branch_conversation(self, key, data):
        with self.lock:
            guard = self.prepare_locks.setdefault(
                "fork:" + key, __import__("threading").Lock()
            )
        with guard:
            return self.branch_locked(key, data)

    def branch_locked(self, key, data):
        if "before" in data and type(data["before"]) is not bool:
            raise ValueError("before must be a boolean")
        operation_id = self._workspace_operation_id("branch", key, data)
        body = {"agent": key, **data}
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            signature, prior = self.operation_receipt(db, data.get("id"), body)
            if prior is not None:
                operation = self._workspace_operation(db, operation_id)
                if operation and operation.get("phase") != "completed":
                    operation.update(phase="completed", result=prior, updated=time.time())
                    self._put_workspace_operation(db, operation)
                    if a.get("workspaceOperation") in {"branch", "branch_recovery"}:
                        a["workspaceOperation"] = None
                        self.put(db, "agents", a)
                return prior
            if self.accounts.get(a.get("accountKey", "default")).get("disconnected"):
                raise ValueError("Reconnect this account before creating a branch")
            operation = self._workspace_operation(db, operation_id)
            resume_local = False
            retry_failed = False
            if operation:
                self._assert_workspace_source(operation, a)
                if operation.get("signature") != signature:
                    raise ValueError("This request id has different content")
                if operation.get("phase") == "completed":
                    return operation.get("result")
                if operation.get("phase") == "provider_ready":
                    resume_local = True
                elif operation.get("phase") == "failed":
                    retry_failed = True
                    operation.update(
                        phase="provider_pending",
                        provider=None,
                        result=None,
                        error=None,
                        updated=time.time(),
                    )
                    self._put_workspace_operation(db, operation)
                    a["workspaceOperation"] = "branch"
                    self.put(db, "agents", a)
                elif operation.get("phase") in self.WORKSPACE_OPERATION_ACTIVE:
                    raise ValueError(
                        "Workspace recovery is required before retrying this operation"
                    )
            from codex_transcript_history import resolve_item
            row = resolve_item(db, key, data.get("message_id"))
            item = json.loads(row["record"])
            turn_id = item.get("turnId")
            if not turn_id or item.get("afterRestore"):
                raise ValueError("This message has no active native turn reference")
            if not resume_local and not retry_failed:
                self.assert_workspace_available(db, a)
            if turn_id == a.get("turnId"):
                raise ValueError("Wait for this turn to finish before branching")
            if operation is None:
                operation = {
                    "id": operation_id,
                    "kind": "branch",
                    "agent": key,
                    "message_id": item["id"],
                    "turn_id": turn_id,
                    "signature": signature,
                    "phase": "provider_pending",
                    "leadId": str(uuid.uuid4()),
                    "created": time.time(),
                    "previousWorkspaceOperation": a.get("workspaceOperation"),
                    "previousStatus": a.get("status"),
                    "previousAutoWake": a.get("autoWake"),
                    "source": self._workspace_source(a),
                }
                self._put_workspace_operation(db, operation)
                a["workspaceOperation"] = "branch"
                self.put(db, "agents", a)
            lead_id = operation["leadId"]
        # A branch starts a new team, without the source team's exception.
        try:
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            if resume_local:
                with self.db() as db:
                    operation = self._workspace_operation(db, operation_id)
                response = operation["provider"]
            else:
                provider_dispatched = False
                try:
                    branch_params = self.new_thread_params({**a, "isLead": True,
                        "model": a["model"] if a.get("isLead") else "gpt-5.6-sol", "needsTitle": False})
                    fork_turn = turn_id
                    if data.get("before"):
                        native = self.connect(a.get("accountKey", "default")).call(
                            "thread/read", {"threadId": a["threadId"], "includeTurns": True})
                        turns = native.get("thread", {}).get("turns", [])
                        index = next((i for i, turn in enumerate(turns) if turn.get("id") == turn_id), None)
                        if index is None:
                            raise ValueError("The selected turn is absent from native history")
                        fork_turn = turns[index - 1]["id"] if index else None
                    self._update_workspace_operation(operation_id, forkTurnId=fork_turn)
                    provider_dispatched = True
                    if fork_turn is None:
                        response = self.connect(a.get("accountKey", "default")).call(
                            "thread/start", branch_params)
                    else:
                        response = self.connect(a.get("accountKey", "default")).call(
                            "thread/fork",
                            {
                                "threadId": a["threadId"],
                                "lastTurnId": fork_turn,
                                "cwd": a["cwd"],
                                "config": self.thread_config(),
                                **{k: v for k, v in branch_params.items() if k in {"approvalPolicy", "sandbox"}},
                                "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                                "developerInstructions": branch_params[
                                    "developerInstructions"
                                ],
                            },
                        )
                except Exception as error:
                    if not provider_dispatched or self._workspace_provider_rejected(error):
                        self._finish_workspace_operation(operation_id, key, error=error)
                    else:
                        self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            try:
                response = self._workspace_provider_result(response)
                if response["thread"]["id"] == a.get("threadId"):
                    raise ValueError("The native branch did not create a new thread")
            except Exception as error:
                self._require_workspace_recovery(operation_id, key, error)
                raise
            if not resume_local:
                try:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        provider=response,
                    )
                except Exception as error:
                    self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            self.checked_actor_in_own_db(key)
            lead = self.create(
                {
                    "id": lead_id,
                    "name": a["name"][:90] + " (branch)",
                    "account_key": a.get("accountKey", "default"),
                    "cwd": a["cwd"],
                    "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                    "prompt": "",
                },
                defer=True,
                draft=True,
                _validate_only=True,
            )
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
                lead.update(
                    yoloMode=a.get("yoloMode"),
                    sandbox=response.get("sandbox", a.get("sandbox")),
                    approvalPolicy=response.get("approvalPolicy", a.get("approvalPolicy")),
                    profile=response.get("activePermissionProfile", a.get("profile")),
                    threadId=response["thread"]["id"],
                    forkedFrom=key,
                    sourceMessage=item["id"],
                    status="idle",
                    autoWake=True,
                    needsTitle=False,
                )
                self.put(db, "agents", lead)
                rows = db.execute(
                    "SELECT record,created FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created",
                    (key,),
                ).fetchall()
                operation = self._workspace_operation(db, operation_id)
                fork_turn = operation.get("forkTurnId", turn_id)
                cutoff = max((r["created"] for r in rows
                    if json.loads(r["record"]).get("turnId") == fork_turn), default=float("-inf"))
                assets = {}

                def copy_assets(records):
                    copied = []
                    for view in records:
                        old_id = view["id"]
                        if old_id not in assets:
                            row = db.execute(
                                "SELECT record FROM runtime_assets WHERE id=?", (old_id,)
                            ).fetchone()
                            if not row:
                                raise ValueError("A source attachment is unavailable")
                            asset = json.loads(row[0])
                            asset.update(id=str(uuid.uuid4()), agent=lead["id"])
                            self.put(db, "assets", asset)
                            assets[old_id] = self.asset_view(asset)
                        copied.append(assets[old_id])
                    return copied

                for r in rows:
                    if r["created"] > cutoff:
                        break
                    record = json.loads(r["record"])
                    if data.get("before") and record.get("turnId") == turn_id:
                        continue
                    inputs = record.get("inputs")
                    if inputs is not None:
                        inputs = [
                            {**event, "assets": copy_assets(event.get("assets", []))}
                            for event in inputs
                        ]
                    self.item(
                        db,
                        lead["id"],
                        record["id"].split(":", 1)[-1],
                        record["role"],
                        record["text"],
                        record.get("title"),
                        inputs=inputs,
                        turnId=record.get("turnId"),
                        streaming=False,
                        assets=copy_assets(record.get("assets", [])),
                    )
                if data.get("before"):
                    prompt = item
                    if item.get("inputs"):
                        prompt = next((entry for index, entry in enumerate(item["inputs"])
                            if data.get("message_id") == (key + ":" + entry['id'] if entry.get('id') else item['id'] + ':' + str(index))), item["inputs"][0])
                    event = db.execute("SELECT text FROM runtime_events WHERE id=? AND agent=?", (prompt.get("id", "").removeprefix(key + ":"), key)).fetchone()
                    full = db.execute("SELECT body FROM runtime_search WHERE rowid=(SELECT search_rowid FROM runtime_search_rows WHERE id=?)", (item["id"],)).fetchone() if not item.get("inputs") else None
                    preceding = []
                    prefix_assets = []
                    for entry in item.get("inputs", []):
                        if entry is prompt:
                            break
                        if entry.get("kind") != "user":
                            continue
                        prior_event = db.execute("SELECT text FROM runtime_events WHERE id=? AND agent=?", (entry.get("id"), key)).fetchone()
                        preceding.append(prior_event[0] if prior_event else entry.get("text", ""))
                        prefix_assets.extend(entry.get("assets", []))
                    lead = {**lead, "draft": {"text": event[0] if event else full[0] if full else prompt.get("text", ""),
                        "prefixText": "\n\n".join(preceding),
                        "assets": copy_assets([*prefix_assets, *prompt.get("assets", [])])}}
                result = self.save_receipt(db, data.get("id"), signature, lead)
            self.loaded.discard(lead["id"])
            self._finish_workspace_operation(operation_id, key, result=result)
            return result
        except Exception as error:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
            if operation and operation.get("phase") == "provider_ready":
                # Keep the known provider response. The exact retry may finish
                # the local transaction without creating another native branch.
                try:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        localError=str(error),
                    )
                except Exception:
                    self._require_workspace_recovery(operation_id, key, error)
            raise

    def profiles(self, data=None):
        with self.lock, self.db() as db:
            if data is None:
                return {"profiles": self.records(db, "profiles")}
            key = data.get("id") or str(uuid.uuid4())
            if data.get("action") == "delete":
                db.execute("DELETE FROM runtime_profiles WHERE id=?", (key,))
                return {"deleted": key}
            role = data.get("role", "reviewer")
            if role not in {"reviewer", "implementer"}:
                raise ValueError("Choose reviewer or implementer")
            profile = {
                "id": key,
                "name": text_field(data.get("name"), "a profile name", 100),
                "role": role,
                "model": text_field(data.get("model"), "a model", 100),
                "effort": data.get("effort") or None,
                "instructions": text_field(
                    data.get("instructions", ""), "instructions", 16000, empty=True
                ),
            }
            if profile["effort"] not in {
                None,
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
            }:
                raise ValueError("Unknown reasoning effort")
            self.put(db, "profiles", profile)
            return profile

    def capabilities(self, key):
        a = self.checked_actor_in_own_db(key)
        cache = self.capability_cache.get(key)
        if cache and time.time() - cache["at"] < 30:
            return cache
        result = {
            "agent": key,
            "at": time.time(),
            "managed": self.tool_definitions(),
            "observed": [],
            "skills": [],
            "servers": [],
            "errors": [],
            "model": a["model"],
            "effort": a.get("effort"),
            "role": a["role"],
            "nativeInventory": "Codex selects native tools per model and configuration. Observed calls appear below.",
        }
        with self.lock, self.db() as db:
            result["observed"] = sorted(
                {
                    t.get("name", t.get("type", ""))
                    for t in self.records(db, "tasks")
                    if t["agent"] == key
                }
            )
        for label, method, params in [
            ("skills", "skills/list", {"cwds": [a["cwd"]], "forceReload": True}),
            (
                "servers",
                "mcpServerStatus/list",
                {"threadId": a.get("threadId"), "limit": 100},
            ),
        ]:
            try:
                response = self.connect(a.get("accountKey", "default")).call(method, params, timeout=15)
                result[label] = response.get("data", response.get("servers", []))
                result[label + "Cursor"] = response.get("nextCursor")
            except Exception as error:
                result["errors"].append(label + ": " + str(error))
        result.update(observedNative=result["observed"], mcp=result["servers"])
        self.capability_cache[key] = result
        return result

    def workspace_snapshot(self, key=None):
        with self.lock, self.db() as db:
            root = self.checked_actor(db, key)["rootId"] if key else None
            agents = [
                a for a in self.records(db, "agents")
                if not a.get("deletedAt") and (root is None or a["rootId"] == root)
            ]
            ids = {a["id"] for a in agents}
            monitors = self.recent_monitors(db, root)
            inbox = []
            for r in self.records(db, "requests"):
                if r["status"] == "pending" and not r.get("deferred") and r["agent"] in ids:
                    inbox.append(
                        {
                            "id": r["id"],
                            "kind": "request",
                            "agent": r["agent"],
                            "title": "Answer required",
                            "text": r["method"],
                            "request": r,
                        }
                    )
            for c in self.complaint_summaries(db):
                if c.get("needsResponse") and (root is None or c["leadId"] in ids):
                    inbox.append(
                        {
                            "id": c["id"],
                            "kind": "complaint",
                            "agent": c["leadId"],
                            "title": c["title"],
                            "text": "The lead must read and respond",
                            "complaint": c,
                        }
                    )
            for task in self.user_tasks(db=db)["items"]:
                if task["status"] == "open" and task["agent"] in ids:
                    inbox.append(
                        {
                            "id": task["id"],
                            "kind": "user_task",
                            "agent": task["agent"],
                            "title": task["title"],
                            "text": task.get("reason") or task["criteria"],
                            "task": task,
                        }
                    )
            works = self.records(db, "work")
            for w in works:
                if w["rootId"] in ids and w["status"] == "review":
                    inbox.append(
                        {
                            "id": w["id"],
                            "kind": "work",
                            "agent": w["rootId"],
                            "title": w["title"],
                            "text": "Result awaits acceptance",
                            "work": w,
                        }
                    )
            for a in agents:
                if a["status"] in {"failed", "interrupted"}:
                    inbox.append(
                        {
                            "id": a["id"],
                            "kind": "agent",
                            "agent": a["id"],
                            "title": a["name"],
                            "text": str(a.get("error") or a["status"]),
                        }
                    )
            for m in monitors:
                if (
                    m["agent"] in ids
                    and m["status"] in {"failed", "lost"}
                    and not m.get("ruleId")
                ):
                    inbox.append(
                        {
                            "id": m["id"],
                            "kind": "monitor",
                            "agent": m["agent"],
                            "title": m["command"],
                            "text": m.get("error") or f"Exit {m.get('exitCode')}",
                            "monitor": m,
                        }
                    )
            for r in self.records(db, "rules"):
                if r["agent"] in ids and r.get("error"):
                    inbox.append(
                        {
                            "id": r["id"],
                            "kind": "rule",
                            "agent": r["agent"],
                            "title": r["name"],
                            "text": r["error"],
                        }
                    )
            return {
                "work": [
                    self.work_view(w, works)
                    for w in works
                    if w["rootId"] in ids and (not root or w["rootId"] == root)
                ],
                "annotations": [
                    v
                    for v in self.records(db, "annotations")
                    if v["agent"] in ids and (not root or v["rootId"] == root)
                ],
                "checkpoints": [
                    v
                    for v in self.records(db, "checkpoints")
                    if v["agent"] in ids and (not key or v["agent"] == key)
                ],
                "plans": [
                    v
                    for v in self.records(db, "plans")
                    if v["id"] in ids and (not root or v["rootId"] == root)
                ],
                "rules": [
                    v
                    for v in self.records(db, "rules")
                    if v["agent"] in ids and (not root or v["rootId"] == root)
                ],
                "inbox": inbox,
                "tasks": self.recent_tasks(db, root),
                "tasksHistoryLimit": 100,
                "monitors": [m for m in monitors if m["agent"] in ids],
            }

    def monitor_log(self, key):
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_monitors WHERE id=?", (key,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown monitor")
            m = json.loads(row[0])
            self.checked_actor(db, m["agent"])
            path = Path(m["log"])
            if not path.resolve().is_relative_to(
                (self.root / "monitor-logs").resolve()
            ):
                raise ValueError("Invalid log path")
            content = path.read_bytes() if path.exists() else m.get("tail", "").encode()
            return {
                "name": key + ".log",
                "mime": "text/plain",
                "base64": base64.b64encode(content).decode(),
                "truncated": m.get("bytes", 0) > len(content),
            }

    def native_command_action(self, data):
        task = self.task_detail(data.get("id"))
        if (
            task.get("kind") != "command"
            or task.get("status") != "running"
            or not task.get("processId")
        ):
            raise ValueError("This native command has no active session")
        if data.get("action") not in {"input", "cancel"}:
            raise ValueError("Choose input or cancel")
        a = self.agent(task["agent"])
        if data["action"] == "cancel":
            server = self.connect(a.get("accountKey", "default"))
            params = {"threadId": a.get("threadId"), "limit": 100}
            # A process number alone can belong to another thread after recovery.
            while True:
                page = server.call("thread/backgroundTerminals/list", params)
                if any(t["itemId"] == task["itemId"] and t["processId"] == str(task["processId"])
                       for t in page["data"]):
                    break
                if not page.get("nextCursor"):
                    raise ValueError("This command no longer has an active native session")
                params["cursor"] = page["nextCursor"]
            result = server.call("thread/backgroundTerminals/terminate", {
                "threadId": params["threadId"], "processId": str(task["processId"])})
            if not result["terminated"]:
                raise ValueError("Codex did not confirm that the command stopped")
            return result
        text = data.get("text")
        text_field(text, "command input", 16000, empty=True)
        instruction = (
            f"Use your native write_stdin tool for session {task['processId']}. "
            f"Send exactly this JSON string as chars: {json.dumps(text)}."
        )
        return self.send(
            a["id"],
            instruction,
            delivery="steer" if a.get("turnId") and a.get("inFlight") else "queue",
        )

    def workspace_blockers(self, db, a):
        cwd = Path(a["cwd"]).resolve()
        blockers = []
        for other in self.records(db, "agents"):
            if not other.get("workspaceOperation") or Path(other["cwd"]).resolve() != cwd:
                continue
            blocker = {"agentId": other["id"], "operation": other["workspaceOperation"]}
            operation_id = other.get("workspaceReservationId")
            operation = self._workspace_operation(db, operation_id) if operation_id else None
            if not operation and other["workspaceOperation"] not in {"checkpoint", "capture"}:
                operation = next(iter(self._workspace_operations(db, other["id"])), None)
            if operation:
                blocker.update(operationId=operation["id"], phase=operation["phase"],
                               created=operation.get("created"), turnId=operation.get("turnId"))
            blockers.append(blocker)
        return blockers

    def assert_workspace_available(self, db, a):
        if safety_retry_active(a):
            raise ValueError('Wait for the model change before another workspace operation')
        if a.get("accountTransferId") and not a.get("inFlight"):
            raise ValueError("Wait for this agent's account transfer to finish")
        blockers = self.workspace_blockers(db, a)
        if blockers:
            from codex_workspace_delivery import WorkspaceBusyError
            raise WorkspaceBusyError(blockers)

    def recent_tasks(self, db, root=None):
        scope = "" if root is None else " AND json_extract(a.record,'$.rootId')=?"
        params = () if root is None else (root, root)
        rows = db.execute(
            f"""SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
                ON json_extract(t.record,'$.agent')=a.id WHERE json_extract(a.record,'$.deletedAt') IS NULL
                {scope} AND json_extract(t.record,'$.status')='running'
                UNION ALL SELECT record FROM (SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
                ON json_extract(t.record,'$.agent')=a.id WHERE json_extract(a.record,'$.deletedAt') IS NULL
                {scope} AND json_extract(t.record,'$.status')!='running'
                ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)""",
            params,
        ).fetchall()
        return [
            {k: v for k, v in json.loads(row[0]).items() if k not in {"tail", "arguments", "error"}}
            for row in rows
        ]

    def recent_monitors(self, db, root=None):
        scope = "" if root is None else """ AND json_extract(record,'$.agent') IN (
            SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=?
            AND json_extract(record,'$.deletedAt') IS NULL)"""
        params = () if root is None else (root, root)
        rows = db.execute(
            f"""SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval') {scope}
          UNION ALL SELECT record FROM (SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') NOT IN ('running','starting','approval') {scope}
          ORDER BY json_extract(record,'$.created') DESC LIMIT 100)""",
            params,
        ).fetchall()
        return [json.loads(row[0]) for row in rows]
