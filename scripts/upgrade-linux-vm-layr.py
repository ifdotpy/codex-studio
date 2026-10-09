#!/usr/bin/env python3
"""Upgrade the existing VM only after an idle-provider preflight and disk backup."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
from typing import Any
import uuid

from codex_linux_vm import Client, LinuxVMError, _atomic_json


def upgrade(state: Path, helper: Path, payload: Path, operation: str,
            allowed_orphans: set[str] | None = None) -> dict[str, Any]:
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,100}', operation):
        raise ValueError('Use a bounded operation ID.')
    client = Client(state, helper=helper)
    allowed_orphans = allowed_orphans or set()
    for handle in allowed_orphans:
        if str(uuid.UUID(handle)) != handle:
            raise ValueError('An approved orphan handle must be a canonical UUID.')
    def active_provider(row: dict[str, Any]) -> bool:
        if row.get('state') == 'lost' and row.get('handle') in allowed_orphans:
            return row.get('agentId') is not None or row.get('reason') != 'outcome_unknown'
        return row.get('state') in {'running', 'starting', 'lost'}
    client.guest_dir = payload / 'vm/guest'
    directory = client.state_dir / 'updates' / operation
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    receipt = directory / 'receipt.json'
    if receipt.exists():
        raise LinuxVMError('This upgrade has a saved receipt. Inspect it before another operation.', uncertain=True)
    providers = client.call('provider.list', {}, timeout=30)['providers']
    active = [row for row in providers if active_provider(row)]
    if active:
        raise LinuxVMError('Active or unproven guest providers prevent a VM restart.')
    backup_bytes = sum((client.state_dir / name).stat().st_blocks * 512
                       for name in ('system.raw', 'data.raw'))
    if shutil.disk_usage(client.state_dir).free < 20 * 1024**3 + backup_bytes:
        raise LinuxVMError('Free host space must exceed 20 GiB plus the allocated disk backup size.')
    evidence: dict[str, Any] = {'operationId': operation, 'stateDir': str(client.state_dir),
        'helper': str(helper), 'payload': str(payload), 'providers': providers,
        'approvedOrphanHandles': sorted(allowed_orphans), 'stage': 'backup'}
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
    active = [row for row in client.call('provider.list', {}, timeout=30)['providers'] if active_provider(row)]
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
    parser.add_argument('--payload', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--operation-id', required=True)
    parser.add_argument('--approved-orphan', action='append', default=[])
    args = parser.parse_args()
    print(json.dumps(upgrade(args.state, args.helper, args.payload, args.operation_id,
                            set(args.approved_orphan)), indent=2))
