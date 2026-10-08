"""Fetch a committed worker branch from the Linux VM into host FETCH_HEAD."""
from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid


def _fetch(client, agent_id, guest_path, branch, destination, *, request_id):
    uuid.UUID(agent_id)
    if not isinstance(branch, str) or not branch or branch.startswith('-'):
        raise ValueError('Supply a worker branch name')
    subprocess.run(['git', 'check-ref-format', '--branch', branch], check=True,
                   capture_output=True, timeout=10)
    destination = Path(destination).expanduser().resolve()
    subprocess.run(['git', '-C', str(destination), 'rev-parse', '--git-dir'], check=True,
                   capture_output=True, timeout=10)
    identity = request_id or str(uuid.uuid4())
    floor = int(os.environ.get('CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES', str(5 * 1024 ** 3)))
    if floor < 0:
        raise ValueError('CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES must be non-negative')
    with tempfile.TemporaryDirectory(prefix='studio-linux-fetch-') as temporary:
        directory = Path(temporary)
        if shutil.disk_usage(directory).free < floor:
            raise RuntimeError('Insufficient host disk space for the guest Git bundle')
        bundle = directory / 'result.bundle'
        export_name = hashlib.sha256(identity.encode()).hexdigest() + '.bundle'
        script = ("import json,pathlib,subprocess,sys; "
                  "d=subprocess.check_output(['git','rev-parse','--git-common-dir'],text=True).strip(); "
                  "p=pathlib.Path(d).resolve()/'studio-linux-exports'; p.mkdir(exist_ok=True); "
                  "b=p/sys.argv[1]; subprocess.run(['git','bundle','create',str(b),sys.argv[2]],check=True); "
                  "print(json.dumps({'path':str(b)}))")
        result, metadata_output = None, bytearray()
        for frame in client.stream('exec', {
            'argv': ['python3', '-c', script, export_name, 'refs/heads/' + branch],
            'cwd': guest_path, 'agentId': agent_id, 'timeoutSeconds': 300},
            request_id=identity, timeout=310):
            if frame.get('event') == 'output' and frame['data'].get('stream') == 'stdout':
                metadata_output.extend(base64.b64decode(frame['data']['data'], validate=True))
                if len(metadata_output) > 8192:
                    raise RuntimeError('The guest export receipt exceeds its size limit')
            if 'result' in frame:
                result = frame['result']
        if result is None or result.get('exitCode') != 0:
            raise RuntimeError('The guest could not export the worker branch; request ID: ' + identity)
        exported_path = json.loads(metadata_output)['path']
        stat = client.request('file.stat', {'path': exported_path, 'agentId': agent_id}, timeout=310)
        size = stat['bytes']
        if size > 4 * 1024 ** 3 or shutil.disk_usage(directory).free < floor + size:
            raise RuntimeError('The guest Git bundle exceeds the host disk limit')
        digest, offset = hashlib.sha256(), 0
        with bundle.open('wb') as output:
            while offset < size:
                chunk = client.request('file.read', {'path': exported_path, 'agentId': agent_id,
                    'token': stat['token'], 'offset': offset, 'maxBytes': 1024 * 1024}, timeout=20)
                data = base64.b64decode(chunk['data'], validate=True)
                if (not data or chunk['offset'] != offset or chunk['nextOffset'] != offset + len(data)
                        or offset + len(data) > size or hashlib.sha256(data).hexdigest() != chunk['sha256']):
                    raise RuntimeError('The guest Git bundle chunk is invalid')
                if shutil.disk_usage(directory).free < floor + len(data):
                    raise RuntimeError('Insufficient host disk space for the guest Git bundle')
                output.write(data)
                digest.update(data)
                offset += len(data)
        if digest.hexdigest() != stat['sha256']:
            raise RuntimeError('The guest Git bundle checksum differs')
        subprocess.run(['git', '-C', str(destination), 'bundle', 'verify', str(bundle)],
                       check=True, capture_output=True, timeout=60)
        completed = subprocess.run(['git', '-C', str(destination), 'fetch', '--no-tags',
                                    str(bundle), 'refs/heads/' + branch], check=True,
                                   capture_output=True, text=True, timeout=120)
        return {'requestId': identity, 'bytes': size, 'branch': branch,
                'output': completed.stderr.strip(), 'guestExportPath': exported_path}


def fetch(client, agent_id, guest_path, branch, destination, *, request_id=None):
    identity = request_id or str(uuid.uuid4())
    try:
        return _fetch(client, agent_id, guest_path, branch, destination, request_id=identity)
    except ValueError:
        raise
    except Exception as error:
        raise RuntimeError('The Linux result fetch did not complete; request ID: ' + identity
                           + '. ' + str(error)) from error


def main():
    import argparse
    import json
    from codex_linux_vm import connect

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('agent_id')
    parser.add_argument('guest_path')
    parser.add_argument('branch')
    parser.add_argument('--cwd', default=os.getcwd())
    parser.add_argument('--request-id')
    args = parser.parse_args()
    print(json.dumps(fetch(connect(), args.agent_id, args.guest_path, args.branch,
                           args.cwd, request_id=args.request_id)))


if __name__ == '__main__':
    main()
