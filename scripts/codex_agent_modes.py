"""Per-chat subagent concurrency and its derived delegation mode."""
import time
from typing import TYPE_CHECKING, Any

from codex_records import AgentRecord

if TYPE_CHECKING:
    import sqlite3
    from codex_runtime import Runtime

DEFAULT_SUBAGENT_CONCURRENCY = 32
MAX_SUBAGENT_CONCURRENCY = 512
DEFAULT_MAX_TEAM_AGENTS = 64
MAX_TEAM_AGENTS = 1024
LEAD_TEAM_RECORDS = 1
QUEUED_WORKER_HEADROOM = 1
DEFAULT_GLOBAL_CONCURRENCY = MAX_SUBAGENT_CONCURRENCY + LEAD_TEAM_RECORDS
CONCURRENCY_SCHEMA_VERSION = 2


def concurrency(agent: AgentRecord) -> int:
    """Read the canonical limit, migrating old mode-only records in memory."""
    if agent.get('subagentConcurrencyVersion', 0) < CONCURRENCY_SCHEMA_VERSION:
        if agent.get('agentMode') == 'single':
            return 0
    if 'concurrency' in agent:
        return agent['concurrency']
    return 0 if agent.get('agentMode') == 'single' else DEFAULT_SUBAGENT_CONCURRENCY


def mode_fields(agent: AgentRecord) -> AgentRecord:
    if agent.get('isLead'):
        limit = concurrency(agent)
        agent['concurrency'] = limit
        agent['subagentConcurrencyVersion'] = CONCURRENCY_SCHEMA_VERSION
        agent['agentMode'] = 'multi' if limit else 'single'
        agent.setdefault('agentModeRevision', 0)
        agent['agentModeSupported'] = True
        agent.setdefault('maxAgentsExplicit', agent.get('maxAgents', DEFAULT_MAX_TEAM_AGENTS) != DEFAULT_MAX_TEAM_AGENTS)
    else:
        agent.pop('concurrency', None)
    return agent


def global_concurrency_limit() -> int:
    """Global process resource ceiling; per-chat limits remain separate."""
    import os
    return max(1, int(os.environ.get('CODEX_CANVAS_CONCURRENCY', str(DEFAULT_GLOBAL_CONCURRENCY))))


def assert_delegation(root: AgentRecord) -> None:
    if concurrency(root) == 0:
        raise ValueError('Single agent mode disables new delegation. Complete the task in the lead chat or select Multi agent.')


def assert_worker_input(runtime: "Runtime", db: "sqlite3.Connection", target: AgentRecord) -> None:
    if target['id'] != target['rootId']:
        assert_delegation(runtime.agent(target['rootId'], db))


def change_mode(runtime: "Runtime", key: str, data: dict[str, object]) -> AgentRecord:
    """Apply new limit requests and exact legacy mode retries atomically."""
    new_request = 'subagent_concurrency' in data
    legacy_request = 'agent_mode' in data
    if new_request and legacy_request:
        raise ValueError('Send one subagent concurrency setting')
    if new_request:
        allowed = {'id', 'subagent_concurrency', 'expected_mode_revision', 'request_id', 'expected_account_key'}
        limit = data.get('subagent_concurrency')
    elif legacy_request:
        allowed = {'id', 'agent_mode', 'expected_mode_revision', 'request_id', 'expected_account_key'}
        mode = data.get('agent_mode')
        if not isinstance(mode, str) or mode not in {'multi', 'single'}:
            raise ValueError('Choose multi or single agent mode')
        limit = DEFAULT_SUBAGENT_CONCURRENCY if mode == 'multi' else 0
    else:
        raise ValueError('A subagent concurrency setting is required')
    if set(data) - allowed:
        raise ValueError('Change subagent concurrency separately from execution settings')
    revision, request = data.get('expected_mode_revision'), data.get('request_id')
    if type(limit) is not int or not 0 <= limit <= MAX_SUBAGENT_CONCURRENCY:
        raise ValueError('Subagent concurrency must be an integer from 0 to 512')
    if type(revision) is not int or revision < 0:
        raise ValueError('A nonnegative expected mode revision is required')
    if not isinstance(request, str) or not 1 <= len(request) <= 200:
        raise ValueError('A subagent concurrency request id is required')
    with runtime.lock, runtime.db() as db:
        agent = runtime.checked_actor(db, key)
        if not agent.get('isLead') or agent['rootId'] != key:
            raise ValueError('Change agent mode on the lead chat')
        body = ({'operation': 'subagent_concurrency', 'agent': key, 'concurrency': limit,
                 'expectedRevision': revision} if new_request else
                {'operation': 'agent_mode', 'agent': key, 'mode': mode, 'expectedRevision': revision})
        signature, previous = runtime.operation_receipt(db, request, body)
        if previous is not None:
            return mode_fields(agent)
        current = concurrency(agent)
        if agent.get('agentModeRevision', 0) != revision:
            raise ValueError('Subagent concurrency changed. Read the current value before saving')
        prior_max_agents = agent.get('maxAgents')
        changed = current != limit
        if current != limit:
            agent.update(concurrency=limit, agentModeRevision=revision + 1,  # type: ignore[call-arg]  # typed-update
                         agentModeChangedAt=time.time(), agentModeChangedBy='user')
        # `maxAgents` is a stored-team guard, not the parallelism limit. Keep
        # enough records available for the requested workers plus the lead.
        if not agent.get('maxAgentsExplicit'):
            minimum_records = limit + LEAD_TEAM_RECORDS + QUEUED_WORKER_HEADROOM
            agent['maxAgents'] = max(agent.get('maxAgents', DEFAULT_MAX_TEAM_AGENTS), minimum_records)
        if changed or agent.get('maxAgents') != prior_max_agents:
            runtime.put(db, 'agents', mode_fields(agent))
        canonical = mode_fields(runtime.agent(key, db))
        runtime.save_receipt(db, request, signature,
                             {'applied': True, 'concurrency': concurrency(canonical),
                              'agentMode': canonical['agentMode'],
                              'agentModeRevision': canonical.get('agentModeRevision', 0)})
        runtime.changed.set()
    if limit > 0:
        from codex_runtime import git_toplevel
        repo = git_toplevel(canonical.get('cwd', ''))
        if repo:
            supported, reason = runtime.image_workspace_support(repo)
            if supported:
                with runtime.lock, runtime.db() as db:
                    current = runtime.agent(key, db)
                    if concurrency(current) > 0:
                        current['imageWorkspaceBaseRepo'] = repo
                        runtime.put(db, 'agents', current)
                if concurrency(current) > 0:
                    canonical['imageWorkspaceBaseRepo'] = repo
                    error_text = None
                    try:
                        runtime.start_image_base(repo, retry_failed=True)
                    except Exception as error:
                        error_text = str(error)[:1200]
                    with runtime.lock, runtime.db() as db:
                        latest = runtime.agent(key, db)
                        if latest.get('imageWorkspaceBaseRepo') == repo:
                            if error_text:
                                latest['imageWorkspaceBaseError'] = error_text
                                canonical['imageWorkspaceBaseError'] = error_text
                            else:
                                latest.pop('imageWorkspaceBaseError', None)
                                canonical.pop('imageWorkspaceBaseError', None)
                            runtime.put(db, 'agents', latest)
            else:
                canonical['imageWorkspaceBaseError'] = reason
                with runtime.lock, runtime.db() as db:
                    latest = runtime.agent(key, db)
                    latest['imageWorkspaceBaseError'] = reason
                    runtime.put(db, 'agents', latest)
    runtime.changed.set()
    return canonical


def guidance(root: AgentRecord) -> str:
    limit = concurrency(root)
    revision = root.get('agentModeRevision', 0)
    mode = 'Single' if limit == 0 else 'Multi'
    if limit == 0:
        text = ('Single agent mode (subagent concurrency 0): the lead completes new work itself. '
                'Do not delegate or start new worker turns. Existing active turns finish; queued work '
                'stays queued until the user raises the limit. Do not interrupt or stop workers.')
    else:
        text = (f'{mode} agent mode (subagent concurrency {limit}): delegate sensibly up to {limit} '
                'simultaneous descendant turns across all providers and reviewer roles. The lead does '
                'not use a subagent slot. Excess worker turns queue. Do not poll unchanged status.')
    return f'[Studio subagent concurrency, revision {revision}] {text}'


def tool_mode_context(runtime: "Runtime", actor_id: str, result: dict[str, Any], key: str | None = None) -> dict[str, Any]:
    # Keep a stable policy attachment for each result, revision and compaction
    # epoch. Suppress later results only after confirmed response delivery.
    with runtime.lock, runtime.db() as db:
        import json
        from codex_efficiency import digest, packed
        actor = runtime.agent(actor_id, db)
        root = mode_fields(runtime.agent(actor['rootId'], db))
        epoch, _, known = runtime.model_known_context(db, actor)
        text = guidance(root)
        version = digest(text)
        db.execute('CREATE TABLE IF NOT EXISTS runtime_model_modes '
                   '(agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version))')
        identity = digest([epoch, version])
        row = db.execute('SELECT record FROM runtime_model_modes WHERE agent=? AND request=? AND version=?',
                         (actor_id, key, identity)).fetchone() if key else None
        if row:
            record = json.loads(row[0])
        else:
            record = {'epoch': epoch, 'version': version, 'revision': root.get('agentModeRevision', 0),
                      'text': text if known.get('agentMode') != version else None}
            if key and record['text']:
                db.execute('INSERT INTO runtime_model_modes VALUES (?,?,?,?)',
                           (actor_id, key, identity, packed(record)))
        if not record['text']:
            return result
        return {**result, 'contentItems': [*result.get('contentItems', []),
                {'type': 'inputText', 'text': record['text']}]}
