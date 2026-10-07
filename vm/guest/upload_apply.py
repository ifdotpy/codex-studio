"""Keep the data disk floor active while a bounded worker applies source files."""
from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import sys

from common import GuestError, require
from upload import MAX_TREE, tree_space


def apply_tree(staging, root, expanded, mode):
    require(type(expanded) is int and 0 <= expanded <= MAX_TREE, "The expanded size is invalid")
    require(mode in {"full", "delta"}, "The upload mode is invalid")
    temporary = staging.parent / "rsync-temp"
    process = None
    try:
        tree_space(root, expanded)
        temporary.mkdir(mode=0o700)
        options = ["--delete"] if mode == "full" else []
        if sys.platform == "linux":
            # Make btrfs account dirty file extents before the final floor check.
            options.append("--fsync")
        # Share the deadline worker's process group so its SIGKILL covers rsync.
        process = subprocess.Popen(["rsync", "-a", "--checksum", "--safe-links",
            "--temp-dir=" + str(temporary), *options, "--", str(staging) + "/", str(root) + "/"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        while True:
            tree_space(root)
            try:
                code = process.wait(timeout=0.05)
                break
            except subprocess.TimeoutExpired:
                continue
        tree_space(root)
        if code:
            raise GuestError("outcome_unknown", "The project sync failed after application started")
    except GuestError as exc:
        if exc.code == "busy":
            raise GuestError("outcome_unknown", "The data disk crossed its free-space floor during project sync; inspect the project root") from exc
        raise
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)
        shutil.rmtree(temporary, ignore_errors=True)
        # Keep the archive for inspection. A partial project has no proven result.
        shutil.rmtree(staging, ignore_errors=True)


if __name__ == "__main__":
    try:
        params = json.loads(sys.stdin.buffer.read(2 * 1024 * 1024 + 1))
        apply_tree(Path(params["staging"]), Path(params["root"]), params["bytes"], params["mode"])
        print(json.dumps({"applied": True}))
    except GuestError as exc:
        print(json.dumps({"error": exc.object()}))
        sys.exit(1)
    except (OSError, ValueError, KeyError, TypeError):
        print(json.dumps({"error": {"code": "outcome_unknown", "message": "The project sync worker failed; inspect the project root"}}))
        sys.exit(1)
