"""VM settings API contracts, without a helper or live user state."""
import copy
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient

from .models import LinuxVMSettingsResponse
from .router import create_router


GIB = 1024 ** 3
SETTINGS = {'cpus': 4, 'memoryBytes': 4 * GIB,
            'systemDiskBytes': 16 * GIB, 'dataDiskBytes': 128 * GIB}


class FakeVM:
    def __init__(self):
        self.settings = copy.deepcopy(SETTINGS)
        self.state = 'stopped'
        self.writes = []

    def get_settings(self):
        return self.settings

    def status(self):
        return {'state': self.state, 'allocatedDiskBytes': 1024}

    def set_settings(self, values):
        if self.state != 'stopped':
            raise RuntimeError('Stop the Linux VM before changing its resource limits.')
        self.writes.append(values)
        self.settings = values
        return values


class VMSettingsApi(unittest.TestCase):
    def setUp(self):
        self.vm = FakeVM()
        self.patcher = patch.dict('sys.modules', {'codex_linux_vm': SimpleNamespace(connect=lambda: self.vm)})
        self.patcher.start()
        self.addCleanup(self.patcher.stop)
        def send(_request, value):
            return JSONResponse(LinuxVMSettingsResponse.model_validate(value).model_dump(mode='json'))
        app = FastAPI()
        app.include_router(create_router(SimpleNamespace(send=send)))
        self.client = TestClient(app)

    def test_read_reports_settings_without_a_write(self):
        response = self.client.get('/api/linux-vm/settings')
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()['settings'], SETTINGS)
        self.assertEqual(response.json()['allocatedDiskBytes'], 1024)
        self.assertEqual(self.vm.writes, [])

    def test_write_uses_the_host_settings_api(self):
        values = {**SETTINGS, 'cpus': 2}
        response = self.client.post('/api/linux-vm/settings', json=values)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()['settings'], values)
        self.assertEqual(self.vm.writes, [values])

    def test_invalid_values_do_not_reach_the_host(self):
        for key, value in [('cpus', True), ('cpus', 0), ('memoryBytes', 1),
                           ('dataDiskBytes', 2048 * GIB), ('systemDiskBytes', 1.5)]:
            response = self.client.post('/api/linux-vm/settings', json={**SETTINGS, key: value})
            self.assertEqual(response.status_code, 422)
        self.assertEqual(self.vm.writes, [])

    def test_live_vm_is_never_stopped_to_save_settings(self):
        self.vm.state = 'running'
        response = self.client.post('/api/linux-vm/settings', json=SETTINGS)
        self.assertEqual(response.status_code, 400)
        self.assertIn('Stop the Linux VM', response.json()['detail'])
        self.assertEqual(self.vm.writes, [])
        self.assertEqual(self.vm.state, 'running')


if __name__ == '__main__':
    unittest.main()
