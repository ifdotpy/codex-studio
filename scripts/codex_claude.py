"""Claude Code subscription transport. Credentials stay with the native CLI."""
import json
import os
from pathlib import Path
import shutil
import shlex
import re
import subprocess
import threading
import time

_lock = threading.Lock()
_cache = {}


def profile_options(profile=None):
    """Validate persisted provider settings before native process creation."""
    if profile is not None and not isinstance(profile, dict):
        raise ValueError('Claude settings must be an object')
    raw = (profile or {}).get('claudeOptions', {} if (profile or {}).get('provider') else profile or {})
    if not isinstance(raw, dict):
        raise ValueError('Claude settings must be an object')
    unknown = set(raw) - {'binaryPath', 'configDir', 'customModels', 'autoCompactWindow', 'launchArgs'}
    if unknown:
        raise ValueError('Unknown Claude setting: ' + ', '.join(sorted(unknown)))
    result = {}
    for field in ('binaryPath', 'configDir'):
        value = raw.get(field, '')
        if not isinstance(value, str) or '\x00' in value:
            raise ValueError('Invalid Claude ' + field)
        if value.strip():
            value = value.strip()
            result[field] = str(Path(value).expanduser().resolve()) if field == 'configDir' or '/' in value else value
    models = raw.get('customModels', [])
    if not isinstance(models, list) or len(models) > 100:
        raise ValueError('Supply at most 100 custom Claude models')
    normalized = []
    for model in models:
        if isinstance(model, str):
            model = {'id': model, 'label': model}
        if not isinstance(model, dict) or set(model) - {'id', 'label'}:
            raise ValueError('Custom Claude models need id and label')
        identifier, label = model.get('id'), model.get('label') or model.get('id')
        if not isinstance(identifier, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._:/\[\]-]{0,199}', identifier):
            raise ValueError('Invalid custom Claude model id')
        if not isinstance(label, str) or not label.strip() or len(label) > 200:
            raise ValueError('Invalid custom Claude model label')
        if any(item['id'] == identifier for item in normalized):
            raise ValueError('Duplicate custom Claude model id')
        normalized.append({'id': identifier, 'label': label.strip()})
    result['customModels'] = normalized
    window = raw.get('autoCompactWindow')
    if window not in (None, ''):
        if isinstance(window, bool) or not re.fullmatch(r'[0-9]+', str(window)) or not 100000 <= int(window) <= 1000000:
            raise ValueError('Auto-compact must be between 100000 and 1000000 tokens')
        result['autoCompactWindow'] = int(window)
    args = raw.get('launchArgs', '')
    if not isinstance(args, str) or len(args) > 4096:
        raise ValueError('Invalid Claude launch arguments')
    try:
        tokens = shlex.split(args)
    except ValueError:
        raise ValueError('Invalid Claude launch argument quotes') from None
    # Transport, credentials and permissions remain owned by their explicit controls.
    switches = {'--chrome', '--no-chrome', '--debug', '--verbose', '--disable-slash-commands'}
    valued = {'--add-dir', '--betas'}
    i = 0
    seen = set()
    while i < len(tokens):
        token = tokens[i]
        if token in seen:
            raise ValueError('Duplicate Claude launch argument: ' + token)
        seen.add(token)
        if token in switches:
            i += 1
        elif token in valued and i + 1 < len(tokens) and not tokens[i + 1].startswith('-'):
            i += 2
        else:
            raise ValueError('Unsupported Claude launch argument: ' + token)
    result['launchArgs'] = shlex.join(tokens)
    return result


def bridge_options(profile=None):
    options = profile_options(profile)
    tokens = shlex.split(options.pop('launchArgs'))
    extra = {}
    while tokens:
        name = tokens.pop(0)[2:]
        value = tokens.pop(0) if name in {'add-dir', 'betas'} else None
        if name in extra:
            raise ValueError('Duplicate Claude launch argument: --' + name)
        extra[name] = value
    options['extraArgs'] = extra
    return options


def installed(profile=None):
    configured = profile_options(profile).get('binaryPath') or os.environ.get('STUDIO_CLAUDE_BIN')
    return (shutil.which(configured or 'claude') or
            (str(Path.home() / '.local/bin/claude')
             if not configured and os.access(Path.home() / '.local/bin/claude', os.X_OK) else None))


def subscription_env(profile=None):
    env = os.environ.copy()
    for key in ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
                'CLAUDE_CODE_OAUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR',
                'CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR', 'CLAUDE_CODE_USE_BEDROCK',
                'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY'):
        env.pop(key, None)
    config = profile_options(profile).get('configDir')
    if config:
        env['CLAUDE_CONFIG_DIR'] = config
    return env


def auth_metadata(profile=None, force=False):
    executable = installed(profile)
    env = subscription_env(profile)
    key = (executable, env.get('CLAUDE_CONFIG_DIR', ''), env.get('HOME', ''))
    with _lock:
        cached = _cache.get(key)
        if not force and cached and time.monotonic() - cached[0] < 15:
            return dict(cached[1])
        result = {'status': 'signedOut', 'accountId': None, 'email': None, 'plan': None}
        if executable:
            try:
                completed = subprocess.run([executable, 'auth', 'status', '--json'],
                    env=env, capture_output=True, text=True, timeout=8)
                data = json.loads(completed.stdout)
                if completed.returncode == 0 and data.get('loggedIn') and data.get('authMethod') == 'claude.ai':
                    identity = data.get('email')
                    if not isinstance(identity, str) or not identity:
                        raise ValueError('Missing account identity')
                    result.update(status='ready', accountId='claude:' + identity,
                                  email=identity, plan=data.get('subscriptionType'),
                                  _credentialIdentity='claude:' + identity)
            except (OSError, ValueError, TypeError, AttributeError, subprocess.TimeoutExpired):
                result.update(status='error', error='Cannot read Claude Code sign-in status')
        _cache[key] = (time.monotonic(), result)
        return dict(result)


def transport(root, profile=None):
    executable = installed(profile)
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
    env = subscription_env(profile)
    metadata = auth_metadata(profile)
    if metadata.get('status') != 'ready':
        raise ValueError('Sign in to Claude Code with a Claude subscription')
    expected = (profile or {}).get('accountId')
    if expected and metadata.get('accountId') != expected:
        raise ValueError('This Claude profile account changed. Restore its original login')
    env['STUDIO_CLAUDE_OPTIONS'] = json.dumps(bridge_options(profile))
    env['STUDIO_CLAUDE_ACCOUNT'] = metadata['email']
    env['PATH'] = str(Path(node).parent) + os.pathsep + env.get('PATH', '')
    env['STUDIO_CLAUDE_BIN'] = executable
    return [node, str(bridge), str(root)], env
