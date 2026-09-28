#!/usr/bin/env python3
"""Isolated first-worker and idle-Claude preparation marks. No model calls."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location(
    "delivery_fixture_server", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args],
                                   stderr=subprocess.DEVNULL).decode().strip()


def spans(marks, fields):
    return {previous + "To" + current: round((marks[current] - marks[previous]) / 1e6, 3)
            for previous, current in zip(fields, fields[1:]) if previous in marks and current in marks}


def measure_worker(count=1000):
    with tempfile.TemporaryDirectory(prefix="studio-first-worker-") as temporary:
        root = Path(temporary)
        repo = root / "repo"
        repo.mkdir()
        git(repo, "init", "-q")
        for index in range(count):
            folder = repo / "src" / f"{index // 100:03d}"
            folder.mkdir(parents=True, exist_ok=True)
            (folder / f"file-{index:05d}.txt").write_text(f"Fixture {index}\n")
        git(repo, "add", "-A")
        git(repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
            "commit", "-qm", "Fixture")
        runtime = Runtime(root / "state", fixture.FakeServer)
        try:
            lead = runtime.create({"name": "Lead", "cwd": str(repo), "prompt": ""},
                                  draft=True, defer=True)
            worker = runtime.create({"name": "Worker", "prompt": "Task", "role": "implementer"},
                                    parent=lead["id"], defer=True)
            marks = {}
            runtime.prepare(worker, marks)
            prepared = runtime.agent(worker["id"])
            with runtime.db() as db:
                checkpoint = next(record for record in runtime.records(db, "checkpoints")
                                  if record["agent"] == worker["id"])
            assert checkpoint["tree"] == git(prepared["cwd"], "rev-parse", "HEAD^{tree}")
            began = time.monotonic_ns()
            assert runtime.snapshot_tree(prepared) == checkpoint["tree"]
            old_snapshot_ms = round((time.monotonic_ns() - began) / 1e6, 3)
            return {"kind": "firstWorker", "files": count,
                    "prepareMs": round((marks["threadReadyAt"] - marks["prepareBeganAt"]) / 1e6, 3),
                    "oldSnapshotTreeMs": old_snapshot_ms,
                    "segmentsMs": spans(marks, ["prepareBeganAt", "prepareGuardAcquiredAt",
                        "prepareChecksDoneAt", "prepareConnectedAt", "worktreeListedAt",
                        "worktreeAddedAt", "firstCheckpointAt", "threadParamsReadyAt",
                        "threadSubmittedAt", "threadReadyAt"])}
        finally:
            runtime.close()


def measure_claude():
    with tempfile.TemporaryDirectory(prefix="studio-claude-prepare-") as temporary:
        root = Path(temporary)
        runtime = Runtime(root / "state", fixture.FakeServer)
        try:
            agent = runtime.create({"name": "Claude lead", "cwd": str(root), "prompt": ""},
                                   draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                agent = runtime.agent(agent["id"], db)
                agent.update(provider="claude", model="gpt-6-astra", threadId="fixture-claude-thread",
                             worktree=False, worktreeReady=True)
                runtime.put(db, "agents", agent)
            marks = {}
            runtime.prepare(agent, marks)
            return {"kind": "idleClaudeLead", "fakeNative": True,
                    "prepareMs": round((marks["threadReadyAt"] - marks["prepareBeganAt"]) / 1e6, 3),
                    "segmentsMs": spans(marks, ["prepareBeganAt", "prepareGuardAcquiredAt",
                        "prepareChecksDoneAt", "prepareConnectedAt", "threadParamsReadyAt",
                        "threadSubmittedAt", "threadReadyAt"])}
        finally:
            runtime.close()


if __name__ == "__main__":
    print(json.dumps(measure_worker()), flush=True)
    print(json.dumps(measure_claude()), flush=True)
