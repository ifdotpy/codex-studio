"""Linux process identity used by the Rust process supervisor journal.

Owner of the encoding: runtime-process/src/supervisor_identity.rs (ProcStat.start_identity).
"""
from pathlib import Path

# /proc/<pid>/stat field 22, relative to the fields following the command's closing parenthesis.
START_TICKS_INDEX = 19


def process_start_time(pid: int) -> str | None:
    if type(pid) is not int or pid < 1:
        return None
    try:
        stat = Path(f'/proc/{pid}/stat').read_text()
    except FileNotFoundError:
        return None
    try:
        fields = stat.rsplit(')', 1)[1].split()
        if fields[0] in {'Z', 'X'}:
            return None
        ticks = int(fields[START_TICKS_INDEX])
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        if ticks < 0 or not boot:
            raise ValueError('Invalid native process identity')
        return f'linux:{boot}:{ticks}'
    except (ValueError, IndexError) as error:
        raise RuntimeError(f'Cannot verify process identity for PID {pid}') from error
