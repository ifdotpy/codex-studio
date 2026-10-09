#!/usr/bin/env python3
"""Caller-level native Windows server exec contract."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest

if os.name != 'nt':
    raise SystemExit('This contract runs on Windows')

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location(
    'server_exec_fixture', Path(__file__).with_name('server-exec-signed-integration.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
Commands = fixture.Commands


class WindowsExecContract(unittest.TestCase):
    setUp = Commands.setUp
    stop_supervisors = Commands.stop_supervisors
    call = Commands.call
    start = Commands.start
    read = Commands.read
    finish = Commands.finish
    tool = Commands.tool
    drain = Commands.drain
    exchange = Commands.exchange

    def process_active(self, pid):
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel32.OpenProcess.restype = wintypes.HANDLE
        kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel32.GetExitCodeProcess.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        handle = kernel32.OpenProcess(0x1000, False, pid)
        if not handle:
            return False
        try:
            code = wintypes.DWORD()
            self.assertTrue(kernel32.GetExitCodeProcess(handle, ctypes.byref(code)))
            return code.value == 259
        finally:
            kernel32.CloseHandle(handle)

    def wait_for_output(self, handle, marker, timeout=8):
        deadline = time.monotonic() + timeout
        value = None
        while time.monotonic() < deadline:
            value = self.read(handle)
            if marker in value['stdout']:
                return value
            time.sleep(.05)
        self.fail(f'Command output did not contain {marker!r}: {value!r}')

    def test_exec_read_input_close_and_cancel(self):
        source = ('import sys;print("READY",flush=True);'
                  'value=sys.stdin.readline();print("INPUT="+value.strip(),flush=True)')
        result = self.start([sys.executable, '-u', '-c', source], timeout=20)
        self.assertIn('value', result, result)
        handle = result['value']['handle']
        self.wait_for_output(handle, 'READY')
        self.call('exec_input', 'windows-input', server=self.b.server_id, handle=handle,
                  input='hello Ω\n', close_stdin=True)
        finished = self.finish(handle)
        self.assertEqual(finished['status'], 'completed')
        self.assertIn('INPUT=hello Ω', finished['stdout'])
        page = self.read(handle, stdout_offset=0)
        self.assertIn('READY', page['stdout'])

        command = [sys.executable, '-u', '-c', 'import time;print("CANCEL_READY",flush=True);time.sleep(60)']
        active = self.start(command, key='cancel-command', timeout=30)
        handle = active['value']['handle']
        self.wait_for_output(handle, 'CANCEL_READY')
        accepted = self.call('exec_cancel', 'windows-cancel', server=self.b.server_id, handle=handle)
        self.assertTrue(accepted['value']['accepted'])
        self.assertEqual(self.finish(handle)['status'], 'cancelled')

    def test_timeout_kills_job_tree_including_grandchild(self):
        marker = self.b.folder / 'windows-grandchild.pid'
        source = ('import subprocess,sys,time;'
                  'child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]);'
                  f'open({str(marker)!r},"w").write(str(child.pid));'
                  'print(child.pid,flush=True);time.sleep(60)')
        result = self.start([sys.executable, '-u', '-c', source], timeout=15)
        self.assertIn('value', result, result)
        value = result['value']
        handle = value['handle']
        deadline = time.monotonic() + 12
        while value['status'] in {'starting', 'running'} and not marker.exists():
            self.assertLess(time.monotonic(), deadline, value)
            time.sleep(.1)
            value = self.read(handle)
        if value['status'] in {'starting', 'running'}:
            value = self.finish(handle, timeout=20)
        self.assertTrue(value['timedOut'], value)
        self.assertEqual(value['status'], 'completed')
        pid = int(marker.read_text(encoding='ascii'))
        deadline = time.monotonic() + 5
        while self.process_active(pid) and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertFalse(self.process_active(pid), f'job descendant {pid} is still active')


if __name__ == '__main__':
    unittest.main(defaultTest='WindowsExecContract', verbosity=2)
