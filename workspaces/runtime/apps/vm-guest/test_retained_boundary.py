"""The retained guest must not resolve the removed backend source tree."""
import os
import re
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from native_identity import process_start_time


class RetainedBoundary(unittest.TestCase):
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

if __name__ == '__main__':
    unittest.main()
