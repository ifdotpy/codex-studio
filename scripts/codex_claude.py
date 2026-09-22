"""Claude Code subscription transport. Credentials stay with the native CLI."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time

_lock = threading.Lock()
_cached = None
_cached_at = 0


def installed():
    configured = os.environ.get('STUDIO_CLAUDE_BIN')
    return (shutil.which(configured or 'claude') or
            (str(Path.home() / '.local/bin/claude')
             if not configured and os.access(Path.home() / '.local/bin/claude', os.X_OK) else None))


def subscription_env():
    env = os.environ.copy()
    # An inherited API key must not change the user's chosen billing source.
    for key in ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
                'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY'):
        env.pop(key, None)
    return env


def auth_metadata():
    global _cached, _cached_at
    with _lock:
        if _cached is not None and time.monotonic() - _cached_at < 15:
            return dict(_cached)
        result = {'status': 'signedOut', 'accountId': None, 'email': None, 'plan': None}
        executable = installed()
        if executable:
            try:
                completed = subprocess.run([executable, 'auth', 'status', '--json'],
                    env=subscription_env(), capture_output=True, text=True, timeout=8)
                data = json.loads(completed.stdout)
                if data.get('loggedIn') and data.get('authMethod') == 'claude.ai':
                    identity = data.get('email')
                    if not identity:
                        raise ValueError('Missing account identity')
                    result.update(status='ready', accountId='claude:' + identity,
                                  email=identity, plan=data.get('subscriptionType'),
                                  _credentialIdentity='claude:' + identity)
            except (OSError, ValueError, subprocess.TimeoutExpired):
                result.update(status='error', error='Cannot read Claude Code sign-in status')
        _cached, _cached_at = result, time.monotonic()
        return dict(result)


def transport(root):
    executable = installed()
    configured_node = os.environ.get('STUDIO_NODE_BIN')
    candidates = [configured_node] if configured_node else [
        shutil.which('node'), '/opt/homebrew/bin/node', '/usr/local/bin/node',
        str(Path.home() / '.local/share/fnm/aliases/default/bin/node'),
        str(Path.home() / 'Library/Application Support/fnm/aliases/default/bin/node'),
        str(Path.home() / '.volta/bin/node'), str(Path.home() / '.local/share/mise/shims/node'),
    ]
    node = next((str(Path(p).resolve()) for p in candidates if p and os.access(p, os.X_OK)), None)
    bridge = Path(__file__).resolve().parent / 'claude_bridge/bridge.mjs'
    if not executable or not node:
        raise ValueError('Install Node.js and Claude Code, then run claude auth login')
    if not (bridge.parent / 'node_modules/@anthropic-ai/claude-agent-sdk/package.json').is_file():
        raise ValueError('Claude support is missing. Run npm ci in scripts/claude_bridge')
    env = subscription_env()
    metadata = auth_metadata()
    if metadata.get('status') != 'ready':
        raise ValueError('Sign in to Claude Code with a Claude subscription')
    env['STUDIO_CLAUDE_ACCOUNT'] = metadata['email']
    env['PATH'] = str(Path(node).parent) + os.pathsep + env.get('PATH', '')
    env['STUDIO_CLAUDE_BIN'] = executable
    return [node, str(bridge), str(root)], env
