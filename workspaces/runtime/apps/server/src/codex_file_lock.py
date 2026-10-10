"""Cross-platform advisory file locks with ``fcntl.flock`` semantics."""
from __future__ import annotations

import errno
import os
from typing import TYPE_CHECKING, Protocol, cast

if TYPE_CHECKING:
    import ctypes


class _FileDescriptor(Protocol):
    def fileno(self) -> int: ...


class _PosixFcntl(Protocol):
    def flock(self, fd: int, operation: int) -> None: ...


class _WindowsCtypes(Protocol):
    def WinDLL(self, name: str, *, use_last_error: bool) -> ctypes.CDLL: ...
    def WinError(self, code: int) -> OSError: ...
    def get_last_error(self) -> int: ...


class _WindowsMsvcrt(Protocol):
    def get_osfhandle(self, fd: int) -> int: ...

LOCK_SH = 1
LOCK_EX = 2
LOCK_NB = 4
LOCK_UN = 8
_WINDOWS_LOCK_OFFSET = 1024 * 1024


def flock(file_or_fd: int | _FileDescriptor, operation: int) -> None:
    """Lock one byte range, and release it when asked.

    POSIX delegates to the native ``flock`` implementation. Windows uses a
    synchronous ``LockFileEx`` lock beyond the metadata region. The operating
    system releases either lock when its file handle closes or its owning process exits.
    """
    fd = file_or_fd if isinstance(file_or_fd, int) else file_or_fd.fileno()
    if os.name != "nt":
        import fcntl

        cast(_PosixFcntl, fcntl).flock(fd, operation)
        return

    import ctypes
    import msvcrt
    from ctypes import wintypes

    class Overlapped(ctypes.Structure):
        _fields_ = [
            ("Internal", ctypes.c_size_t),
            ("InternalHigh", ctypes.c_size_t),
            ("Offset", wintypes.DWORD),
            ("OffsetHigh", wintypes.DWORD),
            ("hEvent", wintypes.HANDLE),
        ]

    windows = cast(_WindowsCtypes, ctypes)
    windows_msvcrt = cast(_WindowsMsvcrt, msvcrt)
    kernel32 = windows.WinDLL("kernel32", use_last_error=True)
    lock_file = kernel32.LockFileEx
    lock_file.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(Overlapped),
    ]
    lock_file.restype = wintypes.BOOL
    unlock_file = kernel32.UnlockFileEx
    unlock_file.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, ctypes.POINTER(Overlapped),
    ]
    unlock_file.restype = wintypes.BOOL

    handle = wintypes.HANDLE(windows_msvcrt.get_osfhandle(fd))
    overlapped = Overlapped()
    overlapped.Offset = _WINDOWS_LOCK_OFFSET & 0xFFFFFFFF
    overlapped.OffsetHigh = _WINDOWS_LOCK_OFFSET >> 32
    if operation & LOCK_UN:
        if not unlock_file(handle, 0, 1, 0, ctypes.byref(overlapped)):
            raise windows.WinError(windows.get_last_error())
        return

    flags = 0
    if operation & LOCK_EX:
        flags |= 0x00000002  # LOCKFILE_EXCLUSIVE_LOCK
    if operation & LOCK_NB:
        flags |= 0x00000001  # LOCKFILE_FAIL_IMMEDIATELY
    if lock_file(handle, flags, 0, 1, 0, ctypes.byref(overlapped)):
        return
    error = windows.get_last_error()
    if operation & LOCK_NB and error in {33, 158}:
        raise BlockingIOError(errno.EAGAIN, "The file lock is already held")
    raise windows.WinError(error)
