"""Python side of the Rust image workspace persisted-row corpus."""
from __future__ import annotations

import json
import os
import shutil
import sys
import time
from pathlib import Path

server_source, root_arg, store_arg = map(Path, sys.argv[1:4])
sys.path.insert(0, str(server_source))
import codex_workspace_images as images  # noqa: E402

root = root_arg.resolve()
store = store_arg.resolve()
os.environ["CODEX_WORKSPACE_STORE"] = str(store)
os.environ["CODEX_WORKSPACE_MIN_FREE_BYTES"] = "0"
os.environ["CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES"] = "0"


class ScriptedBackend:
    def __init__(self) -> None:
        self.failures = ["seal", "clone"]

    def fail(self, operation: str) -> None:
        if self.failures and self.failures[0] == operation:
            self.failures.pop(0)
            raise RuntimeError(f"Workspace backend failed: failpoint:{operation}")

    def supported(self, _root):
        return True, ""

    def current_event_id(self, _root):
        return 41

    def open_base_staging(self, _root, key, version):
        version_path = store / "bases" / key / "versions" / version
        destination = version_path / "repo"
        destination.mkdir(parents=True)
        image = version_path / "base.asif"
        image.touch()
        return {"root": destination, "image": image, "mount": None,
                "versionPath": version_path, "token": 41, "version": version}

    @staticmethod
    def copy_base_tree(source, destination, *, excludes=(), check=None):
        excludes = set(excludes)

        def ignore(directory, names):
            relative = Path(directory).relative_to(source)
            return [name for name in names
                    if (relative / name).as_posix() in excludes
                    or any((relative / name).as_posix().startswith(value + "/")
                           for value in excludes)]

        if check:
            check()
        shutil.copytree(source, destination, dirs_exist_ok=True, symlinks=True, ignore=ignore)

    def seal_base(self, staging):
        self.fail("seal")
        return {"image": staging["image"], "versionPath": staging["versionPath"],
                "token": staging["token"]}

    def clone_workspace(self, image, agent_dir):
        agent_dir.mkdir(parents=True, exist_ok=True)
        self.fail("clone")
        layer = agent_dir / "workspace.asif"
        shutil.copy2(image, layer)
        return layer

    @staticmethod
    def mount_workspace(_layer, mount, *, base_image=None):
        (mount / "repo").mkdir(parents=True, exist_ok=True)
        return {"mount": str(mount), "baseImage": str(base_image) if base_image else None}

    @staticmethod
    def sync_delta(_source, _target, token, *, excludes=()):
        return {"token": token, "changedPaths": ["."], "historyLost": False,
                "scanPaths": ["."], "refreshBase": False}

    @staticmethod
    def unmount_workspace(_mount, *, force=False):
        return {"method": "scripted", "terminatedPids": 0}

    @staticmethod
    def remove_layer(agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=True)

    @staticmethod
    def remove_base_version(path):
        shutil.rmtree(path, ignore_errors=True)

    @staticmethod
    def private_bytes(_path):
        return 0

    @staticmethod
    def exec_prefix():
        return []


backend = ScriptedBackend()
images._backend_instance = backend


def wait_for_build():
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        status = images.base_status(root)
        if status["state"] in {"ready", "failed"}:
            return status
        time.sleep(.005)
    raise TimeoutError("Python image base build did not settle")


def normalize(value):
    if isinstance(value, dict):
        return {key: normalize(child) for key, child in value.items()
                if key not in {"createdAt", "startedAt", "failedAt", "builderPid"}}
    if isinstance(value, list):
        return [normalize(child) for child in value]
    if isinstance(value, str):
        value = value.replace(str(store), "<store>")
        return __import__("re").sub(r"v-[0-9]+", "v-{version}", value)
    return value


snapshots = []
images.start_base_build(root)
failed = wait_for_build()
base_path = store / "bases" / images._repo_key(root) / "base.json"
snapshots.append({"phase": "seal-failed", "base": normalize(images._read_json(base_path))})
while (thread := images._build_threads.get(images._repo_key(root))) is not None and thread.is_alive():
    time.sleep(.005)
failed_version = failed.get("version")
images.start_base_build(root, retry_failed=True)
deadline = time.monotonic() + 5
while images._read_json(base_path, {}).get("version") == failed_version:
    if time.monotonic() >= deadline:
        raise TimeoutError("Python retry did not publish a new build version")
    time.sleep(.005)
ready = wait_for_build()
if ready["state"] != "ready":
    raise RuntimeError(ready.get("error") or "Python base retry failed")
snapshots.append({"phase": "base-ready", "base": normalize(images._read_json(base_path))})

try:
    images.create_workspace(root, "agent-corpus")
except RuntimeError:
    pass
else:
    raise AssertionError("scripted clone failure was not observed")
agent_path = store / "agents" / "agent-corpus" / "agent.json"
snapshots.append({"phase": "clone-failed", "agent": normalize(images._read_json(agent_path))})
images.create_workspace(root, "agent-corpus")
snapshots.append({"phase": "workspace-ready", "agent": normalize(images._read_json(agent_path))})
images.archive_workspace("agent-corpus")
snapshots.append({"phase": "workspace-archived", "agent": normalize(images._read_json(agent_path))})
images.ensure_mounted("agent-corpus")
snapshots.append({"phase": "workspace-remounted", "agent": normalize(images._read_json(agent_path))})
print(json.dumps(snapshots, sort_keys=True, separators=(",", ":")))
