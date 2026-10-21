#!/usr/bin/env python3
"""Upgrade the existing VM guest. A change that the guest can take in place (guest files, layr)
keeps every agent running (codex_linux_vm_update.py). Any other change reboots the VM, only
after an idle-provider preflight and a disk backup."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
from typing import Any

from codex_layout import REPOSITORY_ROOT, VM_GUEST_ROOT
from codex_linux_vm import (Client, LinuxVMError, _atomic_json, is_agent_provider_handle,
                            is_legacy_provider_handle)
from codex_linux_vm_update import apply_in_place, plan as choose

BACKUP_RESERVE_BYTES = 20 * 1024**3


def upgrade(state: Path, helper: Path, payload: Path, operation: str,
            allowed_orphans: set[str] | None = None, *, reboot: bool = False) -> dict[str, Any]:
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,100}', operation):
        raise ValueError('Use a bounded operation ID.')
    client = Client(state, helper=helper)
    # The backend watchdog does not restart the VM while an upgrade holds this lock,
    # and an upgrade does not start during an automatic restart.
    with client.maintenance():
        return _upgrade(client, helper, payload, operation, allowed_orphans, reboot)


def active_provider(row: dict[str, Any], allowed_orphans: set[str]) -> bool:
    """Whether a guest provider row prevents a VM restart (docs/linux-vm-workspaces.md)."""
    if row.get('state') == 'lost' and row.get('handle') in allowed_orphans:
        # The guest proved that the process of this handle is gone, so the agent ID
        # only names its former owner. Another lost reason can hide a live process.
        if row.get('reason') == 'native_process_lost':
            return False
        return row.get('agentId') is not None or row.get('reason') != 'outcome_unknown'
    return row.get('state') in {'running', 'starting', 'lost'}


def _upgrade(client: Client, helper: Path, payload: Path, operation: str,
             allowed_orphans: set[str] | None, reboot: bool = False) -> dict[str, Any]:
    allowed_orphans = allowed_orphans or set()
    for handle in allowed_orphans:
        if not (is_agent_provider_handle(handle) or is_legacy_provider_handle(handle)):
            raise ValueError('An approved orphan handle must be linux-worker:<agent UUID> or a '
                             'canonical UUID.')
    # A packaged payload keeps the guest at vm/guest; a source checkout at its workspace path.
    client.guest_dir = payload / 'vm/guest' if (payload / 'vm/guest').is_dir() else VM_GUEST_ROOT
    directory = client.state_dir / 'updates' / operation
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    receipt = directory / 'receipt.json'
    if receipt.exists():
        raise LinuxVMError('This upgrade has a saved receipt. Inspect it before another operation.', uncertain=True)
    evidence: dict[str, Any] = {'operationId': operation, 'stateDir': str(client.state_dir),
        'helper': str(helper), 'payload': str(payload)}
    if not reboot:
        # The path comes from what changed (docs/linux-vm-workspaces.md, "Guest updates"). The
        # in-place path keeps every provider: no idle preflight, no backup, no VM restart.
        chosen = choose(client)
        evidence['plan'] = {key: chosen.get(key) for key in ('path', 'changed', 'restarts', 'reasons')}
        if chosen['path'] == 'current':
            evidence['stage'] = 'complete'
            _atomic_json(receipt, evidence)
            return evidence
        if chosen['path'] == 'in_place':
            evidence['stage'] = 'in-place'
            _atomic_json(receipt, evidence)
            evidence.update(stage='complete', inPlace=apply_in_place(client, chosen, operation))
            _atomic_json(receipt, evidence)
            return evidence
    providers = client.call('provider.list', {}, timeout=30)['providers']
    active = [row for row in providers if active_provider(row, allowed_orphans)]
    if active:
        raise LinuxVMError('Active or unproven guest providers prevent a VM restart.')
    # The backup is an APFS clone: it shares every block with the disks when it is
    # made. It grows only by the blocks that the guest rewrites while the backup
    # exists, so the reserve covers that growth, not the allocated disk size.
    if shutil.disk_usage(client.state_dir).free < BACKUP_RESERVE_BYTES:
        raise LinuxVMError('Free host space must exceed 20 GiB for the disk backup clones.')
    evidence.update(providers=providers, approvedOrphanHandles=sorted(allowed_orphans), stage='backup')
    _atomic_json(receipt, evidence)
    backups = directory / 'backup'
    backups.mkdir(mode=0o700)
    # APFS clones preserve the existing sparse images without allocating their
    # virtual size. A second clone after graceful shutdown is the consistent backup.
    for name in ('system.raw', 'data.raw', 'seed.iso', 'provision-seed.json'):
        source = client.state_dir / name
        if source.exists():
            subprocess.run(['cp', '-c', str(source), str(backups / name)], check=True, timeout=60)
    evidence['stage'] = 'backed-up'
    _atomic_json(receipt, evidence)
    # Close the small preflight race. A new active provider keeps the VM alive.
    active = [row for row in client.call('provider.list', {}, timeout=30)['providers'] if active_provider(row, allowed_orphans)]
    if active:
        raise LinuxVMError('A provider started after the backup. The VM stays running.')
    client.stop()
    for name in ('system.raw', 'data.raw'):
        target = backups / name
        target.unlink()
        subprocess.run(['cp', '-c', str(client.state_dir / name), str(target)], check=True, timeout=60)
    evidence['backup'] = {name: {'virtualBytes': (backups / name).stat().st_size,
                               'allocatedBytes': (backups / name).stat().st_blocks * 512}
                          for name in ('system.raw', 'data.raw')}
    evidence['stage'] = 'stopped-with-backup'
    _atomic_json(receipt, evidence)
    client._refresh_provision_seed(time.monotonic() + 120)
    evidence['stage'] = 'seed-prepared'
    _atomic_json(receipt, evidence)
    # Clear the bounded old console after its copy, so an old success cannot
    # make this generation ready. Neither disk nor state identity changes.
    (client.state_dir / 'console.log').write_bytes(b'')
    evidence['stage'] = 'boot-started'
    _atomic_json(receipt, evidence)
    status = client.ensure_running(timeout=3600)
    health = client.call('layr.health', {}, timeout=60)
    if health.get('state') != 'ready':
        raise LinuxVMError('The upgraded VM has no proven layr service.', uncertain=True)
    evidence.update(stage='complete', status=status, layr=health,
                    freeHostBytes=shutil.disk_usage(client.state_dir).free)
    _atomic_json(receipt, evidence)
    return evidence


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--payload', type=Path, default=REPOSITORY_ROOT)
    parser.add_argument('--operation-id', required=True)
    parser.add_argument('--approved-orphan', action='append', default=[])
    parser.add_argument('--reboot', action='store_true',
                        help='take the reboot path even when the change can be applied in place')
    args = parser.parse_args()
    print(json.dumps(upgrade(args.state, args.helper, args.payload, args.operation_id,
                            set(args.approved_orphan), reboot=args.reboot), indent=2))
