"""Project labels and chat folders. These operations never change directories."""

import json
import time
import uuid

from codex_work import text_field


def folder_for(runtime, db, path, folder_id):
    if folder_id is None:
        return None
    if not isinstance(folder_id, str):
        raise ValueError("Select a project folder")
    row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
    project = json.loads(row[0]) if row else {}
    if not any(folder['id'] == folder_id for folder in project.get('folders', [])):
        raise ValueError("This folder is not in the selected project")
    return folder_id


def organize_project(runtime, data):
    action = data['action']
    path = runtime.project_directory(data.get('path'), require_existing=False)
    revision = data.get('expected_revision')
    if type(revision) is not int or revision < 0:
        raise ValueError("Supply the current project revision")
    name = text_field(data.get('name'), 'a name', 255) if action != 'remove_folder' else None
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        project = runtime.ensure_project(path, runtime.project_account(path, db=db), db)
        current = project.get('organizationRevision', 0)
        folders = project.get('folders', [])
        target = None
        if action != 'rename':
            try:
                folder_id = str(uuid.UUID(data.get('folder_id', '')))
            except (ValueError, TypeError, AttributeError):
                raise ValueError('Supply a folder ID') from None
            target = next((folder for folder in folders if folder['id'] == folder_id), None)
        if action == 'rename' and project['name'] == name:
            return project
        if action == 'add_folder' and target:
            if target['name'] == name and target.get('parentId') == data.get('parent_id'):
                return project
            raise ValueError('This folder ID has different settings')
        if action == 'rename_folder' and target and target['name'] == name:
            return project
        if action == 'remove_folder' and not target:
            return project
        if revision != current:
            raise ValueError('Project changed. Reload it before saving')
        if action == 'rename':
            project['name'] = name
        elif action in ('add_folder', 'rename_folder'):
            if action == 'rename_folder' and not target:
                raise ValueError('This folder no longer exists')
            parent = folder_for(runtime, db, path, data.get('parent_id')) if action == 'add_folder' else target.get('parentId')
            if any(folder['id'] != folder_id and folder.get('parentId') == parent and folder['name'].casefold() == name.casefold() for folder in folders):
                raise ValueError('A folder with this name already exists here')
            if target:
                target['name'] = name
            else:
                folders.append({'id': folder_id, 'name': name, 'parentId': parent})
        else:
            occupied = any(folder.get('parentId') == folder_id for folder in folders)
            occupied = occupied or db.execute("SELECT 1 FROM runtime_agents WHERE json_extract(record,'$.cwd')=? AND json_extract(record,'$.projectFolder')=? AND json_extract(record,'$.deletedAt') IS NULL LIMIT 1", (path, folder_id)).fetchone()
            if occupied:
                raise ValueError('This folder still contains chats or subfolders')
            folders = [folder for folder in folders if folder['id'] != folder_id]
        project.update(folders=folders, organizationRevision=current + 1, updated=time.time())
        runtime.put(db, 'projects', project)
        return project
