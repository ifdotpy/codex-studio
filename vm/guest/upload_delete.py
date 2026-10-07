"""Delete only validated project-relative paths for a delta upload."""
import json
from pathlib import Path, PurePosixPath
import shutil
import sys

request = json.load(sys.stdin)
root = Path(request["root"]).resolve()
for value in request["deletePaths"]:
    relative = PurePosixPath(value)
    if not value or relative.is_absolute() or ".." in relative.parts or str(relative) == ".":
        sys.exit(1)
    target = root.joinpath(*relative.parts)
    if not target.parent.resolve().is_relative_to(root):
        sys.exit(1)
    if target.is_symlink() or target.is_file():
        target.unlink()
    elif target.exists():
        shutil.rmtree(target)
