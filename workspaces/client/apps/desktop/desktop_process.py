"""Read native process facts for the desktop recovery identity check."""
from __future__ import annotations

import errno
import os
from pathlib import Path
import subprocess
import sys

# Bounds for the OS argument vector: bytes and argument count, respectively.
MAX_LAUNCH_BYTES = 4 * 1024 * 1024
MAX_LAUNCH_ARGUMENTS = 4096
# Only these launch facts may leave the startup-delivery process reader. No credentials.
STARTUP_ENVIRONMENT_KEYS = frozenset({
    'CODEX_AGENTS_STATE_DIR', 'CODEX_AGENTS_SUPERVISOR_MODE', 'CODEX_AGENTS_SUPERVISOR_FALLBACK',
})
# Owners: RuntimeLock::acquire and RuntimeStore::open_inner in runtime-store/src/lib.rs.
# startup-delivery-test.py checks this fixed observation list against those paths.
STARTUP_STATE_FILES = ('runtime.lock', 'canvas.sqlite3', 'analytics.sqlite3')


def _darwin_launch_data(pid):
    import ctypes
    libc = ctypes.CDLL(None, use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2.
    size = ctypes.c_size_t()
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
    if not 4 <= size.value <= MAX_LAUNCH_BYTES:
        raise ValueError('Native process launch data exceeds its limit')
    buffer = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), 'Cannot read native process launch')
    return buffer.raw[:size.value]


def _split_darwin_launch(raw, *, startup_context=True):
    import struct
    if not 4 <= len(raw) <= MAX_LAUNCH_BYTES:
        raise ValueError('Invalid native process launch data')
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
    if not startup_context:
        return command, {}
    environment = {}
    for entry in raw[position:].split(b'\0'):
        if not entry:
            break  # Apple strings are not environment entries.
        key, separator, value = entry.partition(b'=')
        if not separator:
            raise ValueError('Invalid native process environment')
        name = os.fsdecode(key)
        if name in STARTUP_ENVIRONMENT_KEYS:
            if name in environment:
                raise ValueError('Duplicate startup environment fact')
            environment[name] = os.fsdecode(value)
    return command, environment


def process_launch_command(pid: int) -> list[str]:
    """Read bounded OS arguments without executing or changing the process."""
    if sys.platform == 'darwin':
        command, _ = _split_darwin_launch(_darwin_launch_data(pid), startup_context=False)
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


def _startup_bsd_info(pid):
    """Darwin PROC_PIDTBSDINFO, as declared in the SDK's sys/proc_info.h."""
    if sys.platform != 'darwin':
        raise RuntimeError('Startup delivery requires the native desktop process reader')
    import ctypes
    class BsdInfo(ctypes.Structure):
        _fields_ = [
            (name, ctypes.c_uint32) for name in [
                'flags', 'status', 'xstatus', 'pid', 'ppid', 'uid', 'gid', 'ruid', 'rgid',
                'svuid', 'svgid', 'reserved',
            ]
        ] + [('comm', ctypes.c_char * 16), ('name', ctypes.c_char * 32)] + [
            (name, ctypes.c_uint32) for name in ['nfiles', 'pgid', 'pjobc', 'tdev', 'tpgid']
        ] + [('nice', ctypes.c_int32), ('start_seconds', ctypes.c_uint64),
             ('start_microseconds', ctypes.c_uint64)]
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    library.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                    ctypes.c_void_p, ctypes.c_int]
    library.proc_pidinfo.restype = ctypes.c_int
    info = BsdInfo()
    count = library.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
    if count != ctypes.sizeof(info):
        if count == 0 and ctypes.get_errno() in (errno.ESRCH, errno.ENOENT):
            return None
        raise RuntimeError('Cannot read the startup backend birth identity')
    if info.pid != pid or info.uid != os.geteuid():
        raise RuntimeError('The startup backend does not belong to this user')
    return None if info.status == 5 else info  # SZOMB is not a running owner.


def startup_backend_birth(pid):
    """The kernel's exact process birth, including microseconds, or a proven ended PID."""
    info = _startup_bsd_info(pid)
    return None if info is None else f'{info.start_seconds}:{info.start_microseconds}'


def _startup_open_files(output, state, *, text_name='studio-runtime-server'):
    if len(output.encode()) > MAX_LAUNCH_BYTES:
        raise ValueError('Native process file data exceeds its limit')
    opened, texts, record = {}, [], {}
    state_files = {str(state / name):name for name in STARTUP_STATE_FILES} if state is not None else {}
    def consume():
        if not all(key in record for key in ('f', 'D', 'i', 'n')):
            return
        if (not (record['f'] == 'txt' and Path(record['n']).name == text_name)
                and record['n'] not in state_files):
            return
        identity = {'device': int(record['D'], 16), 'inode': int(record['i'])}
        if record['f'] == 'txt' and Path(record['n']).name == text_name:
            texts.append(identity)
        name = state_files.get(record['n'])
        if name is not None:
            if name in opened and opened[name] != identity:
                raise ValueError('Native state file identity is ambiguous')
            opened[name] = identity
    for line in output.splitlines():
        if not line:
            continue
        if line[0] == 'f':
            consume()
            record = {}
        record[line[0]] = line[1:]
    consume()
    return opened, texts


def _darwin_executable_path(pid):
    import ctypes
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    library.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    library.proc_pidpath.restype = ctypes.c_int
    path = ctypes.create_string_buffer(4096)  # PROC_PIDPATHINFO_MAXSIZE from libproc.h.
    if library.proc_pidpath(pid, path, len(path)) <= 0:
        raise RuntimeError('Cannot read the startup backend executable')
    return os.fsdecode(path.value)


def recovery_interpreter_facts(pid):
    """Kernel interpreter image and birth only. No launch environment enters this proof."""
    before = startup_backend_birth(pid)
    if before is None:
        raise RuntimeError('The recovery interpreter ended during observation')
    executable = _darwin_executable_path(pid)
    output = subprocess.check_output(
        ['/usr/sbin/lsof', '-nP', '-a', '-p', str(pid), '-d', 'txt', '-FfDin'],
        text=True, stderr=subprocess.DEVNULL, timeout=2,
    )
    _, texts = _startup_open_files(output, None, text_name=Path(executable).name)
    if startup_backend_birth(pid) != before:
        raise RuntimeError('The recovery interpreter changed during observation')
    return {'pid':pid, 'uid':os.geteuid(), 'birth':before, 'executable':executable, 'textFiles':texts}


def startup_backend_facts(pid, state):
    """Fixed startup-delivery facts only. Raw launch data and credentials never leave this owner."""
    before = startup_backend_birth(pid)
    if before is None:
        raise RuntimeError('The pinned startup backend has ended')
    executable = _darwin_executable_path(pid)
    command, environment = _split_darwin_launch(_darwin_launch_data(pid))
    output = subprocess.check_output(
        ['/usr/sbin/lsof', '-nP', '-a', '-p', str(pid), '-FfDin'],
        text=True, stderr=subprocess.DEVNULL, timeout=2,
    )
    opened, texts = _startup_open_files(output, state)
    if startup_backend_birth(pid) != before:
        raise RuntimeError('The startup backend changed during observation')
    return {'pid': pid, 'uid': os.geteuid(), 'birth': before,
            'executable': executable, 'argv': command,
            'environment': environment, 'openFiles': opened, 'textFiles': texts}
