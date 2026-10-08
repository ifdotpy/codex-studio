"""Windows named-pipe and Job Object support for the process supervisor."""
from __future__ import annotations

import ctypes
import json
import os
from pathlib import Path
import secrets
import sys
import threading
import time
import uuid
from ctypes import wintypes


ERROR_PIPE_CONNECTED = 535
ERROR_PIPE_BUSY = 231
ERROR_NO_DATA = 232
ERROR_BROKEN_PIPE = 109
ERROR_FILE_NOT_FOUND = 2
PIPE_REJECT_REMOTE_CLIENTS = 0x8
FILE_FLAG_FIRST_PIPE_INSTANCE = 0x00080000
PIPE_UNLIMITED_INSTANCES = 255
PIPE_READ_TIMEOUT = 2
JOB_OBJECT_QUERY = 0x0004
JOB_OBJECT_TERMINATE = 0x0008
JOB_OBJECT_EXTENDED_LIMIT_INFORMATION = 9
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
CREATE_SUSPENDED = 0x00000004
CREATE_NEW_PROCESS_GROUP = 0x00000200
CTRL_BREAK_EVENT = 1
_INVALID_HANDLE = ctypes.c_void_p(-1).value


def _api():
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel32.LocalFree.argtypes = [wintypes.HANDLE]
    kernel32.LocalFree.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    return kernel32, advapi32


def _error():
    return ctypes.WinError(ctypes.get_last_error())


def _sid_string(sid):
    kernel32, advapi32 = _api()
    result = wintypes.LPWSTR()
    advapi32.ConvertSidToStringSidW.argtypes = [wintypes.LPVOID, ctypes.POINTER(wintypes.LPWSTR)]
    advapi32.ConvertSidToStringSidW.restype = wintypes.BOOL
    if not advapi32.ConvertSidToStringSidW(sid, ctypes.byref(result)):
        raise _error()
    try:
        return result.value
    finally:
        kernel32.LocalFree(result)


def current_user_sid():
    kernel32, advapi32 = _api()
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    token = wintypes.HANDLE()
    advapi32.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)]
    advapi32.OpenProcessToken.restype = wintypes.BOOL
    if not advapi32.OpenProcessToken(kernel32.GetCurrentProcess(), 0x0008, ctypes.byref(token)):
        raise _error()
    try:
        needed = wintypes.DWORD()
        advapi32.GetTokenInformation.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID,
            wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
        ]
        advapi32.GetTokenInformation.restype = wintypes.BOOL
        advapi32.GetTokenInformation(token, 1, None, 0, ctypes.byref(needed))
        buffer = ctypes.create_string_buffer(needed.value)
        if not advapi32.GetTokenInformation(token, 1, buffer, needed, ctypes.byref(needed)):
            raise _error()
        sid = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p))[0]
        return _sid_string(sid)
    finally:
        kernel32.CloseHandle(token)


class _SecurityDescriptor:
    def __init__(self, sddl):
        _, advapi32 = _api()
        self.kernel32, self.advapi32 = _api()
        self.pointer = wintypes.LPVOID()
        advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
            wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.LPVOID),
            ctypes.POINTER(wintypes.DWORD),
        ]
        advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = wintypes.BOOL
        if not advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, 1, ctypes.byref(self.pointer), None
        ):
            raise _error()

    def close(self):
        if self.pointer:
            self.kernel32.LocalFree(self.pointer)
            self.pointer = None


class _SecurityAttributes(ctypes.Structure):
    _fields_ = [
        ("nLength", wintypes.DWORD),
        ("lpSecurityDescriptor", wintypes.LPVOID),
        ("bInheritHandle", wintypes.BOOL),
    ]


def pipe_identity(root, *, create=False):
    path = Path(root) / "supervisor.pipe-id"
    try:
        identity = path.read_text(encoding="ascii").strip()
    except FileNotFoundError:
        if not create:
            raise
        from codex_private_paths import protect_temp_file

        identity = secrets.token_hex(24)
        temporary = path.with_name(path.name + ".tmp-" + secrets.token_hex(8))
        try:
            descriptor = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            raise RuntimeError("Cannot create a unique supervisor pipe identity file")
        try:
            os.write(descriptor, (identity + "\n").encode("ascii"))
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        protect_temp_file(temporary)
        try:
            os.replace(temporary, path)
        except FileExistsError:
            temporary.unlink(missing_ok=True)
            identity = path.read_text(encoding="ascii").strip()
    if len(identity) != 48 or any(char not in "0123456789abcdef" for char in identity):
        raise RuntimeError("Supervisor pipe identity file is invalid")
    return identity


def pipe_endpoint(root, identity):
    return r"\\.\pipe\CodexStudioSupervisor-" + identity


def _lease_identity(root):
    value = json.loads((Path(root) / "supervisor.lock").read_text(encoding="utf-8"))
    pid = value.get("pid")
    if not isinstance(pid, int) or pid <= 0:
        raise RuntimeError("Supervisor lease does not contain a valid process id")
    return pid


def _token_user_sid(token):
    kernel32, advapi32 = _api()
    needed = wintypes.DWORD()
    advapi32.GetTokenInformation.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.GetTokenInformation.restype = wintypes.BOOL
    advapi32.GetTokenInformation(token, 1, None, 0, ctypes.byref(needed))
    buffer = ctypes.create_string_buffer(needed.value)
    if not advapi32.GetTokenInformation(token, 1, buffer, needed, ctypes.byref(needed)):
        raise _error()
    sid = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p))[0]
    return _sid_string(sid)


def verify_pipe_server(handle, root):
    kernel32, advapi32 = _api()
    server_pid = wintypes.ULONG()
    kernel32.GetNamedPipeServerProcessId.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.ULONG)]
    kernel32.GetNamedPipeServerProcessId.restype = wintypes.BOOL
    if not kernel32.GetNamedPipeServerProcessId(handle, ctypes.byref(server_pid)):
        raise _error()
    if server_pid.value != _lease_identity(root):
        raise PermissionError("Named-pipe server PID does not match the supervisor lease")
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenProcess.restype = wintypes.HANDLE
    process = kernel32.OpenProcess(0x1000, False, server_pid.value)
    if not process:
        raise _error()
    token = wintypes.HANDLE()
    try:
        advapi32.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)]
        advapi32.OpenProcessToken.restype = wintypes.BOOL
        if not advapi32.OpenProcessToken(process, 0x0008, ctypes.byref(token)):
            raise _error()
        if _token_user_sid(token) != current_user_sid():
            raise PermissionError("Named-pipe server user does not match the current user")
    finally:
        if token:
            kernel32.CloseHandle(token)
        kernel32.CloseHandle(process)


class NamedPipeReader:
    def __init__(self, connection):
        self.connection = connection
        self.buffer = bytearray()

    def readline(self, limit=1024 * 1024 + 1, *, timeout=None):
        read_timeout = self.connection.timeout if timeout is None else timeout
        deadline = None if read_timeout is None else time.monotonic() + read_timeout
        while True:
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                if newline + 1 > limit:
                    raise ValueError("Supervisor frame exceeds the size limit")
                result = bytes(self.buffer[:newline + 1])
                del self.buffer[:newline + 1]
                self.connection.settimeout(read_timeout)
                return result
            if len(self.buffer) >= limit:
                raise ValueError("Supervisor frame exceeds the size limit")
            if deadline is None and self.buffer:
                deadline = time.monotonic() + PIPE_READ_TIMEOUT
            if deadline is None:
                self.connection.settimeout(None)
            else:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Named-pipe read timed out")
                self.connection.settimeout(remaining)
            chunk = self.connection.read(65536)
            if not chunk:
                if self.buffer:
                    raise ConnectionError("Supervisor connection ended inside a frame")
                return b""
            if deadline is None:
                deadline = time.monotonic() + PIPE_READ_TIMEOUT
            self.buffer.extend(chunk)

    def close(self):
        pass


class NamedPipeConnection:
    def __init__(self, handle):
        self.handle = wintypes.HANDLE(handle)
        self.closed = False
        self.timeout = None
        self.kernel32, _ = _api()
        self.kernel32.ReadFile.argtypes = [
            wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID,
        ]
        self.kernel32.ReadFile.restype = wintypes.BOOL
        self.kernel32.WriteFile.argtypes = [
            wintypes.HANDLE, wintypes.LPCVOID, wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID,
        ]
        self.kernel32.WriteFile.restype = wintypes.BOOL
        self.kernel32.PeekNamedPipe.argtypes = [
            wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD),
            ctypes.POINTER(wintypes.DWORD),
        ]
        self.kernel32.PeekNamedPipe.restype = wintypes.BOOL
        self.kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel32.CloseHandle.restype = wintypes.BOOL

    def settimeout(self, timeout):
        self.timeout = timeout

    def makefile(self, mode="r", encoding=None):
        if mode != "r":
            raise ValueError("Named-pipe client supports a read stream only")
        return NamedPipeReader(self)

    def read(self, size):
        deadline = None if self.timeout is None else time.monotonic() + self.timeout
        if deadline is not None:
            while True:
                available = wintypes.DWORD()
                if not self.kernel32.PeekNamedPipe(
                    self.handle, None, 0, None, ctypes.byref(available), None
                ):
                    code = ctypes.get_last_error()
                    if code in (ERROR_BROKEN_PIPE, ERROR_NO_DATA):
                        return b""
                    raise ctypes.WinError(code)
                if available.value:
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Named-pipe read timed out")
                time.sleep(min(0.01, remaining))
        buffer = ctypes.create_string_buffer(size)
        count = wintypes.DWORD()
        if not self.kernel32.ReadFile(self.handle, buffer, size, ctypes.byref(count), None):
            code = ctypes.get_last_error()
            if code in (ERROR_BROKEN_PIPE, ERROR_NO_DATA):
                return b""
            raise ctypes.WinError(code)
        return buffer.raw[:count.value]

    def sendall(self, data):
        offset = 0
        while offset < len(data):
            count = wintypes.DWORD()
            view = ctypes.create_string_buffer(data[offset:])
            if not self.kernel32.WriteFile(self.handle, view, len(data) - offset,
                                           ctypes.byref(count), None):
                raise _error()
            if count.value == 0:
                raise ConnectionError("Named-pipe write returned no data")
            offset += count.value

    def shutdown(self, _how):
        return None

    def close(self):
        if not self.closed:
            self.closed = True
            self.kernel32.CloseHandle(self.handle)


def connect_pipe(root, timeout=10):
    kernel32, _ = _api()
    deadline = time.monotonic() + timeout
    kernel32.WaitNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD]
    kernel32.WaitNamedPipeW.restype = wintypes.BOOL
    kernel32.CreateFileW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE,
    ]
    kernel32.CreateFileW.restype = wintypes.HANDLE
    while True:
        try:
            endpoint = pipe_endpoint(root, pipe_identity(root))
        except FileNotFoundError:
            if time.monotonic() >= deadline:
                raise TimeoutError("Supervisor pipe identity is not available")
            time.sleep(0.05)
            continue
        handle = kernel32.CreateFileW(endpoint, 0xC0020000, 0, None, 3, 0, None)
        if handle != _INVALID_HANDLE:
            try:
                verify_pipe_server(handle, root)
            except Exception:
                kernel32.CloseHandle(handle)
                raise
            connection = NamedPipeConnection(handle)
            connection.settimeout(timeout)
            return connection
        error = ctypes.get_last_error()
        if error not in (ERROR_PIPE_BUSY, ERROR_FILE_NOT_FOUND) or time.monotonic() >= deadline:
            raise ctypes.WinError(error)
        kernel32.WaitNamedPipeW(endpoint, max(1, min(500, int((deadline - time.monotonic()) * 1000))))


def verify_pipe_client(handle):
    kernel32, advapi32 = _api()
    advapi32.ImpersonateNamedPipeClient.argtypes = [wintypes.HANDLE]
    advapi32.ImpersonateNamedPipeClient.restype = wintypes.BOOL
    advapi32.RevertToSelf.restype = wintypes.BOOL
    if not advapi32.ImpersonateNamedPipeClient(handle):
        raise _error()
    token = wintypes.HANDLE()
    try:
        kernel32.GetCurrentThread.restype = wintypes.HANDLE
        advapi32.OpenThreadToken.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.BOOL, ctypes.POINTER(wintypes.HANDLE),
        ]
        advapi32.OpenThreadToken.restype = wintypes.BOOL
        advapi32.GetTokenInformation.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID,
            wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
        ]
        advapi32.GetTokenInformation.restype = wintypes.BOOL
        if not advapi32.OpenThreadToken(kernel32.GetCurrentThread(), 0x0008, True, ctypes.byref(token)):
            raise _error()
        needed = wintypes.DWORD()
        advapi32.GetTokenInformation(token, 1, None, 0, ctypes.byref(needed))
        buffer = ctypes.create_string_buffer(needed.value)
        if not advapi32.GetTokenInformation(token, 1, buffer, needed, ctypes.byref(needed)):
            raise _error()
        sid = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p))[0]
        if _sid_string(sid) != current_user_sid():
            raise PermissionError("Named-pipe client identity does not match the supervisor owner")
    finally:
        if token:
            kernel32.CloseHandle(token)
        _revert_to_self_or_exit(advapi32, kernel32, handle)


def _revert_to_self_or_exit(advapi32, kernel32, handle):
    if not advapi32.RevertToSelf():
        kernel32.CloseHandle(handle)
        os._exit(70)


def create_pipe_server(endpoint, connection_handler):
    """Accept same-user local clients with an explicit protected DACL."""
    kernel32, _ = _api()
    descriptor = _SecurityDescriptor("D:P(A;;GA;;;{})".format(current_user_sid()))
    attributes = _SecurityAttributes(ctypes.sizeof(_SecurityAttributes), descriptor.pointer, False)
    kernel32.CreateNamedPipeW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
        ctypes.POINTER(_SecurityAttributes),
    ]
    kernel32.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel32.ConnectNamedPipe.argtypes = [wintypes.HANDLE, wintypes.LPVOID]
    kernel32.ConnectNamedPipe.restype = wintypes.BOOL
    kernel32.DisconnectNamedPipe.argtypes = [wintypes.HANDLE]
    kernel32.DisconnectNamedPipe.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    try:
        first_instance = True
        while True:
            handle = kernel32.CreateNamedPipeW(
                endpoint, 0x00000003 | (FILE_FLAG_FIRST_PIPE_INSTANCE if first_instance else 0),
                0x00000000 | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, ctypes.byref(attributes),
            )
            if handle == _INVALID_HANDLE:
                raise _error()
            first_instance = False
            connected = kernel32.ConnectNamedPipe(handle, None)
            if not connected and ctypes.get_last_error() != ERROR_PIPE_CONNECTED:
                kernel32.CloseHandle(handle)
                continue
            connection = NamedPipeConnection(handle)
            connection.settimeout(PIPE_READ_TIMEOUT)
            threading.Thread(
                target=_dispatch_pipe_client,
                args=(connection, connection_handler),
                daemon=True,
            ).start()
    finally:
        descriptor.close()


def _dispatch_pipe_client(connection, connection_handler):
    reader = NamedPipeReader(connection)
    try:
        first_line = reader.readline(1024 * 1024 + 1, timeout=PIPE_READ_TIMEOUT)
        if not first_line or len(first_line) > 1024 * 1024:
            raise ValueError("Supervisor frame exceeds the size limit")
        verify_pipe_client(connection.handle)
        connection.settimeout(None)
        connection_handler(connection, reader, first_line)
    except Exception as error:
        print("Supervisor named-pipe client rejected: " + type(error).__name__
              + ": " + str(error)[:200], file=sys.stderr, flush=True)
        try:
            connection.sendall((__import__("json").dumps({"error": str(error)[:500]}) + "\n").encode())
        except OSError:
            pass
        connection.close()


class _IoCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
    )]


class _BasicLimitInformation(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_longlong),
        ("PerJobUserTimeLimit", ctypes.c_longlong),
        ("LimitFlags", wintypes.DWORD),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _ExtendedLimitInformation(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _BasicLimitInformation),
        ("IoInfo", _IoCounters),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class _BasicAccountingInformation(ctypes.Structure):
    _fields_ = [
        ("TotalUserTime", ctypes.c_longlong),
        ("TotalKernelTime", ctypes.c_longlong),
        ("ThisPeriodTotalUserTime", ctypes.c_longlong),
        ("ThisPeriodTotalKernelTime", ctypes.c_longlong),
        ("TotalPageFaults", wintypes.DWORD),
        ("TotalProcesses", wintypes.DWORD),
        ("ActiveProcesses", wintypes.DWORD),
        ("TotalTerminatedProcesses", wintypes.DWORD),
    ]


class Job:
    def __init__(self, handle, identity):
        self.handle = wintypes.HANDLE(handle)
        self.identity = identity
        self.kernel32, _ = _api()

    def contains(self, process_handle):
        result = wintypes.BOOL()
        self.kernel32.IsProcessInJob.argtypes = [wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.BOOL)]
        self.kernel32.IsProcessInJob.restype = wintypes.BOOL
        if not self.kernel32.IsProcessInJob(process_handle, self.handle, ctypes.byref(result)):
            raise _error()
        return bool(result.value)

    def terminate(self, exit_code=1):
        self.kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.kernel32.TerminateJobObject.restype = wintypes.BOOL
        if not self.kernel32.TerminateJobObject(self.handle, exit_code):
            raise _error()

    def active_processes(self):
        self.kernel32.QueryInformationJobObject.argtypes = [
            wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID,
            wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
        ]
        self.kernel32.QueryInformationJobObject.restype = wintypes.BOOL
        info = _BasicAccountingInformation()
        if not self.kernel32.QueryInformationJobObject(
            self.handle, 1, ctypes.byref(info), ctypes.sizeof(info), None
        ):
            raise _error()
        return int(info.ActiveProcesses)

    def wait_empty(self, timeout=2):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.active_processes() == 0:
                return True
            time.sleep(0.05)
        return self.active_processes() == 0

    def close(self):
        if self.handle:
            self.kernel32.CloseHandle(self.handle)
            self.handle = wintypes.HANDLE()


def create_job():
    kernel32, _ = _api()
    identity = "Local\\CodexStudioSupervisorJob-" + uuid.uuid4().hex
    descriptor = _SecurityDescriptor("D:P(A;;GA;;;{})".format(current_user_sid()))
    attributes = _SecurityAttributes(ctypes.sizeof(_SecurityAttributes), descriptor.pointer, False)
    kernel32.CreateJobObjectW.argtypes = [ctypes.POINTER(_SecurityAttributes), wintypes.LPCWSTR]
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    handle = kernel32.CreateJobObjectW(ctypes.byref(attributes), identity)
    descriptor.close()
    if not handle:
        raise _error()
    job = Job(handle, identity)
    limits = _ExtendedLimitInformation()
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    kernel32.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD]
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    if not kernel32.SetInformationJobObject(
        job.handle, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
        ctypes.byref(limits), ctypes.sizeof(limits),
    ):
        error = _error()
        job.close()
        raise error
    return job


def open_job(identity):
    kernel32, _ = _api()
    kernel32.OpenJobObjectW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel32.OpenJobObjectW.restype = wintypes.HANDLE
    handle = kernel32.OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, False, identity)
    if not handle:
        return None
    return Job(handle, identity)


def assign_process(process, job):
    kernel32, _ = _api()
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    if not kernel32.AssignProcessToJobObject(job.handle, process._handle):
        raise _error()


def resume_process(process):
    _, _ = _api()
    ntdll = ctypes.WinDLL("ntdll")
    resume = ntdll.NtResumeProcess
    resume.argtypes = [wintypes.HANDLE]
    resume.restype = wintypes.LONG
    status = resume(process._handle)
    if status < 0:
        raise OSError("NtResumeProcess failed with NTSTATUS 0x{:08x}".format(status & 0xFFFFFFFF))


def membership_for_pid(job, pid):
    kernel32, _ = _api()
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenProcess.restype = wintypes.HANDLE
    process = kernel32.OpenProcess(0x0400, False, pid)
    if not process:
        return None
    try:
        return job.contains(process)
    finally:
        kernel32.CloseHandle(process)


def graceful_stop(job, process, pid, creation_time, process_start_time, *,
                  expected_job_identity, wait=2):
    """Send a bounded console stop, then terminate only a verified job tree."""
    if process_start_time(pid) != creation_time or not job.contains(process._handle):
        raise RuntimeError("Refusing to stop a process with changed or unknown job identity")
    kernel32, _ = _api()
    kernel32.GenerateConsoleCtrlEvent.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.GenerateConsoleCtrlEvent.restype = wintypes.BOOL
    kernel32.GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        if process_start_time(pid) != creation_time:
            if job.identity != expected_job_identity:
                raise RuntimeError("Refusing Job Object action after job identity changed")
            if job.active_processes() == 0:
                return "terminated"
            job.terminate()
            process.wait(timeout=2)
            return "killed" if job.wait_empty() else "termination-failed"
        time.sleep(0.05)
    if job.identity != expected_job_identity:
        raise RuntimeError("Refusing Job Object termination after job identity changed")
    if process_start_time(pid) != creation_time:
        if job.active_processes() == 0:
            return "terminated"
        job.terminate()
        process.wait(timeout=2)
        return "killed" if job.wait_empty() else "termination-failed"
    if not job.contains(process._handle):
        raise RuntimeError("Refusing job termination after process identity changed")
    job.terminate()
    process.wait(timeout=2)
    return "killed" if job.wait_empty() else "termination-failed"


def graceful_stop_pid(job, pid, creation_time, process_start_time, *,
                      expected_job_identity, wait=1.5):
    """Stop only a verified job; never signal a PID after closing its handle."""
    if job.identity != expected_job_identity:
        raise RuntimeError("Refusing to stop a process with changed Job Object identity")
    actual = process_start_time(pid)
    if actual is not None and actual != creation_time:
        raise RuntimeError("Refusing to stop a process with changed creation time")
    if actual == creation_time and membership_for_pid(job, pid) is not True:
        raise RuntimeError("Refusing to stop a process with unknown job membership")
    if job.active_processes() == 0:
        return "terminated"
    if job.identity != expected_job_identity:
        raise RuntimeError("Refusing Job Object termination after job identity changed")
    job.terminate()
    return "killed" if job.wait_empty() else "termination-failed"
