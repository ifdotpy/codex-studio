"""Choose new-chat accounts from the destination project, never another chat."""
from pathlib import Path
from typing import TYPE_CHECKING, cast

if TYPE_CHECKING:
    import sqlite3
    from codex_runtime import Runtime


def project_default(runtime: "Runtime", cwd: str | Path | None,
                    db: "sqlite3.Connection") -> str:
    directory = Path(runtime.project_directory(cwd, require_existing=False))
    matches = [project for project in runtime.records(db, "projects")
               if project.get("accountKey")
               and directory.is_relative_to(Path(project["path"]).expanduser().resolve())]
    if matches:
        return max(matches, key=lambda project: len(Path(project["path"]).parts))["accountKey"]
    return cast(str, runtime.accounts.default())
