"""Choose new-chat accounts from the destination project, never another chat."""
from pathlib import Path


def project_default(runtime, cwd, db):
    directory = Path(runtime.project_directory(cwd, require_existing=False))
    matches = [project for project in runtime.records(db, "projects")
               if project.get("accountKey")
               and directory.is_relative_to(Path(project["path"]).expanduser().resolve())]
    if matches:
        return max(matches, key=lambda project: len(Path(project["path"]).parts))["accountKey"]
    return runtime.accounts.default()
