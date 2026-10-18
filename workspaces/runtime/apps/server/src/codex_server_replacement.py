"""Rewrite live references from a lost server identity to its successor (docs/multi-server.md).

The caller owns authorization, request identity, and the write transaction. This module
changes only records that name the peer for future work. Receipts, audit rows, transcripts,
and history keep the identity that was true when they were written.
"""
from __future__ import annotations

import json
import sqlite3
import time
from typing import Any

# Move phases that route future work through the peer. Other phases are in flight or terminal:
# they keep their identity, and their own deadline settles them without a resend.
ROUTED_MOVE_PHASES = frozenset({'ready', 'active'})
# The receipt of an in-flight request to the lost identity. The request is never sent again.
REPLACED_DETAIL = ('The paired server identity was replaced before this request had a receipt. '
                   'Outcome unknown; inspect the target server. Studio does not send it again.')


class ReplacementConflict(ValueError):
    """Live state names both identities in one place; the owner must resolve it first."""


def _table(db: sqlite3.Connection, name: str) -> bool:
    return db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)).fetchone() is not None


def _swap(holder: Any, key: str, old: str, new: str) -> bool:
    if isinstance(holder, dict) and holder.get(key) == old:
        holder[key] = new
        return True
    return False


def _project(project: dict[str, Any], old: str, new: str) -> bool:
    changed = _swap(project, 'homeServerId', old, new)
    locations = project.get('locations')
    if isinstance(locations, list) and any(isinstance(row, dict) and row.get('serverId') == old for row in locations):
        if any(isinstance(row, dict) and row.get('serverId') == new for row in locations):
            raise ReplacementConflict(f"Project {project.get('id')} has a location on both servers. Remove one location first")
        for row in locations:
            _swap(row, 'serverId', old, new)
        project['locationsRevision'] = int(project.get('locationsRevision') or 0) + 1
        changed = True
    aliases = project.get('projectAliases')
    if isinstance(aliases, list) and any(isinstance(row, dict) and row.get('serverId') == old for row in aliases):
        unique: list[Any] = []
        seen: set[tuple[Any, Any]] = set()
        for row in aliases:
            if isinstance(row, dict):
                _swap(row, 'serverId', old, new)
                identity = (row.get('serverId'), row.get('projectId'))
                if identity in seen:
                    continue
                seen.add(identity)
            unique.append(row)
        project['projectAliases'] = unique
        changed = True
    return changed


def _agent(agent: dict[str, Any], old: str, new: str) -> bool:
    # movedFrom, executionArchives and request payloads are history; they stay unchanged.
    changed = False
    for field, key in (('remoteOrigin', 'home'), ('remoteAnchor', 'home'), ('remoteWorker', 'server'), ('movedTo', 'server')):
        changed = _swap(agent.get(field), key, old, new) or changed
    changed = _swap(agent, 'projectServerId', old, new) or changed
    aliases = agent.get('executionRouteAliases')
    if isinstance(aliases, list):
        for row in aliases:
            changed = _swap(row, 'server', old, new) or changed
    return changed


def _move(record: dict[str, Any], old: str, new: str) -> bool:
    if record.get('phase') not in ROUTED_MOVE_PHASES:
        return False
    changed = _swap(record, 'principal', old, new)
    changed = _swap(record, 'server', old, new) or changed
    descriptor = record.get('descriptor')
    if isinstance(descriptor, dict):
        changed = _swap(descriptor.get('link'), 'home', old, new) or changed
        changed = _swap(descriptor.get('preflight'), 'routeHome', old, new) or changed
    changed = _swap(record.get('preflight'), 'routeHome', old, new) or changed
    return changed


def replace_references(runtime: Any, db: sqlite3.Connection, old: str, new: str) -> dict[str, Any]:
    """Rewrite references in the caller's write transaction. Returns counts and the request IDs
    of in-flight operations that now need reconciliation."""
    from codex_multi_server_orchestration import encoded as _encoded
    rewritten = {'projects': 0, 'agents': 0, 'links': 0, 'moves': 0, 'commands': 0}
    if _table(db, 'runtime_projects'):
        for row in db.execute('SELECT record FROM runtime_projects WHERE instr(record, ?) > 0', (old,)).fetchall():
            project = json.loads(row[0])
            if _project(project, old, new):
                project['updated'] = time.time()
                runtime.put(db, 'projects', project)
                rewritten['projects'] += 1
    if _table(db, 'runtime_agents'):
        for row in db.execute('SELECT record FROM runtime_agents WHERE instr(record, ?) > 0', (old,)).fetchall():
            agent = json.loads(row[0])
            if _agent(agent, old, new):
                runtime.put(db, 'agents', agent)
                rewritten['agents'] += 1
    if _table(db, 'runtime_server_links'):
        for link_id, raw in db.execute('SELECT id,record FROM runtime_server_links WHERE instr(record, ?) > 0', (old,)).fetchall():
            link = json.loads(raw)
            if _swap(link, 'home', old, new) | _swap(link, 'server', old, new):
                db.execute('UPDATE runtime_server_links SET record=? WHERE id=?', (_encoded(link), link_id))
                rewritten['links'] += 1
    if _table(db, 'runtime_agent_moves'):
        for move_id, raw in db.execute('SELECT id,record FROM runtime_agent_moves WHERE instr(record, ?) > 0', (old,)).fetchall():
            move = json.loads(raw)
            if _move(move, old, new):
                db.execute('UPDATE runtime_agent_moves SET record=? WHERE id=?', (_encoded(move), move_id))
                rewritten['moves'] += 1
    if _table(db, 'runtime_server_exec'):
        rewritten['commands'] = db.execute('UPDATE runtime_server_exec SET owner=? WHERE owner=?', (new, old)).rowcount
    reconcile: list[str] = []
    if _table(db, 'runtime_server_outbox'):
        now = time.time()
        for key, body in db.execute("SELECT id,body FROM runtime_server_outbox WHERE server=? AND state!='complete' ORDER BY rowid",
                                    (old,)).fetchall():
            result = {'requestId': key, 'outcome': 'unknown', 'terminal': True, 'detail': REPLACED_DETAIL}
            db.execute("UPDATE runtime_server_outbox SET state='complete',result=?,error='server_replaced',completed_at=? WHERE id=?",
                       (_encoded(result), now, key))
            reconcile.append(key)
            envelope = json.loads(body)
            if envelope.get('action') == 'admit':
                # A slot admission has no effect to replay. The worker asks the successor again.
                row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (envelope['payload'].get('worker'),)).fetchone()
                worker = json.loads(row[0]) if row else None
                if worker and (worker.get('remoteAdmission') or {}).get('id') == key:
                    worker.pop('remoteAdmission')
                    runtime.put(db, 'agents', worker)
    changed = getattr(runtime, 'changed', None)
    if changed is not None:
        changed.set()
    return {'rewritten': rewritten, 'reconcile': reconcile}


def previous_local_identities(db: sqlite3.Connection, local: str) -> set[str]:
    """Server IDs that local state uses for this server itself, other than its current ID.

    A home-side link names this server as its home, and a project names its own server as
    its home; destination registrations keep the remote home in projectAliases."""
    found: set[str] = set()
    if _table(db, 'runtime_server_links'):
        found.update(row[0] for row in db.execute(
            "SELECT DISTINCT json_extract(record,'$.home') FROM runtime_server_links "
            "WHERE json_extract(record,'$.side')='home'") if isinstance(row[0], str))
    if _table(db, 'runtime_projects'):
        found.update(row[0] for row in db.execute(
            "SELECT DISTINCT json_extract(record,'$.homeServerId') FROM runtime_projects") if isinstance(row[0], str))
    found.discard(local)
    return found
