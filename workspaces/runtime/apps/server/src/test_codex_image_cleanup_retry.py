"""A failed image cleanup is repeated: removal for a deleted chat, unmount for an archived worker."""
import importlib
from pathlib import Path
import tempfile
import time
from typing import Any, cast
import unittest
from unittest.mock import Mock, patch

# runtime_test_environment puts the server test helpers on the import path.
from runtime_test_environment import make_runtime

importlib.import_module('test_isolation').isolate_supervisor_environment()

import codex_image_cleanup_retry as cleanup
from codex_records import AgentRecord
from codex_runtime import Runtime


class NoNativeServer:
    def __init__(self, *_args: object, **_kwargs: object) -> None:
        raise AssertionError('This fixture must not start a native process')


class ImageCleanupRetry(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='studio-image-cleanup-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        with patch.object(Runtime, 'schedule', lambda _runtime: None), \
                patch.object(Runtime, '_restore_startup_supervisor_handles', lambda _runtime: None):
            self.runtime = make_runtime(Runtime, self.root, NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.removed = Mock(return_value={'state': 'removed'})
        patch.object(self.runtime, 'remove_image_workspace', self.removed).start()
        self.archived = Mock(return_value={'freedBytes': 0, 'state': 'archived'})
        patch('codex_workspace_images.archive_workspace', self.archived).start()
        self.addCleanup(patch.stopall)

    def agent(self, key: str, **values: object) -> None:
        now = time.time()
        record = {'id': key, 'rootId': key, 'parentId': None, 'name': key, 'isLead': False,
                  'role': 'worker', 'provider': 'codex', 'accountKey': 'default', 'epoch': 1,
                  'status': 'paused', 'autoWake': False, 'inFlight': False, 'created': now - 100,
                  'events': 0, 'cwd': str(self.root), 'prompt': 'Fixture', 'model': 'fixture',
                  'effort': 'medium', 'tokensUsed': 0, 'tokenBudget': None, 'tail': '',
                  'imageWorkspace': True, **values}
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', cast(AgentRecord, record))

    def saved(self, key: str) -> dict[str, Any]:
        return dict(self.runtime.agent(key))

    def run_tick(self, now: float) -> None:
        cleanup.tick(self.runtime, now)
        worker = self.runtime.__dict__.get('_image_cleanup_worker')
        if worker is not None:
            worker.join(10)

    def test_a_failed_removal_of_a_deleted_chat_is_repeated_once(self) -> None:
        self.agent('deleted', deletedAt=time.time(), imageWorkspacePhase='archived',
                   imageWorkspaceError=cleanup.REMOVAL_FAILED + 'diskutil timed out')
        self.run_tick(10_000)
        self.removed.assert_called_once_with('deleted')
        saved = self.saved('deleted')
        self.assertEqual(saved['imageWorkspacePhase'], 'removed')
        self.assertNotIn('imageWorkspaceError', saved)
        self.run_tick(20_000)
        self.removed.assert_called_once_with('deleted')

    def test_restorable_and_live_images_are_never_removed(self) -> None:
        # A lead archive keeps the image for a restore; a live chat keeps its image.
        self.agent('archived', deletedAt=time.time(), imageWorkspacePhase='archived',
                   agentArchive={'at': 1, 'by': 'lead', 'reason': 'done', 'epoch': 1,
                                 'cleanupPending': False})
        self.agent('live', imageWorkspacePhase='ready',
                   imageWorkspaceError=cleanup.REMOVAL_FAILED + 'old error')
        self.run_tick(10_000)
        self.removed.assert_not_called()
        self.archived.assert_not_called()

    def test_a_failed_archive_unmount_is_repeated_and_keeps_the_image(self) -> None:
        self.agent('worker', deletedAt=time.time(), imageWorkspacePhase='archiving',
                   imageWorkspaceReady=True,
                   cleanedImageWorkspace={'phase': 'archiving', 'source': '/repo', 'path': '/mnt/repo',
                                          'mount': '/mnt', 'createdAt': 1, 'bytes': None},
                   agentArchive={'at': 1, 'by': 'lead', 'reason': 'done', 'epoch': 1,
                                 'cleanupPending': True},
                   imageWorkspaceError=cleanup.ARCHIVE_FAILED + 'diskutil timed out')
        self.run_tick(10_000)
        self.archived.assert_called_once_with('worker')
        self.removed.assert_not_called()
        saved = self.saved('worker')
        self.assertEqual(saved['imageWorkspacePhase'], 'archived')
        self.assertFalse(saved['agentArchive']['cleanupPending'])
        self.assertNotIn('imageWorkspaceError', saved)

    def test_failures_back_off_and_stop_after_the_last_attempt(self) -> None:
        self.removed.side_effect = RuntimeError('still busy')
        self.agent('stuck', deletedAt=time.time(), imageWorkspacePhase='archived',
                   imageWorkspaceError=cleanup.REMOVAL_FAILED + 'diskutil timed out')
        self.assertEqual(cleanup.retry(self.runtime, 'stuck', 10_000.0), 'failed')
        retry = self.saved('stuck')['imageWorkspaceCleanupRetry']
        self.assertEqual((retry['attempts'], retry['nextAt']), (1, 10_300.0))
        self.assertEqual(cleanup.retry(self.runtime, 'stuck', 10_001.0), 'skipped')
        # Delays of 10, 20, 40 and 80 minutes, then no further attempt.
        for attempt, at, next_at in (
            (2, 10_300.0, 10_900.0),
            (3, 10_900.0, 12_100.0),
            (4, 12_100.0, 14_500.0),
            (5, 14_500.0, 19_300.0),
            (6, 19_300.0, 28_900.0),
        ):
            self.assertEqual(cleanup.retry(self.runtime, 'stuck', at), 'failed')
            retry = self.saved('stuck')['imageWorkspaceCleanupRetry']
            self.assertEqual((retry['attempts'], retry['nextAt']), (attempt, next_at))
        self.assertEqual(cleanup.retry(self.runtime, 'stuck', 28_900.0 + 10**9), 'skipped')
        self.assertEqual(self.removed.call_count, 6)


if __name__ == '__main__':
    unittest.main()
