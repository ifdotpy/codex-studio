"""Updates from a paired server (docs/peer-updates.md): the update.* actions and the puller.

Two orchestration services talk through an in-process transport. Fake ditto and codesign
commands work on fixture apps in temporary folders; nothing touches /Applications.
"""
from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import threading
from typing import Any
import unittest
from unittest.mock import patch
import zipfile

import codex_peer_updates as updates
from codex_multi_server import MAX_RESPONSE_BYTES as MULTI_SERVER_MAX_RESPONSE_BYTES
from codex_multi_server_orchestration import MultiServerService

OLD: dict[str, Any] = {'version': 1, 'builtAt': 1791900000.5, 'commit': 'a' * 40, 'commitTime': 1791899000, 'branch': 'main'}
NEW: dict[str, Any] = {'version': 1, 'builtAt': 1791980000.25, 'commit': 'b' * 40, 'commitTime': 1791979000, 'branch': 'main'}


class FixtureRuntime:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.closed = False
        self.lock = threading.RLock()
        self.changed = threading.Event()

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.root / 'canvas.sqlite3', timeout=5)
        connection.row_factory = sqlite3.Row
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    read_db = db


class Network:
    """Delivers each envelope to the receiving service, as the signed channel does."""

    def __init__(self) -> None:
        self.services: dict[str, MultiServerService] = {}
        self.calls: list[tuple[str, str, dict[str, Any]]] = []
        self.corrupt_chunks = 0

    def transport(self, local: str) -> 'Transport':
        return Transport(self, local)


class Transport:
    def __init__(self, network: Network, local: str) -> None:
        self.network, self.local = network, local

    @property
    def local_server_id(self) -> str:
        return self.local

    def servers(self) -> list[dict[str, Any]]:
        return [{'id': server, 'status': 'paired'} for server in self.network.services if server != self.local]

    def request(self, server: str, envelope: dict[str, Any], *, timeout: int) -> dict[str, Any]:
        self.network.calls.append((server, envelope['action'], copy.deepcopy(envelope['payload'])))
        reply = self.network.services[server].receive(self.local, copy.deepcopy(envelope))
        if envelope['action'] == 'update.chunk' and self.network.corrupt_chunks and reply.get('outcome') == 'applied':
            self.network.corrupt_chunks -= 1
            reply = {**reply, 'value': {**reply['value'], 'sha256': '0' * 64}}
        # The real transport carries JSON; check the size limit of the answer.
        self.assert_size(reply)
        return reply

    @staticmethod
    def assert_size(reply: dict[str, Any]) -> None:
        if len(json.dumps(reply).encode()) > MULTI_SERVER_MAX_RESPONSE_BYTES:
            raise AssertionError('The answer exceeds the signed response limit')


class LinkTransport(Transport):
    """A server with a QUIC link: the archive comes as one stream, read by the export route."""

    def __init__(self, network: Network, local: str, lose_after: int | None = None) -> None:
        super().__init__(network, local)
        self.lose_after = lose_after
        self.streams: list[str] = []

    def stream(self, server: str, path: str, *, timeout: float, sink: Any) -> bool:
        from studio_api.multi_server.router import export_file

        self.streams.append(path)
        opened = export_file(self.network.services[server].runtime.root, path.rsplit('/', 1)[1], '0')
        assert opened is not None
        stream, _size, _start = opened
        sent = 0
        with stream:
            while data := stream.read(64 * 1024):
                if self.lose_after is not None and sent + len(data) > self.lose_after:
                    raise OSError('The link lost the stream')
                sink(data)
                sent += len(data)
        return True


def make_app(folder: Path, build: dict[str, Any], certificate: str = 'Studio Local Signing') -> Path:
    app = folder / updates.APP_NAME
    resources = app / 'Contents' / 'Resources'
    resources.mkdir(parents=True)
    (resources / 'studio-build.json').write_text(json.dumps(build))
    (app / 'Contents' / 'CERTIFICATE').write_text(certificate)
    # Random data does not compress: the archive needs several chunks.
    (resources / 'payload.bin').write_bytes(os.urandom(updates.CHUNK_BYTES * 2 + 1000))
    return app


def fake_run(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    """ditto and codesign for fixture apps."""
    tool, args = Path(argv[0]).name, argv[1:]
    out = ''
    code = 0
    if tool == 'ditto' and args[:3] == ['-c', '-k', '--keepParent']:
        source, target = Path(args[3]), Path(args[4])
        with zipfile.ZipFile(target, 'w') as archive:
            for path in sorted(source.rglob('*')):
                archive.write(path, str(path.relative_to(source.parent)))
    elif tool == 'ditto' and args[:2] == ['-x', '-k']:
        with zipfile.ZipFile(args[2]) as archive:
            archive.extractall(args[3])
    elif tool == 'ditto':
        shutil.copytree(args[0], args[1], symlinks=True)
    elif tool == 'codesign' and args[0] == '--verify':
        # Bytecode outside the seal fails the strict check, as with a real app.
        code = 1 if ((Path(args[-1]) / 'Contents' / 'BAD_SIGNATURE').exists()
                     or any(Path(args[-1]).rglob('*.pyc'))) else 0
    elif tool == 'codesign' and args[:2] == ['-d', '-r-']:
        certificate = (Path(args[2]) / 'Contents' / 'CERTIFICATE').read_text()
        out = f'designated => identifier "{updates.BUNDLE_ID}" and certificate leaf = H"{certificate}"\n'
    else:
        raise AssertionError(f'Unexpected command {argv}')
    return subprocess.CompletedProcess(argv, code, out, '')


class PeerUpdates(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='studio-peer-updates-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.network = Network()
        for name in ('mac', 'server'):
            (self.root / name / 'state').mkdir(parents=True)
            runtime = FixtureRuntime(self.root / name / 'state')
            self.network.services[name] = MultiServerService(runtime, self.network.transport(name))
        self.local_app = make_app(self.root / 'mac', OLD)
        self.peer_app = make_app(self.root / 'server', NEW)
        self.state = self.root / 'mac' / 'state'
        self.peer_state = self.root / 'server' / 'state'
        self.now = 1_800_000_000.0
        self.enabled = True
        self.puller = updates.Puller(self.state, self.local_app, self.network.transport('mac'),
                                     lambda: self.enabled, run=fake_run, clock=lambda: self.now)
        for patcher in (patch.object(updates, 'installed_app', lambda: self.peer_app),
                        patch.object(updates, 'run_tool', fake_run)):
            patcher.start()
            self.addCleanup(patcher.stop)

    def actions(self, name: str) -> list[str]:
        return [action for _, action, _ in self.network.calls if action == name]

    def peer(self, action: str, payload: dict[str, Any]) -> dict[str, Any]:
        envelope = {'requestId': f'{action}-{len(self.network.calls)}', 'action': action, 'payload': payload}
        self.network.calls.append(('server', action, payload))
        return self.network.services['server'].receive('mac', envelope)

    def test_newer_offer_is_pulled_verified_and_staged(self) -> None:
        result = self.puller.pull()
        self.assertEqual(result['outcome'], 'staged', result)
        pending = updates.read_pending(self.state)
        assert pending is not None
        self.assertEqual(pending['build'], NEW)
        self.assertEqual(pending['from'], 'server')
        staged = Path(pending['app'])
        self.assertEqual(staged, self.state / 'updates' / str(NEW['builtAt']) / updates.APP_NAME)
        self.assertEqual((staged / 'Contents/Resources/payload.bin').read_bytes(),
                         (self.peer_app / 'Contents/Resources/payload.bin').read_bytes())
        self.assertGreaterEqual(len(self.actions('update.chunk')), 3)
        self.assertEqual(self.actions('update.release'), ['update.release'])
        # The export is released and the download leaves no partial file.
        self.assertEqual(list((self.peer_state / updates.EXPORTS_DIR).iterdir()), [])
        self.assertEqual(sorted(p.name for p in (self.state / 'updates').iterdir()), [str(NEW['builtAt'])])
        status = updates.status(self.state, self.local_app)
        self.assertEqual(status['build'], OLD)
        self.assertEqual(status['pending'], {'build': NEW, 'server': 'server'})
        self.assertEqual(status['lastPull']['outcome'], 'staged')
        # The next pass finds the staged build and pulls nothing again.
        self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertEqual(len(self.actions('update.export')), 1)

    def test_a_link_stream_replaces_the_chunks(self) -> None:
        transport = LinkTransport(self.network, 'mac')
        self.puller.transport = transport
        self.assertEqual(self.puller.pull()['outcome'], 'staged')
        export = self.actions('update.export')
        self.assertEqual(len(export), 1)
        self.assertEqual(len(transport.streams), 1)
        self.assertRegex(transport.streams[0], r'^/api/servers/exports/[0-9a-f-]{36}\.zip$')
        self.assertEqual(self.actions('update.chunk'), [])
        pending = updates.read_pending(self.state)
        assert pending is not None
        self.assertEqual((Path(pending['app']) / 'Contents/Resources/payload.bin').read_bytes(),
                         (self.peer_app / 'Contents/Resources/payload.bin').read_bytes())

    def test_a_lost_stream_continues_with_chunks_from_its_offset(self) -> None:
        self.puller.transport = LinkTransport(self.network, 'mac', lose_after=300 * 1024)
        self.assertEqual(self.puller.pull()['outcome'], 'staged')
        offsets = [payload['offset'] for _, action, payload in self.network.calls if action == 'update.chunk']
        self.assertEqual(offsets[0], 256 * 1024)
        pending = updates.read_pending(self.state)
        assert pending is not None
        self.assertEqual((Path(pending['app']) / 'Contents/Resources/payload.bin').read_bytes(),
                         (self.peer_app / 'Contents/Resources/payload.bin').read_bytes())

    def test_bytecode_in_the_offering_app_is_removed_before_the_export(self) -> None:
        cache = self.peer_app / 'Contents/Resources/workspace/vm/guest/__pycache__'
        cache.mkdir(parents=True)
        (cache / 'host_exec_protocol.cpython-314.pyc').write_bytes(b'cache')
        self.assertEqual(self.puller.pull()['outcome'], 'staged')
        self.assertEqual(list(cache.iterdir()), [])

    def test_an_offering_app_with_changed_files_exports_nothing(self) -> None:
        (self.peer_app / 'Contents' / 'BAD_SIGNATURE').write_text('')
        result = self.puller.pull()
        self.assertEqual(result['outcome'], 'failed')
        self.assertIn('The installed app fails its signature check', result['reason'])
        self.assertEqual(self.actions('update.chunk'), [])

    def test_same_or_older_build_is_not_pulled(self) -> None:
        for build in (OLD, {**OLD, 'builtAt': OLD['builtAt'] - 1, 'commit': 'c' * 40}):
            (self.peer_app / updates.BUILD_STAMP).write_text(json.dumps(build))
            self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertEqual(self.actions('update.offer'), ['update.offer'] * 2)
        self.assertEqual(self.actions('update.export'), [])
        self.assertIsNone(updates.read_pending(self.state))

    def test_disabled_setting_asks_no_server(self) -> None:
        self.enabled = False
        self.assertEqual(self.puller.pull()['outcome'], 'disabled')
        self.assertEqual(self.network.calls, [])

    def test_bad_chunk_hash_is_recorded_and_skipped_for_an_hour(self) -> None:
        self.network.corrupt_chunks = 1
        result = self.puller.pull()
        self.assertEqual(result['outcome'], 'failed')
        self.assertEqual(result['reason'], 'Invalid update archive chunk')
        self.assertIsNone(updates.read_pending(self.state))
        self.assertFalse((self.state / 'updates' / str(NEW['builtAt'])).exists())
        self.assertEqual([p.name for p in (self.state / 'updates').iterdir()], [])
        saved = json.loads((self.state / updates.STATUS_FILE).read_text())
        self.assertEqual(list(saved['failures']), [updates.build_key(NEW)])
        self.assertEqual(updates.status(self.state, self.local_app)['lastPull']['outcome'], 'failed')
        self.assertEqual(self.actions('update.release'), ['update.release'])
        self.now += updates.FAILURE_RETRY_SECONDS - 1
        self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertEqual(len(self.actions('update.export')), 1)
        self.now += 2
        self.assertEqual(self.puller.pull()['outcome'], 'staged')
        self.assertEqual(len(self.actions('update.export')), 2)

    def test_failed_install_receipt_skips_the_build_for_an_hour(self) -> None:
        updates.write_json(self.state / updates.RECEIPT_FILE,
                           {'build': NEW, 'outcome': 'failed', 'at': self.now, 'from': 'server'})
        self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertEqual(self.actions('update.export'), [])
        self.assertEqual(updates.status(self.state, self.local_app)['lastInstall'],
                         {'at': self.now, 'server': 'server', 'build': NEW, 'outcome': 'failed', 'reason': None})

    def test_other_certificate_is_refused(self) -> None:
        (self.peer_app / 'Contents' / 'CERTIFICATE').write_text('Another Signing')
        result = self.puller.pull()
        self.assertEqual(result['outcome'], 'failed')
        self.assertEqual(result['reason'], 'Another certificate signed the build')
        self.assertIsNone(updates.read_pending(self.state))
        self.assertFalse((self.state / 'updates' / str(NEW['builtAt'])).exists())

    def test_broken_signature_is_refused(self) -> None:
        # The offering app passes its own check; the archive breaks the seal on the way.
        original = updates._make_export

        def broken_export(root: Path, app: Path, build: dict[str, Any], export: str, run: Any) -> None:
            def run_and_break(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
                if Path(argv[0]).name == 'ditto' and argv[1:2] == ['-c']:
                    (app / 'Contents' / 'BAD_SIGNATURE').write_text('')
                return run(argv, timeout)
            try:
                original(root, app, build, export, run_and_break)
            finally:
                (app / 'Contents' / 'BAD_SIGNATURE').unlink(missing_ok=True)

        with patch.object(updates, '_make_export', broken_export):
            result = self.puller.pull()
        self.assertEqual(result['outcome'], 'failed')
        self.assertTrue(result['reason'].startswith('The signature check failed'))
        self.assertIsNone(updates.read_pending(self.state))

    def test_older_peer_without_update_actions_offers_nothing(self) -> None:
        with patch.object(updates, 'ACTIONS', frozenset()):
            self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertEqual(self.actions('update.offer'), ['update.offer'])
        self.assertIsNone(updates.read_pending(self.state))
        self.assertEqual(json.loads((self.state / updates.STATUS_FILE).read_text())['failures'], {})

    def test_older_peer_that_asks_for_a_link_offers_nothing(self) -> None:
        # A server from before this change routes an unknown action to the remote-parent checks.
        def old_receive(principal: str, envelope: dict[str, Any]) -> dict[str, Any]:
            return {'requestId': envelope['requestId'], 'outcome': 'not_applied',
                    'error': 'Supply a remote-parent link'}
        with patch.object(self.network.services['server'], 'receive', old_receive):
            self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertIsNone(updates.read_pending(self.state))

    def test_stale_pending_update_is_removed(self) -> None:
        folder = self.state / 'updates' / str(OLD['builtAt'])
        (folder / updates.APP_NAME).mkdir(parents=True)
        updates.write_json(self.state / updates.PENDING_FILE,
                           {'id': 'x', 'app': str(folder / updates.APP_NAME), 'build': OLD, 'from': 'server'})
        (self.peer_app / updates.BUILD_STAMP).write_text(json.dumps(OLD))
        self.assertEqual(self.puller.pull()['outcome'], 'none')
        self.assertFalse((self.state / updates.PENDING_FILE).exists())
        self.assertFalse(folder.exists())

    def test_pending_file_outside_the_update_folder_is_ignored(self) -> None:
        updates.write_json(self.state / updates.PENDING_FILE,
                           {'id': 'x', 'app': str(self.peer_app), 'build': NEW, 'from': 'server'})
        self.assertIsNone(updates.read_pending(self.state))

    def test_update_actions_offer_chunks_and_release(self) -> None:
        offer = self.peer('update.offer', {})
        self.assertEqual(offer['value'], {'build': NEW})
        refused = self.peer('update.export', {'builtAt': OLD['builtAt']})
        self.assertEqual((refused['outcome'], refused['error']), ('not_applied', 'This server runs another build'))
        export = self.peer('update.export', {'builtAt': NEW['builtAt']})['value']
        archive = self.peer_state / updates.EXPORTS_DIR / (export['export'] + updates.EXPORT_SUFFIX)
        self.assertEqual(export['size'], archive.stat().st_size)
        self.assertEqual(export['sha256'], hashlib.sha256(archive.read_bytes()).hexdigest())
        # A second export of the same build reuses the archive.
        again = self.peer('update.export', {'builtAt': NEW['builtAt']})['value']
        self.assertEqual(again, export)
        data = b''
        while len(data) < export['size']:
            chunk = self.peer('update.chunk', {'export': export['export'], 'offset': len(data)})['value']
            piece = base64.b64decode(chunk['data'])
            self.assertLessEqual(len(piece), updates.CHUNK_BYTES)
            self.assertEqual(chunk['sha256'], hashlib.sha256(piece).hexdigest())
            self.assertEqual(chunk['nextOffset'], len(data) + len(piece))
            Transport.assert_size(chunk)
            data += piece
        self.assertEqual(hashlib.sha256(data).hexdigest(), export['sha256'])
        bad = self.peer('update.chunk', {'export': export['export'], 'offset': export['size']})
        self.assertEqual((bad['outcome'], bad['error']), ('not_applied', 'Invalid update archive offset'))
        # The receipt of a chunk keeps no archive data.
        with self.network.services['server'].runtime.read_db() as db:
            rows = [json.loads(row[0]) for row in db.execute(
                "SELECT result FROM runtime_server_inbox WHERE id LIKE 'update.chunk-%'")]
        self.assertTrue(rows)
        self.assertTrue(all('data' not in row.get('value', {}) for row in rows))
        unknown = self.peer('update.unknown', {})
        self.assertEqual(unknown['error'], updates.UNKNOWN_ACTION)
        self.assertEqual(self.peer('update.release', {'export': export['export']})['value'], {'released': True})
        self.assertFalse(archive.exists())
        gone = self.peer('update.chunk', {'export': export['export'], 'offset': 0})
        self.assertEqual(gone['error'], 'The update archive has expired or was released')

    def test_release_keeps_an_export_another_server_still_reads(self) -> None:
        service = self.network.services['server']
        for principal in ('mac', 'other'):
            with patch.object(service.transport, 'servers', lambda: [{'id': 'mac'}, {'id': 'other'}]):
                envelope = {'requestId': 'export-' + principal, 'action': 'update.export',
                            'payload': {'builtAt': NEW['builtAt']}}
                export = service.receive(principal, envelope)['value']['export']
        archive = self.peer_state / updates.EXPORTS_DIR / (export + updates.EXPORT_SUFFIX)
        self.peer('update.release', {'export': export})
        self.assertTrue(archive.exists())
        with patch.object(service.transport, 'servers', lambda: [{'id': 'mac'}, {'id': 'other'}]):
            service.receive('other', {'requestId': 'release-other', 'action': 'update.release',
                                      'payload': {'export': export}})
        self.assertFalse(archive.exists())

    def test_source_checkout_offers_no_build(self) -> None:
        with patch.object(updates, 'installed_app', lambda: None):
            self.assertEqual(self.peer('update.offer', {})['value'], {'build': None})
            refused = self.peer('update.export', {'builtAt': NEW['builtAt']})
            self.assertEqual(refused['error'], 'This server does not run an installed app')

    def test_build_stamp_validation(self) -> None:
        self.assertEqual(updates.valid_build(NEW), NEW)
        broken: object
        for broken in ({**NEW, 'version': 2}, {**NEW, 'builtAt': True}, {**NEW, 'commit': 'B' * 40},
                       {**NEW, 'commitTime': 1.5}, {**NEW, 'builtAt': float('nan')}, [], None):
            self.assertIsNone(updates.valid_build(broken))

    def test_chunk_answer_fits_the_response_limit(self) -> None:
        answer = {'requestId': 'x' * 128, 'outcome': 'applied',
                  'value': {'offset': updates.MAX_ARCHIVE_BYTES, 'nextOffset': updates.MAX_ARCHIVE_BYTES,
                            'data': base64.b64encode(b'\xff' * updates.CHUNK_BYTES).decode(), 'sha256': 'f' * 64}}
        self.assertLess(len(json.dumps(answer).encode()), MULTI_SERVER_MAX_RESPONSE_BYTES)


if __name__ == '__main__':
    unittest.main()
