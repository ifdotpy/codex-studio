#!/usr/bin/env python3
"""Enable private Tailscale access to an existing Studio server."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=4620)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error('Invalid server port')
    executable = shutil.which('tailscale')
    if not executable:
        parser.error('Install and sign in to Tailscale first')
    with urllib.request.urlopen(f'http://127.0.0.1:{args.port}/api/desktop', timeout=5) as response:
        server = json.load(response)
    if server.get('application') != 'codex-agents' or server.get('mobileProtocol') != 1:
        parser.error('The server needs the mobile update. Restart it after all active work ends.')
    def read(*arguments):
        return json.loads(subprocess.check_output([executable, *arguments], text=True, timeout=15))
    status = read('status', '--json')
    if status.get('BackendState') != 'Running':
        parser.error('Connect Tailscale first')
    hostname = status.get('Self', {}).get('DNSName', '').rstrip('.')
    from codex_remote import validate_origin
    origin = validate_origin('https://' + hostname)
    target = f'http://127.0.0.1:{args.port}'
    current = read('serve', 'status', '--json')
    expected = current.get('Web', {}).get(hostname + ':443', {}).get('Handlers', {}).get('/', {}).get('Proxy')
    if current and (expected != target or current.get('AllowFunnel')):
        parser.error('Tailscale Serve already has another configuration. Preserve it and configure this target manually.')
    path = Path(server['stateDir']) / 'remote-access.json'
    previous = path.read_bytes() if path.exists() else None
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps({'enabled': True, 'origin': origin}) + '\n')
    os.chmod(temporary, 0o600)
    temporary.replace(path)
    try:
        subprocess.run([executable, 'serve', '--bg', '--https=443', target], check=True, timeout=60)
        with urllib.request.urlopen(origin + '/api/desktop', timeout=15) as response:
            remote = json.load(response)
        if remote.get('stateDir') != server['stateDir'] or remote.get('publicOrigin') != origin:
            raise ValueError('The HTTPS endpoint does not match this Studio server')
    except BaseException:
        if previous is None:
            path.unlink(missing_ok=True)
        else:
            path.write_bytes(previous)
        raise
    print(origin)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise SystemExit(str(error))
