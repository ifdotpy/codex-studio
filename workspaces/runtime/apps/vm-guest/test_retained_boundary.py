"""The retained guest must not resolve the removed backend source tree."""
import os
import re
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from common import supervisor_binary
from native_identity import process_start_time


class RetainedBoundary(unittest.TestCase):
    def test_explicit_supervisor_build_is_used_without_a_python_launcher(self):
        binary = '/isolated/build/codex-process-supervisor'
        with patch.dict(os.environ, {'STUDIO_SUPERVISOR_BINARY': binary}):
            self.assertEqual(supervisor_binary(), binary)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux guest process identity')
    def test_linux_identity_matches_the_rust_encoding_owner(self):
        owner = Path(__file__).resolve().parents[2] / 'packages/runtime-process/src/supervisor_identity.rs'
        self.assertIn('format!("linux:{boot_id}:{}", self.start_ticks)', owner.read_text())
        identity = process_start_time(os.getpid())
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        self.assertTrue(identity.startswith(f'linux:{boot}:'))
        self.assertIsNone(process_start_time(0))

    def test_native_protocol_matches_the_rust_owner(self):
        from native import SUPERVISOR_PROTOCOL_V2
        owner = Path(__file__).resolve().parents[2] / 'packages/runtime-providers/src/supervisor_protocol.rs'
        version = re.search(r'pub const SUPERVISOR_PROTOCOL_V2: u64 = (\d+);', owner.read_text())
        self.assertIsNotNone(version)
        self.assertEqual(SUPERVISOR_PROTOCOL_V2, int(version.group(1)))

    def test_guest_source_has_no_backend_import_or_launch(self):
        guest = Path(__file__).parent
        for filename in ('common.py', 'native.py', 'install.sh', 'test_service.py', 'test_linux_integration.py'):
            text = (guest / filename).read_text()
            self.assertNotIn('codex_process_supervisor', text, filename)
            self.assertNotIn('apps/server', text, filename)


if __name__ == '__main__':
    unittest.main()
