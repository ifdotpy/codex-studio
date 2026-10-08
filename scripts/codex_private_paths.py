"""Apply private permissions to Studio state paths."""
from __future__ import annotations

import os
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, cast

if TYPE_CHECKING:
    import ctypes


class _WindowsCtypes(Protocol):
    def WinDLL(self, name: str, *, use_last_error: bool) -> ctypes.CDLL: ...
    def WinError(self, code: int) -> OSError: ...
    def get_last_error(self) -> int: ...


def protect(path: str | Path, *, directory: bool = False) -> None:
    """Restrict a private path to its owner and the system on each platform."""
    target = Path(path)
    if os.name != "nt":
        os.chmod(target, 0o700 if directory else 0o600)
        return

    import ctypes
    from ctypes import wintypes

    windows = cast(_WindowsCtypes, ctypes)
    advapi32 = windows.WinDLL("advapi32", use_last_error=True)
    kernel32 = windows.WinDLL("kernel32", use_last_error=True)
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    kernel32.LocalFree.argtypes = [wintypes.HANDLE]
    kernel32.LocalFree.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    advapi32.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)]
    advapi32.OpenProcessToken.restype = wintypes.BOOL
    advapi32.GetTokenInformation.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.GetTokenInformation.restype = wintypes.BOOL
    advapi32.ConvertSidToStringSidW.argtypes = [wintypes.LPVOID, ctypes.POINTER(wintypes.LPWSTR)]
    advapi32.ConvertSidToStringSidW.restype = wintypes.BOOL
    advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.LPVOID), ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = wintypes.BOOL
    advapi32.GetSecurityDescriptorDacl.argtypes = [
        wintypes.LPVOID, ctypes.POINTER(wintypes.BOOL), ctypes.POINTER(wintypes.LPVOID),
        ctypes.POINTER(wintypes.BOOL),
    ]
    advapi32.GetSecurityDescriptorDacl.restype = wintypes.BOOL
    advapi32.SetNamedSecurityInfoW.argtypes = [
        wintypes.LPWSTR, wintypes.DWORD, wintypes.DWORD,
        wintypes.LPVOID, wintypes.LPVOID, wintypes.LPVOID, wintypes.LPVOID,
    ]
    advapi32.SetNamedSecurityInfoW.restype = wintypes.DWORD
    token = wintypes.HANDLE()
    if not advapi32.OpenProcessToken(kernel32.GetCurrentProcess(), 0x0008, ctypes.byref(token)):
        raise windows.WinError(windows.get_last_error())
    try:
        needed = wintypes.DWORD()
        advapi32.GetTokenInformation(token, 1, None, 0, ctypes.byref(needed))
        buffer = ctypes.create_string_buffer(needed.value)
        if not advapi32.GetTokenInformation(token, 1, buffer, needed, ctypes.byref(needed)):
            raise windows.WinError(windows.get_last_error())
        sid_pointer = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p))[0]
        sid_text = wintypes.LPWSTR()
        if not advapi32.ConvertSidToStringSidW(sid_pointer, ctypes.byref(sid_text)):
            raise windows.WinError(windows.get_last_error())
        try:
            ace = "(A;OICI;FA;;;{})".format(sid_text.value) if directory else "(A;;FA;;;{})".format(sid_text.value)
            descriptor = wintypes.LPVOID()
            if not advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW(
                "D:P" + ace, 1, ctypes.byref(descriptor), None
            ):
                raise windows.WinError(windows.get_last_error())
            try:
                present = wintypes.BOOL()
                defaulted = wintypes.BOOL()
                dacl = wintypes.LPVOID()
                if not advapi32.GetSecurityDescriptorDacl(
                    descriptor, ctypes.byref(present), ctypes.byref(dacl), ctypes.byref(defaulted)
                ) or not present.value:
                    raise windows.WinError(windows.get_last_error())
                result = advapi32.SetNamedSecurityInfoW(
                    str(target), 1, 0x00000004 | 0x80000000,
                    None, None, dacl, None
                )
                if result:
                    raise windows.WinError(result)
            finally:
                kernel32.LocalFree(descriptor)
        finally:
            kernel32.LocalFree(sid_text)
    finally:
        kernel32.CloseHandle(token)


def ensure_private_dir(path: str | Path) -> Path:
    target = Path(path)
    target.mkdir(parents=True, exist_ok=True, mode=0o700)
    if os.name == "nt":
        protect(target, directory=True)
    return target


def protect_temp_file(path: str | Path) -> Path:
    target = Path(path)
    protect(target)
    return target
