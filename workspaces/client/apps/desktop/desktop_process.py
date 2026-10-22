"""Read native process arguments for the desktop recovery identity check."""
from __future__ import annotations

import os
from pathlib import Path
import sys

# Bounds for the OS argument vector: bytes and argument count, respectively.
MAX_LAUNCH_BYTES = 4 * 1024 * 1024
MAX_LAUNCH_ARGUMENTS = 4096


def process_launch_command(pid: int) -> list[str]:
    """Read bounded OS arguments without executing or changing the process."""
    if sys.platform == 'darwin':
        import ctypes
        import struct
        libc = ctypes.CDLL(None, use_errno=True)
        mib = (ctypes.c_int * 3)(1, 49, pid)
        size = ctypes.c_size_t()
        if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
        if not 4 <= size.value <= MAX_LAUNCH_BYTES:
            raise ValueError('Native process launch data exceeds its limit')
        buffer = ctypes.create_string_buffer(size.value)
        if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
        raw = buffer.raw[:size.value]
        argc = struct.unpack('i', raw[:4])[0]
        if not 0 < argc <= MAX_LAUNCH_ARGUMENTS:
            raise ValueError('Invalid native process argument count')
        position = raw.index(b'\0', 4) + 1
        while position < len(raw) and raw[position] == 0:
            position += 1
        command = []
        for _ in range(argc):
            end = raw.index(b'\0', position)
            command.append(os.fsdecode(raw[position:end]))
            position = end + 1
    elif sys.platform.startswith('linux'):
        with (Path('/proc') / str(pid) / 'cmdline').open('rb') as stream:
            raw = stream.read(MAX_LAUNCH_BYTES + 1)
        if len(raw) > MAX_LAUNCH_BYTES or not raw.endswith(b'\0'):
            raise ValueError('Invalid native process launch data')
        command = [os.fsdecode(part) for part in raw[:-1].split(b'\0')]
        if not 0 < len(command) <= MAX_LAUNCH_ARGUMENTS:
            raise ValueError('Invalid native process argument count')
    else:
        raise RuntimeError('Cannot verify a retained native launch on this platform')
    if not command or not command[0]:
        raise ValueError('Invalid native executable')
    return command
