"""Choose and run the path of a VM guest update (docs/linux-vm-workspaces.md, "Guest updates").

The guest's root broker (vm-guest/guest_update.py) owns the rules. This module sends it the
files of the current payload and the record of what only provisioning installs, and the broker
answers with a path:

- current: the guest has these files.
- in_place: only files that the broker may replace changed. They go to the guest in parts, the
  broker writes them and restarts the changed units once no request runs. Agents keep running.
- reboot: anything else (provider CLIs, the Claude bridge, the process supervisor, the
  provision script) or an older guest. upgrade-linux-vm-layr.py then reboots the VM.
"""
from __future__ import annotations

import base64
import gzip
import hashlib
import hmac
import json
import time
from typing import Any

from codex_linux_vm import (Client, LinuxVMError, _cloud_config, _pnpm_version, _provision_script,
                            payload_record)
from codex_shared_values import GUEST_UPDATE_PART_BYTES

OPT = '/opt/codex-studio/'
# Not compared: provision.sh is in the payload record, and the units in /etc name the payload
# generation, which every changed file changes.
NOT_COMPARED = {OPT + 'provision.sh'}
PLAN_TIMEOUT = 120
STAGE_TIMEOUT = 120
# The broker's install can rebuild layr (cargo, up to 20 minutes).
APPLY_TIMEOUT = 1800
# The broker restarts after its drain (at most 10 minutes) and the unit restarts.
SETTLE_TIMEOUT = 900
POLL_SECONDS = 5
UNSUPPORTED = 'The method is not supported'


def desired(client: Client) -> tuple[dict[str, bytes], dict[str, str]]:
    """The guest files of this Studio payload (relative to /opt/codex-studio) and its record."""
    manifest = json.loads((client.state_dir / 'image.json').read_text())
    config = _cloud_config(client.guest_dir, manifest['codex'], manifest['claude'],
                           update_key=client.update_key())
    files: dict[str, bytes] = {}
    for entry in config['write_files']:
        path = entry['path']
        if not path.startswith(OPT) or path in NOT_COMPARED:
            continue
        content = entry['content']
        files[path[len(OPT):]] = (gzip.decompress(base64.b64decode(content))
                                  if entry.get('encoding') == 'gz+b64' else content.encode())
    pnpm = _pnpm_version()
    record = payload_record(manifest['codex'], manifest['claude'], pnpm,
                            _provision_script(manifest['codex'], manifest['claude'], pnpm))
    return files, record


def plan(client: Client) -> dict[str, Any]:
    """The broker's answer for this payload. An older guest has no update methods: reboot."""
    files, record = desired(client)
    manifest = {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}
    try:
        result = client.call('update.plan', {'files': manifest, 'payload': record}, timeout=PLAN_TIMEOUT)
    except LinuxVMError as error:
        if error.detail != UNSUPPORTED:
            raise
        result = {'path': 'reboot', 'changed': [], 'restarts': [],
                  'reasons': ['The guest has no in-place updates: it was provisioned by an older Studio.']}
    return {**result, 'manifest': manifest}


def request_id(kind: str, *parts: object) -> str:
    """One exact request ID per step: a retry of the same step gets the broker's receipt."""
    return 'update-' + kind + ':' + hashlib.sha256(json.dumps(parts).encode()).hexdigest()[:48]


def signature(key: str, operation: str, manifest: dict[str, str]) -> str:
    """The same signature as guest_update.signature: the operation and its whole manifest."""
    body = json.dumps({'operationId': operation, 'files': manifest}, sort_keys=True, separators=(',', ':'))
    return hmac.new(key.encode(), body.encode(), hashlib.sha256).hexdigest()


def apply_in_place(client: Client, chosen: dict[str, Any], operation: str) -> dict[str, Any]:
    """Send the changed files, apply them, and wait for the broker's restarts."""
    if chosen.get('path') != 'in_place':
        raise LinuxVMError('This update needs the reboot path.')
    files, _ = desired(client)
    manifest = chosen['manifest']
    for name in chosen['changed']:
        data = files[name]
        digest = manifest[name]
        for offset in range(0, max(len(data), 1), GUEST_UPDATE_PART_BYTES):
            part = data[offset:offset + GUEST_UPDATE_PART_BYTES]
            client.call('update.stage', {'operationId': operation, 'sha256': digest, 'offset': offset,
                                         'data': base64.b64encode(part).decode()},
                        request_id=request_id('stage', operation, digest, offset), timeout=STAGE_TIMEOUT)
    applied = client.call('update.apply', {'operationId': operation, 'files': manifest,
                                           'signature': signature(client.update_key(), operation, manifest)},
                          request_id=request_id('apply', operation), timeout=APPLY_TIMEOUT)
    status = applied
    deadline = time.monotonic() + SETTLE_TIMEOUT
    while status.get('stage') not in {'complete', 'restart_failed', 'install_failed'}:
        if time.monotonic() >= deadline:
            raise LinuxVMError('The in-place update did not finish its restarts in time: '
                               + str(status.get('stage')), uncertain=True)
        time.sleep(POLL_SECONDS)
        try:
            status = client.call('update.status', {'operationId': operation}, timeout=30)
        except LinuxVMError:
            continue  # The broker or the guest service restarts now.
    if status['stage'] != 'complete':
        raise LinuxVMError('The in-place update failed: ' + str(status.get('error') or status['stage']))
    after = plan(client)
    health = client.call('layr.health', {}, timeout=60)
    if after.get('path') != 'current' or health.get('state') != 'ready':
        raise LinuxVMError('The guest does not have the updated files or a ready layr after the update.',
                           uncertain=True)
    return {'path': 'in_place', 'changed': chosen['changed'], 'restarts': status.get('restarts', []),
            'status': status, 'layr': health}
