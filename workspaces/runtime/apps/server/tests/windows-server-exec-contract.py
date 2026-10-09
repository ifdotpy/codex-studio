#!/usr/bin/env python3
"""Caller-level native Windows server exec contract."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest

if os.name != 'nt':
    raise SystemExit('This contract runs on Windows')

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
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

    def test_backend_restart_reattaches_read_input_and_cancel(self):
        marker = self.b.folder / 'reattach-count'
        source = ('from pathlib import Path;import sys,time;'
                  f'path=Path({str(marker)!r});path.write_text(path.read_text()+"x" if path.exists() else "x");'
                  'print("REATTACH_READY",flush=True);'
                  'value=sys.stdin.readline();print("REATTACH_INPUT="+value.strip(),flush=True);time.sleep(60)')
        response = self.start([sys.executable, '-u', '-c', source], timeout=120)
        handle = response['value']['handle']
        self.wait_for_output(handle, 'REATTACH_READY')
        from codex_process_supervisor import status
        adapter_handle = 'server-command:' + hashlib.sha256(handle.encode()).hexdigest()
        before = next(row for row in status(self.b.state)['handles'] if row['id'] == adapter_handle)

        self.b.restart()

        after = next(row for row in status(self.b.state)['handles'] if row['id'] == adapter_handle)
        self.assertEqual(after['pid'], before['pid'])
        self.assertIn('REATTACH_READY', self.read(handle)['stdout'])
        accepted = self.call('exec_input', 'reattach-input', server=self.b.server_id,
                             handle=handle, input='continued\n')
        self.assertTrue(accepted['value']['accepted'], accepted)
        self.wait_for_output(handle, 'REATTACH_INPUT=continued')
        stopped = self.call('exec_cancel', 'reattach-cancel', server=self.b.server_id, handle=handle)
        self.assertTrue(stopped['value']['accepted'], stopped)
        self.assertEqual(self.finish(handle)['status'], 'cancelled')
        self.assertEqual(marker.read_text(encoding='ascii'), 'x')

    def test_powershell_string_command(self):
        response = self.start('Write-Output "WINDOWS_STRING_COMMAND"', timeout=20)
        value = self.finish(response['value']['handle'])
        self.assertEqual(value['status'], 'completed', value)
        self.assertIn('WINDOWS_STRING_COMMAND', value['stdout'])

    def test_batch_shims_quote_shell_metacharacters_and_reject_unsafe_values(self):
        shim = self.b.folder / 'npm.cmd'
        shim.write_text('@echo off\r\necho SAFE_BATCH\r\n', encoding='ascii')
        response = self.start([str(shim), '--version', '&echo', 'WINDOWS_ARGV_INJECTION'],
                              key='batch-injection-check', timeout=20)
        value = self.finish(response['value']['handle'])
        self.assertEqual(value['stdout'].replace('\r\n', '\n'), 'SAFE_BATCH\n', value)

        safe_args = ['left&right', 'pipe|value', 'less<value', 'greater>value', 'caret^value', 'space value']
        response = self.start([str(shim), *safe_args], key='batch-meta-check', timeout=20)
        value = self.finish(response['value']['handle'])
        self.assertEqual(value['stdout'].replace('\r\n', '\n'), 'SAFE_BATCH\n', value)

        for index, unsafe in enumerate(('percent%value', 'bang!value', 'quote"value', 'line\nbreak')):
            with self.subTest(argument=unsafe):
                refused = self.start([str(shim), unsafe], key='batch-unsafe-' + str(index), timeout=20)
                self.assertEqual(refused['outcome'], 'not_applied', refused)
                self.assertIn('Batch command arguments cannot contain', refused['error'])

    def test_output_saturation_drains_pipes_and_stops_reader_threads(self):
        source = ('import os;data=b"x"*2097152;os.write(1,data);os.write(2,data)')
        response = self.start([sys.executable, '-u', '-c', source], timeout=45, output_limit=4096)
        value = self.finish(response['value']['handle'], timeout=55)
        self.assertEqual(value['status'], 'completed', value)
        self.assertEqual(value['stdoutBytes'], 2097152)
        self.assertEqual(value['stderrBytes'], 2097152)
        self.assertTrue(value['stdoutTruncated'])
        self.assertTrue(value['stderrTruncated'])
        self.assertTrue(value['readerThreadsStopped'], value)

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
        self.assertGreaterEqual(value['stoppedDescendants'], 1, value)
        pid = int(marker.read_text(encoding='ascii'))
        deadline = time.monotonic() + 5
        while self.process_active(pid) and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertFalse(self.process_active(pid), f'job descendant {pid} is still active')


if __name__ == '__main__':
    unittest.main(defaultTest='WindowsExecContract', verbosity=2)
