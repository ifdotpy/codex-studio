"""Delete only validated project-relative paths for a delta upload."""
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sys

from upload import real_directory

request = json.load(sys.stdin)
root = Path(request["root"]).resolve()
for value in request["deletePaths"]:
    relative = PurePosixPath(value)
    if not value or relative.is_absolute() or ".." in relative.parts or str(relative) == ".":
        sys.exit(1)
    with real_directory(root, relative.parts[:-1], missing_ok=True) as parent:
        if parent is None:
            continue
        try:
            os.unlink(relative.name, dir_fd=parent)
        except IsADirectoryError:
            shutil.rmtree(relative.name, dir_fd=parent)
        except FileNotFoundError:
            pass
