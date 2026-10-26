"""Manual macOS FSEvents delivery check; depends on the host event service."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
import codex_vm_mac_sync as sync
from server_test_waits import wait_for_condition


class NativeFileEvents(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    @unittest.skipUnless(sys.platform == 'darwin' and shutil.which('xcrun'), 'needs macOS and Swift')
    def test_the_signed_helper_reports_file_events(self) -> None:
        from codex_layout import DESKTOP_ROOT
        helper = self.root / 'studio-fs-events'
        subprocess.run(['xcrun', 'swiftc', str(DESKTOP_ROOT / 'native/fs-events.swift'), '-O',
                        '-target', 'arm64-apple-macos13.0', '-o', str(helper)], check=True, timeout=300)
        folder = self.root / 'project'
        folder.mkdir()
        watcher = sync.Watcher(helper)
        self.addCleanup(watcher.stop)
        with patch.object(sync, 'QUIET', 0.3):
            watcher.watch([folder])
            wait_for_condition(watcher.ready.is_set, 'file event helper readiness')
            # Drain the initial full scan before creating the change under test.
            wait_for_condition(lambda: watcher.take(folder)[0], 'initial watcher scan')
            (folder / 'sub').mkdir()
            (folder / 'sub/new.txt').write_text('x')
            found: set[str] = set()
            def collect_changed_path() -> bool | None:
                changed, paths = watcher.take(folder)
                if changed:
                    # FSEvents may coalesce a leaf event into a directory or a
                    # full rescan. Consume that scope as the sync worker does.
                    rules = sync.Rules(folder)
                    entries, _ = (sync.scan(folder, rules, {}) if paths is None
                                  else sync.scan_paths(folder, rules, {}, paths))
                    found.update(entries)
                return True if 'sub/new.txt' in found else None
            wait_for_condition(collect_changed_path, 'signed file event helper notification')
        watcher.stop()
        self.assertIn('sub/new.txt', found)


if __name__ == '__main__':
    unittest.main()
