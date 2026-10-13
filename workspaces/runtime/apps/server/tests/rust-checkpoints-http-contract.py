#!/usr/bin/env python3
"""Checkpoint capture and preview interoperate with Python and Rust backends."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
import uuid
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import BackendProcess, HTTPTransportError


AGENT_ID = "checkpoints-contract-agent"
OTHER_AGENT_ID = "checkpoints-contract-other"
IMAGE_AGENT_ID = "checkpoints-contract-image"
VM_AGENT_ID = "checkpoints-contract-vm"
REPOSITORY_ROOT_ENV = "CODEX_CHECKPOINT_TEST_REPOSITORY_ROOT"


def is_rust_backend() -> bool:
    return bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))


def initialize_repository(workspace: Path) -> None:
    (workspace / "tracked.txt").write_text("base checkpoint\n", encoding="utf-8")
    subprocess.run(["git", "add", "tracked.txt"], cwd=workspace, check=True, timeout=30)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=Checkpoint Contract",
            "-c",
            "user.email=checkpoint-contract@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        cwd=workspace,
        check=True,
        timeout=30,
    )


def seed_python_state(backend: BackendProcess) -> str:
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime

runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    cwd = {str(backend.cwd)!r}
    common = {{"rootId": "checkpoints-contract", "epoch": 0, "status": "idle",
              "name": "Checkpoint contract", "role": "orchestrator", "cwd": cwd,
              "isLead": True, "autoWake": False, "approvalPolicy": "never",
              "worktreeReady": True, "accountKey": "default", "threadId": "thread-contract"}}
    agents = (
        {{**common, "id": {AGENT_ID!r}}},
        {{**common, "id": {OTHER_AGENT_ID!r}, "rootId": "checkpoints-contract-other"}},
        {{**common, "id": {IMAGE_AGENT_ID!r}, "rootId": "checkpoints-contract-image",
         "imageWorkspace": True, "workspaceMode": "image"}},
        {{**common, "id": {VM_AGENT_ID!r}, "rootId": "checkpoints-contract-vm",
         "executionMode": "vm"}},
    )
    with runtime.lock, runtime.db() as db:
        for agent in agents:
            runtime.put(db, "agents", agent, sync_rooms=False)
    checkpoint = runtime.capture_checkpoint({AGENT_ID!r}, "Python fixture")
    print(checkpoint["id"])
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    completed = subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=backend._environment(),
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return completed.stdout.strip().splitlines()[-1]


def database_snapshot(state: Path) -> tuple[object, ...]:
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        return tuple(
            (
                table,
                database.execute(f'SELECT id,record FROM "{table}" ORDER BY id').fetchall(),
            )
            for table in ("runtime_agents", "runtime_checkpoints", "runtime_workspace_operations")
        )


def checkpoint_refs(workspace: Path) -> set[str]:
    completed = subprocess.run(
        ["git", "for-each-ref", "--format=%(refname)", "refs/codex-agents/checkpoints"],
        cwd=workspace,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return set(completed.stdout.splitlines())


class CheckpointsHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        repository_root = Path(os.environ.get(REPOSITORY_ROOT_ENV) or scratch)
        repository_root.mkdir(parents=True, exist_ok=True)
        cls.workspace = Path(
            tempfile.mkdtemp(prefix=f"contract-{uuid.uuid4().hex}-", dir=repository_root)
        ).resolve(strict=True)
        cls.addClassCleanup(shutil.rmtree, cls.workspace)
        original_directory = BackendProcess._directory

        def directory(backend: BackendProcess, leaf: str) -> Path:
            if leaf == "workspace":
                return cls.workspace
            return original_directory(backend, leaf)

        BackendProcess._directory = directory
        try:
            cls.backend = BackendProcess(Path(scratch))
        finally:
            BackendProcess._directory = original_directory
        cls.addClassCleanup(cls.backend.close)
        initialize_repository(cls.workspace)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")

        # Keep the test-owned Git repository and checkpoint refs under configured scratch.
        cls.initial_checkpoint_id = seed_python_state(cls.backend)
        cls.backend.ready_samples.append(cls.backend.start())

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Canvas-Workspace": self.backend.identity["workspaceId"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_capture_persists_checkpoint_history_and_git_ref(self) -> None:
        backend = self.backend
        index = (self.workspace / ".git" / "index").read_bytes()
        response = backend.request(
            "POST",
            "/api/checkpoint",
            body={"agent": AGENT_ID, "label": "HTTP checkpoint"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        summary = response.json()
        self.assertEqual(summary["agent"], AGENT_ID)
        self.assertEqual(summary["label"], "HTTP checkpoint")
        self.assertNotIn("historyDelta", summary)
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            row = database.execute(
                "SELECT record FROM runtime_checkpoints WHERE id=?", (summary["id"],)
            ).fetchone()
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            operation = json.loads(
                database.execute(
                    "SELECT record FROM runtime_workspace_operations WHERE id LIKE ? "
                    "ORDER BY rowid DESC LIMIT 1",
                    ("capture:%",),
                ).fetchone()[0]
            )
        self.assertIsNotNone(row)
        record = json.loads(row[0])
        self.assertEqual(record["id"], summary["id"])
        self.assertEqual(record["historyParent"], self.initial_checkpoint_id)
        self.assertEqual(agent["checkpointHistoryHead"], summary["id"])
        self.assertEqual(operation["phase"], "completed")
        self.assertIn(summary["ref"], checkpoint_refs(self.workspace))
        self.assertEqual((self.workspace / ".git" / "index").read_bytes(), index)

    def test_preview_reads_python_written_checkpoint_without_database_writes(self) -> None:
        backend = self.backend
        (self.workspace / "tracked.txt").write_text("edited workspace\n", encoding="utf-8")
        before = database_snapshot(backend.state)
        response = backend.request(
            "POST",
            "/api/checkpoint/preview",
            body={"agent": AGENT_ID, "checkpoint": self.initial_checkpoint_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        preview = response.json()
        self.assertEqual(preview["checkpoint"]["id"], self.initial_checkpoint_id)
        self.assertEqual(preview["diff"], preview["patch"])
        self.assertIn("base checkpoint", preview["patch"])
        self.assertEqual(preview["truncated"], False)
        self.assertEqual(preview["canRestore"], not is_rust_backend())
        self.assertEqual(database_snapshot(backend.state), before)

    def test_preview_bounds_large_patch_output(self) -> None:
        backend = self.backend
        large_file = self.workspace / "large.txt"
        large_file.write_text("old\n" * 40_000, encoding="utf-8")
        captured = backend.request(
            "POST",
            "/api/checkpoint",
            body={"agent": AGENT_ID, "label": "large preview"},
            headers=self.headers(),
        )
        self.assertEqual(captured.status, 200, captured.body[:1000])
        checkpoint_id = captured.json()["id"]
        large_file.write_text("new\n" * 40_000, encoding="utf-8")
        before = database_snapshot(backend.state)
        response = backend.request(
            "POST",
            "/api/checkpoint/preview",
            body={"agent": AGENT_ID, "checkpoint": checkpoint_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        preview = response.json()
        self.assertTrue(preview["truncated"])
        self.assertEqual(len(preview["patch"]), 300_000)
        self.assertEqual(preview["diff"], preview["patch"])
        self.assertEqual(database_snapshot(backend.state), before)

    def test_refusals_do_not_write_database_or_checkpoint_refs(self) -> None:
        backend = self.backend
        before = (database_snapshot(backend.state), checkpoint_refs(self.workspace))
        cases = (
            (
                "/api/checkpoint",
                {"agent": "missing-checkpoint-agent", "label": "refused"},
            ),
            (
                "/api/checkpoint/preview",
                {"agent": OTHER_AGENT_ID, "checkpoint": self.initial_checkpoint_id},
            ),
        )
        for path, body in cases:
            response = backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 400, (path, response.body[:1000]))
            self.assertEqual(
                (database_snapshot(backend.state), checkpoint_refs(self.workspace)), before
            )

    @unittest.skipUnless(is_rust_backend(), "Stored Git option validation belongs to Rust")
    def test_preview_rejects_non_object_id_tree_without_writes(self) -> None:
        backend = self.backend
        marker = self.workspace / "preview-option-injection.txt"
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            raw = database.execute(
                "SELECT record FROM runtime_checkpoints WHERE id=?",
                (self.initial_checkpoint_id,),
            ).fetchone()[0]
            checkpoint = json.loads(raw)
            checkpoint["tree"] = f"--output={marker}"
            database.execute(
                "UPDATE runtime_checkpoints SET record=? WHERE id=?",
                (json.dumps(checkpoint), self.initial_checkpoint_id),
            )
            database.commit()
        before = database_snapshot(backend.state)
        try:
            response = backend.request(
                "POST",
                "/api/checkpoint/preview",
                body={"agent": AGENT_ID, "checkpoint": self.initial_checkpoint_id},
                headers=self.headers(),
            )
            self.assertEqual(response.status, 500, response.body[:1000])
            self.assertFalse(marker.exists())
            self.assertEqual(database_snapshot(backend.state), before)
        finally:
            with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
                database.execute(
                    "UPDATE runtime_checkpoints SET record=? WHERE id=?",
                    (raw, self.initial_checkpoint_id),
                )
                database.commit()

    @unittest.skipUnless(is_rust_backend(), "Provider and execution 501s belong to Rust")
    def test_provider_and_execution_dependencies_return_named_501_without_writes(self) -> None:
        before = (database_snapshot(self.backend.state), checkpoint_refs(self.workspace))
        cases = (
            (
                "/api/branch",
                {"agent": AGENT_ID, "id": "branch-contract", "message_id": "branch-message"},
                "Not implemented: requires provider-session/thread-fork",
            ),
            (
                "/api/checkpoint/restore",
                {
                    "agent": AGENT_ID,
                    "checkpoint": self.initial_checkpoint_id,
                    "expectedTree": "0" * 40,
                },
                "Not implemented: requires provider-session/thread-fork",
            ),
            (
                "/api/checkpoint",
                {"agent": IMAGE_AGENT_ID},
                "Not implemented: requires workspace-state/image-execution",
            ),
            (
                "/api/checkpoint",
                {"agent": VM_AGENT_ID},
                "Not implemented: requires workspace-state/image-execution",
            ),
        )
        for path, body, error in cases:
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, (path, response.body[:1000]))
            self.assertEqual(response.json()["error"], error)
            self.assertEqual(
                (database_snapshot(self.backend.state), checkpoint_refs(self.workspace)), before
            )

    def test_crash_after_update_ref_leaves_dangling_ref_and_recovers_on_startup(self) -> None:
        backend = self.backend
        before_refs = checkpoint_refs(self.workspace)
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            before_agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )

        wrapper_dir = self.workspace.parent / f"git-wrapper-{uuid.uuid4().hex}"
        wrapper_dir.mkdir()
        self.addCleanup(shutil.rmtree, wrapper_dir)
        marker = wrapper_dir / "update-ref-ran"
        real_git = shutil.which("git")
        if real_git is None:
            self.fail("git must be available for the checkpoint contract")
        wrapper = wrapper_dir / "git"
        wrapper.write_text(
            "#!/bin/sh\n"
            'if [ "$3" = "update-ref" ]; then\n'
            f"  {shlex.quote(real_git)} \"$@\"\n"
            "  status=$?\n"
            '  if [ "$status" -eq 0 ]; then\n'
            '    : > "$STUDIO_CHECKPOINT_UPDATE_REF_MARKER"\n'
            '    kill -KILL "$PPID"\n'
            "  fi\n"
            '  exit "$status"\n'
            "fi\n"
            f"exec {shlex.quote(real_git)} \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        original_environment = BackendProcess._environment

        def environment_with_failure(backend_instance: BackendProcess) -> dict[str, str]:
            environment = original_environment(backend_instance)
            environment["PATH"] = str(wrapper_dir) + os.pathsep + environment["PATH"]
            environment["STUDIO_CHECKPOINT_UPDATE_REF_MARKER"] = str(marker)
            return environment

        BackendProcess._environment = environment_with_failure
        try:
            backend.stop()
            backend.start()
            try:
                backend.request(
                    "POST",
                    "/api/checkpoint",
                    body={"agent": AGENT_ID, "label": "crash window"},
                    headers=self.headers(),
                    timeout=5,
                )
            except (HTTPTransportError, OSError, TimeoutError):
                pass
        finally:
            BackendProcess._environment = original_environment

        self.assertTrue(marker.is_file(), "failure injection must run after update-ref succeeds")
        process = backend._process
        self.assertIsNotNone(process)
        assert process is not None
        process.wait(timeout=10)
        backend.stop()
        after_refs = checkpoint_refs(self.workspace)
        new_refs = after_refs - before_refs
        self.assertEqual(len(new_refs), 1, sorted(new_refs))
        dangling_ref = next(iter(new_refs))
        dangling_id = dangling_ref.rsplit("/", 1)[-1]
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            self.assertIsNone(
                database.execute(
                    "SELECT 1 FROM runtime_checkpoints WHERE id=?", (dangling_id,)
                ).fetchone()
            )
            pending = [
                (operation_id, json.loads(record))
                for operation_id, record in database.execute(
                    "SELECT id,record FROM runtime_workspace_operations ORDER BY rowid"
                )
                if json.loads(record).get("phase") in {"capture_pending", "capture_running"}
            ]
            self.assertEqual(len(pending), 1)
            operation_id, operation = pending[0]
            self.assertEqual(operation["agent"], AGENT_ID)
            self.assertEqual(operation["phase"], "capture_running")
            crashed_agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            self.assertEqual(crashed_agent["checkpointHistoryHead"], before_agent["checkpointHistoryHead"])
            self.assertEqual(crashed_agent["workspaceReservationId"], operation_id)

        backend.start()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            recovered = json.loads(
                database.execute(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?", (operation_id,)
                ).fetchone()[0]
            )
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            self.assertIsNone(
                database.execute(
                    "SELECT 1 FROM runtime_checkpoints WHERE id=?", (dangling_id,)
                ).fetchone()
            )
        self.assertEqual(recovered["phase"], "failed")
        self.assertEqual(
            recovered["error"], "Server restarted before checkpoint capture finished"
        )
        self.assertIsNone(agent.get("workspaceReservationId"))
        self.assertIsNone(agent.get("workspaceOperation"))
        self.assertEqual(
            agent["checkpointError"], "Server restarted before checkpoint capture finished"
        )
        self.assertIn(dangling_ref, checkpoint_refs(self.workspace))

    @unittest.skipUnless(is_rust_backend(), "Rust capture recovery belongs to Rust")
    def test_startup_completes_capture_with_a_persisted_checkpoint(self) -> None:
        backend = self.backend
        response = backend.request(
            "POST",
            "/api/checkpoint",
            body={"agent": AGENT_ID, "label": "persisted recovery"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        checkpoint_id = response.json()["id"]
        operation_id = f"capture:{checkpoint_id}"
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            operation = json.loads(
                database.execute(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?",
                    (operation_id,),
                ).fetchone()[0]
            )
            operation["phase"] = "capture_running"
            database.execute(
                "UPDATE runtime_workspace_operations SET record=? WHERE id=?",
                (json.dumps(operation), operation_id),
            )
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            agent["workspaceOperation"] = "capture"
            agent["workspaceReservationId"] = operation_id
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), AGENT_ID),
            )
            database.commit()

        backend.stop()
        backend.start()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            recovered = json.loads(
                database.execute(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?",
                    (operation_id,),
                ).fetchone()[0]
            )
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            self.assertIsNotNone(
                database.execute(
                    "SELECT 1 FROM runtime_checkpoints WHERE id=?", (checkpoint_id,)
                ).fetchone()
            )
        self.assertEqual(recovered["phase"], "completed")
        self.assertIsNone(recovered.get("error"))
        self.assertEqual(agent["checkpointHistoryHead"], checkpoint_id)
        self.assertIsNone(agent.get("workspaceReservationId"))
        self.assertIsNone(agent.get("workspaceOperation"))

    @unittest.skipUnless(is_rust_backend(), "Rust startup recovery belongs to Rust")
    def test_startup_recovers_python_automatic_checkpoint_reservation(self) -> None:
        backend = self.backend
        operation_id = f"checkpoint:{uuid.uuid4()}"
        operation = {
            "id": operation_id,
            "kind": "checkpoint",
            "agent": AGENT_ID,
            "cwd": str(self.workspace),
            "epoch": 0,
            "turnId": "automatic-turn",
            "phase": "capture_running",
        }
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
            agent["workspaceOperation"] = "checkpoint"
            agent["workspaceReservationId"] = operation_id
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), AGENT_ID),
            )
            database.execute(
                "INSERT INTO runtime_workspace_operations(id,record) VALUES(?,?)",
                (operation_id, json.dumps(operation)),
            )
            database.commit()

        backend.stop()
        backend.start()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
            recovered = json.loads(
                database.execute(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?",
                    (operation_id,),
                ).fetchone()[0]
            )
            agent = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (AGENT_ID,)
                ).fetchone()[0]
            )
        self.assertEqual(recovered["phase"], "failed")
        self.assertEqual(
            recovered["error"], "Server restarted before checkpoint capture finished"
        )
        self.assertIsNone(agent.get("workspaceReservationId"))
        self.assertIsNone(agent.get("workspaceOperation"))
        self.assertEqual(
            agent["checkpointError"], "Server restarted before checkpoint capture finished"
        )


if __name__ == "__main__":
    unittest.main()
