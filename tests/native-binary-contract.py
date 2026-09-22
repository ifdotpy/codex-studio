#!/usr/bin/env python3
"""Exercise installed binary approval without credentials or model requests."""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_native_binary as native


def schemas():
    return {
        'ClientRequest.json': {'oneOf': [
            {'properties': {'method': {'enum': [method]}}}
            for method in native.REQUIRED_METHODS]},
        'v2/TurnStartParams.json': {
            'properties': {'cyberAccessProgram': {}},
            'definitions': {'CyberAccessProgram': {'enum': ['standard', 'daybreakBlue', 'daybreakRed']}}},
        'v2/ModelListResponse.json': {'definitions': {'Model': {
            'properties': {'availableAccessPrograms': {}}}}},
    }


def fake_binary(directory, *, version='0.155.0-alpha.16', supports_daybreak=True, bad_smoke=False):
    fixture = schemas()
    if not supports_daybreak:
        fixture['v2/ModelListResponse.json']['definitions']['Model']['properties'] = {}
    path = Path(directory) / 'codex'
    path.parent.mkdir(parents=True, exist_ok=True)
    source = f'''#!{sys.executable}
import json,os,pathlib,sys
assert 'OPENAI_API_KEY' not in os.environ
assert 'CODEX_API_KEY' not in os.environ
assert 'ANTHROPIC_API_KEY' not in os.environ
assert os.environ['HOME'] == os.environ['CODEX_HOME']
assert pathlib.Path.cwd() == pathlib.Path(os.environ['CODEX_HOME']).resolve()
if '--version' in sys.argv:
 print('codex-cli {version}')
elif 'generate-json-schema' in sys.argv:
 root=pathlib.Path(sys.argv[sys.argv.index('--out')+1])
 for name,value in {fixture!r}.items():
  target=root/name; target.parent.mkdir(parents=True,exist_ok=True)
  target.write_text(json.dumps(value))
else:
 for line in sys.stdin:
  frame=json.loads(line)
  if frame['method']=='initialized':continue
  assert frame['method'] in ('initialize','config/read','thread/loaded/list')
  result={{'initialize':{{'userAgent':'fixture'}},'config/read':{{'config':{{}}}},'thread/loaded/list':{{'data':[]}}}}[frame['method']]
  if {bad_smoke!r}:result=None
  print(json.dumps({{'id':frame['id'],'result':result}}),flush=True)
'''
    path.write_text(source)
    path.chmod(0o755)
    host = path.parent / 'codex-code-mode-host'
    host.write_text(f'''#!{sys.executable}
import json,sys,struct
if '--help' in sys.argv:
 print('Usage: codex-code-mode-host --listen ADDRESS')
else:
 raw=sys.stdin.buffer.read()
 assert struct.unpack('<I',raw[:4])[0] == len(raw)-4
 assert json.loads(raw[4:])['type'] == 'connection/hello'
 result=json.dumps({{'type':'connection/ready','selectedVersion':1,'capabilities':[]}}).encode()
 sys.stdout.buffer.write(struct.pack('<I',len(result))+result)
''')
    host.chmod(0o755)
    return path


class NativeBinaryContract(unittest.TestCase):
    def test_semantic_order(self):
        versions = ['0.155.0-alpha.16', '0.154.9', '0.155.1', '0.155.0', '0.155.0-alpha.2']
        self.assertEqual(sorted(versions, key=native.version_key),
                         ['0.154.9', '0.155.0-alpha.2', '0.155.0-alpha.16', '0.155.0', '0.155.1'])
        with self.assertRaises(ValueError):
            native.version_key('unrecognized')

    def test_discovery_deduplicates_and_does_not_pin_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            old = fake_binary(root / 'old', version='0.154.0')
            new = fake_binary(root / 'new', version='0.155.1')
            alias = root / 'alias'
            alias.mkdir()
            (alias / 'codex').symlink_to(old)
            env = {'CODEX_BIN': str(old), 'PATH': f'{alias}:{new.parent}'}
            with patch.object(native, 'CHATGPT_CODEX', root / 'missing'):
                rows = native.discover_candidates(env=env)
            self.assertEqual([row['version'] for row in rows], ['0.155.1', '0.154.0'])
            self.assertEqual(rows[1]['identity'], {**native.file_identity(old),
                                                 'companions': native.companion_identities(old)})

    def test_approve_snapshot_isolated_and_repeatable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            expected = hashlib.sha256(source.read_bytes()).hexdigest()
            with patch.dict(os.environ, {'OPENAI_API_KEY': 'never-expose-openai',
                                        'CODEX_API_KEY': 'never-expose-codex',
                                        'ANTHROPIC_API_KEY': 'never-expose-anthropic'}):
                approved = native.approve_candidate(source, root / 'state')
                repeated = native.approve_candidate(source, root / 'state')
            target = Path(approved['path'])
            self.assertEqual(approved['sha256'], expected)
            self.assertEqual(target, root.resolve() / 'state/native-runtime/builds' / approved['bundleSha256'] / 'codex')
            self.assertTrue((target.parent / 'codex-code-mode-host').is_file())
            self.assertEqual(approved['approvalRevision'], 2)
            self.assertEqual(target.stat().st_mode & 0o777, 0o755)
            self.assertEqual(repeated['path'], str(target))
            self.assertEqual(approved['checks']['smoke'], ['initialize', 'config/read', 'thread/loaded/list'])
            source.write_text('upstream changed')
            self.assertEqual(hashlib.sha256(target.read_bytes()).hexdigest(), expected)
            self.assertFalse(list(target.parent.parent.glob('.validate-*')))

    def test_missing_daybreak_rejects_before_publish(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source', supports_daybreak=False)
            with self.assertRaisesRegex(ValueError, 'availableAccessPrograms'):
                native.approve_candidate(source, root / 'state')
            self.assertEqual(list((root / 'state/native-runtime/builds').iterdir()), [])

    def test_invalid_smoke_rejects_before_publish(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source', bad_smoke=True)
            with self.assertRaisesRegex(ValueError, 'initialize'):
                native.approve_candidate(source, root / 'state')
            self.assertEqual(list((root / 'state/native-runtime/builds').iterdir()), [])

    def test_required_method_rejection(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = schemas()
            fixture['ClientRequest.json']['oneOf'] = fixture['ClientRequest.json']['oneOf'][:-1]
            for name, value in fixture.items():
                path = Path(temporary) / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(value))
            with self.assertRaisesRegex(ValueError, 'model/list'):
                native._schema_check(temporary)

    def test_corrupt_approved_path_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            approved = native.approve_candidate(source, root / 'state')
            target = Path(approved['path'])
            target.write_text('corrupt')
            with self.assertRaisesRegex(RuntimeError, 'unexpected digest'):
                native.approve_candidate(source, root / 'state')
            self.assertEqual(target.read_text(), 'corrupt')

    def test_output_and_time_are_bounded(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(native, 'MAX_OUTPUT', 1024):
                with self.assertRaisesRegex(ValueError, 'size limit'):
                    native._run([sys.executable, '-c', 'print("x"*4096)'], temporary)
            with self.assertRaises(TimeoutError):
                native._run([sys.executable, '-c', 'import time; time.sleep(10)'], temporary, timeout=.1)

    def test_missing_companion_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            source.with_name('codex-code-mode-host').unlink()
            with self.assertRaisesRegex(ValueError, 'codex-code-mode-host'):
                native.approve_candidate(source, root / 'state')
            with patch.object(native, 'CHATGPT_CODEX', root / 'missing'):
                rows = native.discover_candidates(env={'CODEX_BIN': str(source), 'PATH': ''})
            self.assertIn('codex-code-mode-host', rows[0]['error'])

    def test_host_change_creates_new_bundle_and_discovery_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            with patch.object(native, 'CHATGPT_CODEX', root / 'missing'):
                before = native.discover_candidates(env={'CODEX_BIN': str(source), 'PATH': ''})
                first = native.approve_candidate(source, root / 'state')
                host = source.with_name('codex-code-mode-host')
                host.write_text(host.read_text() + '\n# new host\n')
                after = native.discover_candidates(env={'CODEX_BIN': str(source), 'PATH': ''})
                second = native.approve_candidate(source, root / 'state')
            self.assertEqual(first['sha256'], second['sha256'])
            self.assertNotEqual(first['bundleSha256'], second['bundleSha256'])
            self.assertNotEqual(before[0]['identity'], after[0]['identity'])
            self.assertNotEqual(Path(first['path']).with_name(host.name).read_bytes(), host.read_bytes())

    def test_bad_companion_does_not_publish_partial_bundle(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            source.with_name('codex-code-mode-host').write_text(f'#!{sys.executable}\nprint("wrong program")\n')
            with self.assertRaisesRegex(ValueError, 'help smoke'):
                native.approve_candidate(source, root / 'state')
            self.assertEqual(list((root / 'state/native-runtime/builds').iterdir()), [])

    def test_host_help_without_ipc_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            source.with_name('codex-code-mode-host').write_text(
                f'#!{sys.executable}\nprint("Usage: codex-code-mode-host --listen ADDRESS")\n')
            with self.assertRaisesRegex(ValueError, 'invalid IPC frame'):
                native.approve_candidate(source, root / 'state')
            self.assertEqual(list((root / 'state/native-runtime/builds').iterdir()), [])

    def test_existing_partial_bundle_is_not_repaired(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = fake_binary(root / 'source')
            first = native.approve_candidate(source, root / 'state')
            host = Path(first['path']).with_name('codex-code-mode-host')
            host.unlink()
            with self.assertRaisesRegex(RuntimeError, 'unexpected digest'):
                native.approve_candidate(source, root / 'state')
            self.assertFalse(host.exists())


if __name__ == '__main__':
    unittest.main()
