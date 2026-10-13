#!/usr/bin/env python3
"""Workspace file read behavior through the Python and Rust HTTP backends."""

from __future__ import annotations

import base64
import hashlib
import json
import mimetypes
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import BackendProcess, TimedHTTPContractTest


def seed_python_workspace(backend, workspace: Path) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import base64, os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime

runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    cwd = Path({str(workspace)!r})
    common = {{"rootId": "files-contract-live", "epoch": 0, "status": "idle",
              "name": "Files contract", "role": "orchestrator", "cwd": str(cwd),
              "isLead": True, "autoWake": True, "approvalPolicy": "never",
              }}
    live = {{**common, "id": "files-contract-live"}}
    deleted = {{**common, "id": "files-contract-deleted", "rootId": "files-contract-deleted"}}
    image = {{**common, "id": "files-contract-image", "rootId": "files-contract-image",
             "imageWorkspace": True, "workspaceMode": "image"}}
    virtual = {{**common, "id": "files-contract-vm", "rootId": "files-contract-vm",
                "executionMode": "vm"}}
    with runtime.lock, runtime.db() as db:
        for agent in (live, deleted, image, virtual):
            runtime.put(db, "agents", agent, sync_rooms=False)
    runtime.upload_asset({{
        "agent": live["id"], "name": "upload.txt",
        "base64": base64.b64encode(b"python-written upload").decode("ascii"),
    }})
    deleted["deletedAt"] = 1.0
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", deleted, sync_rooms=False)
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=backend._environment(),
        check=True,
        timeout=30,
    )


def state_snapshot(state: Path):
    with sqlite3.connect(state / "canvas.sqlite3") as database:
        return {
            table: database.execute(f"SELECT id,record FROM {table} ORDER BY id").fetchall()
            for table in ("runtime_agents", "runtime_assets")
        }


class FilesHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = BackendProcess(Path(scratch))
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.workspace = Path(tempfile.mkdtemp(prefix="files-workspace-", dir=scratch))
        cls.addClassCleanup(shutil.rmtree, cls.workspace)
        cls.outside = cls.workspace.parent / f"{cls.workspace.name}-outside.md"
        cls.addClassCleanup(cls.outside.unlink, missing_ok=True)
        outside = cls.outside
        outside.write_text("symlink and absolute paths stay supported", encoding="utf-8")
        (cls.workspace / "link.md").symlink_to(outside)
        cls.preview_extensions = ("avif", "bmp", "webm", "ogg", "ts", "wav")
        for extension in cls.preview_extensions:
            (cls.workspace / f"preview.{extension}").write_bytes(b"preview")
        if hasattr(os, "mkfifo"):
            os.mkfifo(cls.workspace / "pipe")
        with (cls.workspace / "large.pdf").open("wb") as file:
            file.truncate(20 * 1024 * 1024 + 1)
        seed_python_workspace(cls.backend, cls.workspace)
        cls.backend.ready_samples.append(cls.backend.start(allow_sync_unavailable=True))

    def _headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_file_info_and_content_follow_python_written_workspace_state(self) -> None:
        backend = self.backend
        absolute_path = str(self.outside.resolve())
        info = backend.request(
            "GET",
            "/api/file-info?" + urlencode({"agent": "files-contract-live", "path": absolute_path}),
            headers=self._headers(),
        )
        self.assertEqual(info.status, 200, info.body[:1000])
        self.assertEqual(
            info.json(),
            {
                "path": absolute_path,
                "name": self.outside.name,
                "mime": "text/markdown",
                "size": len("symlink and absolute paths stay supported"),
            },
        )

        content = backend.request(
            "GET",
            "/api/file?" + urlencode({"agent": "files-contract-live", "path": "link.md"}),
            headers=self._headers(),
        )
        self.assertEqual(content.status, 200, content.body[:1000])
        self.assertEqual(
            content.json()["base64"],
            base64.b64encode(b"symlink and absolute paths stay supported").decode("ascii"),
        )
        self.assertEqual(content.json()["mime"], "text/markdown")

        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            asset_rows = database.execute("SELECT id,record FROM runtime_assets ORDER BY id").fetchall()
        asset_record = next(
            (row for row in asset_rows if json.loads(row[1]).get("name") == "upload.txt"),
            None,
        )
        self.assertIsNotNone(asset_record)
        uploaded = backend.request(
            "GET", "/api/file?" + urlencode({"asset": asset_record[0]}), headers=self._headers()
        )
        self.assertEqual(uploaded.status, 200, uploaded.body[:1000])
        self.assertEqual(
            base64.b64decode(uploaded.json()["base64"]), b"python-written upload"
        )

    def test_missing_and_deleted_agent_refusals_do_not_write_state(self) -> None:
        backend = self.backend
        before = state_snapshot(backend.state)
        response = backend.request(
            "GET",
            "/api/file-info?" + urlencode({"agent": "files-contract-deleted", "path": "missing"}),
            headers=self._headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertIn("deleted", response.json()["error"].lower())
        self.assertEqual(state_snapshot(backend.state), before)

        missing = backend.request(
            "GET",
            "/api/file?" + urlencode({"agent": "files-contract-live", "path": "missing"}),
            headers=self._headers(),
        )
        self.assertEqual(missing.status, 400, missing.body[:1000])
        self.assertEqual(state_snapshot(backend.state), before)

    def test_upload_replays_by_id_and_reads_through_asset_endpoint(self) -> None:
        backend = self.backend
        asset_id = "8f319af0-59e9-4c6a-8ec8-e99c9e354092"
        content = b"uploaded through the HTTP boundary"
        body = {
            "agent": "files-contract-live",
            "id": asset_id,
            "name": "asset.txt",
            "base64": base64.b64encode(content).decode("ascii"),
        }
        before = state_snapshot(backend.state)
        response = backend.request("POST", "/api/assets", body=body, headers=self._headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        asset = response.json()
        self.assertEqual(asset["id"], asset_id)
        self.assertEqual(asset["agent"], body["agent"])
        self.assertEqual(asset["name"], body["name"])
        self.assertEqual(asset["size"], len(content))
        self.assertEqual(asset["hash"], hashlib.sha256(content).hexdigest())
        self.assertNotIn("path", asset)

        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            stored = database.execute(
                "SELECT record FROM runtime_assets WHERE id=?", (asset_id,)
            ).fetchone()
        self.assertIsNotNone(stored)
        record = json.loads(stored[0])
        self.assertEqual(Path(record["path"]).read_bytes(), content)
        readback = backend.request(
            "GET", "/api/file?" + urlencode({"asset": asset_id}), headers=self._headers()
        )
        self.assertEqual(readback.status, 200, readback.body[:1000])
        self.assertEqual(base64.b64decode(readback.json()["base64"]), content)
        self.assertEqual(readback.json()["mime"], "text/plain")

        after_upload = state_snapshot(backend.state)
        replay = backend.request("POST", "/api/assets", body=body, headers=self._headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json(), asset)
        self.assertEqual(state_snapshot(backend.state), after_upload)
        conflict = backend.request(
            "POST",
            "/api/assets",
            body={**body, "base64": base64.b64encode(b"different content").decode("ascii")},
            headers=self._headers(),
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertIn("different content", conflict.json()["error"])
        self.assertEqual(state_snapshot(backend.state), after_upload)
        renamed = backend.request(
            "POST", "/api/assets", body={**body, "name": "renamed.txt"}, headers=self._headers()
        )
        self.assertEqual(renamed.status, 400, renamed.body[:1000])
        self.assertEqual(state_snapshot(backend.state), after_upload)

        data_alias = backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": "data-alias.txt",
                "data": base64.b64encode(b"data alias").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(data_alias.status, 200, data_alias.body[:1000])
        self.assertTrue(data_alias.json()["id"])

    def test_upload_accepts_the_former_fixed_temporary_filename(self) -> None:
        content = b"temporary-looking filename is user data"
        response = self.backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "id": "a17dfca8-c8b2-4c19-8d7e-106460fd6835",
                "name": ".payload.tmp",
                "base64": base64.b64encode(content).decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            record = database.execute(
                "SELECT record FROM runtime_assets WHERE id=?",
                ("a17dfca8-c8b2-4c19-8d7e-106460fd6835",),
            ).fetchone()
        self.assertIsNotNone(record)
        self.assertEqual(Path(json.loads(record[0])["path"]).read_bytes(), content)

    def test_upload_strips_python_whitespace_and_refuses_blank_names(self) -> None:
        backend = self.backend
        before = state_snapshot(backend.state)
        accepted = backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": "  normalized.txt \t",
                "base64": base64.b64encode(b"normalized").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        self.assertEqual(accepted.json()["name"], "normalized.txt")
        after_accepted = state_snapshot(backend.state)

        refused = backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": " \t\n ",
                "base64": base64.b64encode(b"blank name").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(state_snapshot(backend.state), after_accepted)
        self.assertEqual(len(after_accepted["runtime_assets"]), len(before["runtime_assets"]) + 1)

        too_long = backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": "x" * 256,
                "base64": base64.b64encode(b"too long").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(too_long.status, 400, too_long.body[:1000])
        self.assertEqual(state_snapshot(backend.state), after_accepted)

    def test_compressed_upload_mime_matches_python_guess_type(self) -> None:
        response = self.backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": "archive.tar.gz",
                "base64": base64.b64encode(b"archive").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["mime"], mimetypes.guess_type("archive.tar.gz")[0])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_corrupt_replay_record_is_refused_without_writing(self) -> None:
        backend = self.backend
        asset_id = "8f319af0-59e9-4c6a-8ec8-e99c9e354092"
        body = {
            "agent": "files-contract-live",
            "id": asset_id,
            "name": "asset.txt",
            "base64": base64.b64encode(b"uploaded through the HTTP boundary").decode("ascii"),
        }
        created = backend.request("POST", "/api/assets", body=body, headers=self._headers())
        self.assertEqual(created.status, 200, created.body[:1000])
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            original = database.execute(
                "SELECT record FROM runtime_assets WHERE id=?", (asset_id,)
            ).fetchone()[0]
            corrupt = json.loads(original)
            corrupt.pop("path")
            database.execute(
                "UPDATE runtime_assets SET record=? WHERE id=?",
                (json.dumps(corrupt), asset_id),
            )
        before = state_snapshot(backend.state)
        try:
            response = backend.request("POST", "/api/assets", body=body, headers=self._headers())
            self.assertEqual(response.status, 500, response.body[:1000])
            self.assertEqual(
                response.json()["error"], f"Invalid stored object runtime_assets/{asset_id}"
            )
            self.assertEqual(state_snapshot(backend.state), before)
        finally:
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "UPDATE runtime_assets SET record=? WHERE id=?", (original, asset_id)
                )

    def test_oversized_decoded_upload_is_rejected_without_writes(self) -> None:
        backend = self.backend
        before = state_snapshot(backend.state)
        encoded = base64.b64encode(b"x" * (20 * 1024 * 1024 + 1)).decode("ascii")
        response = backend.request(
            "POST",
            "/api/assets",
            body={"agent": "files-contract-live", "name": "too-large.bin", "base64": encoded},
            headers=self._headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertIn("20 MiB", response.json()["error"])
        self.assertEqual(state_snapshot(backend.state), before)

    def test_upload_refusals_do_not_write_state(self) -> None:
        backend = self.backend
        before = state_snapshot(backend.state)
        before_uploads = sorted(path.name for path in (backend.state / "uploads").iterdir())
        cases = (
            {"agent": "files-contract-live", "name": "bad.txt", "base64": "!!!"},
            {
                "agent": "files-contract-missing",
                "name": "bad.txt",
                "base64": base64.b64encode(b"bytes").decode("ascii"),
            },
            {
                "agent": "files-contract-deleted",
                "name": "deleted.txt",
                "base64": base64.b64encode(b"bytes").decode("ascii"),
            },
        )
        for body in cases:
            with self.subTest(agent=body["agent"]):
                response = backend.request("POST", "/api/assets", body=body, headers=self._headers())
                self.assertEqual(response.status, 400, response.body[:1000])
                if body["agent"] == "files-contract-deleted":
                    self.assertIn("deleted", response.json()["error"].lower())
                self.assertEqual(state_snapshot(backend.state), before)
                self.assertEqual(
                    sorted(path.name for path in (backend.state / "uploads").iterdir()),
                    before_uploads,
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_upload_cleans_orphaned_file_before_persisting_record(self) -> None:
        backend = self.backend
        orphan = backend.state / "uploads" / "crash-orphan"
        orphan.mkdir()
        (orphan / ".payload.tmp").write_bytes(b"unindexed payload")
        response = backend.request(
            "POST",
            "/api/assets",
            body={
                "agent": "files-contract-live",
                "name": "recovery.txt",
                "base64": base64.b64encode(b"recovered").decode("ascii"),
            },
            headers=self._headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertFalse(orphan.exists())

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_execution_backed_workspaces_have_named_501_without_writes(self) -> None:
        backend = self.backend
        before = state_snapshot(backend.state)
        for agent_id in ("files-contract-image", "files-contract-vm"):
            response = backend.request(
                "GET",
                "/api/file-info?" + urlencode({"agent": agent_id, "path": "outside.md"}),
                headers=self._headers(),
            )
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires workspace-state/image-execution",
            )
            self.assertEqual(state_snapshot(backend.state), before)

    def test_file_content_preview_limit_is_enforced(self) -> None:
        response = self.backend.request(
            "GET",
            "/api/file?" + urlencode({"agent": "files-contract-live", "path": "large.pdf"}),
            headers=self._headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertIn("preview limit", response.json()["error"])

    def test_preview_media_mime_matches_python_mimetypes(self) -> None:
        for extension in self.preview_extensions:
            name = f"preview.{extension}"
            expected = mimetypes.guess_type(name)[0] or "application/octet-stream"
            info = self.backend.request(
                "GET",
                "/api/file-info?"
                + urlencode({"agent": "files-contract-live", "path": name}),
                headers=self._headers(),
            )
            self.assertEqual(info.status, 200, info.body[:1000])
            self.assertEqual(info.json()["mime"], expected, extension)
            content = self.backend.request(
                "GET",
                "/api/file?" + urlencode({"agent": "files-contract-live", "path": name}),
                headers=self._headers(),
            )
            self.assertEqual(content.status, 200, content.body[:1000])
            self.assertEqual(content.json()["mime"], expected, extension)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO files require a Unix platform")
    def test_fifo_files_are_rejected_before_opening(self) -> None:
        response = self.backend.request(
            "GET",
            "/api/file?" + urlencode({"agent": "files-contract-live", "path": "pipe"}),
            headers=self._headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])


if __name__ == "__main__":
    unittest.main()
