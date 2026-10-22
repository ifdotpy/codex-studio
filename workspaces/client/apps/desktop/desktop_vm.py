"""VM maintenance path and lock checked by the desktop installer."""
import os
from pathlib import Path

MAINTENANCE_LOCK = 'maintenance.lock'


def vm_state_dir(state: Path) -> Path:
    """The VM state directory of a Studio state directory."""
    return Path(os.environ.get('CODEX_LINUX_VM_STATE_DIR') or state / 'linux-vm').expanduser().resolve()
