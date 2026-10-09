"""Project locations through real runtimes and the signed server boundary."""
from __future__ import annotations

import json
from pathlib import Path
import runpy
import sys
from typing import Any, cast
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]


def fixture(name: str) -> dict[str, Any]:
    sys.path.insert(0, str(ROOT / 'tests'))
    try:
        return runpy.run_path(str(ROOT / 'tests' / name), run_name='project_locations_fixture')
    finally:
        sys.path.pop(0)


CROSS = fixture('multi-server-orchestration-contract.py')
SIGNED = fixture('multi-server-signed-integration.py')


class ProjectLocationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.case = CROSS['CrossServer']()
        self.case.setUp()
        self.addCleanup(self.case.tearDown)
        self.case.root = self.case.root.resolve()
        self.home, self.remote = self.case.home, self.case.remote
        self.project = self.home.projects({'path': str(self.case.root / 'home'), 'name': 'Shared'})
        self.project = self.home.projects({'action': 'rename', 'path': self.project['id'], 'name': 'Shared', 'expected_revision': 0})
        self.args = {'action': 'add_location', 'project': self.project['id'], 'server': 'remote',
                     'path': str(self.case.root / 'remote'), 'request_id': 'location-one'}

    def test_add_registers_destination_and_retries_exactly(self) -> None:
        service = self.case.network['home']
        result = service.tools(self.case.lead, self.args, 'location-one')
        self.assertEqual(result['outcome'], 'applied')
        project = self.home.projects()['items'][0]
        self.assertEqual(project['id'], self.project['id'])
        self.assertEqual({row['serverId'] for row in project['locations']}, {'home', 'remote'})
        remote = self.remote.projects()['items'][0]
        self.assertEqual(remote['projectAliases'], [{'serverId': 'home', 'projectId': project['id'], 'name': 'Shared'}])
        self.assertEqual(remote['accountKey'], self.remote.accounts.default())
        before = len(self.case.transports['home'].calls)
        self.assertEqual(service.tools(self.case.lead, self.args, 'location-one'), result)
        self.assertEqual(len(self.case.transports['home'].calls), before)
        with self.assertRaises(ValueError):
            service.tools(self.case.lead, {**self.args, 'path': str(self.case.root)}, 'location-one')
        with self.assertRaises(ValueError):
            service.tools(self.case.lead, {**self.args, 'path': str(self.case.root)}, 'different-id')

    def test_substituted_reply_records_no_location_or_completion(self) -> None:
        for field, value in [('path', '/unrequested-folder'), ('projectId', '/other-project'),
                             ('homeProjectId', 'project:other'), ('serverId', 'other'), ('requestId', 'other')]:
            with self.subTest(field=field):
                args = {**self.args, 'request_id': 'substitute-' + field}
                with patch.object(self.case.transports['home'], 'request',
                        side_effect=lambda server, envelope, **kw: {
                            'requestId': envelope['requestId'], 'outcome': 'applied',
                            'value': {'serverId': 'remote', 'path': self.args['path'],
                                      'projectId': self.args['path'], 'homeProjectId': self.project['id'],
                                      'canonicalPath': self.args['path'], 'requestId': envelope['requestId'], field: value}}):
                    with self.assertRaises(PermissionError):
                        self.home.projects(args)
                self.assertEqual(len(self.home.projects()['items'][0]['locations']), 1)
                with self.home.db() as db:
                    row = db.execute("SELECT id,state FROM runtime_server_outbox WHERE json_extract(body,'$.payload.request.path')=? ORDER BY rowid DESC", (self.args['path'],)).fetchone()
                    self.assertNotEqual(row['state'], 'complete')
                    self.assertIsNone(db.execute('SELECT 1 FROM runtime_operation_receipts WHERE id=?', (row['id'],)).fetchone())
                    # Each malformed response is independent from the next probe.
                    db.execute('DELETE FROM runtime_server_outbox WHERE id=?', (row['id'],))

    def test_normalized_requested_path_accepts_the_matching_reply(self) -> None:
        result = self.home.projects({**self.args, 'path': self.args['path'] + '/../remote'})
        self.assertEqual(result['outcome'], 'applied')
        self.assertEqual(self.home.projects()['items'][0]['locations'][1]['path'], self.args['path'])

    def test_destination_symlink_keeps_requested_and_canonical_paths(self) -> None:
        link = self.case.root / 'remote-link'
        link.symlink_to(self.case.root / 'remote', target_is_directory=True)
        result = self.home.projects({**self.args, 'path': str(link)})
        self.assertEqual(result['outcome'], 'applied')
        location = self.home.projects()['items'][0]['locations'][1]
        self.assertEqual(location['path'], self.args['path'])
        self.assertEqual(location['requestedPath'], str(link))
        self.assertEqual(result['value']['path'], str(link))
        self.assertEqual(result['value']['canonicalPath'], self.args['path'])

    def test_deleted_creation_replays_receipt_and_refuses_changed_content(self) -> None:
        args = {'server': 'remote', 'path': self.args['path'], 'name': 'Original', 'request_id': 'deleted-create'}
        original = self.home.projects(args)
        self.home.projects({'action': 'remove', 'path': original['projectId']})
        with self.assertRaises(ValueError):
            self.home.projects({**args, 'name': 'Substituted'})
        self.assertEqual(self.home.projects(args), original)
        self.assertNotIn(original['projectId'], [row['id'] for row in self.home.projects()['items']])

    def test_abstract_local_project_accepts_bound_chat_folder(self) -> None:
        import uuid
        created = self.home.projects({'server': 'local', 'path': self.project['path'], 'request_id': 'abstract-local'})
        folder_id = str(uuid.uuid4())
        self.home.projects({'action': 'add_folder', 'path': created['projectId'], 'name': 'Notes',
                            'folder_id': folder_id, 'expected_revision': 0})
        chat = self.home.new_lead({'id': str(uuid.uuid4()), 'cwd': self.project['path'], 'reuse_empty': False,
                                  'project_id': created['projectId'], 'project_server_id': 'home',
                                  'project_folder': folder_id})
        self.assertEqual(chat['projectId'], created['projectId'])
        self.assertEqual(chat['projectFolder'], folder_id)

    def test_remote_only_project_uses_abstract_id_at_metadata_apis(self) -> None:
        import uuid
        created = self.home.projects({'server': 'remote', 'path': self.args['path'], 'name': 'Remote only',
                                      'request_id': 'remote-only'})
        self.assertEqual(created['outcome'], 'applied')
        project_id = created['projectId']
        self.assertTrue(project_id.startswith('project:'))
        project = next(row for row in self.home.projects()['items'] if row['id'] == project_id)
        self.assertEqual(len(project['locations']), 1)
        self.assertEqual(project['locations'][0]['serverId'], 'remote')
        renamed = self.home.projects({'action': 'rename', 'path': project_id, 'name': 'Renamed', 'expected_revision': 0})
        self.assertEqual(renamed['name'], 'Renamed')
        folder = self.home.projects({'action': 'add_folder', 'path': project_id, 'name': 'Notes',
                                    'folder_id': str(uuid.uuid4()), 'expected_revision': 1})
        self.assertEqual(folder['folders'][0]['name'], 'Notes')
        changed = self.home.projects({'action': 'set_account', 'path': project_id, 'account_key': self.home.accounts.default(),
                                     'expected_revision': project['accountRevision']})
        self.assertEqual(changed['id'], project_id)
        self.assertEqual(self.home.projects({'server': 'remote', 'path': self.args['path'], 'name': 'Remote only',
                                            'request_id': 'remote-only'}), created)

    def abstract_with_local_location(self, key: str) -> str:
        created = self.home.projects({'server': 'remote', 'path': self.args['path'], 'request_id': key})
        self.home.projects({'action': 'add_location', 'project': created['projectId'], 'server': 'home',
                            'path': self.project['path'], 'request_id': key + '-local'})
        return str(created['projectId'])

    def other_account(self) -> str:
        folder = self.case.root / 'other-account'
        folder.mkdir(exist_ok=True)
        (folder / 'auth.json').write_text(json.dumps({'OPENAI_API_KEY': 'fixture-only-key'}))
        return str(self.home.accounts.register(str(folder)))

    def bound_chat(self, project_id: str, **settings: Any) -> dict[str, Any]:
        import uuid
        return cast(dict[str, Any], self.home.new_lead({'id': str(uuid.uuid4()), 'cwd': self.project['path'], 'reuse_empty': False,
                                  'project_id': project_id, 'project_server_id': 'home', **settings}))

    def test_bound_chat_uses_abstract_project_account_and_catalog(self) -> None:
        project_id = self.abstract_with_local_location('settings-account')
        other = self.other_account()
        self.home.projects({'action': 'set_account', 'path': project_id, 'account_key': other, 'expected_revision': 1})
        with patch.object(self.home, 'catalog', wraps=self.home.catalog) as catalog:
            chat = self.bound_chat(project_id, model='gpt-6-astra')
        self.assertEqual(chat['accountKey'], other)
        catalog.assert_called_with(other)
        self.assertNotEqual(self.home.projects()['items'][0].get('accountKey'), other)

    def test_bound_chat_and_workers_use_project_account_membership(self) -> None:
        project_id = self.abstract_with_local_location('settings-accounts')
        other = self.other_account()
        self.home.projects({'action': 'set_accounts', 'path': project_id, 'account_key': other,
                            'account_keys': [other], 'expected_revision': 1})
        chat = self.bound_chat(project_id)
        self.assertEqual(chat['accountKey'], other)
        with self.assertRaisesRegex(ValueError, 'account of this project'):
            self.bound_chat(project_id, account_key='default')
        receipt = self.home.spawn_agents(chat, {'agents': [{'name': 'Reviewer', 'prompt': 'Inspect', 'role': 'reviewer'}]}, 'member-worker')
        self.assertEqual(self.home.agent(receipt['agents'][0]['id'])['accountKey'], other)
        with self.assertRaisesRegex(ValueError, 'worker account of this project'):
            self.home.spawn_agents(chat, {'agents': [{'name': 'Reviewer', 'prompt': 'Inspect', 'role': 'reviewer', 'account_key': 'default'}]}, 'outside-worker')

    def test_bound_chat_worker_uses_abstract_project_base(self) -> None:
        project_id = self.abstract_with_local_location('settings-base')
        self.home.projects({'action': 'set_worker_base', 'path': project_id, 'base_ref': 'logical-base', 'expected_revision': 0})
        chat = self.bound_chat(project_id)
        with patch('codex_runtime.git_toplevel', return_value=self.project['path']), \
             patch('codex_worker_base.resolve_worker_base', return_value={'baseRef': 'logical-base', 'baseCommit': 'a' * 40, 'mainRef': 'main', 'behindMain': None}) as resolve:
            receipt = self.home.spawn_agents(chat, {'agents': [{'name': 'Worker', 'prompt': 'Inspect', 'workspace': 'worktree'}]}, 'logical-base-worker')
        resolve.assert_called_with(self.project['path'], 'logical-base')
        self.assertEqual(self.home.agent(receipt['agents'][0]['id'])['workerBaseRef'], 'logical-base')

    def test_bound_chat_worker_environment_uses_abstract_project(self) -> None:
        from codex_project_locations import settings_key
        from codex_worker_environment import select
        project_id = self.abstract_with_local_location('settings-environment')
        self.home.projects({'action': 'set_worker_environment', 'path': project_id, 'environment': 'linux', 'expected_revision': 0})
        chat = self.bound_chat(project_id)
        with self.home.read_db() as db:
            key = settings_key(self.home, db, chat['cwd'], chat)
        self.assertEqual(select(self.home, {}, chat['cwd'], project_key=key), 'linux')

    def test_chat_creation_uses_registered_destination_binding(self) -> None:
        import uuid
        self.home.projects(self.args)
        args = {'id': str(uuid.uuid4()), 'cwd': self.args['path'], 'reuse_empty': False,
                'project_id': self.project['id'], 'project_server_id': 'home'}
        chat = self.remote.new_lead(args)
        self.assertEqual(chat['cwd'], self.args['path'])
        self.assertEqual(chat['projectId'], self.project['id'])
        self.assertEqual(chat['projectServerId'], 'home')
        with self.remote.read_db() as db:
            self.assertEqual(self.remote.agent_entity_view(db, chat)['serverId'], self.remote._project_server_id)
        self.assertEqual(self.remote.new_lead(args)['id'], chat['id'])
        with self.assertRaises(ValueError):
            self.remote.new_lead({**args, 'id': str(uuid.uuid4()), 'cwd': str(self.case.root)})
        with self.assertRaises(ValueError):
            self.remote.new_lead({**args, 'project_server_id': 'other'})

    def test_lost_reply_recovers_without_registering_again(self) -> None:
        self.case.transports['home'].drop_reply = True
        first = self.home.projects(self.args)
        self.assertEqual(first['outcome'], 'unknown')
        self.assertEqual(len(self.home.projects()['items'][0]['locations']), 1)
        with patch.object(self.remote, 'ensure_project', wraps=self.remote.ensure_project) as register:
            result = self.home.projects(self.args)
            register.assert_not_called()
        self.assertEqual(result['outcome'], 'applied')
        self.assertEqual(len(self.home.projects()['items'][0]['locations']), 2)

    def test_crash_after_destination_commit_uses_effect_receipt(self) -> None:
        first = self.home.projects(self.args)
        receiver = self.case.network['remote']
        with self.home.read_db() as db:
            envelope = json.loads(db.execute('SELECT body FROM runtime_server_outbox WHERE id=?', (first['requestId'],)).fetchone()[0])
        with self.remote.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?", (first['requestId'],))
        with patch.object(receiver, '_receive', wraps=receiver._receive) as apply:
            recovered = receiver.receive('home', envelope)
            apply.assert_not_called()
        self.assertEqual(recovered['outcome'], 'applied')
        self.assertEqual(recovered['value'], first['value'])

    def test_remove_keeps_destination_project_and_files(self) -> None:
        self.home.projects(self.args)
        result = self.home.projects({**self.args, 'action': 'remove_location', 'path': None, 'request_id': 'remove-one'})
        self.assertEqual(result['outcome'], 'applied')
        self.assertEqual(len(self.home.projects()['items'][0]['locations']), 1)
        self.assertEqual(self.remote.projects()['items'][0]['projectAliases'], [])
        self.assertTrue((self.case.root / 'remote').is_dir())
        with self.assertRaises(ValueError):
            self.home.projects({**self.args, 'action': 'remove_location', 'server': 'home', 'request_id': 'remove-last'})

    def test_offline_change_serializes_other_location_changes(self) -> None:
        self.case.transports['home'].offline = True
        self.assertEqual(self.home.projects(self.args)['outcome'], 'unknown')
        with self.assertRaises(ValueError):
            self.home.projects({**self.args, 'action': 'remove_location', 'server': 'home', 'request_id': 'remove-while-pending'})
        self.case.transports['home'].offline = False
        self.assertEqual(self.home.projects(self.args)['outcome'], 'applied')

    def test_migration_keeps_legacy_fields_and_chat_directory(self) -> None:
        from codex_project_locations import migrate
        with self.home.db() as db:
            original = {**self.project, 'folders': [{'id': 'folder', 'name': 'Notes', 'parentId': None}]}
            for key in ('locations', 'locationsRevision', 'homeServerId'):
                original.pop(key, None)
            db.execute('UPDATE runtime_projects SET record=? WHERE id=?', (json.dumps(original), original['id']))
        migrate(self.home)
        migrated = self.home.projects()['items'][0]
        for key, value in original.items():
            self.assertEqual(migrated[key], value)
        self.assertEqual(migrated['locations'], [{'serverId': 'home', 'path': original['path'], 'projectId': original['id']}])
        self.assertEqual(self.home.agent(self.case.lead['id'])['cwd'], original['path'])
        with self.home.read_db() as db:
            payload = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (self.case.lead['id'],)).fetchone()[0])
        self.assertEqual(payload['value']['serverId'], self.home._project_server_id)
        migrate(self.home)
        self.assertEqual(self.home.projects()['items'][0], migrated)

    def test_folder_browse_uses_destination_home_and_full_paths(self) -> None:
        from codex_project_locations import folders
        row = folders(str(self.case.root / 'remote'))
        self.assertEqual(row['path'], str(self.case.root / 'remote'))
        self.assertIn({'name': 'state', 'path': str(self.case.root / 'remote' / 'state')}, row['directories'])
        self.assertEqual(row['parent'], str(self.case.root))

    def test_matching_origin_is_a_suggestion_and_excludes_credentials(self) -> None:
        from codex_project_locations import git_origin, matches
        from codex_multi_server_orchestration import GitCommand
        with patch('codex_multi_server_orchestration.git_command', return_value=GitCommand([], {})), \
             patch('codex_multi_server_orchestration.bounded_command', return_value=(b'https://user:secret@example.test/team/repo.git?token=secret\n', 0, False)):
            origin = git_origin(self.project['path'])
            self.assertEqual(origin, 'example.test/team/repo')
            self.assertEqual(len(matches(self.home, origin or '')['matches']), 1)
            candidate = self.case.root / 'unregistered'
            candidate.mkdir()
            (candidate / '.git').mkdir()
            suggested = matches(self.home, origin or '', str(self.case.root))['matches']
            self.assertIn(str(candidate), [row['path'] for row in suggested])
            self.assertNotIn(str(candidate), [row['path'] for row in self.home.projects()['items']])
        self.assertEqual(len(self.home.projects()['items'][0]['locations']), 1)


class SignedProjectLocationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.case = SIGNED['SignedIntegration']()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.a, self.b = self.case.a, self.case.b
        invitation = self.a.local({'action': 'create_invite', 'requestId': 'invite-a'})['invitation']
        self.b.local({'action': 'accept_invite', 'invitation': invitation, 'requestId': 'accept-b'})
        self.folder = self.b.folder / 'remote-project'
        self.folder.mkdir()
        self.folder = self.folder.resolve()
        project = self.a.runtime.projects({'path': str(self.a.folder), 'name': 'Shared'})
        self.args = {'action': 'add_location', 'project': project['id'], 'server': self.b.server_id,
                     'path': str(self.folder), 'request_id': 'signed-location'}

    def test_browser_api_uses_signed_registration_without_session_token(self) -> None:
        response = self.a.client.post('/api/projects', json=self.args, headers={'X-Canvas-Token': 'test-token'})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()['outcome'], 'applied')
        wire = self.case.actions('add_location')[-1]
        self.assertIn('X-Studio-Signature', wire['headers'])
        self.assertNotIn('X-Canvas-Token', wire['headers'])
        payload = json.loads(wire['raw'])['payload']
        self.assertNotIn('account_key', payload)
        self.assertNotIn('test-token', wire['raw'].decode())
        self.assertEqual(self.b.runtime.projects()['items'][0]['path'], str(self.folder))
        browse = self.a.client.get('/api/directories', params={'server': self.b.server_id, 'path': str(self.folder.parent)},
                                   headers={'X-Canvas-Token': 'test-token'})
        self.assertEqual(browse.status_code, 200, browse.text)
        self.assertIn({'name': self.folder.name, 'path': str(self.folder)}, browse.json()['directories'])

    def test_browser_creates_remote_only_project_and_destination_chat(self) -> None:
        import uuid
        response = self.a.client.post('/api/projects', json={'server': self.b.server_id,
            'path': str(self.folder), 'name': 'Remote only', 'request_id': 'remote-only-browser'},
            headers={'X-Canvas-Token': 'test-token'})
        self.assertEqual(response.status_code, 200, response.text)
        project_id = response.json()['projectId']
        project = next(row for row in self.a.runtime.projects()['items'] if row['id'] == project_id)
        self.assertEqual(project['id'], project_id)
        self.assertEqual(len(project['locations']), 1)
        created = self.b.client.post('/api/leads', json={'id': str(uuid.uuid4()), 'cwd': str(self.folder),
            'reuse_empty': False, 'project_id': project_id, 'project_server_id': self.a.server_id},
            headers={'X-Canvas-Token': 'test-token'})
        self.assertEqual(created.status_code, 200, created.text)
        self.assertEqual(created.json()['cwd'], str(self.folder))
        self.assertEqual(created.json()['projectId'], project_id)

    def test_revoked_or_removed_home_server_refuses_chat_binding(self) -> None:
        import uuid
        self.a.runtime.projects(self.args)
        for remove in (False, True):
            with self.subTest(remove=remove):
                if remove:
                    with self.b.runtime.db() as db:
                        db.execute('DELETE FROM runtime_access_clients WHERE id=?', (self.a.server_id,))
                else:
                    self.b.runtime.paired_access().revoke(self.a.server_id)
                args = {'id': str(uuid.uuid4()), 'cwd': str(self.folder), 'reuse_empty': False,
                        'project_id': self.args['project'], 'project_server_id': self.a.server_id}
                with self.assertRaises(PermissionError):
                    self.b.runtime.new_lead(args)
                with self.b.runtime.read_db() as db:
                    self.assertIsNone(db.execute('SELECT 1 FROM runtime_agents WHERE id=?', (args['id'],)).fetchone())

    def test_wrong_owner_is_refused_before_project_registration(self) -> None:
        self.b.runtime.paired_access().identity_cache.clear()
        with patch('codex_multi_server._peer_login', return_value='other@example.test'):
            result = self.a.runtime.projects(self.args)
        self.assertEqual(result['outcome'], 'unknown')
        self.assertEqual(self.case.actions('add_location')[-1]['status'], 403)
        self.assertEqual(self.b.runtime.projects()['items'], [])

    def test_revoked_server_is_refused_before_project_registration(self) -> None:
        self.b.runtime.paired_access().revoke(self.a.server_id)
        result = self.a.runtime.projects(self.args)
        self.assertEqual(result['outcome'], 'unknown')
        self.assertEqual(self.case.actions('add_location')[-1]['status'], 403)
        self.assertEqual(self.b.runtime.projects()['items'], [])
        self.a.runtime.paired_access().revoke(self.b.server_id)
        with self.assertRaises(PermissionError):
            self.a.runtime.projects({**self.args, 'request_id': 'after-revoke'})
