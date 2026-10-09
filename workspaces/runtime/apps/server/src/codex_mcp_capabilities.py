"""MCP prompt fields and read-only initialization proof for teleport preflight."""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
from typing import Any
from codex_claude import node_executable
from codex_native_errors import NativeRpcError
from codex_layout import CLAUDE_BRIDGE_ROOT


def prompt_catalog(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    result = []
    for row in rows:
        raw = row.get('tools', {})
        # Native status uses a HashMap. Codex normalizes model identities in sorted order.
        entries = sorted(raw.items()) if isinstance(raw, dict) else ((entry.get('name'), entry) for entry in raw)
        tools = [{'name': name, 'definition': {key: value for key, value in tool.items()
                  if key in {'name', 'description', 'inputSchema', 'outputSchema', 'title'}}}
                 for name, tool in entries]
        # Codex Apps derives model namespaces from connector metadata. Other metadata
        # (resource links, owner profiles, output templates) does not define the prefix.
        lookup = raw if isinstance(raw, dict) else {entry['name']: entry for entry in raw}
        if row.get('name') == 'codex_apps':
            for entry in tools:
                meta = lookup[entry['name']].get('_meta') or {}
                entry['definition']['connector'] = {key: meta[key] for key in
                    ('connector_id', 'connector_name', 'connector_description') if key in meta}
        info = row.get('serverInfo') or {}
        result.append({'name': row.get('name'), 'serverInfo': {key: info[key] for key in ('name', 'version') if key in info},
                       'tools': tools})
    return result


def instruction_proofs(server: Any, agent: dict[str, Any], rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    fallback = [{'name': row.get('name'), 'proofLevel': 'server_info_fallback'} for row in rows]
    try:
        config = server.call('config/read', {'includeLayers': False, **({'cwd': agent['cwd']} if agent.get('cwd') else {})}, timeout=3)['config']
        servers = []
        for row in rows:
            raw = (config.get('mcp_servers') or {}).get(row.get('name'))
            if not isinstance(raw, dict) or raw.get('enabled') is False:
                continue
            if raw.get('command'):
                transport = {'type': 'stdio', 'command': raw['command'], 'args': raw.get('args', []),
                             'env': {key: os.environ[key] for key in (raw.get('env_vars') or []) if key in os.environ}}
                transport['env'].update(raw.get('env') or {})
                if raw.get('cwd'):
                    transport['cwd'] = raw['cwd']
            elif raw.get('url'):
                # Only explicit local headers are available. Native OAuth remains in the native client.
                headers = dict(raw.get('http_headers') or {})
                headers.update({key: os.environ[value] for key, value in (raw.get('env_http_headers') or {}).items() if value in os.environ})
                token_env = raw.get('bearer_token_env_var')
                if token_env and token_env in os.environ:
                    headers['Authorization'] = 'Bearer ' + os.environ[token_env]
                transport = {'type': 'http', 'url': raw['url'], 'headers': headers}
            else:
                continue
            servers.append({'name': row['name'], 'config': transport})
        node = node_executable()
        if not servers or not node:
            return fallback
        reader = CLAUDE_BRIDGE_ROOT / 'codex-catalog-proof.mjs'
        response = subprocess.run([node, str(reader)], input=json.dumps({'servers': servers}),
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=20, check=True)
        proofs = json.loads(response.stdout)
        verified = {row['name']: row for row in proofs if isinstance(row, dict)
                    and row.get('proofLevel') == 'initialize_instructions'
                    and isinstance(row.get('instructionsHash'), str) and len(row['instructionsHash']) == 64}
        return [verified.get(row['name'], row) for row in fallback]
    except (KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError, NativeRpcError):
        return fallback


def instruction_differences(source: dict[str, Any], target: dict[str, Any]) -> list[str]:
    after = {row['name']: row for row in target.get('mcpProof', [])}
    return [row['name'] for row in source.get('mcpProof', [])
            if row.get('instructionsHash') and after.get(row['name'], {}).get('instructionsHash')
            and row['instructionsHash'] != after[row['name']]['instructionsHash']]


def capabilities_match(source: dict[str, Any], target: dict[str, Any]) -> bool:
    return ({key: value for key, value in source.items() if key != 'mcpProof'} ==
            {key: value for key, value in target.items() if key != 'mcpProof'} and not instruction_differences(source, target))


def proof_levels(source: dict[str, Any], target: dict[str, Any]) -> list[dict[str, Any]]:
    before = {row['name']: row for row in source.get('mcpProof', [])}
    return [{'name': row['name'], 'proofLevel': 'initialize_instructions' if
             row.get('instructionsHash') and before.get(row['name'], {}).get('instructionsHash') else 'server_info_fallback'}
            for row in target.get('mcpProof', [])]
