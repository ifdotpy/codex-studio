"""Select worker environments without changing the lead or reviewer host."""
from pathlib import Path
import json
import time


def validate(value):
    if value not in ("host", "linux"):
        raise ValueError('environment must be "host" or "linux"')
    return value


def project_default(runtime, cwd, db):
    directory = Path(runtime.project_directory(cwd, require_existing=False))
    matches = [project for project in runtime.records(db, "projects")
               if directory.is_relative_to(Path(project["path"]).expanduser().resolve())
               and "workerEnvironment" in project]
    if not matches:
        return "host"
    return validate(max(matches, key=lambda p: len(Path(p["path"]).parts))["workerEnvironment"])


def select(runtime, spec, cwd):
    if spec.get("role", "implementer") != "implementer":
        if spec.get("environment", "host") != "host":
            raise ValueError("Reviewers must use the host environment")
        return "host"
    if "environment" in spec:
        return validate(spec["environment"])
    with runtime.lock, runtime.read_db() as db:
        return project_default(runtime, cwd, db)


def set_project_default(runtime, data):
    environment = validate(data.get("environment"))
    revision = data.get("expected_revision")
    if type(revision) is not int or revision < 0:
        raise ValueError("Supply the current worker environment revision")
    path = runtime.project_directory(data.get("path"), require_existing=True)
    with runtime.lock, runtime.db() as db:
        db.execute("BEGIN IMMEDIATE")
        row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
        project = json.loads(row[0]) if row else None
        current = project.get("workerEnvironmentRevision", 0) if project else 0
        if (project and project.get("workerEnvironment", "host") == environment
                and revision in (current, current - 1)):
            return project
        if revision != current:
            raise ValueError("Worker environment changed. Reload before saving")
        if project is None:
            project = {"id": path, "path": path, "name": Path(path).name or path,
                       "created": time.time()}
        project.update(workerEnvironment=environment, workerEnvironmentRevision=current + 1)
        runtime.put(db, "projects", project)
        return project
