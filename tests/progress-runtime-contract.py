#!/usr/bin/env python3
"""Progress context and file access follow the existing agent permission mode."""
from contextlib import nullcontext
import importlib.util
import os
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('progress_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ProgressRuntimeContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update

    def test_only_this_agents_progress_directory_is_added_to_workspace_write(self):
        lead = self.agent_update(self.lead(), yoloMode=False)
        other = self.worker(lead, role='implementer')
        for actor in (lead, other):
            params = self.runtime.new_thread_params(actor)
            path = self.runtime.progress_file(actor)
            self.assertTrue(path.is_file())
            self.assertIn(str(path), params['developerInstructions'])
            self.assertEqual(params['sandbox'], 'workspace-write')
            self.assertEqual(params['config']['sandbox_workspace_write.writable_roots'], [str(path.parent)])
            self.assertEqual(self.runtime.turn_permissions(actor), {
                'approvalPolicy': 'on-request', 'sandboxPolicy': {
                    'type': 'workspaceWrite', 'writableRoots': [actor['cwd'], str(path.parent)],
                    'networkAccess': False}})
        self.assertNotEqual(self.runtime.progress_file(lead).parent, self.runtime.progress_file(other).parent)

    def test_read_only_and_full_access_modes_keep_their_meaning(self):
        lead = self.agent_update(self.lead(), yoloMode=False)
        reviewer = self.worker(lead, role='reviewer')
        params = self.runtime.new_thread_params(reviewer)
        self.assertEqual(params['sandbox'], 'read-only')
        self.assertNotIn('sandbox_workspace_write.writable_roots', params['config'])
        self.assertEqual(self.runtime.turn_permissions(reviewer), {
            'approvalPolicy': 'on-request', 'sandboxPolicy': {'type': 'readOnly'}})
        full = {**lead, 'yoloMode': True}
        self.assertEqual(self.runtime.turn_permissions(full), {
            'approvalPolicy': 'never', 'sandboxPolicy': {'type': 'dangerFullAccess'}})
        self.assertNotIn('sandbox_workspace_write.writable_roots', self.runtime.new_thread_params(full)['config'])
        legacy = dict(lead); legacy.pop('yoloMode')
        self.assertEqual(self.runtime.turn_permissions(legacy), {})

    def test_invalid_progress_storage_does_not_block_turns_or_grant_invalid_roots(self):
        for kind in ('fifo', 'symlink', 'permission'):
            with self.subTest(kind=kind):
                actor = self.agent_update(self.lead('Progress ' + kind), yoloMode=False)
                path = self.runtime.progress_file(actor)
                preserved = 'Existing content must stay intact\n'
                target = self.project / (kind + '-target.md')
                target.write_text(preserved)
                if kind == 'fifo':
                    path.unlink()
                    os.mkfifo(path)
                elif kind == 'symlink':
                    path.unlink()
                    path.symlink_to(target)
                else:
                    path.write_text(preserved)
                before = path.lstat()
                original_open = os.open
                def denied(name, *args, **kwargs):
                    if name == 'PROGRESS.md':
                        raise PermissionError('fixture progress access denied')
                    return original_open(name, *args, **kwargs)
                guard = patch('codex_progress.os.open', side_effect=denied) if kind == 'permission' else nullcontext()
                with guard:
                    params = self.runtime.new_thread_params(actor)
                    self.assertEqual(params['sandbox'], 'workspace-write')
                    self.assertNotIn('sandbox_workspace_write.writable_roots', params['config'])
                    self.assertIn(str(path), params['developerInstructions'])
                    self.assertEqual(self.runtime.turn_permissions(actor)['sandboxPolicy'], {
                        'type': 'workspaceWrite', 'writableRoots': [actor['cwd']], 'networkAccess': False})
                    with self.runtime.lock, self.runtime.db() as db:
                        event = 'progress-invalid-' + kind
                        self.runtime.enqueue(db, actor, 'user', 'Continue the real task', event)
                        context = self.runtime.model_turn_context(db, actor, event)
                        self.assertIn(str(path), context)
                    panel = self.runtime.get_panel(actor['id'])
                    self.assertTrue(panel['error'])
                    self.assertEqual(panel['markdown'], '')
                    self.assertIsNone(panel['revision'])
                after = path.lstat()
                self.assertEqual((after.st_ino, after.st_mode), (before.st_ino, before.st_mode))
                self.assertEqual(target.read_text(), preserved)
                if kind == 'symlink':
                    self.assertEqual(path.readlink(), target)
                elif kind == 'permission':
                    self.assertEqual(path.read_text(), preserved)

    def test_context_is_delivered_once_per_epoch_and_does_not_overwrite_the_file(self):
        actor = self.worker(self.lead(), threadId='existing-thread')
        path = self.runtime.progress_file(actor)
        path.write_text('Verified user content\n')
        def context(key, delivered=False, **changes):
            with self.runtime.lock, self.runtime.db() as db:
                current = {**self.runtime.agent(actor['id'], db), **changes}
                self.runtime.enqueue(db, current, 'user', 'Continue', key)
                value = self.runtime.model_turn_context(db, current, key)
                if delivered:
                    db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (key,))
                return value
        self.assertIn(str(path), context('uncertain'))
        self.assertIn(str(path), context('delivered', True))
        self.assertNotIn('[Studio progress file]', context('unchanged', True))
        self.assertIn(str(path), context('compacted', compactions=1))
        for topic in ('panel', 'background'):
            self.assertIn(str(path), str(self.runtime.model_context(actor['id'], {'topic': topic})['content']))
        self.runtime.new_thread_params(actor)
        self.assertEqual(path.read_text(), 'Verified user content\n')


if __name__ == '__main__':
    unittest.main()
