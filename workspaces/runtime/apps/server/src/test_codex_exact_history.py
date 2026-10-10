"""Exact native bytes, inherited boundaries, and hostile archive checks."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
import uuid
import zipfile

from codex_exact_history import export_codex, extend_codex_history, frozen_codex_parameters, unpack_codex, private_write


class ExactHistory(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-exact-history-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / 'source'
        self.sessions = self.home / 'sessions'
        self.sessions.mkdir(parents=True)
        self.archive = self.root / 'history.zip'

    def rollout(self, parent: dict[str, object] | None = None) -> Path:
        key = str(uuid.uuid4())
        payload: dict[str, object] = {'id': key, 'base_instructions': {'text': 'Exact base\n'},
                                     'dynamic_tools': [{'name': 'z'}, {'name': 'a'}]}
        if parent:
            payload['history_base'] = parent
        path = self.sessions / ('rollout-' + key + '.jsonl')
        records = [{'type': 'session_meta', 'payload': payload},
                   {'type': 'response_item', 'payload': {'type': 'message', 'content': '€  two  spaces\r\n'}},
                   {'type': 'turn_context', 'payload': {'model': 'gpt-test', 'effort': 'high',
                                                      'developer_instructions': 'Unchanged\n'}}]
        path.write_bytes(b''.join((json.dumps(record, ensure_ascii=False, indent=None) + '\n').encode()
                                 for record in records))
        return path

    def test_complete_bytes_and_settings_keep_order(self) -> None:
        source = self.rollout()
        descriptor = export_codex(self.home, source, self.archive)
        copied = unpack_codex(self.archive, self.root / 'target')
        self.assertEqual(copied.read_bytes(), source.read_bytes())
        self.assertEqual(descriptor['bytes'], len(source.read_bytes()))
        settings = frozen_codex_parameters(copied)
        self.assertEqual(settings['baseInstructions'], 'Exact base\n')
        self.assertEqual(settings['developerInstructions'], 'Unchanged\n')
        self.assertEqual([tool['name'] for tool in settings['dynamicTools']], ['z', 'a'])

    def test_ancestor_keeps_the_exact_boundary(self) -> None:
        ancestor = self.rollout()
        boundary = len(ancestor.read_bytes())
        ancestor_id = json.loads(ancestor.read_bytes().splitlines()[0])['payload']['id']
        child = self.rollout({'thread_id': ancestor_id, 'end_byte_offset': boundary})
        with ancestor.open('ab') as stream:
            stream.write(b'{"type":"response_item","payload":"after fork"}\n')
        descriptor = export_codex(self.home, child, self.archive)
        unpack_codex(self.archive, self.root / 'target')
        imported = self.root / 'target/sessions' / ancestor.name
        self.assertEqual(imported.read_bytes(), ancestor.read_bytes()[:boundary])
        self.assertEqual(len(descriptor['files']), 2)

    def test_refuses_truncation_and_credentials(self) -> None:
        source = self.rollout()
        with patch('codex_exact_history.MAX_HISTORY_BYTES', len(source.read_bytes()) - 1):
            with self.assertRaisesRegex(ValueError, 'No history was truncated'):
                export_codex(self.home, source, self.archive)
        self.assertFalse(self.archive.exists())
        auth = self.home / 'auth.jsonl'
        auth.write_bytes(source.read_bytes())
        with self.assertRaisesRegex(ValueError, 'Only native session'):
            export_codex(self.home, auth, self.archive)

    def test_refuses_unlisted_files_before_publication(self) -> None:
        export_codex(self.home, self.rollout(), self.archive)
        with zipfile.ZipFile(self.archive, 'a') as archive:
            archive.writestr('../../auth.json', 'fixture secret')
        target = self.root / 'target'
        with self.assertRaisesRegex(ValueError, 'unlisted files'):
            unpack_codex(self.archive, target)
        self.assertFalse(target.exists())

    def test_refuses_checksum_changes_and_conflicting_retry(self) -> None:
        source = self.rollout()
        export_codex(self.home, source, self.archive)
        target = self.root / 'target'
        copied = unpack_codex(self.archive, target)
        copied.write_bytes(b'conflicting native history\n')
        with self.assertRaisesRegex(ValueError, 'different bytes'):
                unpack_codex(self.archive, target)

    def test_review_rollout_identity_and_settings_match_before_publication(self) -> None:
        source = self.rollout()
        export_codex(self.home, source, self.archive)
        target = self.root / 'target'
        with self.assertRaisesRegex(ValueError, 'native identity differs'):
            unpack_codex(self.archive, target, str(uuid.uuid4()))
        self.assertFalse(target.exists())
        thread = json.loads(source.read_bytes().splitlines()[0])['payload']['id']
        with self.assertRaisesRegex(ValueError, 'native settings differ'):
            unpack_codex(self.archive, target, thread, {})
        self.assertFalse(target.exists())

    def test_existing_native_prefix_cannot_change_or_be_active(self) -> None:
        old = self.rollout()
        thread = json.loads(old.read_bytes().splitlines()[0])['payload']['id']
        imported = self.root / 'incoming.jsonl'
        imported.write_bytes(old.read_bytes().replace('€'.encode(), b'changed'))
        native = Mock()
        native.call.return_value = {'thread': {'path': str(old), 'status': {'type': 'idle'}}}
        backup = self.root / 'backup.jsonl'
        with self.assertRaisesRegex(ValueError, 'exact destination prefix'):
            extend_codex_history(native, self.home, thread, imported, backup)
        self.assertFalse(backup.exists())
        self.assertEqual([call.args[0] for call in native.call.call_args_list], ['thread/read'])
        native.call.return_value['thread']['status']['type'] = 'active'
        imported.write_bytes(old.read_bytes())
        with self.assertRaisesRegex(ValueError, 'session is active'):
            extend_codex_history(native, self.home, thread, imported, backup)

    def test_review_append_before_replace_is_not_erased(self) -> None:
        old = self.rollout()
        thread = json.loads(old.read_bytes().splitlines()[0])['payload']['id']
        before = old.read_bytes()
        appended = b'{"type":"response_item","payload":"concurrent append"}\n'
        imported = self.root / 'incoming.jsonl'
        imported.write_bytes(before + b'{"type":"response_item","payload":"moved turn"}\n')
        backup = self.root / 'backup.jsonl'
        native = Mock()
        native.call.return_value = {'thread': {'path': str(old), 'status': {'type': 'idle'}}}
        original_read = Path.read_bytes
        archived = False
        injected = False
        def call(method: str, params: dict[str, object], timeout: int = 20) -> dict[str, object]:
            nonlocal archived
            if method == 'thread/archive':
                archived = True
            return {'thread': {'path': str(old), 'status': {'type': 'idle'}}}
        native.call.side_effect = call
        def read_with_append(path: Path) -> bytes:
            nonlocal injected
            data = original_read(path)
            if path.resolve() == old.resolve() and archived and not injected:
                injected = True
                with old.open('ab') as stream:
                    stream.write(appended)
            return data
        with patch.object(Path, 'read_bytes', read_with_append):
            with self.assertRaisesRegex(ValueError, 'changed|prefix'):
                extend_codex_history(native, self.home, thread, imported, backup)
        self.assertEqual(old.read_bytes(), before + appended)

    def test_review_unlocked_append_at_publication_is_never_erased(self) -> None:
        old = self.rollout()
        thread = json.loads(old.read_bytes().splitlines()[0])['payload']['id']
        before = old.read_bytes()
        concurrent = b'{"type":"response_item","payload":"unlocked writer"}\n'
        suffix = b'{"type":"response_item","payload":"moved turn"}\n'
        imported = self.root / 'incoming.jsonl'
        imported.write_bytes(before + suffix)
        backup = self.root / 'backup.jsonl'
        native = Mock()
        native.call.return_value = {'thread': {'path': str(old), 'status': {'type': 'idle'}}}
        original_replace, original_write = os.replace, os.write
        injected = False
        def inject() -> None:
            nonlocal injected
            if not injected:
                injected = True
                with old.open('ab') as writer:
                    writer.write(concurrent)
        def replace(source: str | Path, destination: str | Path) -> None:
            if Path(str(destination)).resolve() == old.resolve():
                inject()
            original_replace(source, destination)
        def write(fd: int, data: bytes) -> int:
            if os.fstat(fd).st_ino == old.stat().st_ino:
                inject()
            return original_write(fd, data)
        with patch('codex_exact_history.os.replace', replace), patch('codex_exact_history.os.write', write):
            try:
                extend_codex_history(native, self.home, thread, imported, backup)
            except ValueError:
                pass
        self.assertTrue(injected)
        self.assertIn(concurrent, old.read_bytes())
        self.assertTrue(old.read_bytes().startswith(before))
        self.assertEqual(backup.read_bytes(), before)

    def test_review_changed_size_refuses_before_suffix_write(self) -> None:
        old = self.rollout()
        before = old.read_bytes()
        concurrent = b'{"type":"response_item","payload":"before suffix"}\n'
        imported = self.root / 'incoming.jsonl'
        imported.write_bytes(before + b'{"type":"response_item","payload":"moved turn"}\n')
        native = Mock()
        native.call.return_value = {'thread': {'path': str(old), 'status': {'type': 'idle'}}}
        thread = json.loads(before.splitlines()[0])['payload']['id']
        original = os.fstat
        reads = 0
        def stat(fd: int) -> os.stat_result:
            nonlocal reads
            reads += 1
            if reads == 2:
                with old.open('ab') as writer:
                    writer.write(concurrent)
            return original(fd)
        with patch('codex_exact_history.os.fstat', stat):
            with self.assertRaisesRegex(ValueError, 'changed before append'):
                extend_codex_history(native, self.home, thread, imported, self.root / 'backup.jsonl')
        self.assertEqual(old.read_bytes(), before + concurrent)


if __name__ == '__main__':
    unittest.main()
