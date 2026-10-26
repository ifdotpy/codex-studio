"""In-place VM guest updates from the Mac (docs/linux-vm-workspaces.md, "Guest updates").

The guest side is the real root broker handler (vm-guest/guest_update.py) on a temporary
folder, with a recorded install script and systemctl. No VM, helper or user state is used.
"""
from __future__ import annotations

import asyncio
import importlib
import importlib.util
import json
from pathlib import Path
import shlex
import sys
import tempfile
from typing import Any, cast
import unittest
from unittest.mock import patch

import codex_linux_vm as vm
import codex_linux_vm_update as update
from codex_layout import VM_GUEST_ROOT
from codex_shared_values import GUEST_PAYLOAD_RECORD_PATH, GUEST_UPDATE_KEY_PATH

# The guest modules import each other by name, as in the guest. They are not in the typed
# runtime, so the type checker does not follow this import.
sys.path.append(str(VM_GUEST_ROOT))
guest_update: Any = importlib.import_module('guest_update')

SERVICE = 'vm/guest/service.py'


class Runner:
    """The guest install script and systemctl: recorded, never run."""

    def __init__(self) -> None:
        self.calls: list[tuple[list[str], dict[str, str]]] = []
        self.restart_code = 0

    async def __call__(self, argv: list[str], env: dict[str, str], timeout: float) -> tuple[int, str]:
        self.calls.append((list(argv), dict(env)))
        if argv[-1].endswith('install.sh'):
            return 0, 'installed'
        return self.restart_code, '' if self.restart_code == 0 else 'unit failed'


class Guest:
    """The guest service and its root broker for the update methods, layr.health and
    provider.list. A scheduled broker restart drops the next status request, as a real one does."""

    def __init__(self, root: Path) -> None:
        self.opt = root / 'opt'
        self.key = root / 'update-key'
        self.record = root / 'guest-payload.json'
        self.runner = Runner()
        self.handlers = guest_update.UpdateHandlers(root / 'broker', root / 'layr', opt=self.opt,
                                                    key_path=self.key, record_path=self.record,
                                                    runner=self.runner)
        self.loop = asyncio.new_event_loop()
        self.calls: list[tuple[str, dict[str, Any], str | None]] = []
        self.supported = True
        self.providers: list[dict[str, Any]] = []
        self.restarted = False

    def close(self) -> None:
        for task in self.handlers.tasks:
            task.cancel()
        self.loop.run_until_complete(asyncio.sleep(0))
        self.loop.close()

    def call(self, method: str, params: dict[str, Any], request_id: str | None) -> Any:
        self.calls.append((method, params, request_id))
        if method == 'provider.list':
            return {'providers': self.providers}
        if method == 'layr.health':
            return {'state': 'ready'}
        if not method.startswith('update.'):
            raise AssertionError('unexpected guest method ' + method)
        if not self.supported:
            raise vm.LinuxVMError(f'Linux guest {method}: unsupported', code='invalid_request',
                                  detail='The method is not supported')
        scheduled = any(argv[0] == '/usr/bin/systemd-run' for argv, _ in self.runner.calls)
        if method == 'update.status' and scheduled and not self.restarted:
            self.restarted = True
            # The new broker process proves the restart that the old one scheduled.
            self.handlers.finish_restarts(10 ** 10)
            raise vm.LinuxVMError('Linux guest update.status failed: connection reset', uncertain=True)
        result = self.loop.run_until_complete(self.handlers.dispatch(request_id, method, params, None))
        # Let the restart task of an earlier apply run between two requests.
        self.loop.run_until_complete(asyncio.sleep(0.01))
        return result


class FakeClient(vm.Client):
    guest: Guest

    def call(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
             timeout: float = 60) -> Any:
        return self.guest.call(method, params or {}, request_id)

    def stop(self, *args: Any, **kwargs: Any) -> Any:
        raise AssertionError('The VM must not stop')


class InPlaceUpdateTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='vm-update-')
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.state = root / 'state'
        self.state.mkdir(mode=0o700)
        (self.state / 'image.json').write_text(json.dumps({'codex': '0.160.1', 'claude': '2.1.291'}))
        self.guest = Guest(root / 'guest')
        self.addCleanup(self.guest.close)
        FakeClient.guest = self.guest
        self.client = FakeClient(self.state, helper=root / 'helper')
        # A guest provisioned by this payload: the key, the record and every file.
        self.guest.key.parent.mkdir(parents=True, exist_ok=True)
        self.guest.key.write_text(self.client.update_key() + '\n')
        self.files, record = update.desired(self.client)
        self.guest.record.write_text(json.dumps(record))
        for name, data in self.files.items():
            (self.guest.opt / name).parent.mkdir(parents=True, exist_ok=True)
            (self.guest.opt / name).write_bytes(data)
        path = Path(vm.__file__).with_name('upgrade-linux-vm-layr.py')
        spec = importlib.util.spec_from_file_location('upgrade_linux_vm_layr', path)
        assert spec is not None and spec.loader is not None
        self.upgrade = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.upgrade)

    def run_upgrade(self, operation: str, *, reboot: bool = False) -> dict[str, Any]:
        def client(state: Path, helper: Path) -> FakeClient:
            return FakeClient(state, helper=helper)

        with patch.object(self.upgrade, 'Client', client), \
                patch.object(update, 'POLL_SECONDS', 0):
            return cast(dict[str, Any], self.upgrade.upgrade(
                self.state, self.state.parent / 'helper', self.state.parent, operation, reboot=reboot))

    def methods(self) -> list[str]:
        return [method for method, _, _ in self.guest.calls]

    def receipt(self, operation: str) -> dict[str, Any]:
        return cast(dict[str, Any], json.loads((self.state / 'updates' / operation / 'receipt.json').read_text()))

    def test_a_current_guest_needs_no_change(self) -> None:
        result = self.run_upgrade('current-1')
        self.assertEqual((result['stage'], result['plan']['path']), ('complete', 'current'))
        self.assertEqual(self.methods(), ['update.plan'])
        self.assertEqual(self.receipt('current-1')['stage'], 'complete')

    def test_a_guest_file_change_updates_in_place_and_keeps_the_providers(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        # A provider runs: the in-place path does not ask, stop or back up anything.
        self.guest.providers = [{'state': 'running'}]
        with patch.object(update, 'GUEST_UPDATE_PART_BYTES', 4096):
            result = self.run_upgrade('in-place-1')
        self.assertEqual(result['stage'], 'complete')
        self.assertEqual(result['inPlace']['changed'], [SERVICE])
        restarts = [guest_update.GUEST_UNIT, guest_update.ADMIN_UNIT]
        self.assertEqual(result['inPlace']['restarts'], restarts)
        self.assertEqual((self.guest.opt / SERVICE).read_bytes(), self.files[SERVICE])
        self.assertNotIn('provider.list', self.methods())
        self.assertFalse((self.state / 'updates/in-place-1/backup').exists())
        # Each part has its own exact request ID, so a retry gets the broker's receipt.
        stages = [(params['offset'], request) for method, params, request in self.guest.calls
                  if method == 'update.stage']
        parts = -(-len(self.files[SERVICE]) // 4096)
        self.assertEqual([offset for offset, _ in stages], [index * 4096 for index in range(parts)])
        self.assertEqual(len({request for _, request in stages}), parts)
        # The install restarts nothing; the broker restarts the units from a separate unit.
        commands = self.guest.runner.calls
        self.assertEqual(commands[0][1], {'STUDIO_DEFER_RESTARTS': '1'})
        self.assertEqual(commands[-1][0][0], '/usr/bin/systemd-run')
        self.assertEqual(commands[-1][0][-3:], ['restart', *restarts])
        self.assertTrue(self.guest.restarted)
        self.assertEqual(self.receipt('in-place-1')['inPlace']['status']['stage'], 'complete')

    def test_a_failed_unit_restart_fails_the_update(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        self.guest.runner.restart_code = 1
        with self.assertRaisesRegex(vm.LinuxVMError, 'in-place update failed'):
            self.run_upgrade('in-place-2')
        self.assertEqual(self.receipt('in-place-2')['stage'], 'in-place')
        # The saved receipt holds that operation. A new one finishes its restarts: the file is
        # already written, but the broker keeps it as unfinished work.
        self.guest.runner.restart_code = 0
        self.guest.restarted = False
        self.guest.runner.calls.clear()
        result = self.run_upgrade('in-place-3')
        self.assertEqual((result['stage'], result['inPlace']['changed']), ('complete', [SERVICE]))
        self.assertEqual(self.guest.runner.calls[-1][0][-3:],
                         ['restart', guest_update.GUEST_UNIT, guest_update.ADMIN_UNIT])

    def assert_reboot_path(self, operation: str) -> None:
        # The reboot path keeps its idle-provider preflight before any backup or stop.
        self.guest.providers = [{'state': 'running'}]
        with self.assertRaisesRegex(vm.LinuxVMError, 'Active or unproven'):
            self.run_upgrade(operation)
        self.assertEqual(self.methods(), ['update.plan', 'provider.list'])
        self.assertEqual(self.guest.runner.calls, [])

    def test_a_process_supervisor_change_needs_a_reboot(self) -> None:
        # Running provider processes use the supervisor.
        (self.guest.opt / 'scripts/codex_process_supervisor.py').write_bytes(b'old supervisor\n')
        self.assertEqual(update.plan(self.client)['reboot'], ['scripts/codex_process_supervisor.py'])
        self.guest.calls.clear()
        self.assert_reboot_path('reboot-supervisor')

    def test_new_provider_versions_need_a_reboot(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        self.guest.record.write_text(json.dumps({'codex': 'older'}))
        self.assert_reboot_path('reboot-versions')

    def test_a_guest_without_the_update_key_needs_a_reboot(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        self.guest.key.unlink()
        self.assert_reboot_path('reboot-key')

    def test_a_guest_without_update_methods_needs_a_reboot(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        self.guest.supported = False
        self.assert_reboot_path('reboot-old-guest')

    def test_the_reboot_flag_skips_the_plan(self) -> None:
        (self.guest.opt / SERVICE).write_bytes(b'old service\n')
        self.guest.providers = [{'state': 'running', 'live': True}]
        with self.assertRaisesRegex(vm.LinuxVMError, 'Active or unproven'):
            self.run_upgrade('reboot-flag', reboot=True)
        self.assertEqual(self.methods(), ['provider.list'])

    def test_the_mac_and_the_broker_sign_the_same_text(self) -> None:
        key = self.client.update_key()
        manifest = {SERVICE: 'a' * 64, 'vm/layr/src/main.rs': 'b' * 64}
        self.assertEqual(update.signature(key, 'op-1', manifest),
                         guest_update.signature(key.encode(), 'op-1', dict(reversed(manifest.items()))))
        self.assertNotEqual(update.signature(key, 'op-1', manifest), update.signature(key, 'op-2', manifest))

    def test_the_update_key_is_made_once_for_the_owner_only(self) -> None:
        key = self.client.update_key()
        self.assertEqual((self.state / 'update-key').stat().st_mode & 0o777, 0o600)
        self.assertEqual(FakeClient(self.state, helper=self.client.helper).update_key(), key)
        (self.state / 'update-key').write_text('not a key\n')
        with self.assertRaisesRegex(vm.LinuxVMError, 'update key file is invalid'):
            self.client.update_key()

    def test_provisioning_writes_the_key_and_the_record_that_the_plan_compares(self) -> None:
        key = self.client.update_key()
        config = vm._cloud_config(VM_GUEST_ROOT, '0.160.1', '2.1.291', update_key=key)
        entries = {entry['path']: entry for entry in config['write_files']}
        self.assertEqual((entries[GUEST_UPDATE_KEY_PATH]['content'], entries[GUEST_UPDATE_KEY_PATH]['permissions']),
                         (key + '\n', '0600'))
        script = entries['/opt/codex-studio/provision.sh']['content']
        line = next(row for row in script.splitlines() if row.endswith('> ' + GUEST_PAYLOAD_RECORD_PATH))
        written = json.loads(shlex.split(line)[2])
        _, record = update.desired(self.client)
        self.assertEqual(written, record)
        # Without a key (an older caller) provisioning writes no key file.
        plain = vm._cloud_config(VM_GUEST_ROOT, '0.160.1', '2.1.291')
        self.assertNotIn(GUEST_UPDATE_KEY_PATH, {entry['path'] for entry in plain['write_files']})

if __name__ == '__main__':
    unittest.main()
