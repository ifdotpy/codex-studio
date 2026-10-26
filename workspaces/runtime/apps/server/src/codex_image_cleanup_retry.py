"""Retry image workspace cleanup that failed once and was never tried again.

Two failures leave an image behind:

- Deleting a chat removes its image (Runtime.delete_conversation). When the unmount times out,
  the agent keeps "Image removal after deletion failed" and the image stays on disk.
- Archiving a worker unmounts its image and keeps it for a restore
  (codex_agent_management._cleanup_image_workspace). When the unmount times out, the image stays
  mounted with the phase "archiving".

Each retry repeats only the step that failed: a removal for a deleted chat, an unmount for an
archived worker. An archived worker keeps its image, because a lead can restore it. The retries
back off and stop after MAX_ATTEMPTS.
"""
from __future__ import annotations

import json
import threading
import time
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from codex_records import AgentRecord
    from codex_runtime import Runtime

REMOVAL_FAILED = 'Image removal after deletion failed: '
ARCHIVE_FAILED = 'Image workspace archive failed: '
SCAN_SECONDS = 300
FIRST_DELAY_SECONDS = 300
MAX_ATTEMPTS = 6  # Delays of 5, 10, 20, 40 and 80 minutes between the attempts.
MAX_PER_SCAN = 4


def kind(agent: "AgentRecord") -> str | None:
    """The failed step to repeat, or None when this agent needs no retry."""
    error = agent.get('imageWorkspaceError')
    if not agent.get('deletedAt') or not agent.get('imageWorkspace') or not isinstance(error, str):
        return None
    if error.startswith(REMOVAL_FAILED) and agent.get('imageWorkspacePhase') != 'removed':
        return 'remove'
    if (error.startswith(ARCHIVE_FAILED) and agent.get('agentArchive')
            and agent.get('imageWorkspacePhase') == 'archiving'):
        return 'archive'
    return None


def due(agent: "AgentRecord", now: float) -> bool:
    retry = agent.get('imageWorkspaceCleanupRetry')
    return not retry or (retry['attempts'] < MAX_ATTEMPTS and now >= retry['nextAt'])


def tick(rt: "Runtime", now: float | None = None) -> None:
    now = time.time() if now is None else now
    if now - rt.__dict__.get('_image_cleanup_scan', 0) < SCAN_SECONDS:
        return
    rt.__dict__['_image_cleanup_scan'] = now
    worker = rt.__dict__.get('_image_cleanup_worker')
    if worker is not None and worker.is_alive():
        return
    with rt.lock, rt.read_db() as db:
        rows = db.execute(
            "SELECT record FROM runtime_agents WHERE json_type(record,'$.deletedAt') IS NOT NULL "
            "AND json_extract(record,'$.imageWorkspaceError') IS NOT NULL").fetchall()
    candidates = [agent['id'] for agent in (json.loads(row[0]) for row in rows)
                  if kind(agent) and due(agent, now)][:MAX_PER_SCAN]
    if not candidates:
        return
    worker = threading.Thread(target=_run, args=(rt, candidates), name='studio-image-cleanup', daemon=True)
    rt.__dict__['_image_cleanup_worker'] = worker
    worker.start()


def _run(rt: "Runtime", agent_ids: list[str]) -> None:
    for agent_id in agent_ids:
        if rt.closed:
            return
        try:
            retry(rt, agent_id)
        except Exception as error:
            rt.__dict__['_image_cleanup_error'] = {'at': time.time(), 'agent': agent_id,
                                                   'errorType': type(error).__name__}


def retry(rt: "Runtime", agent_id: str, now: float | None = None) -> str:
    with rt.lock, rt.read_db() as db:
        agent = rt.agent(agent_id, db)
    step = kind(agent)
    if step is None or not due(agent, time.time() if now is None else now):
        return 'skipped'
    error: Exception | None = None
    if step == 'remove':
        try:
            rt.remove_image_workspace(agent_id)
        except Exception as caught:
            error = caught
    else:
        from codex_agent_management import _cleanup_image_workspace
        result = _cleanup_image_workspace(rt, agent_id)
        if result.get('state') != 'archived':
            error = RuntimeError(str(result.get('reason') or 'the unmount failed'))
    now = time.time() if now is None else now
    with rt.lock, rt.db() as db:
        current = rt.agent(agent_id, db)
        if error is None:
            current.pop('imageWorkspaceError', None)
            current.pop('imageWorkspaceCleanupRetry', None)
            if step == 'remove':
                current['imageWorkspacePhase'] = 'removed'
            else:
                archive = current.get('agentArchive')
                if archive and archive.get('cleanupPending') and not current.get('worktreeCleanup'):
                    archive['cleanupPending'] = False
        else:
            saved = current.get('imageWorkspaceCleanupRetry')
            attempts = (saved['attempts'] if saved else 0) + 1
            current['imageWorkspaceCleanupRetry'] = {
                'attempts': attempts, 'at': now,
                'nextAt': now + FIRST_DELAY_SECONDS * 2 ** (attempts - 1),
                'error': str(error)[:500]}
        rt.put(db, 'agents', current)
    return 'done' if error is None else 'failed'
