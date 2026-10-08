"""Lead-only worker inspection and reversible, guarded team cleanup."""
import json
import os
import stat
import subprocess
import time
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, cast

from codex_records import ImageWorkspaceCleanupResultRecord

if TYPE_CHECKING:
    import sqlite3
    from collections.abc import Callable, Mapping
    from typing import Any
    from codex_records import AgentRecord, AccountHistoryRecord, WorktreeCleanupRecord, WorktreeCleanupErrorRecord
    from codex_runtime import Runtime


def management_tools(tool: "Callable[[str, str, dict[str, Any], list[str]], dict[str, Any]]", text: dict[str, "Any"]) -> list[dict[str, "Any"]]:
    return [tool('orchestration_agent_manage',
        'Manage your own descendant workers. inspect returns archive blockers. recover checks exact unconfirmed input IDs in native history, requeues only IDs absent from an idle thread, and reconciles an existing turn. For a transferred thread with no source rollout, recover reports the missing history and does not replay inputs. '
        'archive hides an inactive worker, detaches and retains its image workspace, or removes a safe Studio Git worktree. It reports why a workspace stays. '
        'archive_finished archives finished descendants with safe workspaces and reports kept workers. '
        'For archive or archive_finished, unassign_work=true returns open assigned tasks to the unassigned ready backlog in the same transaction. '
        'Task results and decisions remain. The default keeps the assigned_work blocker. Other archive blockers remain. '
        'Retry archive with a new tool call after an image detach interruption or partial Git removal. The exact original Git link and archive ref are required. '
        'Changed or untracked files stay for inspection. A missing Git link leaves the folder for manual review. '
        'reset_tools releases an idle worker subscription after native command and receipt checks. Give a reason. '
        'Codex can end its idle session after its configured idle window (60 seconds by default); send new work after confirmed closure to start fresh tools. '
        'restore reattaches an archived image or recreates a removed Git worktree, then returns the worker paused; use orchestration_send to resume. Deleting a worker removes its image. '
        'list and list_archived are paged. '
        'maintenance_report lists old archived or deleted workspaces, bases, and Git worktrees; it does not remove them. '
        'For Linux VM workers, it includes VM disk allocation, disk free space, and RAM. '
        'park waits for a named event after the current turn. list_parked shows event waits. cancel_park wakes a worker. '
        'emit_event wakes every worker waiting for that event once; supply a stable request_id. '
        'Only the lead may archive, restore, recover, reset tools, or emit an event.',
        {'action': {'type': 'string', 'enum': ['inspect', 'list', 'recover', 'reset_tools', 'archive', 'archive_finished', 'restore', 'list_archived', 'maintenance_report', 'park', 'list_parked', 'cancel_park', 'emit_event']},
         'agent_id': text, 'reason': {'type': 'string', 'maxLength': 1000},
         'unassign_work': {'type': 'boolean'},
         'event': {'type': 'string', 'minLength': 1, 'maxLength': 120},
         'request_id': {'type': 'string', 'minLength': 1, 'maxLength': 200},
         'limit': {'type': 'integer', 'minimum': 1, 'maximum': 50}, 'cursor': text}, ['action'])]


def _authorize(rt: "Runtime", db: "sqlite3.Connection", actor_id: str, epoch: int | None, target: "AgentRecord | None" = None) -> "AgentRecord":
    actor = rt.agent(actor_id, db)
    if (not actor.get('isLead') or actor.get('deletedAt') or not actor.get('autoWake')
            or (epoch is not None and actor['epoch'] != epoch)):
        raise ValueError('Only the active orchestrator can manage its workers')
    if target is not None:
        if target.get('isLead') or target['id'] == actor_id or target['rootId'] != actor['rootId']:
            raise ValueError('You can manage only your own descendant workers')
        seen = set()
        cursor = target
        while cursor.get('parentId') != actor_id:
            if not cursor.get('parentId') or cursor['id'] in seen:
                raise ValueError('This worker is not your descendant')
            seen.add(cursor['id'])
            cursor = rt.agent(cursor['parentId'], db)  # type: ignore[arg-type]  # typed-narrowing: parent traversal checks this id
    return actor


def _brief(a: "AgentRecord") -> dict[str, "Any"]:
    return {k: a.get(k) for k in ('id', 'name', 'parentId', 'status', 'inFlight', 'threadId',
                                 'turnId', 'error', 'lastEvent', 'agentArchive', 'nativeRelease', 'parkedEvent',
                                 'imageWorkspace', 'imageWorkspaceReady', 'imageWorkspacePhase',
                                 'imageWorkspaceError', 'imageWorkspaceCreatedAt',
                                 'cleanedImageWorkspace')}


def _team_agents(rt: "Runtime", db: "sqlite3.Connection", root_id: str, *, include_deleted: bool = False) -> list["AgentRecord"]:
    scoped = getattr(rt, 'team_agents', None)  # type: Callable[..., list[AgentRecord]] | None
    if scoped:
        return scoped(db, root_id, include_deleted=include_deleted)
    # Small contract-test runtimes expose only the generic record reader.
    return [a for a in rt.records(db, 'agents') if a.get('rootId') == root_id
            and (include_deleted or not a.get('deletedAt'))]


def _cleanup_image_workspace(rt: "Runtime", agent_id: str) -> dict[str, "Any"]:
    from codex_workspace_images import archive_workspace
    with rt.lock, rt.db() as db:
        agent = rt.agent(agent_id, db)
    saved = agent.get('cleanedImageWorkspace') or {}
    retry_archive = saved.get('phase') == 'archiving'
    try:
        if not retry_archive:
            saved = {'source': agent.get('imageWorkspaceRepo'),
                     'path': agent.get('cwd'), 'mount': agent.get('imageWorkspaceMount'),
                     'createdAt': agent.get('imageWorkspaceCreatedAt'),
                     'bytes': None, 'phase': 'archiving'}
            with rt.lock, rt.db() as db:
                current = rt.agent(agent_id, db)
                current.update(imageWorkspacePhase='archiving',
                               cleanedImageWorkspace=saved)  # type: ignore[call-arg]  # typed-update
                rt.put(db, 'agents', current)
        if agent.get('environment') == 'linux':
            from codex_linux_workspaces import dispose
            removed = dispose(rt, agent_id)
        else:
            removed = archive_workspace(agent_id)
        saved = {**saved, 'phase': 'archived',
                 'freedBytes': removed.get('freedBytes', 0)}
        with rt.lock, rt.db() as db:
            current = rt.agent(agent_id, db)
            current.update(imageWorkspaceReady=False,
                           imageWorkspacePhase='archived', cleanedImageWorkspace=saved)  # type: ignore[call-arg]  # typed-update
            current['imageWorkspace'] = True
            rt.put(db, 'agents', current)
        return {'state': 'archived', 'bytes': saved.get('bytes'),
                'freedBytes': saved['freedBytes']}
    except Exception as error:
        with rt.lock, rt.db() as db:
            current = rt.agent(agent_id, db)
            current['imageWorkspaceError'] = 'Image workspace archive failed: ' + str(error)[:1200]
            rt.put(db, 'agents', current)
        return {'state': 'kept', 'bytes': 0, 'reason': str(error)[:500]}


def _completed_native_turn(a: "AgentRecord") -> bool:
    attempt = a.get('startAttempt') or {}
    return (bool(a.get('lastCompletedTurn')) and a.get('lastCompletedTurn') == attempt.get('turnId')
            and a.get('turnId') is None and not a.get('inFlight') and _finished(a))


def _missing_transferred_history(db: "sqlite3.Connection", agent: "AgentRecord", rt: "Runtime | None" = None) -> dict[str, "Any"] | None:
    """Identify empty transfer targets without retrying any saved input."""
    thread = agent.get('threadId')
    error = agent.get('error') or ''
    if not thread or not isinstance(error, str):
        return None
    expected = (f'no rollout found for thread id {thread}',
                f'invalid paginated history lineage for {thread}: missing source rollout')
    if not any(message in error for message in expected):
        if rt is None:
            return None
        from codex_account_transfer import AccountTransfers
        for row in db.execute('SELECT record FROM runtime_account_transfers'):
            transfer = json.loads(row[0])
            member = (transfer.get('members') or {}).get(agent['id']) or {}
            if (member.get('phase') != 'completed' or member.get('sourceThreadId') != thread
                    or member.get('sourceAccountKey') != agent.get('accountKey', 'default')
                    or not member.get('error') or '[Errno 2]' not in str(member['error'])
                    or member.get('result') or member.get('nativeMethod')):
                continue
            proof = AccountTransfers.source_history_missing(
                rt.accounts.home(member['sourceAccountKey']), thread, member['error'])
            if proof:
                return {'status':'history_missing', 'agent':_brief(agent),
                        'evidence':{'transferId':transfer['id'], 'sourceThreadId':thread,
                                    'targetAccountKey':transfer.get('targetAccountKey'),
                                    'sourceHistoryMissing':proof},
                        'replayed':False,
                        'next':'The source native rollout and paginated records are missing. '
                               'Recover by starting a fresh thread on the transfer target. '
                               'Saved inputs remain untouched.'}
        return None
    for entry in reversed(agent.get('accountHistory') or []):
        if entry.get('threadId') is not None or entry.get('targetThreadId') != thread:
            continue
        row = db.execute('SELECT record FROM runtime_account_transfers WHERE id=?',
                         (entry.get('transferId'),)).fetchone()
        if not row:
            continue
        transfer = json.loads(row[0])
        member = (transfer.get('members') or {}).get(agent['id']) or {}
        result = member.get('result') or {}
        if (member.get('phase') == 'completed'
                and member.get('nativeMethod') == 'thread/start'
                and member.get('sourceThreadId') is None
                and result.get('thread', {}).get('id') == thread):
            counts = {}
            for kind, status, number in db.execute(
                    'SELECT kind,status,count(*) FROM runtime_events WHERE agent=? AND epoch=? GROUP BY kind,status',
                    (agent['id'], agent['epoch'])):
                counts[kind + ':' + status] = number
            return {'status':'history_missing', 'agent':_brief(agent),
                    'evidence':{'transferId':transfer['id'], 'sourceThreadId':None,
                                'targetThreadId':thread, 'events':counts},
                    'replayed':False,
                    'next':'The source had no native thread at transfer and the target has no saved rollout. '
                           'Keep this worker failed with its Studio records intact. Ask the owner before starting a fresh task. '
                           'Do not resend its failed or pending inputs.'}
    return None


def _blockers(rt: "Runtime", db: "sqlite3.Connection", a: "AgentRecord", *, unassign_work: bool = False) -> list[dict[str, "Any"]]:
    from codex_workspace import active_task_records
    key = a['id']
    result = []
    def add(kind, ids):  # type: (str, list[str]) -> None
        if ids: result.append({'kind': kind, 'count': len(ids), 'ids': ids[:20]})
    if a.get('inFlight') or a['status'] in {'starting', 'running', 'approval', 'queued'}:
        add('active_turn', [key])
    if a.get('parkedEvent'):
        add('parked_event', [a['parkedEvent']])
    if a.get('workspaceOperation'): add('workspace_operation', [key])
    preparation = rt.preparations.get(key)
    if preparation and not preparation['future'].done(): add('thread_preparation', [key])
    children = (json.loads(row[0]) for row in db.execute(
        "SELECT record FROM runtime_agents WHERE json_extract(record,'$.parentId')=? ORDER BY rowid", (key,)))
    add('descendants', [x['id'] for x in children if not x.get('deletedAt')])
    add('input_delivery', [r[0] for r in db.execute(
        "SELECT id FROM runtime_events WHERE agent=? AND epoch=? AND status IN ('pending','reserved','dispatching','uncertain')", (key, a['epoch']))])
    add('monitors', [row[0] for row in db.execute(
        "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.agent')=? "
        "AND json_extract(record,'$.status') IN ('starting','running','approval') ORDER BY rowid", (key,))])
    tasks = active_task_records(db, ('running','starting','pending','unknown'), agent=key)
    for task in tasks:
        if (not _finished(a) or task['status'] != 'running' or task.get('kind') != 'command'
                or not str(task.get('processId', '')).isdigit() or not task.get('turnId')
                or not db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                  (key + ':' + task['turnId'],)).fetchone()):
            continue
        if a.get('environment') == 'linux':
            continue
        try:
            os.kill(int(task['processId']), 0)
        except ProcessLookupError:
            task.update(status='lost', finished=time.time(), error='Command process ended without an exit receipt. Outcome unknown.')
            rt.put(db, 'tasks', task)
        except (OSError, ValueError):
            pass
    add('background_tasks', [x['id'] for x in tasks if x['status'] in {'running','starting','pending','unknown'}])
    add('questions', [row[0] for row in db.execute(
        "SELECT id FROM runtime_requests WHERE json_extract(record,'$.agent')=? "
        "AND json_extract(record,'$.status')='pending' ORDER BY rowid", (key,))])
    if not unassign_work:
        add('assigned_work', [row[0] for row in db.execute(
            "SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=? "
            "AND (json_extract(record,'$.status') IS NULL "
            "OR json_extract(record,'$.status') NOT IN ('accepted','cancelled')) ORDER BY rowid", (key,))])
    rt.reconcile_tool_requests(db, key)
    add('tool_requests', [row[0] for row in db.execute(
        "SELECT id FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
        "AND (json_extract(record,'$.stage') IN ('queued','running') OR "
        "(json_extract(record,'$.outcome')='unknown' AND ?))",
        (key, int(not _finished(a))))])
    return result


def _git(repo: str | Path, *args: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True,
                          timeout=300)


def _worktree_entries(repo: str | Path) -> list[dict[str, str]]:
    listing = _git(repo, 'worktree', 'list', '--porcelain', '-z').stdout.decode('utf-8', 'surrogateescape')
    return [dict(line.split(' ', 1) for line in block.split('\0') if ' ' in line)
            for block in listing.split('\0\0')]


def _worktree_registration(repo: str | Path, root: str | Path) -> dict[str, str] | None:
    entries = _worktree_entries(repo)
    return next((item for item in entries if item.get('worktree')
                 and Path(item['worktree']).resolve() == Path(root).resolve()), None)


def _branch_commit(repo: str | Path, branch: str | None) -> str | None:
    if not branch:
        return None
    branch = branch.removeprefix('refs/heads/')
    if not branch or branch.startswith('-') or '\0' in branch:
        return None
    try:
        return _git(repo, 'rev-parse', '--verify', 'refs/heads/' + branch + '^{commit}').stdout.decode().strip()
    except (OSError, subprocess.SubprocessError):
        return None


def _save_archive_ref(repo: str | Path, agent_id: str, head: str) -> None:
    ref = 'refs/codex-agents/archive/' + agent_id
    try:
        previous = _git(repo, 'rev-parse', '--verify', ref + '^{commit}').stdout.decode().strip()
    except subprocess.CalledProcessError:
        previous = None
    if previous and previous != head:
        history = 'refs/codex-agents/archive-history/' + agent_id + '/' + previous
        _git(repo, 'update-ref', history, previous)
    _git(repo, 'update-ref', ref, head)


def _prune_missing_registration(repo: str | Path, root: str | Path) -> bool:
    """Prune only repository metadata after confirming the target path is absent."""
    root = Path(root)
    if root.exists() or root.is_symlink():
        return False
    _git(repo, 'worktree', 'prune', '--expire=now')
    return True


def _worker_worktree_root(agent_id: str, cwd: str | Path) -> Path | None:
    path = Path(cwd).resolve()
    roots = [p for p in (path, *path.parents)
             if p.name == agent_id and p.parent.name == 'codex-agents'
             and p.parent.parent.name == '.worktrees']
    return roots[0] if len(roots) == 1 else None


def _worktree_check(rt: "Runtime", actor_id: str, agent_id: str, epoch: int | None, *, unassign_work: bool = False) -> tuple["WorktreeCleanupRecord | None", str | None]:
    """Use one safety check for archive, bulk archive, and maintenance reports."""
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        _authorize(rt, db, actor_id, epoch, a)
        blockers = _blockers(rt, db, a, unassign_work=unassign_work)
        if blockers:
            return None, ', '.join(item['kind'] for item in blockers)
        if not a.get('worktreeReady'):
            return None, 'no registered worker worktree'
        cwd = Path(a['cwd']).resolve()
        root = _worker_worktree_root(agent_id, cwd)
        if root is None:
            return None, 'worker path is outside the Studio worktree folder'
        repo = root.parent.parent.parent
        relative = cwd.relative_to(root)
        if any(other['id'] != agent_id and other.get('worktreeReady')
               and Path(other.get('cwd', '')).resolve().is_relative_to(root)
               for other in rt.records(db, 'agents')):
            return None, 'another agent uses this worktree'
        identity = [a['epoch'], a['cwd'], a.get('deletedAt')]  # type: list[int | float | str | None]
    try:
        if Path(_git(repo, 'rev-parse', '--show-toplevel').stdout.decode().strip()).resolve() != repo:
            return None, 'repository root differs from the recorded path'
        entries = _worktree_entries(repo)
        entry = next((item for item in entries if item.get('worktree')
                      and Path(item['worktree']).resolve() == root), None)
        if any(item.get('worktree') and Path(item['worktree']).resolve() != root
               and Path(item['worktree']).resolve().is_relative_to(root) for item in entries):
            return None, 'another registered worktree is nested inside this path'
        if not root.exists() and not root.is_symlink():
            candidates = list(dict.fromkeys((
                value.removeprefix('refs/heads/') for value in
                ((entry or {}).get('branch', ''), a.get('branch', ''), 'codex-agent/' + agent_id)
                if value)))
            branch = candidates[0] if candidates else None  # type: str | None
            head = None
            for candidate in candidates:
                head = _branch_commit(repo, candidate)
                if head:
                    branch = candidate
                    break
            return {'root': str(root), 'repo': str(repo), 'relative': str(relative),
                    'branch': branch, 'head': head, 'missing': True,
                    'identity': identity}, None
        if not entry:
            return _legacy_worktree_info(a, root, repo, relative, identity), None
        if not stat.S_ISREG(_file_identity(root / '.git')[2]):
            return None, 'the remaining Git link is not a regular file'
        if _git(root, 'status', '--porcelain', '--untracked-files=normal').stdout:
            return None, 'tracked or untracked files have changes'
        head = _git(root, 'rev-parse', 'HEAD').stdout.decode().strip()
        if entry.get('HEAD') != head:
            return None, 'worktree registration HEAD differs'
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return None, 'git check failed: ' + str(error)[:180]
    return {'root': str(root), 'repo': str(repo), 'relative': str(relative),
            'branch': entry.get('branch', '').removeprefix('refs/heads/') or None,
            'head': head, 'identity': identity, 'gitFile': _file_identity(root / '.git'),
            'rootFile': _file_identity(root)}, None




def _file_identity(path: str | Path) -> list[int]:
    value = os.stat(path, follow_symlinks=False)
    return [value.st_dev, value.st_ino, value.st_mode, value.st_size,
            value.st_mtime_ns, value.st_ctime_ns]


def _legacy_worktree_info(agent: "AgentRecord", root: Path, repo: Path, relative: Path, identity: list[int | float | str | None]) -> "WorktreeCleanupRecord":
    """Accept only an archived worktree with its original, exact Git link."""
    archive = agent.get('agentArchive') or {}
    branch = 'codex-agent/' + agent['id']
    gitdir = repo / '.git' / 'worktrees' / agent['id']
    gitfile = root / '.git'
    if (archive.get('at') != agent.get('deletedAt') or archive.get('epoch') != agent['epoch']
            or not archive.get('at') or agent.get('branch') != branch
            or root.is_symlink() or not root.is_dir() or gitfile.is_symlink()
            or gitdir.exists() or gitdir.is_symlink() or not (repo / '.git').is_dir()):
        raise ValueError('worktree registration is missing without exact archive evidence')
    fd = os.open(gitfile, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    try:
        value = os.fstat(fd)
        if not stat.S_ISREG(value.st_mode):
            raise ValueError('the remaining Git link is not a regular file')
        git_identity = [value.st_dev, value.st_ino, value.st_mode, value.st_size,
                        value.st_mtime_ns, value.st_ctime_ns]
        link = os.read(fd, 4097)
        if _file_identity(gitfile) != git_identity:
            raise ValueError('the remaining Git link changed during its identity check')
    finally:
        os.close(fd)
    if (len(link) > 4096 or not link.startswith(b'gitdir: ') or not link.endswith(b'\n')
            or b'\n' in link[:-1] or not Path(os.fsdecode(link[8:-1])).is_absolute()
            or Path(os.fsdecode(link[8:-1])).resolve() != gitdir):
        raise ValueError('the remaining Git link differs from the saved worktree identity')
    head = _git(repo, 'rev-parse', '--verify', 'refs/codex-agents/archive/' + agent['id']).stdout.decode().strip()
    if not head or _branch_commit(repo, branch) != head:
        raise ValueError('archive ref and worker branch differ; keep the remaining folder')
    saved = agent.get('worktreeCleanup')
    if saved:
        expected = {'root': str(root), 'repo': str(repo), 'relative': str(relative),
                    'branch': branch, 'head': head, 'identity': identity}
        if (any(saved.get(key) != value for key, value in expected.items())
                or (saved.get('gitFile') and saved['gitFile'] != git_identity)
                or (saved.get('rootFile') and saved['rootFile'][:3] != _file_identity(root)[:3])):
            raise ValueError('the remaining worktree differs from its saved cleanup identity')

    return {'root': str(root), 'repo': str(repo), 'relative': str(relative),
            'branch': branch, 'head': head, 'identity': identity,
            'gitFile': git_identity, 'rootFile': _file_identity(root),
            'registrationRepair': {'gitdir': str(gitdir), 'gitFile': git_identity,
                                   'rootFile': _file_identity(root)}}


def _repair_worktree_registration(info: "Mapping[str, Any]") -> None:
    """Restore Git metadata only. Do not overwrite remaining worktree files."""
    repair = info['registrationRepair']
    root, repo, gitdir = Path(info['root']), Path(info['repo']), Path(repair['gitdir'])
    if (_file_identity(root) != repair['rootFile'] or _file_identity(root / '.git') != repair['gitFile']
            or gitdir != repo / '.git' / 'worktrees' / root.name or gitdir.exists() or gitdir.is_symlink()
            or _worktree_registration(repo, root) is not None
            or _branch_commit(repo, info['branch']) != info['head']
            or _git(repo, 'rev-parse', '--verify', 'refs/codex-agents/archive/' + root.name).stdout.decode().strip() != info['head']):
        raise ValueError('worktree identity changed before Git registration repair')
    if any(item.get('branch') == 'refs/heads/' + info['branch'] for item in _worktree_entries(repo)):
        raise ValueError('another registered worktree uses the saved branch')
    temporary = Path(tempfile.mkdtemp(prefix='.studio-archive-repair-', dir=root.parent))
    stage = None
    try:
        _git(repo, 'worktree', 'add', '--detach', '--no-checkout', str(temporary), info['head'])
        stage = Path((temporary / '.git').read_text().strip().removeprefix('gitdir: '))
        if stage.parent != gitdir.parent or stage == gitdir:
            raise ValueError('temporary Git registration differs from the repair scope')
        _git(temporary, 'symbolic-ref', 'HEAD', 'refs/heads/' + info['branch'])
        _git(temporary, 'read-tree', info['head'])
        (stage / 'gitdir').write_text(str(root / '.git') + '\n')
        # mkdir claims the absent registration without replacing an existing path.
        gitdir.mkdir()
        for child in stage.iterdir():
            child.rename(gitdir / child.name)
        stage.rmdir()
    finally:
        if stage is not None and not stage.exists():
            (temporary / '.git').unlink(missing_ok=True)
        if not any(temporary.iterdir()):
            temporary.rmdir()
    # Deleted tracked files are consistent with the failed removal. All other
    # changes must remain for inspection. Git never writes their contents here.
    changes = _git(root, 'status', '--porcelain', '-z', '--untracked-files=all').stdout
    if any(entry and entry[:3] != b' D ' for entry in changes.split(b'\0')):
        raise ValueError('repaired worktree has modified or untracked files; keep it for inspection')


def _worktree_removal_check(info: "Mapping[str, Any]") -> None:
    """Verify one exact clean snapshot without a Runtime lock or writer."""
    root, repo = info['root'], info['repo']
    if (_file_identity(Path(root) / '.git') != info['gitFile']
            or _file_identity(root)[:3] != info['rootFile'][:3]):
        raise ValueError('the original Git link or worktree root changed')
    changes = _git(root, 'status', '--porcelain', '-z', '--untracked-files=all').stdout
    if info.get('registrationRepair'):
        dirty = any(entry and entry[:3] != b' D ' for entry in changes.split(b'\0'))
    else:
        dirty = bool(changes)
    if dirty:
        raise ValueError('files changed after the safety check')
    if _git(root, 'rev-parse', 'HEAD').stdout.decode().strip() != info['head']:
        raise ValueError('HEAD changed after the safety check')
    entries = _worktree_entries(repo)
    if not any(item.get('worktree') and Path(item['worktree']).resolve() == Path(root)
               and item.get('HEAD') == info['head']
               and (item.get('branch', '').removeprefix('refs/heads/') or None) == info['branch']
               for item in entries):
        raise ValueError('worktree registration changed after the safety check')
    if any(item.get('worktree') and Path(item['worktree']).resolve() != Path(root)
           and Path(item['worktree']).resolve().is_relative_to(Path(root)) for item in entries):
        raise ValueError('another registered worktree appeared inside this path')
    if (_file_identity(Path(root) / '.git') != info['gitFile']
            or _file_identity(root)[:3] != info['rootFile'][:3]):
        raise ValueError('the original Git link or worktree root changed')


def _cleanup_worktree(rt: "Runtime", actor_id: str, agent_id: str, epoch: int | None, checked: tuple[dict[str, "Any"] | None, str | None] | None = None) -> dict[str, "Any"]:
    with rt.lock, rt.db() as db:
        current = rt.agent(agent_id, db)
        _authorize(rt, db, actor_id, epoch, current)
        saved = current.get('worktreeCleanup')
        if not current.get('worktreeReady') and current.get('cleanedWorktree'):
            cleaned = current['cleanedWorktree']
            if cleaned.get('missing'):
                return {'state': 'missing', 'reason': cleaned.get('note'), 'bytes': 0}
            return {'state': 'removed', 'bytes': cleaned.get('bytes')}
        if not current.get('worktreeReady'):
            return {'state': 'none', 'bytes': 0}
    if saved and not Path(saved['root']).exists():
        try:
            try:
                ref = _git(saved['repo'], 'rev-parse', '--verify',
                           'refs/codex-agents/archive/' + agent_id).stdout.decode().strip()
            except subprocess.CalledProcessError:
                ref = None
            missing_recovery = bool(saved.get('missing'))
            if (missing_recovery or (_worktree_registration(saved['repo'], saved['root']) is None
                                     and ref == saved['head'])):
                if saved.get('missing') and saved.get('head') and ref != saved['head']:
                    _save_archive_ref(saved['repo'], agent_id, saved['head'])  # type: ignore[arg-type]  # typed-narrowing: saved head was validated above
                if saved.get('missing'):
                    _prune_missing_registration(saved['repo'], saved['root'])
                with rt.lock, rt.db() as db:
                    a = rt.agent(agent_id, db)
                    _authorize(rt, db, actor_id, epoch, a)
                    if (a.get('worktreeCleanup') != saved
                            or [a['epoch'], a['cwd'], a.get('deletedAt')] != saved.get('identity')):
                        raise ValueError('Worker changed during cleanup recovery')
                    a.update(cwd=str(Path(saved['repo']) / saved['relative']), worktreeReady=False)  # type: ignore[call-arg]  # typed-update
                    a.pop('worktreeCleanup', None)
                    a['cleanedWorktree'] = ({**saved, 'bytes': 0,
                                             'note': ('worktree folder missing; branch saved to archive ref'
                                                      if saved.get('head') else 'worktree folder missing; nothing to save')}
                                            if saved.get('missing') else
                                            saved)
                    rt.put(db, 'agents', a)
                if saved.get('missing'):
                    reason = ('worktree folder missing; branch saved to archive ref'
                              if saved.get('head') else 'worktree folder missing; nothing to save')  # type: str | None
                    return {'state': 'missing', 'reason': reason, 'bytes': 0}
                return {'state': 'removed', 'bytes': saved.get('bytes')}
        except (OSError, subprocess.SubprocessError):
            pass
        return {'state': 'kept', 'reason': 'worktree removal outcome is unknown; inspect its path and ref', 'bytes': 0}
    info: Mapping[str, Any]
    registered = bool(saved and saved.get('registrationRepair')
                      and _worktree_registration(saved['repo'], saved['root']) is not None)
    info, reason = (
        ({key: value for key, value in saved.items() if key not in {'bytes', 'error'}}, None)  # type: ignore[assignment, union-attr]  # typed-narrowing: absent info has a reason
        if registered else checked or _worktree_check(rt, actor_id, agent_id, epoch))
    if reason:
        return {'state': 'kept', 'reason': reason, 'bytes': 0}
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        _authorize(rt, db, actor_id, epoch, a)
        if not a.get('agentArchive') or [a['epoch'], a['cwd'], a.get('deletedAt')] != info['identity']:
            return {'state': 'kept', 'reason': 'worker changed after the safety check', 'bytes': 0}
        blockers = _blockers(rt, db, a)
        if blockers:
            return {'state': 'kept', 'reason': ', '.join(b['kind'] for b in blockers), 'bytes': 0}
        a['worktreeCleanup'] = info  # type: ignore[typeddict-item]  # typed-narrowing: validated mapping contains persisted identity
        rt.put(db, 'agents', a)
    root, repo = info['root'], info['repo']
    if info.get('missing'):
        reason = 'worktree folder missing; nothing to save'
        try:
            if info.get('head'):
                _save_archive_ref(repo, agent_id, info['head'])
                reason = 'worktree folder missing; branch saved to archive ref'
            _prune_missing_registration(repo, root)
        except (OSError, subprocess.SubprocessError) as error:
            reason = ('worktree folder missing; branch exists but archive ref or prune failed: '
                      + str(error)[:120]) if info.get('head') else (
                          'worktree folder missing; nothing to save; prune failed: ' + str(error)[:120])
        with rt.lock, rt.db() as db:
            a = rt.agent(agent_id, db)
            if not a.get('worktreeCleanup') or any(a['worktreeCleanup'].get(k) != info[k] for k in info):
                raise ValueError('Worker changed while recording its missing worktree')
            a.update(cwd=str(Path(repo) / info['relative']), worktreeReady=False)  # type: ignore[call-arg]  # typed-update
            a['cleanedWorktree'] = {**a['worktreeCleanup'], 'bytes': 0, 'missing': True,
                                    'note': reason}
            a.pop('worktreeCleanup', None)
            rt.put(db, 'agents', a)
        return {'state': 'missing', 'reason': reason, 'bytes': 0}
    try:
        if info.get('registrationRepair') and _worktree_registration(repo, root) is None:
            _repair_worktree_registration(info)
        with rt.lock, rt.db() as db:
            a = rt.agent(agent_id, db)
            _authorize(rt, db, actor_id, epoch, a)
            if (not a.get('worktreeCleanup')
                    or any(a['worktreeCleanup'].get(k) != info[k] for k in info)
                    or [a['epoch'], a['cwd'], a.get('deletedAt')] != info['identity']
                    or _blockers(rt, db, a)):
                raise ValueError('worker changed before worktree removal')
        _worktree_removal_check(info)
        _save_archive_ref(repo, agent_id, info['head'])
        _git(repo, 'worktree', 'remove', '--force', root)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        failure = {'message': str(error)[:500], 'at': time.time(), 'by': actor_id}  # type: WorktreeCleanupErrorRecord
        stderr = getattr(error, 'stderr', None)
        if isinstance(stderr, bytes):
            stderr = stderr[:4096].decode('utf-8', 'replace')
        if isinstance(stderr, str) and stderr.strip():
            failure['stderr'] = stderr[:4096]
        if isinstance(getattr(error, 'errno', None), int):
            failure['errno'] = error.errno  # type: ignore[union-attr, typeddict-item]  # typed-narrowing: runtime guard confirms integer errno
        if isinstance(getattr(error, 'returncode', None), int):
            failure['returncode'] = error.returncode  # type: ignore[union-attr]  # typed-narrowing: runtime guard confirms integer returncode
        with rt.lock, rt.db() as db:
            a = rt.agent(agent_id, db)
            if (a.get('worktreeCleanup')
                    and all(a['worktreeCleanup'].get(key) == info[key] for key in info)):
                a['worktreeCleanup']['error'] = failure
                rt.put(db, 'agents', a)
        summary = failure.get('stderr', failure['message']).strip()
        return {'state': 'kept', 'reason': 'worktree removal failed: ' + summary[:180], 'bytes': 0}
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        if not a.get('worktreeCleanup') or any(a['worktreeCleanup'].get(k) != info[k] for k in info):
            raise ValueError('Worker changed after worktree removal; inspect its archive')
        a.update(cwd=str(Path(repo) / info['relative']), worktreeReady=False)  # type: ignore[call-arg]  # typed-update
        a['cleanedWorktree'] = a['worktreeCleanup']
        a.pop('worktreeCleanup', None)
        rt.put(db, 'agents', a)
    return {'state': 'removed', 'bytes': None}


def _finished(a: "AgentRecord") -> bool:
    return a['status'] in {'completed', 'failed', 'interrupted'} or (a['status'] == 'paused' and not a.get('autoWake'))


def parked_after_turn(a: "AgentRecord") -> None:
    """Apply a park requested during a turn after its result reaches the parent."""
    if not a.get('parkedEvent'):
        return
    if a.get('parkAfterTurn'):
        a.pop('parkAfterTurn', None)
        a['autoWake'] = False
    if not a.get('autoWake'):
        a['status'] = 'parked'


def _park_actor(rt: "Runtime", db: "sqlite3.Connection", actor_id: str, epoch: int | None) -> "AgentRecord":
    actor = rt.agent(actor_id, db)
    if (actor.get('deletedAt') or not actor.get('autoWake')
            or (epoch is not None and actor['epoch'] != epoch)):
        raise ValueError('The caller is stopped')
    return actor


def _park_target(rt: "Runtime", db: "sqlite3.Connection", actor: "AgentRecord", target: "AgentRecord") -> None:
    if target.get('deletedAt') or target.get('agentArchive') or target.get('isLead'):
        raise ValueError('Choose a live worker')
    if target['rootId'] != actor['rootId']:
        raise ValueError('This worker belongs to another team')
    if target['id'] == actor['id']:
        return
    seen = set()
    current = target
    while current.get('parentId') != actor['id']:
        if not current.get('parentId') or current['id'] in seen:
            raise ValueError('This worker is not your descendant')
        seen.add(current['id'])
        current = rt.agent(current['parentId'], db)  # type: ignore[arg-type]  # typed-narrowing: parent traversal checks this id


def _event_name(value: object) -> str:
    if (not isinstance(value, str) or not 1 <= len(value) <= 120
            or value != value.strip() or any(ord(char) < 33 or ord(char) > 126 for char in value)):
        raise ValueError('Use an event name with 1 to 120 visible ASCII characters')
    return value


def _park_action(rt: "Runtime", actor_id: str, args: dict[str, "Any"], epoch: int | None) -> dict[str, "Any"]:
    action = args['action']
    with rt.lock, rt.db() as db:
        actor = _park_actor(rt, db, actor_id, epoch)
        if action == 'list_parked':
            rows = [_brief(a) for a in _team_agents(rt, db, actor['rootId'])
                    if a.get('parkedEvent')
                    and not a.get('deletedAt')]
            if not actor.get('isLead'):
                visible = []
                for row in rows:
                    target = rt.agent(row['id'], db)
                    try:
                        _park_target(rt, db, actor, target)
                    except ValueError:
                        continue
                    visible.append(row)
                rows = visible
            return rt.model_page(sorted(rows, key=lambda a: a['id']), args,
                                 [actor['rootId'], actor_id, 'parked_workers'])
        if action == 'emit_event':
            if not actor.get('isLead'):
                raise ValueError('Only the lead can emit an event')
            name = _event_name(args.get('event'))
            request_id = args.get('request_id')
            if not isinstance(request_id, str) or not 1 <= len(request_id) <= 200:
                raise ValueError('Supply a stable request_id')
            request_key = 'park-request:' + actor['rootId'] + ':' + request_id
            signature, previous = rt.operation_receipt(
                db, request_key, {'action': action, 'event': name})
            if previous is not None:
                return previous
            event_key = 'park-event:' + actor['rootId'] + ':' + name
            event_signature, fired = rt.operation_receipt(
                db, event_key, {'action': action, 'event': name})
            if fired is not None:
                return rt.save_receipt(db, request_key, signature, {**fired, 'replayed': True})
            woken = []
            for target in _team_agents(rt, db, actor['rootId']):
                park = target.get('parkReceipt') or {}
                if (target['rootId'] != actor['rootId'] or target.get('deletedAt')
                        or target.get('parkedEvent') != name
                        or park.get('event') != name
                        or park.get('epoch') != target.get('epoch')
                        or park.get('sequence') != target.get('parkSequence')):
                    continue
                target.pop('parkedEvent', None)
                target.pop('parkAfterTurn', None)
                target.pop('parkReceipt', None)
                target['autoWake'] = True
                if target['status'] == 'parked':
                    target['status'] = 'queued'
                rt.put(db, 'agents', target)
                rt.enqueue(db, target, 'event_wake',
                           'Event ' + name + ' occurred. Continue your assigned work.',
                           'park-wake:' + actor['rootId'] + ':' + name + ':' + target['id'])
                woken.append(target['id'])
            result = {'event': name, 'woken': woken, 'count': len(woken)}
            rt.save_receipt(db, event_key, event_signature, result)
            return rt.save_receipt(db, request_key, signature, result)
        target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
        _park_target(rt, db, actor, target)
        if action == 'park':
            name = _event_name(args.get('event'))
            _, fired = rt.operation_receipt(
                db, 'park-event:' + actor['rootId'] + ':' + name,
                {'action': 'emit_event', 'event': name})
            if fired is not None:
                raise ValueError('This event already occurred')
            if target.get('parkedEvent'):
                if target['parkedEvent'] != name:
                    raise ValueError('Cancel the current event wait first')
                return {'status': target['status'], 'agent': _brief(target), 'replayed': True}
            pending = db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND epoch=? "
                                 "AND status IN ('pending','reserved','dispatching','uncertain') LIMIT 1",
                                 (target['id'], target['epoch'])).fetchone()
            if pending:
                raise ValueError('Deliver or resolve pending input before parking')
            if target.get('inFlight'):
                target['parkAfterTurn'] = True
            else:
                target.update(autoWake=False, status='parked')  # type: ignore[call-arg]  # typed-update
            target['parkSequence'] = target.get('parkSequence', 0) + 1
            target['parkedEvent'] = name
            target['parkReceipt'] = {
                'event': name, 'epoch': target['epoch'],
                'sequence': target['parkSequence'],
            }
            rt.put(db, 'agents', target)
            return {'status': target['status'], 'agent': _brief(target)}
        if not target.get('parkedEvent'):
            return {'status': target['status'], 'agent': _brief(target), 'replayed': True}
        old = target.pop('parkedEvent')
        target.pop('parkAfterTurn', None)
        target.pop('parkReceipt', None)
        target['autoWake'] = True
        if target['status'] == 'parked':
            target['status'] = 'queued'
        rt.put(db, 'agents', target)
        rt.enqueue(db, target, 'event_wake', 'The wait for event ' + old + ' was cancelled. Continue your assigned work.',
                   'park-cancel:' + target['id'] + ':' + str(target['parkSequence']))
        return {'status': target['status'], 'agent': _brief(target)}


def reviewer_result_delivered(rt: "Runtime", db: "sqlite3.Connection", parent_id: str, event_id: str) -> None:
    """Queue cleanup only after native delivery confirms the child result."""
    if not event_id.startswith('child:'):
        return
    event = db.execute("SELECT kind,text,status FROM runtime_events WHERE id=? AND agent=?",
                       (event_id, parent_id)).fetchone()
    if not event or event['kind'] != 'child_result' or event['status'] != 'delivered':
        return
    try:
        child_id = json.loads(event['text'])['agent_id']
        child = rt.agent(child_id, db)
    except (KeyError, TypeError, ValueError):
        return
    if (child.get('role') != 'reviewer' or child.get('parentId') != parent_id
            or child.get('agentArchive') or child.get('reviewArchiveScheduled')
            or child.get('status') != 'completed'):
        return
    child['reviewArchiveScheduled'] = event_id
    rt.put(db, 'agents', child)
    rt.delivery_executor().submit(_archive_reviewer, rt, child['rootId'], parent_id, child_id, event_id)


def _archive_reviewer(rt: "Runtime", lead_id: str, parent_id: str, child_id: str, event_id: str) -> None:
    delay = 1
    while not rt.closed:
        with rt.lock, rt.db() as db:
            event = db.execute('SELECT status FROM runtime_events WHERE id=? AND agent=?',
                               (event_id, parent_id)).fetchone()
            child = rt.agent(child_id, db)
            if (not event or event['status'] != 'delivered'
                    or child.get('reviewArchiveScheduled') != event_id):
                return
        try:
            outcome = manage_agent(rt, lead_id, {'action': 'archive', 'agent_id': child_id,
                                                 'reason': 'Reviewer result delivered to parent'})
            blockers = outcome.get('blockers') or []
            error = None if outcome['status'] == 'archived' else str(blockers or outcome.get('status'))[:180]
            retryable = bool(blockers)
        except Exception as caught:
            outcome = {}
            error = str(caught)[:180]
            retryable = False
        if outcome.get('status') == 'archived':
            return
        with rt.lock, rt.db() as db:
            child = rt.agent(child_id, db)
            if child.get('reviewArchiveScheduled') != event_id:
                return
            child['reviewArchiveError'] = error
            if not retryable:
                child.pop('reviewArchiveScheduled', None)
            else:
                attempts = child.get('reviewArchiveAttempts', 0) + 1
                child['reviewArchiveAttempts'] = attempts
                child['reviewArchiveNextAt'] = time.time() + delay
            rt.put(db, 'agents', child)
        if not retryable:
            return
        rt.changed.wait(delay)
        rt.changed.clear()
        delay = min(60, delay * 2)


def _archive_finished(rt: "Runtime", actor_id: str, epoch: int | None, *, unassign_work: bool = False) -> dict[str, "Any"]:
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        agents = [a for a in _team_agents(rt, db, actor['rootId'])
                  if a['id'] != actor_id and not a.get('deletedAt') and _finished(a)]
        by_id = {a['id']: a for a in agents}
        def depth(a):  # type: ("AgentRecord") -> int
            count = 0
            while a.get('parentId') in by_id:
                count += 1
                a = by_id[a['parentId']]  # type: ignore[index]  # typed-narrowing: loop guard confirms parent exists
            return count
        agents.sort(key=lambda a: (-depth(a), a['id']))
    result = {'archived': 0, 'freedBytes': 0, 'unknownBytes': 0, 'kept': [], 'unassignedWork': []}  # type: dict[str, Any]
    for a in agents:
        checked = (_worktree_check(rt, actor_id, a['id'], epoch, unassign_work=unassign_work)
                   if a.get('worktreeReady') else (None, None))
        if checked[1]:
            result['kept'].append({'id': a['id'], 'reason': checked[1]})
            continue
        try:
            archived = manage_agent(rt, actor_id, {'action': 'archive', 'agent_id': a['id'],
                                                    'reason': 'Finished worker cleanup',
                                                    'unassign_work': unassign_work}, epoch)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            result['kept'].append({'id': a['id'], 'reason': str(error)[:180]})
            continue
        if archived['status'] == 'archived':
            result['archived'] += 1
            result['unassignedWork'].extend(archived.get('unassignedWork', []))
            cleanup = archived.get('workspace') or archived.get('worktree') or {'state': 'none', 'bytes': 0}
            if cleanup['state'] in {'removed', 'conflict'}:
                if cleanup['bytes'] is None:
                    result['unknownBytes'] += 1
                else:
                    result['freedBytes'] += cleanup['bytes']
                if cleanup['state'] == 'conflict':
                    result.setdefault('notes', []).append({'id': a['id'], 'reason': cleanup['reason']})
            elif cleanup['state'] == 'missing':
                result.setdefault('notes', []).append({'id': a['id'], 'reason': cleanup['reason']})
            elif cleanup['state'] not in {'none', 'archived'}:
                result['kept'].append({'id': a['id'], 'reason': cleanup['reason']})
        else:
            result['kept'].append({'id': a['id'], 'reason': ', '.join(b['kind'] for b in archived['blockers'])})
    return result


def worktree_maintenance_report(rt: "Runtime", actor_id: str, epoch: int | None = None) -> dict[str, "Any"]:
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        team = [a for a in _team_agents(rt, db, actor['rootId'], include_deleted=True)
                if a['id'] != actor_id]
        agents = [a for a in team if a.get('worktreeReady')]
    report = []
    for agent in agents:
        key = agent['id']
        root = _worker_worktree_root(key, agent.get('cwd', ''))
        folder_missing = root is not None and not root.exists() and not root.is_symlink()
        if not agent.get('deletedAt') and not folder_missing:
            continue
        info, reason = _worktree_check(rt, actor_id, key, epoch)
        report.append({'id': key, 'reason': reason or
                       ('worktree folder missing' if info and info.get('missing') else 'removable'),
                       'folderMissing': folder_missing})
    try:
        from codex_workspace_images import base_status, list_workspaces
    except ImportError:
        return {'worktrees': report, 'workspaces': [], 'bases': []}
    workspace_rows = []
    workspaces = list_workspaces()
    if isinstance(workspaces, dict):
        workspaces = workspaces.get('workspaces', [])
    team_by_id = {a['id']: a for a in team}
    for workspace in workspaces:
        agent_id = workspace.get('agentId') or workspace.get('agent_id') or workspace.get('id')
        if agent_id and agent_id in team_by_id:
            agent = team_by_id[agent_id]
            if agent.get('deletedAt') or not agent.get('imageWorkspaceReady'):
                workspace_rows.append({'id': agent_id, 'state': workspace.get('state'),
                                       'path': workspace.get('mount'), 'reason': 'archived or stale'})
    repos = {a.get('imageWorkspaceRepo') for a in team if a.get('imageWorkspaceRepo') and a.get('environment') != 'linux'}
    if actor.get('imageWorkspaceBaseRepo'):
        repos.add(actor['imageWorkspaceBaseRepo'])
    bases = []
    for repo in sorted(repos):  # type: ignore[type-var]  # typed-narrowing: truthy repository values are paths
        status = base_status(repo)
        bases.append({'repo': repo, **status})
    result = {'worktrees': report, 'workspaces': workspace_rows, 'bases': bases}
    if any(agent.get('environment') == 'linux' for agent in team):
        try:
            from codex_linux_workspaces import resources
            result['linuxVM'] = resources(rt)
        except Exception as error:
            result['linuxVM'] = {'state': 'unavailable', 'errorType': type(error).__name__}
    return result


def _restore_worktree(info: "WorktreeCleanupRecord") -> tuple[str | None, str | None]:
    repo = Path(info['repo'])
    root = Path(info['root'])
    branch = info['branch']
    ref = 'refs/codex-agents/archive/' + root.name
    if root != repo / '.worktrees' / 'codex-agents' / root.name:
        return 'saved worktree path differs from its identity', None
    try:
        head = _git(repo, 'rev-parse', '--verify', ref).stdout.decode().strip()
        if head != info['head']:
            return 'the archive ref changed', None
        if root.exists():
            entry = _worktree_registration(repo, root)
            if not entry or _git(root, 'rev-parse', 'HEAD').stdout.decode().strip() != head:
                return 'the saved worktree path already exists with different content', None
            return None, entry.get('branch', '').removeprefix('refs/heads/') or None
        if branch:
            existing = subprocess.run(['git', '-C', str(repo), 'rev-parse', '--verify',
                                       'refs/heads/' + branch], capture_output=True, timeout=300)
            if existing.returncode == 0 and existing.stdout.decode().strip() == head:
                try:
                    _git(repo, 'worktree', 'add', str(root), branch)
                    return None, branch
                except subprocess.CalledProcessError:
                    if root.exists():
                        raise
            elif existing.returncode != 0:
                _git(repo, 'worktree', 'add', '-b', branch, str(root), ref)
                return None, branch
        _git(repo, 'worktree', 'add', '--detach', str(root), ref)
    except (OSError, subprocess.SubprocessError) as error:
        return 'worktree restore failed: ' + str(error)[:180], None
    return None, None


def _archive_record(rt: "Runtime", db: "sqlite3.Connection", target: "AgentRecord", actor_id: str, reason: str, *, cleanup_pending: bool, unassign_work: bool = False) -> dict[str, "Any"]:
    if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
        raise ValueError('Give an archive reason with 1 to 1000 characters')
    unknown = [row[0] for row in db.execute(
        "SELECT id FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
        "AND json_extract(record,'$.outcome')='unknown' ORDER BY id", (target['id'],))]
    at = time.time()
    unassigned = []
    if unassign_work:
        works = [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=? "
            "AND (json_extract(record,'$.status') IS NULL "
            "OR json_extract(record,'$.status') NOT IN ('accepted','cancelled')) ORDER BY rowid",
            (target['id'],))]
        if any(work.get('rootId') != target['rootId'] for work in works):
            raise ValueError('An assigned task belongs to another team')
        for work in works:
            work.setdefault('releases', []).append({'agent': target['id'], 'reason': reason.strip(),
                                                    'created': at, 'by': actor_id})
            work.update(owner=None, status='ready', version=work.get('version', 0) + 1, updated=at)
            rt.put(db, 'work', work)
            unassigned.append(work['id'])
    target.update(deletedAt=at, autoWake=False, status='paused', epoch=target['epoch'] + 1)  # type: ignore[call-arg]  # typed-update
    target['agentArchive'] = {'at': at, 'by': actor_id, 'reason': reason.strip(),
                              'epoch': target['epoch'], 'cleanupPending': cleanup_pending,
                              'unknownToolRequests': unknown, 'unassignedWork': unassigned}
    rt.put(db, 'agents', target)
    # The task releases and archive share this transaction. Other workers' claims
    # stay with the scheduler; archive must not postpone its global sweep.
    return _brief(target)


def manage_agent(rt: "Runtime", actor_id: str, args: dict[str, "Any"], epoch: int | None = None) -> dict[str, "Any"]:
    action = args.get('action')
    if action not in {'inspect','list','recover','reset_tools','archive','archive_finished','restore','list_archived','maintenance_report',
                      'park','list_parked','cancel_park','emit_event'}:
        raise ValueError('Unknown agent management action')
    unassign_work = args.get('unassign_work', False)
    if type(unassign_work) is not bool:
        raise ValueError('unassign_work must be a boolean')
    if unassign_work and action not in {'archive', 'archive_finished'}:
        raise ValueError('unassign_work applies only to archive or archive_finished')
    if action in {'park', 'list_parked', 'cancel_park', 'emit_event'}:
        return _park_action(rt, actor_id, args, epoch)
    if action == 'archive_finished':
        return _archive_finished(rt, actor_id, epoch, unassign_work=unassign_work)
    if action == 'maintenance_report':
        return worktree_maintenance_report(rt, actor_id, epoch)
    if action == 'reset_tools':
        reason = args.get('reason')
        if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
            raise ValueError('Give a tool reset reason with 1 to 1000 characters')
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
            _authorize(rt, db, actor_id, epoch, target)
            if target.get('deletedAt') or target.get('agentArchive'):
                raise ValueError('Restore this worker before tool reset')
        from codex_native_release import release_agent
        result = release_agent(rt, target['id'], reason=reason.strip(), actor_id=actor_id,
                               actor_epoch=epoch)
        return {**result, 'agent': _brief(rt.agent(target['id'])),
                'loss': ('After native closure, open code-mode cells and their JavaScript state end. '
                         'MCP server processes and their memory end. Codex terminates any unified-exec '
                         'process missed by the native background-terminal check; its result is unknown. '
                         'The transcript and Studio receipts remain. No input is replayed.'),
                'next': ('Wait for native thread closure before sending work. The configured native '
                         'idle window defaults to 60 seconds after unsubscribe and inactivity.')}
    observed = None
    if action == 'archive':
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
            _authorize(rt, db, actor_id, epoch, target)
            if not target.get('deletedAt'):
                blockers = _blockers(rt, db, target, unassign_work=unassign_work)
                if blockers: return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
                if (not target.get('agentArchive') and not target.get('threadId')
                        and not target.get('imageWorkspaceReady')
                        and not target.get('worktreeReady') and not target.get('worktreeCleanup')
                        and not target.get('cleanedWorktree')):
                    archived_agent = _archive_record(rt, db, target, actor_id, args.get('reason', ''),
                                                     cleanup_pending=False, unassign_work=unassign_work)
                    return {'status': 'archived', 'agent': archived_agent,
                            'worktree': {'state': 'none', 'bytes': 0},
                            'unassignedWork': archived_agent['agentArchive']['unassignedWork']}
                observed = (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                connection = rt.agent_connection(target)
                server = rt.server_for(observed[2], connection)
        if observed and observed[1]:
            try:
                if server is None: raise ValueError('Owning account is offline')
                native = server.call('thread/read', {'threadId': observed[1], 'includeTurns': False}, timeout=5)['thread']
            except Exception:
                # A durable completion of the last submitted turn proves that
                # Studio has no native turn to wait for, even if the account is offline.
                if not _completed_native_turn(target):
                    return {'status': 'blocked', 'agent': _brief(target),
                            'blockers': [{'kind': 'native_state_unconfirmed', 'count': 1, 'ids': [target['id']]}]}
            else:
                if (not isinstance(native, dict) or native.get('id') != observed[1]
                        or not isinstance(native.get('status'), dict)
                        or native['status'].get('type') not in {'idle', 'notLoaded', 'systemError'}):
                    return {'status': 'blocked', 'agent': _brief(target),
                            'blockers': [{'kind': 'native_state_unconfirmed', 'count': 1, 'ids': [target['id']]}]}
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        if action == 'list':
            agents = [a for a in _team_agents(rt, db, actor['rootId'])
                      if a['id'] != actor_id
                      and not a.get('deletedAt')]
            rows = sorted((_brief(a) for a in agents), key=lambda x: x['id'])
            page = rt.model_page(rows, args, [actor['rootId'], 'workers'])
            return page
        if action == 'list_archived':
            agents = [a for a in _team_agents(rt, db, actor['rootId'], include_deleted=True)
                      if a.get('agentArchive') and a.get('deletedAt')]
            rows = sorted((_brief(a) for a in agents), key=lambda x: x['id'])
            page = rt.model_page(rows, args, [actor['rootId'], 'archived_workers'])
            return page
        target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
        _authorize(rt, db, actor_id, epoch, target)
        archived = target.get('agentArchive')
        if target.get('deletedAt') and not archived:
            raise ValueError('This worker was deleted, not archived')
        if action == 'archive' and archived:
            if archived.get('cleanupPending') or (target.get('worktreeReady') and not target.get('imageWorkspace')):
                target['agentArchive']['cleanupPending'] = True
                rt.put(db, 'agents', target)
                archived_agent = _brief(target)
            else:
                cleaned = target.get('cleanedWorktree') if not target.get('worktreeReady') else None
                cleaned_image = target.get('cleanedImageWorkspace') if not target.get('imageWorkspaceReady') else None
                worktree = ({'state': 'missing', 'reason': cleaned.get('note'), 'bytes': 0}
                            if cleaned and cleaned.get('missing') else
                            {'state': 'removed', 'bytes': cleaned.get('bytes')}
                            if cleaned else
                            {'state': 'kept', 'reason': 'already archived', 'bytes': 0})
                return {'status': 'archived', 'agent': _brief(target), 'replayed': True,
                        'workspace': (target.get('imageWorkspaceCleanupResult')
                                      or ({'state': 'archived', 'bytes': cleaned_image.get('bytes', 0)}
                                          if cleaned_image else None)), 'worktree': worktree,
                        'unassignedWork': archived.get('unassignedWork', [])}
        if action == 'restore':
            if not archived:
                return {'status': 'not_archived', 'agent': _brief(target)}
            if target.get('deletedAt') != archived['at'] or target['epoch'] != archived['epoch']:
                raise ValueError('The archived worker changed; restoration requires inspection')
            if archived.get('cleanupPending'):
                return {'status': 'blocked', 'reason': 'Archive cleanup is still in progress'}
            parent = rt.agent(target['parentId'], db)  # type: ignore[arg-type]  # typed-narrowing: archive authorization guarantees parent identity
            if parent.get('deletedAt'):
                raise ValueError('Restore the parent first')
            restore_info = target.get('cleanedWorktree')
            restore_image = target.get('cleanedImageWorkspace')
            if restore_info and restore_info.get('missing') and not restore_info.get('head'):
                return {'status': 'blocked',
                        'reason': 'The worktree folder and branch are missing; no verified ref can restore it'}
            if target.get('worktreeCleanup'):
                return {'status': 'blocked', 'reason': 'Worktree removal is incomplete; inspect its saved path'}
            if target.get('worktreeReady') and not target.get('imageWorkspaceReady'):
                root = _worker_worktree_root(target['id'], target.get('cwd', ''))
                if (root is None or not root.is_dir() or root.is_symlink()
                        or not Path(target.get('cwd', '')).is_dir()):
                    return {'status': 'blocked',
                            'reason': 'The recorded worktree folder is missing and has no verified restore ref'}
            if not restore_info and not restore_image:
                target.pop('deletedAt', None)
                target.pop('agentArchive', None)
                target.update(status='paused', autoWake=False, epoch=target['epoch'] + 1)  # type: ignore[call-arg]  # typed-update
                if target.get('imageWorkspace') and not target.get('imageWorkspaceReady'):
                    target['imageWorkspacePhase'] = 'read_only'
                rt.put(db, 'agents', target)
                return {'status': 'restored', 'agent': _brief(target), 'next': 'Use orchestration_send to resume with an instruction'}
            restore_identity = (target['epoch'], target['deletedAt'])
        if action == 'inspect':
            blockers = [] if archived else _blockers(rt, db, target, unassign_work=unassign_work)
            return {'agent': _brief(target), 'canArchive': not archived and not blockers,
                    'blockers': blockers, 'schedulerAlive': rt.scheduler.is_alive()}
        if archived and action not in {'restore', 'archive'}: raise ValueError('Restore this worker before recovery')
        request_recovery = (rt.reconcile_tool_requests(db, target['id'])
                            if action == 'recover' else None)
        if action == 'archive' and not archived:
            if (observed != (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                    or connection != rt.agent_connection(target)
                    or server is not rt.server_for(observed[2], connection) or rt.closed):
                raise ValueError('Worker changed during native inspection; inspect it again')
            reason = args.get('reason', '')
            if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
                raise ValueError('Give an archive reason with 1 to 1000 characters')
            blockers = _blockers(rt, db, target, unassign_work=unassign_work)
            if blockers:
                return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
            archived_agent = _archive_record(rt, db, target, actor_id, reason, cleanup_pending=True, unassign_work=unassign_work)
    if action == 'archive':
        image_archive = target.get('cleanedImageWorkspace') or {}
        if target.get('imageWorkspaceReady') or image_archive.get('phase') == 'archiving':
            cleanup = _cleanup_image_workspace(rt, target['id'])
        elif target.get('imageWorkspace'):
            cleanup = {'state': 'none', 'bytes': 0}
            with rt.lock, rt.db() as db:
                current = rt.agent(target['id'], db)
                current['imageWorkspacePhase'] = 'archived'
                rt.put(db, 'agents', current)
        else:
            cleanup = _cleanup_worktree(rt, actor_id, target['id'], epoch)
        with rt.lock, rt.db() as db:
            current = rt.agent(target['id'], db)
            if current.get('imageWorkspace'):
                current['imageWorkspaceCleanupResult'] = cast(
                    ImageWorkspaceCleanupResultRecord, cleanup
                )
            cleanup_failed = current.get('imageWorkspace') and cleanup.get('state') == 'kept'
            if (current.get('agentArchive') and current['agentArchive'].get('cleanupPending')
                    and not current.get('worktreeCleanup') and not cleanup_failed):
                current['agentArchive']['cleanupPending'] = False
                rt.put(db, 'agents', current)
        return {'status': 'archived', 'agent': archived_agent,
                'workspace': cleanup if target.get('imageWorkspace') else None,
                'worktree': cleanup if not target.get('imageWorkspace') else None,
                'unassignedWork': archived_agent['agentArchive'].get('unassignedWork', [])}
    if action == 'restore':
        if restore_image:
            from codex_workspace_images import ensure_mounted
            try:
                workspace = rt.ensure_image_workspace(target)
            except Exception as error:
                return {'status': 'blocked', 'reason': 'Image workspace restore failed: ' + str(error)[:500]}
            cwd = (Path(cast(str, workspace.get('path') or workspace.get('repoPath')))
                   / cast(str, target.get('imageWorkspaceSubpath', '.')))
            path_check = subprocess.run([*rt.workspace_exec_prefix({**target, 'cwd': str(cwd)}), 'test', '-d', str(cwd)],
                                        capture_output=True, timeout=30)
            if path_check.returncode:
                return {'status': 'blocked', 'reason': 'The restored image workspace folder is missing'}
            with rt.lock, rt.db() as db:
                target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
                _authorize(rt, db, actor_id, epoch, target)
                if (target['epoch'], target.get('deletedAt')) != restore_identity:
                    return {'status': 'blocked', 'reason': 'Worker changed during image workspace restoration'}
                target.update(cwd=str(cwd), imageWorkspace=True, imageWorkspaceReady=True,
                              imageWorkspacePhase='ready', imageWorkspaceMount=workspace['mount'],
                              branch=None, status='paused', autoWake=False,
                              epoch=target['epoch'] + 1)  # type: ignore[call-arg]  # typed-update
                target.pop('deletedAt', None)
                target.pop('agentArchive', None)
                target.pop('cleanedImageWorkspace', None)
                rt.put(db, 'agents', target)
                return {'status': 'restored', 'agent': _brief(target),
                        'next': 'Use orchestration_send to resume with an instruction'}
        reason, restored_branch = _restore_worktree(restore_info)  # type: ignore[arg-type]  # typed-narrowing: image restore exits before worktree
        if reason:
            return {'status': 'blocked', 'reason': reason}
        if not (Path(restore_info['root']) / restore_info['relative']).is_dir():  # type: ignore[index]  # typed-narrowing: image restore exits before worktree
            return {'status': 'blocked', 'reason': 'The restored worktree does not contain the saved worker path'}
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)  # type: ignore[arg-type]  # typed-suspect: absent agent id might reach runtime lookup
            _authorize(rt, db, actor_id, epoch, target)
            if (target['epoch'], target.get('deletedAt')) != restore_identity:
                raise ValueError('Worker changed during worktree restoration; inspect it')
            target.update(cwd=str(Path(restore_info['root']) / restore_info['relative']), worktreeReady=True,  # type: ignore[index]  # typed-narrowing: image restore exits before worktree
                          branch=restored_branch, status='paused', autoWake=False, epoch=target['epoch'] + 1)  # type: ignore[call-arg]  # typed-update
            target.pop('deletedAt', None)
            target.pop('agentArchive', None)
            target.pop('cleanedWorktree', None)
            rt.put(db, 'agents', target)
            return {'status': 'restored', 'agent': _brief(target), 'next': 'Use orchestration_send to resume with an instruction'}
    # Native reads must not hold the runtime lock or a SQLite transaction.
    input_recovery = None
    if action == 'recover':
        with rt.lock, rt.db() as db:
            current = rt.agent(target['id'], db)
            _authorize(rt, db, actor_id, epoch, current)
            missing = _missing_transferred_history(db, current, rt)
        if missing:
            from codex_account_transfer import transfer_store
            return transfer_store(rt).recover_empty_transferred_thread(target['id'])
        from codex_context_repair import recover_unconfirmed_inputs
        input_recovery = recover_unconfirmed_inputs(rt, target['id'])
        if input_recovery.get('status') == 'waiting':
            return {'status': 'waiting', 'recovery': {'status': 'waiting'},
                    'inputRecovery': input_recovery, 'requests': request_recovery,
                    'next': 'Native input history is unavailable or the thread is active. Keep the input waiting and recover again later.'}
    result = rt.reconcile_turn(target['id'])
    return {'status': 'checked', 'recovery': result, 'inputRecovery': input_recovery, 'requests': request_recovery,
            'next': 'Inspect the worker. Use orchestration_send for an explicit continuation; never replay unknown mutations'}
