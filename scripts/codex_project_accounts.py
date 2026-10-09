"""Project account membership, independent of native permissions and live chats."""
import json
import time
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from codex_records import ProjectRecord
    from codex_runtime import Runtime


def set_project_accounts(runtime: "Runtime", data: dict[str, object]) -> "ProjectRecord":
    from codex_project_locations import project_key
    path = project_key(runtime, data.get("path"), require_existing=True)
    keys = data.get("account_keys")
    if not isinstance(keys, list) or not keys or any(not isinstance(k, str) for k in keys):
        raise ValueError("Select at least one project account")
    keys = sorted(set(keys))
    default = data.get("account_key")
    if default not in keys:
        raise ValueError("The default must be a selected project account")
    revision = data.get("expected_revision")
    if type(revision) is not int or revision < 0:
        raise ValueError("Supply the current project account revision")
    accounts = {key: runtime.accounts.get(key) for key in keys}
    for key in keys:
        if accounts[key]["status"] != "ready":
            raise ValueError("Sign in to the selected project accounts first")
    with runtime.lock, runtime.db() as db:
        db.execute("BEGIN IMMEDIATE")
        row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
        project: "ProjectRecord | None" = json.loads(row[0]) if row else None
        current = project.get("accountRevision", 0) if project else 0
        if project and project.get("accountKey") == default and project.get("accountKeys") == keys and revision in (current, current - 1):
            return project
        existing_keys = (project.get("accountKeys") or [project.get("accountKey")]) if project else []
        for key in keys:
            if accounts[key].get("deleted"):
                raise ValueError("Deleted accounts cannot be added to a project")
            if accounts[key].get("disconnected") and (key == default or key not in existing_keys):
                raise ValueError("Reconnect this account before adding it to a project or selecting it as the default")
        if revision != current:
            raise ValueError("Project accounts changed. Reload before saving")
        if project is None:
            project = {"id": path, "path": path, "name": Path(path).name or path, "created": time.time()}  # type: ignore[assignment]  # typed-narrowing: draft gains account fields next
        project.update(accountKey=default, accountKeys=keys, accountRevision=current + 1)  # type: ignore[call-arg, union-attr]  # typed-update
        runtime.put(db, "projects", project)  # type: ignore[arg-type]  # typed-narrowing: populated project follows update below
        return project  # type: ignore[return-value]  # typed-narrowing: populated project follows update below
