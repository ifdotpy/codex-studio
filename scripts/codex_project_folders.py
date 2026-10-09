"""Project labels and chat folders. These operations never change directories."""

import json
import time
import uuid

from codex_work import text_field


class SidebarOrderConflict(ValueError):
    pass


def sidebar_order(db):
    row = db.execute("SELECT record FROM runtime_sidebar_order WHERE id='current'").fetchone()
    return json.loads(row[0]) if row else {'revision': 0, 'groups': None}


def reorder_sidebar(runtime, data):
    """Save the first client's legacy order or a revision checked reorder."""
    request_id = text_field(data.get('request_id'), 'a request ID', 255)
    revision = data.get('expected_revision')
    groups = data.get('groups')
    if type(revision) is not int or revision < 0:
        raise ValueError('Supply the current sidebar order revision')
    if (not isinstance(groups, dict) or len(groups) > 500 or
            any(not isinstance(key, str) or len(key) > 4096 or
                not isinstance(ids, list) or len(ids) > 10000 or
                any(not isinstance(item, str) or len(item) > 4096 for item in ids) or
                len(set(ids)) != len(ids) for key, ids in groups.items()) or
            sum(len(ids) for ids in groups.values()) > 10000 or
            len(json.dumps(groups)) > 5_000_000):
        raise ValueError('Invalid sidebar order')
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        signature, previous = runtime.operation_receipt(
            db, 'sidebar-order:' + request_id, {'operation': 'sidebar-order', 'body': data})
        if previous is not None:
            return previous
        current = sidebar_order(db)
        if revision != current['revision']:
            raise SidebarOrderConflict('Sidebar order changed. Refreshed from the server')
        if current['groups'] is not None and data.get('migration'):
            raise SidebarOrderConflict('Sidebar order changed. Refreshed from the server')
        result = {'revision': revision + 1, 'groups': groups}
        db.execute("INSERT INTO runtime_sidebar_order(id,record) VALUES ('current',?) "
                   "ON CONFLICT(id) DO UPDATE SET record=excluded.record", (json.dumps(result),))
        from codex_sync_entities import patch
        patch(db, 'workspace', 'current', {'sidebarOrder': result})
        return runtime.save_receipt(db, 'sidebar-order:' + request_id, signature, result)


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
    from codex_project_locations import project_key
    path = project_key(runtime, data.get('path'))
    revision = data.get('expected_revision')
    if type(revision) is not int or revision < 0:
        raise ValueError("Supply the current project revision")
    name = text_field(data.get('name'), 'a name', 255) if action != 'remove_folder' else None
    with runtime.lock, runtime.db() as db:
        db.execute('BEGIN IMMEDIATE')
        row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
        project = json.loads(row[0]) if row else runtime.ensure_project(path, runtime.project_account(path, db=db), db)
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
