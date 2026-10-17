"""Rosetta for Linux in the VM (docs/linux-vm-workspaces.md, "x86_64 binaries").

The guest side is the real guest module (vm-guest/rosetta.py) on temporary folders, with a
recorded mount, dpkg and apt. No VM, helper, network or user state is used.
"""
from __future__ import annotations

import configparser
import importlib
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any
import unittest

import codex_linux_vm as vm
from codex_layout import REPOSITORY_ROOT, VM_GUEST_ROOT
from codex_shared_values import LINUX_VM_ROSETTA_TAG

# The guest modules import each other by name, as in the guest. They are not in the typed
# runtime, so the type checker does not follow these imports.
sys.path.append(str(VM_GUEST_ROOT))
rosetta: Any = importlib.import_module('rosetta')
guest_update: Any = importlib.import_module('guest_update')

HELPER = REPOSITORY_ROOT / 'workspaces/client/apps/desktop/native/linux-vm'


def unescape(value: str) -> bytes:
    """Decode the binfmt_misc magic and mask notation: \\xHH escapes and plain characters."""
    return value.encode().decode('unicode_escape').encode('latin-1')


class Recorder:
    """mount, dpkg-query, dpkg and apt-get: recorded, never run."""

    def __init__(self, *, mount_code: int = 0, mount: Path | None = None, installed: bool = False,
                 failing: str | None = None) -> None:
        self.calls: list[list[str]] = []
        self.mount_code, self.mount, self.installed, self.failing = mount_code, mount, installed, failing

    def __call__(self, argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
        self.calls.append(argv)
        code, stdout = 0, ''
        if argv[:3] == ['mount', '-t', 'virtiofs']:
            code = self.mount_code
            if code == 0 and self.mount is not None:
                interpreter = self.mount / 'rosetta'
                interpreter.write_text('')
                interpreter.chmod(0o755)
        elif argv[0] == 'dpkg-query':
            code, stdout = (0, 'install ok installed') if self.installed else (1, '')
        elif self.failing is not None and argv[:2] == self.failing.split():
            code = 100
        return subprocess.CompletedProcess(argv, code, stdout, 'E: recorded failure' if code else '')


class RosettaTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory(prefix='vm-rosetta-')
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.mount = self.root / 'media/rosetta'
        self.binfmt = self.root / 'binfmt_misc'
        self.binfmt.mkdir()
        (self.binfmt / 'register').write_text('')

    def kernel_entry(self, interpreter: Path, flags: str = 'F') -> None:
        """What the kernel shows for a registered handler."""
        (self.binfmt / 'rosetta').write_text(
            f'enabled\ninterpreter {interpreter}\nflags: {flags}\noffset 0\nmagic 7f454c46\nmask ffffffff\n')

    def test_the_handler_matches_x86_64_executables_and_shared_objects_only(self) -> None:
        rule = rosetta.binfmt_rule(Path('/media/rosetta/rosetta'))
        _, name, kind, offset, magic, mask, interpreter, flags = rule.split(':')
        self.assertEqual((name, kind, offset, interpreter, flags),
                         ('rosetta', 'M', '', '/media/rosetta/rosetta', 'F'))
        magic_bytes, mask_bytes = unescape(magic), unescape(mask)
        self.assertEqual((len(magic_bytes), len(mask_bytes)), (20, 20))

        def header(elf_class: int, e_type: int, machine: int) -> bytes:
            return (b'\x7fELF' + bytes([elf_class, 1, 1]) + bytes(9)
                    + e_type.to_bytes(2, 'little') + machine.to_bytes(2, 'little'))

        def matches(data: bytes) -> bool:
            return all(byte & bit == want & bit for byte, bit, want in zip(data, mask_bytes, magic_bytes))

        self.assertTrue(matches(header(2, 2, 0x3e)))  # ET_EXEC, x86_64
        self.assertTrue(matches(header(2, 3, 0x3e)))  # ET_DYN (PIE), x86_64
        self.assertFalse(matches(header(2, 3, 0xb7)))  # arm64 stays native
        self.assertFalse(matches(header(1, 2, 0x3e)))  # 32-bit
        self.assertFalse(matches(header(2, 1, 0x3e)))  # ET_REL object file

    def test_without_the_share_activation_succeeds_and_registers_nothing(self) -> None:
        runner = Recorder(mount_code=32)
        self.assertEqual(rosetta.activate(mount=self.mount, binfmt=self.binfmt, runner=runner), 0)
        self.assertEqual(runner.calls, [['mount', '-t', 'virtiofs', '-o', 'ro', LINUX_VM_ROSETTA_TAG,
                                         str(self.mount)]])
        self.assertEqual((self.binfmt / 'register').read_text(), '')

    def test_the_share_is_mounted_and_registered_once(self) -> None:
        runner = Recorder(mount=self.mount)
        self.assertEqual(rosetta.activate(mount=self.mount, binfmt=self.binfmt, runner=runner), 0)
        interpreter = self.mount / 'rosetta'
        self.assertEqual((self.binfmt / 'register').read_text(), rosetta.binfmt_rule(interpreter))
        # A second boot step with the same handler writes nothing.
        self.kernel_entry(interpreter)
        (self.binfmt / 'register').write_text('')
        self.assertEqual(rosetta.activate(mount=self.mount, binfmt=self.binfmt, runner=runner), 0)
        self.assertEqual((self.binfmt / 'register').read_text(), '')

    def test_a_handler_with_other_flags_is_replaced(self) -> None:
        runner = Recorder(mount=self.mount)
        self.kernel_entry(self.mount / 'rosetta', flags='OCF')
        self.assertEqual(rosetta.activate(mount=self.mount, binfmt=self.binfmt, runner=runner), 0)
        self.assertEqual((self.binfmt / 'rosetta').read_text(), '-1')
        self.assertEqual((self.binfmt / 'register').read_text(), rosetta.binfmt_rule(self.mount / 'rosetta'))

    def test_a_share_without_the_translator_is_a_visible_failure(self) -> None:
        runner = Recorder()
        self.assertEqual(rosetta.activate(mount=self.mount, binfmt=self.binfmt, runner=runner), 1)
        self.assertEqual((self.binfmt / 'register').read_text(), '')

    def test_the_amd64_runtime_comes_from_the_amd64_archive(self) -> None:
        os_release = self.root / 'os-release'
        os_release.write_text('NAME="Ubuntu"\nVERSION_CODENAME=noble\n')
        sources = self.root / 'codex-studio-amd64.sources'
        runner = Recorder()
        self.assertEqual(rosetta.install_runtime(sources=sources, os_release=os_release, runner=runner), 0)
        packages = ['libc6:amd64', 'libstdc++6:amd64', 'zlib1g:amd64']
        self.assertEqual([call for call in runner.calls if call[0] != 'dpkg-query'], [
            ['dpkg', '--add-architecture', 'amd64'],
            ['apt-get', 'update'],
            ['apt-get', 'install', '-y', '--no-install-recommends', '-o', 'DPkg::Lock::Timeout=300', *packages]])
        stanzas = [dict(line.split(': ', 1) for line in block.splitlines())
                   for block in sources.read_text().strip().split('\n\n')]
        self.assertEqual([(row['URIs'], row['Suites']) for row in stanzas], [
            ('http://archive.ubuntu.com/ubuntu', 'noble noble-updates'),
            ('http://security.ubuntu.com/ubuntu', 'noble-security')])
        for row in stanzas:
            self.assertEqual((row['Types'], row['Components'], row['Architectures'], row['Signed-By']),
                             ('deb', 'main', 'amd64', '/usr/share/keyrings/ubuntu-archive-keyring.gpg'))
        self.assertEqual(sources.stat().st_mode & 0o777, 0o644)

    def test_an_installed_runtime_needs_no_network(self) -> None:
        runner = Recorder(installed=True)
        self.assertEqual(rosetta.install_runtime(sources=self.root / 'unused.sources',
                                                 os_release=self.root / 'missing', runner=runner), 0)
        self.assertEqual({call[0] for call in runner.calls}, {'dpkg-query'})
        self.assertFalse((self.root / 'unused.sources').exists())

    def test_a_failed_apt_step_stops_and_reports(self) -> None:
        os_release = self.root / 'os-release'
        os_release.write_text('VERSION_CODENAME=noble\n')
        runner = Recorder(failing='apt-get update')
        self.assertEqual(rosetta.install_runtime(sources=self.root / 'amd64.sources', os_release=os_release,
                                                 runner=runner), 1)
        self.assertNotIn('install', [call[1] for call in runner.calls if call[0] == 'apt-get'])
        with self.assertRaises(ValueError):
            rosetta.amd64_sources('noble\nURIs: http://example.invalid')


class RosettaPayloadTests(unittest.TestCase):
    def test_provisioning_and_in_place_updates_install_the_guest_side(self) -> None:
        config = vm._cloud_config(VM_GUEST_ROOT, '0.160.1', '2.1.291')
        paths = {entry['path'] for entry in config['write_files']}
        for name in ('rosetta.py', 'codex-studio-rosetta.service', 'shared_values_generated.py'):
            self.assertIn('/opt/codex-studio/vm/guest/' + name, paths)
        # An existing VM takes these files without a reboot, and the unit runs again.
        plan = guest_update.classify(['vm/guest/rosetta.py', 'vm/guest/codex-studio-rosetta.service'])
        self.assertEqual(plan['reboot'], [])
        self.assertTrue(plan['install'])
        self.assertIn('codex-studio-rosetta.service', plan['restarts'])

    def test_the_unit_and_the_install_keep_rosetta_optional(self) -> None:
        unit = configparser.ConfigParser(interpolation=None)
        unit.optionxform = str  # type: ignore[assignment,method-assign]
        unit.read_string((VM_GUEST_ROOT / 'codex-studio-rosetta.service').read_text())
        self.assertEqual(unit['Service']['ExecStart'],
                         '/usr/bin/python3 /opt/codex-studio/vm/guest/rosetta.py activate')
        self.assertEqual(unit['Install']['WantedBy'], 'multi-user.target')
        self.assertNotIn('Requires', unit['Unit'])
        install = (VM_GUEST_ROOT / 'install.sh').read_text().splitlines()
        self.assertIn('install -m 0644 "$script_dir/codex-studio-rosetta.service" '
                      '/etc/systemd/system/codex-studio-rosetta.service', install)
        enable = next(line for line in install if 'enable --now codex-studio-rosetta.service' in line)
        runtime = next(line for line in install if 'rosetta.py" install-runtime' in line)
        # set -e must not stop the guest install for an optional capability.
        self.assertIn(' || ', enable)
        self.assertIn(' || ', runtime)

    def test_the_helper_attaches_the_share_with_the_generated_tag(self) -> None:
        source = (HELPER / 'main.swift').read_text()
        self.assertIn('VZVirtioFileSystemDeviceConfiguration(tag: LINUX_VM_ROSETTA_TAG)', source)
        self.assertIn('if VZLinuxRosettaDirectoryShare.availability == .installed {', source)
        self.assertNotIn('installRosetta', source)
        self.assertIn('"rosetta": rosetta]', source)
        self.assertIn(f'let LINUX_VM_ROSETTA_TAG = "{LINUX_VM_ROSETTA_TAG}"',
                      (HELPER / 'shared_values_generated.swift').read_text())
        self.assertIn('"shared_values_generated.swift"', (HELPER / 'build.mjs').read_text())


if __name__ == '__main__':
    unittest.main()
