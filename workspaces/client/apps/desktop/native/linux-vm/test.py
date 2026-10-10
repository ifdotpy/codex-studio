#!/usr/bin/env python3
"""Opt-in native VM proof. Uses isolated disks and no account credentials."""
from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / 'workspaces/runtime/apps/server/src'))
from codex_linux_vm import Client, Settings, LinuxVMError, _GIB
import codex_linux_vm


def command(client, argv, cwd='/home/studio', *, agent_id=None):
    params = {'argv': argv, 'cwd': cwd, 'timeoutSeconds': 120}
    if agent_id:
        params['agentId'] = agent_id
    stdout = bytearray()
    stderr = bytearray()
    result = None
    for frame in client.stream('exec', params, timeout=150):
        if frame.get('event') == 'output':
            target = stdout if frame['data']['stream'] == 'stdout' else stderr
            target.extend(base64.b64decode(frame['data']['data']))
        if 'result' in frame:
            result = frame['result']
    if not result or result.get('exitCode') != 0:
        raise AssertionError(f'Guest command failed: {result}, {stderr.decode(errors="replace")[-1000:]}')
    return stdout.decode()


def prove(client, temporary):
    metrics = {'status': client.status()}
    metrics['versions'] = json.loads(command(client, ['python3', '-c',
        'import subprocess,json;print(json.dumps({k:subprocess.check_output(v,text=True).strip() for k,v in '
        '{"codex":["codex","--version"],"claude":["claude","--version"],"node":["node","--version"]}.items()}))']))
    root = '/var/lib/codex-studio/projects/proof-' + uuid.uuid4().hex
    command(client, ['python3', '-c',
        'import pathlib,subprocess,sys; p=pathlib.Path(sys.argv[1]);p.mkdir();(p/"base.txt").write_text("base\\n");'
        'subprocess.run(["git","init","-b","main",str(p)],check=True,stdout=subprocess.DEVNULL);'
        'subprocess.run(["git","-C",str(p),"-c","user.name=VM Test","-c","user.email=vm@example.invalid","add","."],check=True);'
        'subprocess.run(["git","-C",str(p),"-c","user.name=VM Test","-c","user.email=vm@example.invalid","commit","-m","Base"],check=True,stdout=subprocess.DEVNULL)', root],
        cwd='/var/lib/codex-studio/projects')
    start = time.monotonic()
    base = client.request('workspace.startBase', {'root': root, 'timeoutSeconds': 180}, timeout=200)
    assert base['state'] == 'ready', base
    agent = 'native-proof-' + uuid.uuid4().hex
    workspace = client.request('workspace.create', {'root': root, 'agentId': agent, 'timeoutSeconds': 180}, timeout=200)
    metrics['workspaceSeconds'] = round(time.monotonic() - start, 3)
    script = ('import pathlib,subprocess,json,base64; p=pathlib.Path.cwd();(p/"agent.txt").write_text("guest commit\\n");'
        'subprocess.run(["git","add","."],check=True);'
        'subprocess.run(["git","-c","user.name=VM Test","-c","user.email=vm@example.invalid","commit","-m","Guest result"],check=True,stdout=subprocess.DEVNULL);'
        'subprocess.run(["git","bundle","create","result.bundle","--all"],check=True);'
        'print(json.dumps({"commit":subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),"bundle":base64.b64encode(pathlib.Path("result.bundle").read_bytes()).decode()}))')
    result = json.loads(command(client, ['python3', '-c', script], cwd=workspace['path'], agent_id=agent))
    bundle = temporary / 'result.bundle'
    bundle.write_bytes(base64.b64decode(result['bundle']))
    repository = temporary / 'fetched'
    subprocess.run(['git', 'init', str(repository)], check=True, capture_output=True, timeout=10)
    subprocess.run(['git', '-C', str(repository), 'fetch', str(bundle), 'refs/heads/main'], check=True, capture_output=True, timeout=10)
    fetched = subprocess.check_output(['git', '-C', str(repository), 'rev-parse', 'FETCH_HEAD'], text=True, timeout=10).strip()
    assert fetched == result['commit'], (fetched, result)
    metrics['fetchedCommit'] = fetched
    client.request('workspace.remove', {'agentId': agent}, timeout=150)
    provider = client.request('provider.start', {'argv': ['python3', '-u', '-c',
        'import sys;print("READY",flush=True);print(sys.stdin.readline().strip(),flush=True)'], 'cwd': '/home/studio'})
    pid = metrics['status']['pid']
    client.close()
    reattached = Client(client.state_dir, helper=client.helper)
    assert reattached.status()['pid'] == pid
    handles = reattached.request('provider.list')['providers']
    assert any(row['handle'] == provider['handle'] for row in handles), handles
    reattached.request('provider.write', {'handle': provider['handle'], 'data': base64.b64encode(b'AFTER_RECONNECT\n').decode(), 'close': True})
    deadline = time.monotonic() + 15
    output = b''
    after = 0
    while time.monotonic() < deadline and b'AFTER_RECONNECT' not in output:
        frames = list(reattached.stream('provider.attach', {'handle': provider['handle'], 'afterSeq': after, 'waitMs': 1000}, timeout=2))
        output += b''.join(base64.b64decode(frame['data']['data']) for frame in frames if frame.get('event') == 'output')
        after = next(frame['result']['nextSeq'] for frame in frames if 'result' in frame)
    assert b'AFTER_RECONNECT' in output, frames
    metrics['providerReconnect'] = True
    handle = 'codex-proof-' + uuid.uuid4().hex
    native = reattached.request('provider.start', {'argv': ['codex', 'app-server'], 'cwd': '/home/studio',
                                'transport': 'native', 'handle': handle})
    operation = uuid.uuid4().hex
    reattached.request('provider.rpc', {'handle': handle, 'action': 'write', 'operationId': operation,
        'nativeId': 1, 'message': {'id': 1, 'method': 'initialize', 'params': {
            'clientInfo': {'name': 'studio-linux-vm-proof', 'version': '1'},
            'capabilities': {'experimentalApi': True}}}})
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        info = reattached.request('provider.rpc', {'handle': handle, 'action': 'info'})
        if info.get('initResult'):
            metrics['nativeCodexInitialize'] = True
            break
        time.sleep(0.05)
    else:
        raise AssertionError('Linux Codex did not return its native initialize result')
    reattached.request('provider.stop', {'handle': handle})
    metrics['guestFilesystem'] = reattached.request('health')['filesystem']
    assert metrics['guestFilesystem'] == 'btrfs'
    return metrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        '--guest-dir',
        type=Path,
        default=ROOT / 'workspaces/runtime/apps/vm-guest',
    )
    parser.add_argument('--existing-state', type=Path)
    parser.add_argument('--helper', type=Path)
    parser.add_argument('--state-dir', type=Path)
    parser.add_argument('--keep-state', action='store_true')
    parser.add_argument('--provision-restart-check', action='store_true')
    args = parser.parse_args()
    if args.keep_state and not (args.state_dir or args.existing_state):
        parser.error('--keep-state requires --state-dir or --existing-state')
    if args.state_dir and args.state_dir.exists():
        parser.error('--state-dir must identify a new directory')
    if args.provision_restart_check and args.existing_state:
        parser.error('--provision-restart-check requires a new VM')
    with tempfile.TemporaryDirectory(prefix='studio-vm-native-', dir='/tmp') as name:
        temporary = Path(name)
        helper = args.helper or temporary / 'studio-linux-vm'
        if not args.helper:
            subprocess.run(['node', str(ROOT / 'desktop/native/linux-vm/build.mjs'), str(helper)], check=True, timeout=150)
        client = Client(args.existing_state or args.state_dir or temporary / 'vm', helper=helper)
        client.guest_dir = args.guest_dir
        try:
            start = time.monotonic()
            if not args.existing_state:
                factory = codex_linux_vm._cloud_config
                def fault_config(*values):
                    config = factory(*values)
                    if args.provision_restart_check:
                        entry = next(row for row in config['write_files'] if row['path'] == '/opt/codex-studio/provision.sh')
                        # Fail on every boot of the old seed. Recovery must replace it.
                        entry['content'] = entry['content'].replace('stage codex', '\n'.join([
                            'stage codex',
                            'echo "STUDIO_PROVISION_ERROR: codex: injected-restart-check" >&2',
                            'exit 1']), 1)
                    return config
                with patch.object(codex_linux_vm, '_cloud_config', side_effect=fault_config):
                    client.create(Settings(cpus=2, memoryBytes=4*_GIB, systemDiskBytes=16*_GIB, dataDiskBytes=64*_GIB))
            create_seconds = time.monotonic() - start
            start = time.monotonic()
            if args.provision_restart_check:
                try:
                    client.ensure_running(timeout=300)
                except LinuxVMError as error:
                    assert 'injected-restart-check' in str(error), str(error)
                else:
                    raise AssertionError('The injected first provision did not fail')
                disks = {name: (client.state_dir / name).stat().st_ino for name in ('system.raw', 'data.raw')}
            ready = client.ensure_running(timeout=3600)
            boot_seconds = time.monotonic() - start
            metrics = prove(client, temporary)
            metrics.update(createSeconds=round(create_seconds,3), bootSeconds=round(boot_seconds,3), health=ready['health'])
            if args.provision_restart_check:
                metrics['provisionRestart'] = True
                assert disks == {name: (client.state_dir / name).stat().st_ino for name in disks}
                metrics['disksPreserved'] = True
                metrics['downloads'] = ready.get('provision', {}).get('downloads', [])
                mac = command(client, ['cat', '/sys/class/net/enp0s1/address']).strip()
                assert mac == (client.state_dir / 'network-mac').read_text().strip()
                metrics['networkMacPreserved'] = True
            print(json.dumps(metrics), flush=True)
        finally:
            if not args.existing_state and not args.keep_state:
                client.stop()


if __name__ == '__main__':
    main()
