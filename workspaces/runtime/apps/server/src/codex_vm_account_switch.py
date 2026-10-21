"""Change the account of chats that run in the Linux VM, with the same native session.

A VM chat keeps its Claude session in the VM, in the profile folder of its account
(`<line owner home>/.claude/studio-accounts/<account digest>/projects/...`). The bridge keeps
its own session records per chat, not per account. A switch at an idle boundary therefore
stops the chat's provider, copies the session files into the target profile, and changes
`accountKey`. The next turn starts the provider with the target account and resumes the same
thread, so the context stays complete. The source files stay in place.
"""
from __future__ import annotations

import base64
import copy
import json
import time
import uuid
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from codex_account_transfer import AccountTransfers
    from codex_records import AccountHistoryRecord, AccountTransferRecord, AgentRecord

# Run as the line owner in the VM: copy the native session of each bridge session of the chat,
# with its folder of subagent and tool files, and the project's Claude memory.
_COPY = r"""
import json, os, shutil, sys
agent, source, target = sys.argv[1:4]
home = os.path.expanduser("~")
sessions = os.path.join(home, ".codex", "studio-bridges", agent, "sessions")
copied = []
names = sorted(os.listdir(sessions)) if os.path.isdir(sessions) else []
for name in names:
    if not name.endswith(".json") or name.endswith(".meta.json"):
        continue
    with open(os.path.join(sessions, name)) as stream:
        session = json.load(stream).get("session") or {}
    native = session.get("nativeId") or session.get("id") or name[:-5]
    projects = os.path.join(home, source, "projects")
    for folder in sorted(os.listdir(projects)) if os.path.isdir(projects) else []:
        found = os.path.join(projects, folder, native + ".jsonl")
        if not os.path.isfile(found):
            continue
        destination = os.path.join(home, target, "projects", folder)
        os.makedirs(destination, mode=0o700, exist_ok=True)
        shutil.copy2(found, os.path.join(destination, native + ".jsonl"))
        for extra in (native, "memory"):
            path = os.path.join(projects, folder, extra)
            if os.path.isdir(path):
                shutil.copytree(path, os.path.join(destination, extra), dirs_exist_ok=True)
        copied.append(native)
print(json.dumps({"copied": copied}))
"""


def needs_switch(members: list["AgentRecord"], target: str) -> list["AgentRecord"]:
    return [a for a in members if a.get('environment') == 'linux' and not a.get('deletedAt')
            and a.get('accountKey', 'default') != target]


def switch(transfers: "AccountTransfers", key: str, target: str, request_id: str,
           scope: str) -> "AccountTransferRecord":
    """Switch every VM member of the team, then record a completed transfer."""
    from codex_account_transfer import ACTIVE
    rt = transfers.rt
    target_provider = rt.accounts.get(target).get('provider', 'codex')
    with rt.lock, rt.db() as db:
        members = rt.team_agents(db, key, include_id=key)
        if scope == 'subagents':
            members = [a for a in members if a['id'] != key]
        changing = needs_switch(members, target)
        if any(a.get('environment') != 'linux' and not a.get('deletedAt')
               and a.get('accountKey', 'default') != target for a in members):
            raise ValueError('This team has Mac and Linux VM chats. Change the account of each part on its own.')
        if target_provider != 'claude':
            raise ValueError('A Linux VM chat can change only to another Claude account.')
        # As on the Mac, a chat of another provider stays on its account.
        left = [a for a in changing
                if rt.accounts.get(a.get('accountKey', 'default')).get('provider', 'codex') != 'claude']
        changing = [a for a in changing if a not in left]
        for a in changing:
            if a.get('inFlight') or a['status'] in ACTIVE:
                raise ValueError('Wait until the Linux VM chats finish their turns.')
            blocker = transfers.local_blocker(db, a)
            if blocker:
                raise ValueError(blocker)
        # The switch marker holds new provider starts of these chats until the account changes.
        for a in changing:
            a['vmAccountSwitch'] = {'id': request_id, 'targetAccountKey': target, 'at': time.time()}
            rt.put(db, 'agents', a)
    results = {}
    try:
        for a in changing:
            results[a['id']] = _switch_one(rt, a, target, request_id)
    except BaseException:
        with rt.lock, rt.db() as db:
            for a in changing:
                current = rt.agent(a['id'], db)
                # A chat whose files may have been copied keeps its source account; a retry
                # with the same request ID repeats the same guest requests.
                current.pop('vmAccountSwitch', None)
                rt.put(db, 'agents', current)
        raise
    with rt.lock, rt.db() as db:
        op = {'id': request_id, 'leadId': key, 'targetAccountKey': target, 'targetProvider': target_provider,
              'status': 'completed', 'scope': scope, 'created': time.time(), 'members': {},
              'requests': {request_id: {'leadId': key, 'targetAccountKey': target, 'scope': scope}},
              'vm': True}
        for a in changing:
            current = rt.agent(a['id'], db)
            history: AccountHistoryRecord = {'transferId': request_id, 'accountKey': current.get('accountKey', 'default'),
                       'threadId': current.get('threadId'), 'provider': 'claude',
                       'targetAccountKey': target, 'targetProvider': 'claude',
                       'targetThreadId': current.get('threadId') or '', 'at': time.time(),
                       'vmSessionCopy': results[a['id']]}
            current.setdefault('accountHistory', []).append(history)
            current['accountKey'] = target
            if current.get('executionSettingsAccountKey'):
                current['executionSettingsAccountKey'] = target
            if current.get('isLead'):
                defaults = copy.deepcopy(current.get('workerDefaults') or rt.worker_defaults(current))
                defaults['accountKey'] = target
                current['workerDefaults'] = defaults
            current.pop('vmAccountSwitch', None)
            current.pop('nativeFailureHold', None)
            if current.get('status') == 'failed':
                current['error'] = None
            rt.put(db, 'agents', current)
            op['members'][a['id']] = {'phase': 'completed', 'name': current.get('name'), 'provider': 'claude',
                                      'sourceAccountKey': history['accountKey'],
                                      'sourceThreadId': current.get('threadId')}
        for a in left:
            provider = rt.accounts.get(a.get('accountKey', 'default')).get('provider', 'codex')
            op['members'][a['id']] = {'phase': 'left', 'name': a.get('name'), 'provider': provider,
                                      'sourceAccountKey': a.get('accountKey', 'default'),
                                      'sourceThreadId': a.get('threadId'),
                                      'reason': f'Uses {provider}; the destination account uses claude'}
        if scope == 'subagents':
            root = rt.agent(key, db)
            defaults = copy.deepcopy(root.get('workerDefaults') or rt.worker_defaults(root))
            defaults['accountKey'] = target
            root['workerDefaults'] = defaults
            rt.put(db, 'agents', root)
        transfers.save(db, op)  # type: ignore[arg-type]  # the VM record has the transfer fields that the UI reads
    rt.changed.set()
    return op  # type: ignore[return-value]


def _switch_one(rt: Any, agent: "AgentRecord", target: str, request_id: str) -> dict[str, Any]:
    from codex_linux_vm_credentials import profile_path
    from codex_linux_workspaces import client
    from codex_vm_agents import context, flush_turns, request
    flush_turns(rt, agent)
    from codex_linux_vm import provider_handle
    handle = provider_handle(agent['id'])
    # Each step is safe to repeat: a stop of a stopped handle and a second copy change nothing.
    # A new ID per attempt copies the turns that the chat ran since an earlier failed attempt.
    key = 'vm-account:' + request_id + ':' + agent['id'] + ':' + uuid.uuid4().hex
    server = rt.__dict__.get('linux_servers', {}).pop(agent['id'], None)
    if server is not None:
        server.close()
    client(rt).request('provider.stop', {'handle': handle}, request_id=key + ':stop', timeout=60)
    # The stopped handle can no longer answer a lost start; a new start uses the target identity.
    (rt.root / 'linux-vm' / 'providers' / agent['id'] / 'start.json').unlink(missing_ok=True)
    line = context(rt, agent)
    source = profile_path(agent.get('accountKey', 'default'), 'claude')
    destination = profile_path(target, 'claude')
    result = request(rt, 'layr.exec', {'agentId': agent['id'], 'cwd': line['path'],
                                       'argv': ['python3', '-c', _COPY, agent['id'], source, destination]},
                     key + ':copy', 300)
    if result.get('exitCode') != 0:
        raise ValueError('The VM session copy failed: '
                         + base64.b64decode(result.get('stderr', '') or '').decode(errors='replace')[-500:])
    copied = json.loads(base64.b64decode(result.get('stdout', '')).decode() or '{}')
    return {'sourceProfile': source, 'targetProfile': destination, 'copied': copied.get('copied', [])}
