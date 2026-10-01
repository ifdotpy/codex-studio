"""Lead-only worker inspection and reversible, guarded team cleanup."""
import json
import os
import subprocess
import time
from pathlib import Path


def management_tools(tool, text):
    return [tool('orchestration_agent_manage',
        'Manage your own descendant workers. inspect returns archive blockers. recover checks exact unconfirmed input IDs in native history, requeues only IDs absent from an idle thread, and reconciles an existing turn. For a transferred thread with no source rollout, recover reports the missing history and does not replay inputs. '
        'archive hides an inactive worker and removes its clean Studio worktree after saving an archive ref; it reports why a worktree stays. '
        'archive_finished archives finished descendants with safe worktrees and reports freed bytes and kept workers. '
        'reset_tools releases an idle worker subscription after native command and receipt checks. Give a reason. '
        'Codex can end its idle session after its configured idle window (60 seconds by default); send new work after confirmed closure to start fresh tools. '
        'restore recreates a removed worktree and returns an archived worker paused; use orchestration_send to resume. '
        'list and list_archived are paged and include cached worktree disk use. inspect includes a worker size and team total. '
        'maintenance_report lists old archived or deleted worktrees without removal. Only the lead may use this tool.',
        {'action': {'type': 'string', 'enum': ['inspect', 'list', 'recover', 'reset_tools', 'archive', 'archive_finished', 'restore', 'list_archived', 'maintenance_report']},
         'agent_id': text, 'reason': {'type': 'string', 'maxLength': 1000},
         'limit': {'type': 'integer', 'minimum': 1, 'maximum': 50}, 'cursor': text}, ['action'])]


def _authorize(rt, db, actor_id, epoch, target=None):
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
            cursor = rt.agent(cursor['parentId'], db)
    return actor


def _brief(a):
    return {k: a.get(k) for k in ('id', 'name', 'parentId', 'status', 'inFlight', 'threadId',
                                 'turnId', 'error', 'lastEvent', 'agentArchive', 'nativeRelease')}


def _completed_native_turn(a):
    attempt = a.get('startAttempt') or {}
    return (bool(a.get('lastCompletedTurn')) and a.get('lastCompletedTurn') == attempt.get('turnId')
            and a.get('turnId') is None and not a.get('inFlight') and _finished(a))


def _missing_transferred_history(db, agent, rt=None):
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


def _blockers(rt, db, a):
    from codex_workspace import active_monitors, active_task_records
    key = a['id']
    result = []
    def add(kind, ids):
        if ids: result.append({'kind': kind, 'count': len(ids), 'ids': ids[:20]})
    if a.get('inFlight') or a['status'] in {'starting', 'running', 'approval', 'queued'}:
        add('active_turn', [key])
    if a.get('workspaceOperation'): add('workspace_operation', [key])
    preparation = rt.preparations.get(key)
    if preparation and not preparation['future'].done(): add('thread_preparation', [key])
    add('descendants', [x['id'] for x in rt.records(db, 'agents') if x.get('parentId') == key and not x.get('deletedAt')])
    add('input_delivery', [r[0] for r in db.execute(
        "SELECT id FROM runtime_events WHERE agent=? AND epoch=? AND status IN ('pending','reserved','dispatching','uncertain')", (key, a['epoch']))])
    add('monitors', [x['id'] for x in active_monitors(db) if x.get('agent') == key and x.get('status') in {'starting','running','approval'}])
    tasks = active_task_records(db, ('running','starting','pending','unknown'), agent=key)
    for task in tasks:
        if (not _finished(a) or task['status'] != 'running' or task.get('kind') != 'command'
                or not str(task.get('processId', '')).isdigit() or not task.get('turnId')
                or not db.execute('SELECT 1 FROM runtime_completed_turns WHERE id=?',
                                  (key + ':' + task['turnId'],)).fetchone()):
            continue
        try:
            os.kill(int(task['processId']), 0)
        except ProcessLookupError:
            task.update(status='lost', finished=time.time(), error='Command process ended without an exit receipt. Outcome unknown.')
            rt.put(db, 'tasks', task)
        except (OSError, ValueError):
            pass
    add('background_tasks', [x['id'] for x in tasks if x['status'] in {'running','starting','pending','unknown'}])
    add('questions', [x['id'] for x in rt.records(db, 'requests') if x.get('agent') == key and x.get('status') == 'pending'])
    add('assigned_work', [x['id'] for x in rt.records(db, 'work') if x.get('owner') == key and x.get('status') not in {'accepted','cancelled'}])
    rt.reconcile_tool_requests(db, key)
    add('tool_requests', [row[0] for row in db.execute(
        "SELECT id FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
        "AND (json_extract(record,'$.stage') IN ('queued','running') OR "
        "(json_extract(record,'$.outcome')='unknown' AND ?))",
        (key, int(not _finished(a))))])
    return result


def _git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True,
                          timeout=300)


def _worktree_entries(repo):
    listing = _git(repo, 'worktree', 'list', '--porcelain', '-z').stdout.decode('utf-8', 'surrogateescape')
    return [dict(line.split(' ', 1) for line in block.split('\0') if ' ' in line)
            for block in listing.split('\0\0')]


def _worktree_registration(repo, root):
    entries = _worktree_entries(repo)
    return next((item for item in entries if item.get('worktree')
                 and Path(item['worktree']).resolve() == Path(root).resolve()), None)


def _worktree_check(rt, actor_id, agent_id, epoch):
    """Use one safety check for archive, bulk archive, and maintenance reports."""
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        _authorize(rt, db, actor_id, epoch, a)
        blockers = _blockers(rt, db, a)
        if blockers:
            return None, ', '.join(item['kind'] for item in blockers)
        if not a.get('worktreeReady'):
            return None, 'no registered worker worktree'
        cwd = Path(a['cwd']).resolve()
        roots = [p for p in (cwd, *cwd.parents)
                 if p.name == agent_id and p.parent.name == 'codex-agents'
                 and p.parent.parent.name == '.worktrees']
        if len(roots) != 1:
            return None, 'worker path is outside the Studio worktree folder'
        root = roots[0]
        repo = root.parent.parent.parent
        relative = cwd.relative_to(root)
        if any(other['id'] != agent_id and other.get('worktreeReady')
               and Path(other.get('cwd', '')).resolve().is_relative_to(root)
               for other in rt.records(db, 'agents')):
            return None, 'another agent uses this worktree'
        identity = [a['epoch'], a['cwd'], a.get('deletedAt')]
    try:
        if Path(_git(repo, 'rev-parse', '--show-toplevel').stdout.decode().strip()).resolve() != repo:
            return None, 'repository root differs from the recorded path'
        entries = _worktree_entries(repo)
        entry = next((item for item in entries if item.get('worktree')
                      and Path(item['worktree']).resolve() == root), None)
        if not entry:
            return None, 'worktree registration is missing'
        if any(item.get('worktree') and Path(item['worktree']).resolve() != root
               and Path(item['worktree']).resolve().is_relative_to(root) for item in entries):
            return None, 'another registered worktree is nested inside this path'
        if _git(root, 'status', '--porcelain', '--untracked-files=normal').stdout:
            return None, 'tracked or untracked files have changes'
        head = _git(root, 'rev-parse', 'HEAD').stdout.decode().strip()
        if entry.get('HEAD') != head:
            return None, 'worktree registration HEAD differs'
    except (OSError, subprocess.SubprocessError) as error:
        return None, 'git check failed: ' + str(error)[:180]
    return {'root': str(root), 'repo': str(repo), 'relative': str(relative),
            'branch': entry.get('branch', '').removeprefix('refs/heads/') or None,
            'head': head, 'identity': identity}, None


def _cleanup_worktree(rt, actor_id, agent_id, epoch, checked=None):
    with rt.lock, rt.db() as db:
        current = rt.agent(agent_id, db)
        _authorize(rt, db, actor_id, epoch, current)
        saved = current.get('worktreeCleanup')
        if not current.get('worktreeReady') and current.get('cleanedWorktree'):
            return {'state': 'removed', 'bytes': current['cleanedWorktree'].get('bytes')}
        if not current.get('worktreeReady'):
            return {'state': 'none', 'bytes': 0}
    if saved and not Path(saved['root']).exists():
        try:
            ref = _git(saved['repo'], 'rev-parse', '--verify',
                       'refs/codex-agents/archive/' + agent_id).stdout.decode().strip()
            if _worktree_registration(saved['repo'], saved['root']) is None and ref == saved['head']:
                with rt.lock, rt.db() as db:
                    a = rt.agent(agent_id, db)
                    _authorize(rt, db, actor_id, epoch, a)
                    if a.get('worktreeCleanup') != saved:
                        raise ValueError('Worker changed during cleanup recovery')
                    a.update(cwd=str(Path(saved['repo']) / saved['relative']), worktreeReady=False)
                    a.pop('worktreeCleanup', None)
                    a['cleanedWorktree'] = saved
                    rt.put(db, 'agents', a)
                return {'state': 'removed', 'bytes': saved.get('bytes')}
        except (OSError, subprocess.SubprocessError):
            pass
        return {'state': 'kept', 'reason': 'worktree removal outcome is unknown; inspect its path and ref', 'bytes': 0}
    info, reason = checked or _worktree_check(rt, actor_id, agent_id, epoch)
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
        a['worktreeCleanup'] = info
        rt.put(db, 'agents', a)
    root, repo = info['root'], info['repo']
    try:
        # du can be slow on Chromium trees. A timeout leaves the byte count unknown.
        try:
            size = subprocess.run(['du', '-sk', root], check=True, capture_output=True,
                                  timeout=180)
            measured = int(size.stdout.split()[0]) * 1024
        except (OSError, ValueError, subprocess.SubprocessError):
            measured = None
        with rt.lock, rt.db() as db:
            a = rt.agent(agent_id, db)
            if a.get('worktreeCleanup') == info:
                a['worktreeCleanup']['bytes'] = measured
                rt.put(db, 'agents', a)
        if _git(root, 'status', '--porcelain', '--untracked-files=normal').stdout:
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
        _git(repo, 'update-ref', 'refs/codex-agents/archive/' + agent_id, info['head'])
        _git(repo, 'worktree', 'remove', '--force', root)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if Path(root).exists():
            with rt.lock, rt.db() as db:
                a = rt.agent(agent_id, db)
                if a.get('worktreeCleanup') and all(a['worktreeCleanup'].get(k) == info[k] for k in info):
                    a.pop('worktreeCleanup', None)
                    rt.put(db, 'agents', a)
        return {'state': 'kept', 'reason': 'worktree removal failed: ' + str(error)[:180], 'bytes': 0}
    with rt.lock, rt.db() as db:
        a = rt.agent(agent_id, db)
        if not a.get('worktreeCleanup') or any(a['worktreeCleanup'].get(k) != info[k] for k in info):
            raise ValueError('Worker changed after worktree removal; inspect its archive')
        a.update(cwd=str(Path(repo) / info['relative']), worktreeReady=False)
        a['cleanedWorktree'] = a['worktreeCleanup']
        a.pop('worktreeCleanup', None)
        rt.put(db, 'agents', a)
    return {'state': 'removed', 'bytes': measured}


def _finished(a):
    return a['status'] in {'completed', 'failed', 'interrupted'} or (a['status'] == 'paused' and not a.get('autoWake'))


def _archive_finished(rt, actor_id, epoch):
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        agents = [a for a in rt.records(db, 'agents') if a['rootId'] == actor['rootId']
                  and a['id'] != actor_id and not a.get('deletedAt') and _finished(a)]
        by_id = {a['id']: a for a in agents}
        def depth(a):
            count = 0
            while a.get('parentId') in by_id:
                count += 1
                a = by_id[a['parentId']]
            return count
        agents.sort(key=lambda a: (-depth(a), a['id']))
    result = {'archived': 0, 'freedBytes': 0, 'unknownBytes': 0, 'kept': []}
    for a in agents:
        checked = (_worktree_check(rt, actor_id, a['id'], epoch)
                   if a.get('worktreeReady') else (None, None))
        if checked[1]:
            result['kept'].append({'id': a['id'], 'reason': checked[1]})
            continue
        try:
            archived = manage_agent(rt, actor_id, {'action': 'archive', 'agent_id': a['id'],
                                                    'reason': 'Finished worker cleanup'}, epoch)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            result['kept'].append({'id': a['id'], 'reason': str(error)[:180]})
            continue
        if archived['status'] == 'archived':
            result['archived'] += 1
            cleanup = archived['worktree']
            if cleanup['state'] == 'removed':
                if cleanup['bytes'] is None:
                    result['unknownBytes'] += 1
                else:
                    result['freedBytes'] += cleanup['bytes']
            elif cleanup['state'] != 'none':
                result['kept'].append({'id': a['id'], 'reason': cleanup['reason']})
        else:
            result['kept'].append({'id': a['id'], 'reason': ', '.join(b['kind'] for b in archived['blockers'])})
    return result


def worktree_maintenance_report(rt, actor_id, epoch=None):
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        agents = [a['id'] for a in rt.records(db, 'agents') if a['rootId'] == actor['rootId']
                  and a['id'] != actor_id and a.get('deletedAt') and a.get('worktreeReady')]
    return [{'id': key, 'reason': _worktree_check(rt, actor_id, key, epoch)[1] or 'removable'}
            for key in agents]


def _restore_worktree(info):
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


def manage_agent(rt, actor_id, args, epoch=None):
    action = args.get('action')
    if action not in {'inspect','list','recover','reset_tools','archive','archive_finished','restore','list_archived','maintenance_report'}:
        raise ValueError('Unknown agent management action')
    if action == 'archive_finished':
        return _archive_finished(rt, actor_id, epoch)
    if action == 'maintenance_report':
        return {'worktrees': worktree_maintenance_report(rt, actor_id, epoch)}
    if action == 'reset_tools':
        reason = args.get('reason')
        if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
            raise ValueError('Give a tool reset reason with 1 to 1000 characters')
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)
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
            target = rt.agent(args.get('agent_id'), db)
            _authorize(rt, db, actor_id, epoch, target)
            if not target.get('deletedAt'):
                blockers = _blockers(rt, db, target)
                if blockers: return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
                observed = (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                connection = rt.connection_ids.get(observed[2])
                server = rt.servers.get(observed[2])
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
                        or native['status'].get('type') not in {'idle', 'notLoaded'}):
                    return {'status': 'blocked', 'agent': _brief(target),
                            'blockers': [{'kind': 'native_state_unconfirmed', 'count': 1, 'ids': [target['id']]}]}
    with rt.lock, rt.db() as db:
        actor = _authorize(rt, db, actor_id, epoch)
        if action == 'list':
            from codex_worktree_disk import management_view
            agents = [a for a in rt.records(db, 'agents')
                      if a['rootId'] == actor['rootId'] and a['id'] != actor_id
                      and not a.get('deletedAt')]
            sizes, disk = management_view(rt, agents)
            rows = sorted((_brief(a) for a in agents), key=lambda x: x['id'])
            page = rt.model_page(rows, args, [actor['rootId'], 'workers'])
            page['items'] = [{**row, 'worktreeDisk': sizes[row['id']]} for row in page['items']]
            return {**page, 'disk': disk}
        if action == 'list_archived':
            from codex_worktree_disk import management_view
            agents = [a for a in rt.records(db, 'agents')
                      if a['rootId'] == actor['rootId'] and a.get('agentArchive') and a.get('deletedAt')]
            sizes, disk = management_view(rt, agents)
            rows = sorted((_brief(a) for a in agents), key=lambda x: x['id'])
            page = rt.model_page(rows, args, [actor['rootId'], 'archived_workers'])
            page['items'] = [{**row, 'worktreeDisk': sizes[row['id']]} for row in page['items']]
            return {**page, 'disk': disk}
        target = rt.agent(args.get('agent_id'), db)
        _authorize(rt, db, actor_id, epoch, target)
        archived = target.get('agentArchive')
        if target.get('deletedAt') and not archived:
            raise ValueError('This worker was deleted, not archived')
        if action == 'archive' and archived:
            if archived.get('cleanupPending'):
                archived_agent = _brief(target)
            else:
                cleaned = target.get('cleanedWorktree') if not target.get('worktreeReady') else None
                return {'status': 'archived', 'agent': _brief(target), 'replayed': True,
                        'worktree': ({'state': 'removed', 'bytes': cleaned.get('bytes')}
                                     if cleaned else {'state': 'kept', 'reason': 'already archived', 'bytes': 0})}
        if action == 'restore':
            if not archived:
                return {'status': 'not_archived', 'agent': _brief(target)}
            if target.get('deletedAt') != archived['at'] or target['epoch'] != archived['epoch']:
                raise ValueError('The archived worker changed; restoration requires inspection')
            parent = rt.agent(target['parentId'], db)
            if parent.get('deletedAt'):
                raise ValueError('Restore the parent first')
            restore_info = target.get('cleanedWorktree')
            if target.get('worktreeCleanup'):
                return {'status': 'blocked', 'reason': 'Worktree removal is incomplete; inspect its saved path'}
            if not restore_info:
                target.pop('deletedAt', None)
                target.pop('agentArchive', None)
                target.update(status='paused', autoWake=False, epoch=target['epoch'] + 1)
                rt.put(db, 'agents', target)
                return {'status': 'restored', 'agent': _brief(target), 'next': 'Use orchestration_send to resume with an instruction'}
            restore_identity = (target['epoch'], target['deletedAt'])
        if action == 'inspect':
            from codex_worktree_disk import management_view
            team = [a for a in rt.records(db, 'agents')
                    if a['rootId'] == actor['rootId'] and a['id'] != actor_id
                    and (not a.get('deletedAt') or a['id'] == target['id'])]
            sizes, disk = management_view(rt, team)
            blockers = [] if archived else _blockers(rt, db, target)
            return {'agent': {**_brief(target), 'worktreeDisk': sizes[target['id']]},
                    'disk': disk, 'canArchive': not archived and not blockers,
                    'blockers': blockers, 'schedulerAlive': rt.scheduler.is_alive()}
        if archived and action not in {'restore', 'archive'}: raise ValueError('Restore this worker before recovery')
        request_recovery = (rt.reconcile_tool_requests(db, target['id'])
                            if action == 'recover' else None)
        if action == 'archive' and not archived:
            if (observed != (target['epoch'], target.get('threadId'), target.get('accountKey', 'default'))
                    or connection != rt.connection_ids.get(observed[2])):
                raise ValueError('Worker changed during native inspection; inspect it again')
            reason = args.get('reason', '')
            if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 1000:
                raise ValueError('Give an archive reason with 1 to 1000 characters')
            blockers = _blockers(rt, db, target)
            if blockers:
                return {'status': 'blocked', 'agent': _brief(target), 'blockers': blockers}
            unknown = [row[0] for row in db.execute(
                "SELECT id FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=? "
                "AND json_extract(record,'$.outcome')='unknown' ORDER BY id", (target['id'],))]
            at = time.time()
            target.update(deletedAt=at, autoWake=False, status='paused', epoch=target['epoch'] + 1)
            target['agentArchive'] = {'at': at, 'by': actor_id, 'reason': reason.strip(),
                                      'epoch': target['epoch'], 'cleanupPending': True,
                                      'unknownToolRequests': unknown}
            rt.put(db, 'agents', target)
            if hasattr(rt, 'release_failed_work'):
                rt.release_failed_work(db, rt.records(db, 'agents'), force=True)
            archived_agent = _brief(target)
    if action == 'archive':
        cleanup = _cleanup_worktree(rt, actor_id, target['id'], epoch)
        with rt.lock, rt.db() as db:
            current = rt.agent(target['id'], db)
            if (current.get('agentArchive') and current['agentArchive'].get('cleanupPending')
                    and not current.get('worktreeCleanup')):
                current['agentArchive']['cleanupPending'] = False
                rt.put(db, 'agents', current)
        return {'status': 'archived', 'agent': archived_agent, 'worktree': cleanup}
    if action == 'restore':
        reason, restored_branch = _restore_worktree(restore_info)
        if reason:
            return {'status': 'blocked', 'reason': reason}
        with rt.lock, rt.db() as db:
            target = rt.agent(args.get('agent_id'), db)
            _authorize(rt, db, actor_id, epoch, target)
            if (target['epoch'], target.get('deletedAt')) != restore_identity:
                raise ValueError('Worker changed during worktree restoration; inspect it')
            target.update(cwd=str(Path(restore_info['root']) / restore_info['relative']), worktreeReady=True,
                          branch=restored_branch, status='paused', autoWake=False, epoch=target['epoch'] + 1)
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
