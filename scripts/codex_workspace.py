"""Workspace files, checkpoints, conversation forks, and capability discovery."""

import base64
import hashlib
import json
import mimetypes
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

from codex_work import text_field


class WorkspaceMixin:
    def setup_workspace(self, db):
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        """)
        for a in self.records(db, "agents"):
            if a.get("workspaceOperation"):
                a.update(
                    workspaceOperation=None,
                    autoWake=False,
                    status="interrupted",
                    error="Workspace operation interrupted. Inspect files before continuing.",
                )
                self.put(db, "agents", a)
        self.capability_cache = {}

    @staticmethod
    def project_directory(value, require_existing=True):
        text_field(value, "a project path", 4096)
        path = Path(value).expanduser().resolve()
        if require_existing and not path.is_dir():
            raise ValueError("Select an existing project directory")
        return str(path)

    def projects(self, data=None, db=None):
        if data is None:
            if db is None:
                with self.lock, self.db() as connection:
                    return self.projects(db=connection)
            return {"items": sorted(self.records(db, "projects"), key=lambda p: (p["created"], p["id"]))}
        action = data.get("action", "register")
        if action not in ("register", "remove"):
            raise ValueError("Unknown project action")
        path = self.project_directory(data.get("path"), require_existing=action == "register")
        name = text_field(data["name"], "a project name", 255) if "name" in data else Path(path).name or path
        with self.lock, self.db() as connection:
            if action == "remove":
                removed = connection.execute("DELETE FROM runtime_projects WHERE id=?", (path,)).rowcount
                return {"id": path, "removed": bool(removed)}
            existing = connection.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
            if existing:
                return json.loads(existing[0])
            project = {"id": path, "path": path, "name": name, "created": time.time()}
            self.put(connection, "projects", project)
            return project

    def workspace_path(self, agent_id, path):
        a = self.agent(agent_id)
        self.check_account_project(a)
        root = Path(a["cwd"]).resolve()
        supplied = Path(text_field(path, "a path", 4096)).expanduser()
        resolved = (supplied if supplied.is_absolute() else root / supplied).resolve()
        if not resolved.is_relative_to(root):
            raise ValueError("The file is outside this agent workspace")
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
        self.check_account_project(a)
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

    def changes(self, agent_id):
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

    def checkpoint_capture(
        self, agent_id, label="Checkpoint", turn_id=None, internal=False
    ):
        if internal:
            return self.capture_checkpoint(agent_id, label, turn_id)
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            self.assert_workspace_idle(a)
            a["workspaceOperation"] = "capture"
            self.put(db, "agents", a)
        try:
            return self.capture_checkpoint(agent_id, label, turn_id)
        finally:
            with self.lock, self.db() as db:
                a = self.agent(agent_id, db)
                a["workspaceOperation"] = None
                self.put(db, "agents", a)
            self.changed.set()

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

    def checkpoint_after_turn(self, key, turn_id):
        try:
            self.checkpoint_capture(key, "After turn", turn_id, True)
        except Exception as error:
            with self.lock, self.db() as db:
                a = self.agent(key, db)
                a["checkpointError"] = str(error)
                self.put(db, "agents", a)
        finally:
            with self.lock, self.db() as db:
                a = self.agent(key, db)
                a["workspaceOperation"] = None
                self.put(db, "agents", a)
            self.changed.set()

    def checkpoint_preview(self, key, checkpoint_id):
        a = self.checked_actor_in_own_db(key)
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_checkpoints WHERE id=?", (checkpoint_id,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown checkpoint")
            checkpoint = json.loads(row[0])
            if checkpoint["agent"] != key:
                raise ValueError("Checkpoint belongs to another agent")
        tree = self.snapshot_tree(a)
        patch = self.git(
            a, ["diff", "--no-ext-diff", "--no-color", tree, checkpoint["tree"]]
        ).decode(errors="replace")
        return {
            "checkpoint": checkpoint,
            "expectedTree": tree,
            "diff": patch[:300000],
            "patch": patch[:300000],
            "truncated": len(patch) > 300000,
            "canRestore": bool(a.get("worktreeReady") and not a.get("inFlight")),
        }

    def restore_checkpoint(self, key, data):
        with self.lock:
            a = self.checked_actor_in_own_db(key)
            self.check_account_project(a)
            self.assert_workspace_idle(a)
            if not a.get("worktreeReady"):
                raise ValueError(
                    "Restore is available only in an isolated worker worktree"
                )
            preview = self.checkpoint_preview(
                key, data.get("checkpoint_id") or data.get("checkpoint")
            )
            if data.get("expectedTree") != preview["expectedTree"]:
                raise ValueError("Files changed after the preview. Preview again")
            checkpoint = preview["checkpoint"]
            with self.db() as db:
                a["workspaceOperation"] = "restore"
                self.put(db, "agents", a)
        try:
            # Create the matching conversation before changing files. A provider rejection leaves files intact.
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
            if self.snapshot_tree(a) != preview["expectedTree"]:
                raise ValueError("Files changed while preparing restore. Preview again")
            # Only this isolated, idle worktree is changed; its old state remains a saved checkpoint.
            self.checkpoint_capture(key, "Before restore", internal=True)
            # Record the preview tree in this isolated index so added files are removed on restore too.
            self.git(a, ["read-tree", preview["expectedTree"]])
            self.git(a, ["read-tree", "--reset", "-u", checkpoint["tree"]])
            with self.lock, self.db() as db:
                current = self.agent(key, db)
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
                self.loaded.add(current["id"])
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
            return {"status": "restored", "checkpoint": checkpoint["id"]}
        finally:
            with self.lock, self.db() as db:
                current = self.agent(key, db)
                current["workspaceOperation"] = None
                self.put(db, "agents", current)

    def assert_workspace_idle(self, a):
        with self.db() as db:
            for other in self.records(db, "agents"):
                if Path(other["cwd"]).resolve() == Path(a["cwd"]).resolve() and (
                    other.get("inFlight") or other.get("workspaceOperation")
                ):
                    raise ValueError("An agent is using this workspace")
            if any(
                m["cwd"] == a["cwd"]
                and m["status"] in {"running", "starting", "approval"}
                for m in self.records(db, "monitors")
            ):
                raise ValueError("A monitor is using this workspace")
            peers = {
                v["id"]
                for v in self.records(db, "agents")
                if Path(v["cwd"]).resolve() == Path(a["cwd"]).resolve()
            }
            if any(
                t["agent"] in peers and t["status"] == "running"
                for t in self.records(db, "tasks")
            ):
                raise ValueError("A command or tool is still active")

    def branch_conversation(self, key, data):
        with self.lock:
            guard = self.prepare_locks.setdefault(
                "fork:" + key, __import__("threading").Lock()
            )
        with guard:
            return self.branch_locked(key, data)

    def branch_locked(self, key, data):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            signature, prior = self.operation_receipt(
                db, data.get("id"), {"agent": key, **data}
            )
            if prior is not None:
                return prior
            row = db.execute(
                "SELECT record FROM runtime_items WHERE id=? AND agent=?",
                (data.get("message_id"), key),
            ).fetchone()
            if not row:
                raise ValueError("Unknown message")
            item = json.loads(row[0])
            turn_id = item.get("turnId")
            if not turn_id or item.get("afterRestore"):
                raise ValueError("This message has no active native turn reference")
            self.assert_workspace_available(db, a)
            if turn_id == a.get("turnId"):
                raise ValueError("Wait for this turn to finish before branching")
        # A branch starts a new team, without the source team's exception.
        self.accounts.check_project(a.get("accountKey", "default"), a["cwd"])
        response = self.connect(a.get("accountKey", "default")).call(
            "thread/fork",
            {
                "threadId": a["threadId"],
                "lastTurnId": turn_id,
                "cwd": a["cwd"],
                "config": self.thread_config(),
                **{k: v for k, v in self.new_thread_params(a).items() if k in {"approvalPolicy", "sandbox"}},
                "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                "developerInstructions": self.new_thread_params(a, inherit_account_rule_override=False)[
                    "developerInstructions"
                ],
            },
        )
        self.checked_actor_in_own_db(key)
        lead = self.create(
            {
                "name": a["name"][:90] + " (branch)",
                "account_key": a.get("accountKey", "default"),
                "cwd": a["cwd"],
                "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                "prompt": "",
            },
            defer=True,
            draft=True,
        )
        with self.lock, self.db() as db:
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
            self.loaded.add(lead["id"])
            rows = db.execute(
                "SELECT record,created FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created",
                (key,),
            ).fetchall()
            cutoff = max(
                r["created"]
                for r in rows
                if json.loads(r["record"]).get("turnId") == turn_id
            )
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
            return self.save_receipt(db, data.get("id"), signature, lead)

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
        self.check_account_project(a)
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
            ("skills", "skills/list", {"cwds": [a["cwd"]]}),
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
            agents = [a for a in self.records(db, "agents") if not a.get("deletedAt")]
            ids = {a["id"] for a in agents}
            root = self.agent(key, db)["rootId"] if key else None
            inbox = []
            for r in self.records(db, "requests"):
                if r["status"] == "pending" and r["agent"] in ids:
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
                if c.get("needsResponse"):
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
                if task["status"] == "open":
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
            for m in self.recent_monitors(db):
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
        text = (
            "\u0003"
            if data["action"] == "cancel"
            else text_field(data.get("text"), "command input", 16000, empty=True)
        )
        instruction = (
            "Use your native write_stdin tool for session "
            + str(task["processId"])
            + ". Send exactly this JSON string as chars: "
            + json.dumps(text)
            + ". This is a user request to "
            + (
                "interrupt this command."
                if data["action"] == "cancel"
                else "send command input."
            )
        )
        a = self.agent(task["agent"])
        return self.send(
            a["id"],
            instruction,
            delivery="steer" if a.get("turnId") and a.get("inFlight") else "queue",
        )

    def assert_workspace_available(self, db, a):
        self.check_account_project(a, db)
        cwd = Path(a["cwd"]).resolve()
        if any(
            other.get("workspaceOperation") and Path(other["cwd"]).resolve() == cwd
            for other in self.records(db, "agents")
        ):
            raise ValueError("A workspace operation is active in this directory")

    def recent_monitors(self, db):
        rows = db.execute(
            """SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval')
          UNION ALL SELECT record FROM (SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') NOT IN ('running','starting','approval') ORDER BY json_extract(record,'$.created') DESC LIMIT 100)"""
        ).fetchall()
        return [json.loads(row[0]) for row in rows]
