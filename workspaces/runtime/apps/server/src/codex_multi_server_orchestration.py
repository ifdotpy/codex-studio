"""Durable orchestration over the owner-paired server channel.

The transport authenticates both ends. This module never accepts an address or
credential from an agent. Agent IDs remain UUIDs; remote workers have a local
proxy and an explicit remote-parent link. SQLite owns queues and receipts.
"""
from __future__ import annotations

import copy

import base64
from dataclasses import dataclass
import hashlib
import json
import os
import selectors
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
import traceback
from typing import Any, Protocol, cast
import uuid

TIMEOUT = 20
MAX_BUNDLE = 256 * 1024 * 1024
CHUNK = 128 * 1024
RECEIPT_AGE = 7 * 86400
EXPORT_AGE = 86400
INPUT_ATTEMPTS = 8
SPAWN_ATTEMPTS = 8
PATH = '/api/servers/orchestration'


class Transport(Protocol):
    @property
    def local_server_id(self) -> str: ...
    def servers(self) -> list[dict[str, Any]]: ...
    def request(self, server: str, envelope: dict[str, Any], *, timeout: int) -> dict[str, Any]: ...


def identity(*parts: str) -> str:
    return str(uuid.uuid5(uuid.NAMESPACE_URL, 'studio-cross-server:' + ':'.join(parts)))


def encoded(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)


def expired_receipt(key: str) -> dict[str, Any]:
    return {'requestId': key, 'outcome': 'unknown', 'expired': True,
            'detail': 'The receipt has expired. This identity cannot execute again.'}


def live_receipt(result: dict[str, Any]) -> dict[str, Any]:
    value = result.get('value')
    if isinstance(value, dict) and value.get('outputExpiresAt', float('inf')) <= time.time():
        return expired_receipt(result['requestId'])
    return result


def compact_receipt(action: str, result: dict[str, Any]) -> dict[str, Any]:
    if action == 'exec_receipt' and result.get('outcome') == 'applied':
        # This effect-free probe always reads the original receipt again. Its
        # own receipt must not retain another copy of the command output.
        return {**result, 'value': {'requestId': result['value'].get('requestId'),
                                   'outcome': result['value'].get('outcome')}}
    if action != 'chunk' or result.get('outcome') != 'applied':
        return result
    return {**result, 'value': {k: v for k, v in result['value'].items() if k != 'data'}}


def retry_delay(attempts: int) -> int:
    return min(300, 5 << min(max(attempts, 0), 6))


# Only these repository values can affect the Git commands below. All other
# keys require a known neutral override or fail before a content command starts.
GIT_DATA_KEYS = frozenset({
    'core.repositoryformatversion', 'core.bare', 'core.filemode', 'core.ignorecase',
    'core.precomposeunicode', 'core.logallrefupdates', 'core.symlinks', 'core.worktree',
    'core.autocrlf', 'core.eol', 'core.safecrlf', 'core.ignorestat', 'core.trustctime',
    'core.checkstat', 'core.bigfilethreshold', 'core.compression', 'core.abbrev',
    'core.untrackedcache', 'core.sparsecheckout', 'core.sparsecheckoutcone',
    'core.protecthfs', 'core.protectntfs', 'core.warnambiguousrefs',
    'extensions.objectformat', 'extensions.compatobjectformat', 'extensions.refstorage',
    'extensions.worktreeconfig', 'extensions.preciousobjects', 'extensions.relativeworktrees',
    'user.name', 'user.email', 'user.signingkey', 'init.defaultbranch',
    'gpg.format', 'gpg.ssh.allowedsignersfile', 'gpg.ssh.revocationfile',
    'gc.autopacklimit', 'gc.autodetach', 'gc.pruneexpire', 'gc.worktreepruneexpire',
    'gc.reflogexpire', 'gc.reflogexpireunreachable', 'gc.aggressivewindow',
    'gc.aggressivedepth', 'gc.bigpackthreshold', 'gc.writecommitgraph',
    'pack.window', 'pack.depth', 'pack.threads',
    'pack.windowmemory', 'pack.deltacachesize', 'pack.deltacachelimit',
    'pack.packsizelimit', 'pack.compression', 'pack.reuseobjects', 'pack.reusedeltas',
    'pack.usebitmaps', 'pack.writebitmaps', 'pack.writebitmaphashcache',
    'pack.usebitmapindex', 'pack.writebitmaplookuptable', 'pack.indexversion',
    'pack.island', 'pack.islandcore', 'pack.allowpackreuse',
    'index.version', 'index.threads', 'index.recordendofindexentries', 'index.sparse',
    'feature.manyfiles', 'feature.experimental',
    'fetch.prune', 'fetch.prunetags', 'fetch.fsckobjects', 'fetch.writecommitgraph',
    'fetch.parallel', 'fetch.unpacklimit', 'fetch.negotiationalgorithm',
    'fetch.showforcedupdates', 'fetch.bundlecreationtoken',
    'transfer.fsckobjects', 'transfer.unpacklimit',
    'status.showuntrackedfiles', 'status.relativepaths', 'status.short', 'status.branch',
    'status.aheadbehind', 'status.renames', 'status.renamelimit',
    'log.decorate', 'log.abbrevcommit', 'log.date', 'log.follow', 'log.allrefupdates',
    'diff.algorithm', 'diff.renamelimit', 'diff.ignoresubmodules', 'diff.mnemonicprefix',
    'diff.noprefix', 'diff.context', 'diff.interhunkcontext',
    'lfs.repositoryformatversion', 'lfs.url', 'lfs.pushurl', 'lfs.storage',
    'lfs.fetchinclude', 'lfs.fetchexclude', 'lfs.locksverify', 'lfs.basictransfersonly',
    'lfs.concurrenttransfers', 'lfs.pruneoffsetdays', 'lfs.pruneverifyremotealways',
})
GIT_OVERRIDES = {
    'core.fsmonitor': 'false', 'core.hookspath': '/dev/null', 'core.pager': 'cat',
    'core.attributesfile': '/dev/null', 'core.excludesfile': '/dev/null',
    'core.alternaterefscommand': '', 'core.alternaterefsprefixes': '',
    'core.askpass': 'false', 'core.sshcommand': 'false', 'core.gitproxy': 'false',
    'credential.helper': '', 'uploadpack.packobjectshook': '',
    'fetch.recursesubmodules': 'false', 'submodule.recurse': 'false',
    'status.submodulesummary': 'false', 'maintenance.auto': 'false', 'gc.auto': '0',
    'diff.renames': 'false', 'diff.external': '', 'log.showsignature': 'false',
    'commit.gpgsign': 'false', 'gpg.program': 'false', 'gpg.ssh.program': 'false',
    'gpg.x509.program': 'false', 'protocol.allow': 'never',
    'protocol.ext.allow': 'never', 'protocol.ssh.allow': 'never',
    'protocol.http.allow': 'never', 'protocol.https.allow': 'never',
    'protocol.file.allow': 'never',
}


@dataclass(frozen=True)
class GitCommand:
    argv: list[str]
    environment: dict[str, str]


def git_environment(settings: list[tuple[str, str]] | None = None) -> dict[str, str]:
    environment = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    environment.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
                       GIT_TERMINAL_PROMPT='0', GIT_NO_LAZY_FETCH='1')
    if settings is not None:
        environment['GIT_CONFIG_COUNT'] = str(len(settings))
        for i, (key, value) in enumerate(settings):
            environment['GIT_CONFIG_KEY_' + str(i)] = key
            environment['GIT_CONFIG_VALUE_' + str(i)] = value
    return environment


def git_data_key(key: str) -> bool:
    if key in GIT_DATA_KEYS:
        return True
    group, _, rest = key.partition('.')
    name, separator, field = rest.rpartition('.')
    return bool(name and separator and (
        group == 'remote' and field in {'url', 'fetch', 'pushurl'}
        or group == 'branch' and field in {'remote', 'merge', 'rebase'}
        or group == 'submodule' and field in {'url', 'active', 'path'}))


def git_neutral_value(key: str) -> str | None:
    if key in GIT_OVERRIDES:
        return GIT_OVERRIDES[key]
    group, _, rest = key.partition('.')
    name, separator, field = rest.rpartition('.')
    if name and separator:
        if group == 'filter' and field in {'clean', 'smudge', 'process'}:
            return ''
        if group == 'filter' and field == 'required':
            return 'false'
        if group == 'diff' and field in {'command', 'textconv'}:
            return ''
        if group == 'credential' and field == 'helper':
            return ''
        if group == 'protocol' and field == 'allow':
            return 'never'
    return None


def git_command(directory: Path | str, *arguments: str, file_transport: bool = False) -> GitCommand:
    settings = list(GIT_OVERRIDES.items())
    argv = ['git', '--no-pager', '--no-optional-locks', '-C', str(directory)]
    # Config inspection itself does not process content or execute helpers.
    raw, code, truncated = bounded_command(GitCommand(
        [*argv, 'config', '--list', '--includes', '--show-scope', '--name-only', '-z'], git_environment(settings)))
    if truncated or code:
        raise ValueError('Git could not read the effective repository configuration')
    entries = raw.split(b'\0')[:-1] if raw.endswith(b'\0') else []
    if raw and (not raw.endswith(b'\0') or len(entries) % 2):
        raise ValueError('Invalid Git repository configuration')
    for i in range(0, len(entries), 2):
        scope, entry = entries[i:i + 2]
        if scope == b'command':
            continue  # These are the structured overrides above.
        key = os.fsdecode(entry)
        if scope not in {b'local', b'worktree'}:
            raise ValueError('Unsupported Git configuration scope for key: ' + repr(key))
        neutral = git_neutral_value(key)
        if neutral is not None:
            settings.append((key, neutral))
        elif not git_data_key(key):
            raise ValueError('Unsupported Git repository configuration key: ' + repr(key))
    if file_transport:
        settings.append(('protocol.file.allow', 'always'))
    return GitCommand([*argv, *arguments], git_environment(settings))


def git_read(directory: Path | str, *arguments: str) -> tuple[bytes, int]:
    output, code, truncated = bounded_command(git_command(directory, *arguments))
    if truncated:
        raise ValueError('The Git metadata exceeds 64 KiB')
    return output, code


class MultiServerService:
    def __init__(self, runtime: Any, transport: Transport) -> None:
        self.runtime, self.transport = runtime, transport
        self._running = False
        self._claim_lock = threading.RLock()
        # The scheduler claim must never wait behind an inbound SQLite writer.
        self._tick_lock = threading.Lock()
        self._diagnostic_lock = threading.Lock()
        self._command_lock = threading.RLock()
        self._command_service: Any = None
        self._move_service: Any = None
        self._pruned_at = 0.0
        with runtime.db() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT);
                CREATE INDEX IF NOT EXISTS runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
                CREATE TABLE IF NOT EXISTS runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT);
                CREATE TABLE IF NOT EXISTS runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
                CREATE TABLE IF NOT EXISTS runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT);
                CREATE TABLE IF NOT EXISTS runtime_server_retired (
                    id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
                    server TEXT, PRIMARY KEY(id,direction));
                CREATE TABLE IF NOT EXISTS runtime_server_output_tools (
                    id TEXT PRIMARY KEY, expires REAL NOT NULL);
                CREATE INDEX IF NOT EXISTS runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
                CREATE TABLE IF NOT EXISTS runtime_server_sequence (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
                INSERT OR IGNORE INTO runtime_server_sequence VALUES(1,0);
            ''')
            for table in ('outbox', 'inbox'):
                columns = {row[1] for row in db.execute('PRAGMA table_info(runtime_server_' + table + ')')}
                for column in ('created_at', 'completed_at'):
                    if column not in columns:
                        db.execute('ALTER TABLE runtime_server_' + table + ' ADD COLUMN ' + column + ' REAL NOT NULL DEFAULT 0')
                db.execute('UPDATE runtime_server_' + table + ' SET created_at=? WHERE created_at=0', (time.time(),))
                db.execute('UPDATE runtime_server_' + table + ' SET completed_at=? WHERE completed_at=0 AND state=?', (time.time(), 'complete'))
            maximum = db.execute('SELECT COALESCE(MAX(rowid),0) FROM runtime_server_outbox').fetchone()[0]
            if 'fingerprint' not in {r[1] for r in db.execute('PRAGMA table_info(runtime_server_outbox)')}:
                db.execute('ALTER TABLE runtime_server_outbox ADD COLUMN fingerprint TEXT')
            if 'actor' not in {r[1] for r in db.execute('PRAGMA table_info(runtime_server_retired)')}:
                db.execute('ALTER TABLE runtime_server_retired ADD COLUMN actor TEXT')
            for column in ('principal', 'actor'):
                if column not in {r[1] for r in db.execute('PRAGMA table_info(runtime_server_inbox)')}:
                    db.execute('ALTER TABLE runtime_server_inbox ADD COLUMN ' + column + ' TEXT')
            db.execute('UPDATE runtime_server_sequence SET value=MAX(value,?) WHERE id=1', (maximum,))

    def commands(self) -> Any:
        with self._command_lock:
            if self._command_service is None:
                from codex_server_exec import ServerExec
                self._command_service = ServerExec(self)
            return self._command_service

    def moves(self) -> Any:
        with self._command_lock:
            if self._move_service is None:
                from codex_agent_move import AgentMoves
                self._move_service = AgentMoves(self)
            return self._move_service

    def close(self) -> None:
        with self._command_lock:
            if self._command_service is not None:
                self._command_service.close()

    def exec_completed(self, principal: str, actor: str, record: dict[str, Any]) -> None:
        key = identity('command-exit', record['handle'], record['status'])
        payload = {'actor': actor, 'handle': record['handle'], 'record': record}
        if principal == self.server_id:
            self._exec_event(principal, payload, key)
        else:
            with self.runtime.db() as db:
                if (db.execute('SELECT 1 FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
                        or db.execute("SELECT 1 FROM runtime_server_retired WHERE id=? AND direction='out'", (key,)).fetchone()):
                    return
                self.queue(db, principal, 'exec_event', payload, key)

    @staticmethod
    def _exec_owner(db: Any, server: str, actor: str, handle: str) -> None:
        row = db.execute('SELECT server,body FROM runtime_server_outbox WHERE id=?', (handle,)).fetchone()
        envelope = json.loads(row['body']) if row else {}
        if (not row or row['server'] != server or envelope.get('action') != 'exec'
                or envelope.get('payload', {}).get('actor') != actor):
            raise PermissionError('The command handle belongs to another actor or server')

    def _exec_event(self, principal: str, payload: dict[str, Any], key: str) -> dict[str, Any]:
        with self.runtime.lock, self.runtime.db() as db:
            self._exec_owner(db, principal, payload['actor'], payload['handle'])
            if not db.execute('SELECT 1 FROM runtime_agents WHERE id=?', (payload['actor'],)).fetchone():
                return {'stored': False, 'detail': 'The lead no longer exists'}
            actor = self.runtime.agent(payload['actor'], db)
            if actor.get('deletedAt'):
                return {'stored': False, 'detail': 'The lead was deleted'}
            record = payload['record']
            self.runtime.enqueue_recovery_event(db, actor, 'monitor_exit', encoded({
                'id': payload['handle'], 'serverId': principal, 'status': record['status'],
                'exitCode': record.get('exitCode'), 'signal': record.get('signal'),
                'duration': record.get('duration'), 'timedOut': record.get('timedOut', False),
                'error': record.get('error'), 'detail': 'Read output with orchestration_servers action=exec_read.'}), key)
        return {'stored': True}

    @property
    def server_id(self) -> str:
        return self.transport.local_server_id

    def link(self, db: Any, link_id: str) -> dict[str, Any]:
        row = db.execute('SELECT record FROM runtime_server_links WHERE id=?', (link_id,)).fetchone()
        if not row:
            raise ValueError('Unknown remote-parent link')
        return json.loads(row[0])  # type: ignore[no-any-return]

    def queue(self, db: Any, server: str, action: str, payload: dict[str, Any], key: str) -> str:
        body = encoded({'requestId': key, 'action': action, 'payload': payload})
        if len(body.encode('utf-8')) > 256 * 1024:
            raise ValueError('The remote request exceeds 256 KiB; use a smaller batch')
        digest = hashlib.sha256(body.encode()).hexdigest()
        row = db.execute('SELECT server,body,fingerprint FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
        if row and (row[0] != server or (row[2] or hashlib.sha256(row[1].encode()).hexdigest()) != digest):
            raise ValueError('This request id has different content')
        retired = db.execute('SELECT signature,server FROM runtime_server_retired WHERE id=? AND direction=?', (key, 'out')).fetchone()
        if retired:
            if retired['server'] != server or retired['signature'] != hashlib.sha256(body.encode()).hexdigest():
                raise ValueError('This request id has different content')
            return key
        db.execute('INSERT OR IGNORE INTO runtime_server_outbox(id,server,body,state,created_at,fingerprint) VALUES (?,?,?,?,?,?)',
                   (key, server, body, 'queued', time.time(), digest))
        self.runtime.changed.set()
        return key

    def deliver(self, key: str) -> dict[str, Any]:
        # Retry only the same durable envelope. The receiver never repeats an
        # uncertain operation. Do not hold runtime or SQLite locks during HTTP.
        with self.runtime.read_db() as db:
            row = db.execute('SELECT * FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
            if not row:
                if db.execute('SELECT 1 FROM runtime_server_retired WHERE id=? AND direction=?', (key, 'out')).fetchone():
                    return expired_receipt(key)
                raise ValueError('Unknown remote request receipt')
            envelope, server = json.loads(row['body']), row['server']
            if row['state'] == 'complete' and envelope['action'] != 'chunk':
                return live_receipt(json.loads(row['result']))
            if envelope['action'] == 'input' and db.execute("""
                SELECT 1 FROM runtime_server_outbox earlier
                WHERE earlier.rowid < (SELECT rowid FROM runtime_server_outbox WHERE id=?)
                  AND earlier.state!='complete' AND earlier.server=?
                  AND json_extract(earlier.body,'$.action')='input'
                  AND json_extract(earlier.body,'$.payload.worker')=?
                  AND json_extract(earlier.body,'$.payload.link')=? LIMIT 1
                """, (key, server, envelope['payload']['worker'], envelope['payload']['link'])).fetchone():
                return {'requestId': key, 'outcome': 'unknown', 'queued': True,
                        'waitingFor': 'The earlier input receipt'}
        result: dict[str, Any]
        try:
            if envelope['action'] == 'exec' and envelope['payload'].get('receiptDigest'):
                probe = {'requestId': identity('exec-receipt', key), 'action': 'exec_receipt',
                    'payload': {'handle': key, 'actor': envelope['payload']['actor'],
                                'digest': envelope['payload']['receiptDigest']}}
                reply = (self.receive(self.server_id, probe) if server == self.server_id
                         else self.transport.request(server, probe, timeout=TIMEOUT))
                result = reply['value'] if reply.get('outcome') == 'applied' else {'requestId': key, 'outcome': 'unknown'}
            else:
                result = (self.receive(self.server_id, envelope) if server == self.server_id
                          else self.transport.request(server, envelope, timeout=TIMEOUT))
            if result.get('requestId') != key or result.get('outcome') not in {'applied', 'not_applied', 'unknown'}:
                raise RuntimeError('The paired server returned an invalid receipt')
        except Exception as error:
            with self.runtime.lock, self.runtime.db() as db:
                self._compact_exec(db, envelope)
                final = self._retry(db, envelope, type(error).__name__)
                if final is not None:
                    return final
            return {'requestId': key, 'outcome': 'unknown', 'status': 'offline', 'queued': True}
        with self.runtime.lock, self.runtime.db() as db:
            self._compact_exec(db, envelope)
            current = db.execute('SELECT state,result FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
            if current['state'] == 'complete' and envelope['action'] != 'chunk':
                # A concurrent response cannot reopen a terminal unknown input.
                return live_receipt(json.loads(current['result']))
            if result['outcome'] != 'unknown':
                db.execute("UPDATE runtime_server_outbox SET state='complete',result=?,error=NULL,completed_at=? WHERE id=?",
                           (encoded(compact_receipt(envelope['action'], result)), time.time(), key))
                if envelope['action'] in {'add_location', 'remove_location'} and result['outcome'] == 'applied':
                    from codex_project_locations import applied as location_applied
                    location_applied(self.runtime, db, envelope, server, result)
                if envelope['action'] == 'spawn':
                    if result['outcome'] == 'applied':
                        self._spawn_received(db, envelope['payload'], result['value'])
                    elif result['outcome'] == 'not_applied':
                        for worker_id in envelope['payload']['workers']:
                            proxy = self.runtime.agent(worker_id, db)
                            proxy.update(status='failed', inFlight=False, error=result['error'])
                            self.runtime.put(db, 'agents', proxy)
                        self.runtime.release_failed_work(db, self.runtime.release_work_agents(db), force=True)
                if envelope['action'] == 'admit' and result['outcome'] == 'applied':
                    worker = self.runtime.agent(envelope['payload']['worker'], db)
                    admission = worker.get('remoteAdmission') or {}
                    if admission.get('id') == key and worker['epoch'] == envelope['payload']['epoch']:
                        admission['state'] = 'granted'
                        worker['remoteLastAdmission'] = key
                        worker['remoteAdmission'] = admission
                        self.runtime.put(db, 'agents', worker)
                        self.runtime.changed.set()
            else:
                final = self._retry(db, envelope, None)
                if final is not None:
                    return final
        return result

    def _compact_exec(self, db: Any, envelope: dict[str, Any]) -> None:
        if envelope['action'] != 'exec' or envelope['payload'].get('receiptDigest'):
            return
        digest = hashlib.sha256(encoded([self.server_id, 'exec', envelope['payload']]).encode()).hexdigest()
        body = encoded({**envelope, 'payload': {'actor': envelope['payload']['actor'], 'receiptDigest': digest}})
        db.execute('UPDATE runtime_server_outbox SET body=?,fingerprint=COALESCE(fingerprint,?) WHERE id=?',
                   (body, hashlib.sha256(encoded(envelope).encode()).hexdigest(), envelope['requestId']))

    def _retry(self, db: Any, envelope: dict[str, Any], error: str | None) -> dict[str, Any] | None:
        key = envelope['requestId']
        row = db.execute('SELECT * FROM runtime_server_outbox WHERE id=?', (key,)).fetchone()
        if row['state'] == 'complete' and envelope['action'] != 'chunk':
            return live_receipt(json.loads(row['result']))
        attempts = row['attempts'] + 1
        action = envelope['action']
        bound = {'input': INPUT_ATTEMPTS, 'spawn': SPAWN_ATTEMPTS}.get(action)
        if bound is not None and attempts >= bound:
            result = {'requestId': key, 'outcome': 'unknown', 'terminal': True,
                      'detail': 'outcome unknown, inspect the worker'}
            db.execute("UPDATE runtime_server_outbox SET state='complete',attempts=?,result=?,completed_at=?,error=? WHERE id=?",
                (attempts, encoded(result), time.time(), error, key))
            workers = (envelope['payload']['workers'] if action == 'spawn'
                       else [envelope['payload']['worker']])
            text = 'Remote ' + action + ' ' + key + ': outcome unknown, inspect the worker ' + ', '.join(workers) + '.'
            if action == 'spawn':
                for worker_id in workers:
                    proxy = self.runtime.agent(worker_id, db)
                    if proxy.get('remoteStateSequence') or proxy.get('remoteAdmissionRequest'):
                        # Reverse-channel evidence already proves a worker
                        # exists. Keep its current state and occupied slot.
                        continue
                    # No native turn was proved absent. Release the local
                    # reservation, but never resend or replace this spawn.
                    proxy.update(status='paused', inFlight=False, remoteReservation=False,
                                 error=text)
                    self.runtime.put(db, 'agents', proxy)
                # Keep task ownership: an unknown worker may still exist.
                # Reassignment requires inspection of the remote effect.
            worker = self.runtime.agent(workers[0], db)
            lead = self.runtime.agent(worker['rootId'], db)
            self.runtime.enqueue_recovery_event(db, lead, 'agent_message', encoded({
                'sender': worker['id'], 'sender_name': worker['name'], 'text': text,
                'request_id': key}), identity(action + '-unknown-notice', key))
            return result
        db.execute("UPDATE runtime_server_outbox SET attempts=?,next_at=?,error=?,"
                   "state=CASE WHEN json_extract(body,'$.action')='chunk' THEN 'queued' ELSE state END WHERE id=?",
                   (attempts, time.time() + retry_delay(row['attempts']), error, key))
        return None

    def tick(self) -> None:
        self.moves().tick()
        with self._tick_lock:
            if self._running or self.runtime.closed:
                return
            self._running = True
        def run() -> None:
            try:
                if time.time() - self._pruned_at > 3600:
                    self.prune()
                with self.runtime.read_db() as db:
                    keys = [r[0] for r in db.execute("""
                        SELECT current.id FROM runtime_server_outbox current
                        WHERE current.state='queued' AND current.next_at<=?
                          AND (json_extract(current.body,'$.action')!='input' OR NOT EXISTS (
                            SELECT 1 FROM runtime_server_outbox earlier
                            WHERE earlier.rowid<current.rowid AND earlier.state!='complete'
                              AND earlier.server=current.server
                              AND json_extract(earlier.body,'$.action')='input'
                              AND json_extract(earlier.body,'$.payload.worker')=json_extract(current.body,'$.payload.worker')
                              AND json_extract(earlier.body,'$.payload.link')=json_extract(current.body,'$.payload.link')))
                        ORDER BY current.rowid LIMIT 8
                        """, (time.time(),))]
                for key in keys:
                    if self.runtime.closed:
                        break
                    self.deliver(key)
            finally:
                with self._tick_lock:
                    self._running = False
        self.runtime.delivery_executor().submit(run)

    def prune(self) -> None:
        now = time.time()
        folder = self.runtime.root / 'server-exports'
        if folder.is_dir():
            for path in folder.iterdir():
                if path.suffix in {'.bundle', '.partial'}:
                    try:
                        if path.stat().st_mtime < now - EXPORT_AGE:
                            path.unlink(missing_ok=True)
                    except FileNotFoundError:
                        pass
        with self.runtime.db() as db:
            from codex_server_exec import expire_tool_output
            for row in db.execute('SELECT id,expires FROM runtime_server_output_tools WHERE expires<=?', (now,)).fetchall():
                expired = expire_tool_output({'serverOutputExpiresAt': row['expires']})
                db.execute('UPDATE runtime_tool_results SET result=? WHERE id=?', (encoded(expired), row['id']))
                receipt = self.runtime.tool_request(row['id'], db)
                if receipt and receipt.get('result'):
                    receipt['result'] = expired
                    self.runtime.put(db, 'tool_requests', receipt)
                db.execute('DELETE FROM runtime_server_output_tools WHERE id=?', (row['id'],))
            for table in ('outbox', 'inbox'):
                for row in db.execute('SELECT id FROM runtime_server_' + table +
                        " WHERE json_extract(result,'$.value.outputExpiresAt')<=?", (now,)).fetchall():
                    db.execute('UPDATE runtime_server_' + table + ' SET result=? WHERE id=?',
                               (encoded(expired_receipt(row['id'])), row['id']))
            for row in db.execute("SELECT id,server,body,fingerprint FROM runtime_server_outbox WHERE state='complete' AND completed_at<?", (now - RECEIPT_AGE,)).fetchall():
                envelope = json.loads(row['body'])
                actor = envelope['payload'].get('actor') if envelope['action'].startswith('exec') else None
                db.execute('INSERT OR IGNORE INTO runtime_server_retired(id,direction,signature,server,actor) VALUES(?,?,?,?,?)',
                    (row['id'], 'out', row['fingerprint'] or hashlib.sha256(row['body'].encode()).hexdigest(), row['server'], actor))
                db.execute('DELETE FROM runtime_server_outbox WHERE id=?', (row['id'],))
            db.execute("INSERT OR IGNORE INTO runtime_server_retired(id,direction,signature,server) SELECT id,'in',signature,NULL FROM runtime_server_inbox WHERE state='complete' AND completed_at<?", (now - RECEIPT_AGE,))
            db.execute("DELETE FROM runtime_server_inbox WHERE state='complete' AND completed_at<?", (now - RECEIPT_AGE,))
        if self._command_service is not None:
            self._command_service.prune(now)
        self._pruned_at = now

    def spawn(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        specs = args.get('agents')
        if not actor.get('isLead') or not isinstance(specs, list) or not 1 <= len(specs) <= 64:
            raise ValueError('Only the lead can create 1 to 64 remote workers')
        targets = {s.get('server', args.get('server')) for s in specs if isinstance(s, dict)}
        if len(targets) != 1:
            raise ValueError('Use one server per spawn batch')
        server = targets.pop()
        if not isinstance(server, str):
            raise ValueError('Select a paired server')
        if server not in {s['id'] for s in self.transport.servers()}:
            raise ValueError('Select a paired server')
        for spec in specs:
            if not isinstance(spec, dict) or not isinstance(spec.get('cwd'), str) or not Path(spec['cwd']).is_absolute():
                raise ValueError('Supply an absolute cwd on the remote server for every worker')
            if not isinstance(spec.get('name'), str) or not 1 <= len(spec['name'].strip()) <= 100 or not isinstance(spec.get('prompt'), str) or not 1 <= len(spec['prompt'].strip()) <= 32000:
                raise ValueError('Every worker needs a name and task')
        link_id = identity(self.server_id, actor['id'], key)
        workers = [str(uuid.uuid5(uuid.NAMESPACE_URL, 'remote-worker:' + link_id + ':' + str(i))) for i in range(len(specs))]
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch']:
                raise ValueError('The parent was stopped')
            from codex_agent_modes import assert_delegation
            assert_delegation(current)
            # A remote batch reserves execution slots before the remote server
            # can start. Unknown/offline batches retain these slots.
            previous_request = db.execute('SELECT server,body,state,result,error FROM runtime_server_outbox WHERE id=?', (identity(link_id, 'spawn'),)).fetchone()
            retired_request = db.execute('SELECT 1 FROM runtime_server_retired WHERE id=? AND direction=?', (identity(link_id, 'spawn'), 'out')).fetchone()
            if retired_request:
                old = self.link(db, link_id)
                if old['server'] != server or old['specs'] != [{k:v for k,v in s.items() if k != 'server'} for s in specs]:
                    raise ValueError('This request id has different content')
                return {**expired_receipt(key), 'remoteRequestId': identity(link_id, 'spawn'), 'server': server,
                    'agents': [{'id': w, 'name': s['name'], 'cwd': s['cwd'], 'server': server} for w,s in zip(workers,specs)]}
            if previous_request:
                old = json.loads(previous_request['body'])['payload']
                if previous_request['server'] != server or old['specs'] != [{k:v for k,v in s.items() if k != 'server'} for s in specs]:
                    raise ValueError('This request id has different content')
                return {'requestId': key, 'remoteRequestId': identity(link_id, 'spawn'), 'server': server,
                        'agents': [{'id': w, 'name': s['name'], 'cwd': s['cwd'], 'server': server} for w,s in zip(workers,specs)],
                        'outcome': json.loads(previous_request['result'])['outcome'] if previous_request['result'] else 'unknown',
                        'recovery': 'Read the saved remote request receipt.'}
            existing = self.runtime.team_agents(db, current['rootId'])
            busy = self.runtime.dispatch_active_slots(db)
            from codex_runtime import team_capacity_counts
            active, _finished = team_capacity_counts(existing, current['rootId'])
            if active + len(workers) > current['maxAgents']:
                raise ValueError('The remote batch exceeds the team agent limit')
            if sum(a['rootId'] == current['rootId'] and a['id'] != current['rootId'] for a in busy) + len(workers) > current['concurrency']:
                raise ValueError('The remote batch exceeds available execution slots')
            tasks = [s['task_id'] for s in specs if s.get('task_id')]
            if len(tasks) != len(set(tasks)):
                raise ValueError('Assign each task to one worker')
            works = {}
            for task in tasks:
                work = self.runtime.work_by_id(db, task, current['rootId'])
                if not work or work.get('owner') or work['status'] in {'review', 'accepted', 'cancelled'}:
                    raise ValueError('The remote task is unknown or already assigned')
                works[task] = work
            payload = {'link': link_id, 'home': self.server_id, 'parent': actor['id'],
                       'parentEpoch': actor['epoch'], 'root': actor['rootId'], 'workers': workers,
                       'concurrency': current['concurrency'], 'yoloMode': current.get('yoloMode', False),
                       'defaults': self.runtime.worker_defaults(current), 'leadModel': current['model'],
                       'specs': [{k: v for k, v in s.items() if k != 'server'} for s in specs],
                       'tasks': works}
            remote_key = identity(link_id, 'spawn')
            prior = db.execute('SELECT body FROM runtime_server_outbox WHERE id=?', (remote_key,)).fetchone()
            if prior:
                # Validate immutable caller content without regenerating task
                # versions or permission settings on a retry.
                old = json.loads(prior[0])['payload']
                if old['specs'] != payload['specs'] or old['parent'] != actor['id']:
                    raise ValueError('This request id has different content')
            else:
                db.execute('INSERT INTO runtime_server_links VALUES (?,?)', (link_id, encoded({**payload, 'server': server, 'side': 'home'})))
                for worker_id, spec in zip(workers, specs):
                    proxy = {**current, 'id': worker_id, 'isLead': False, 'parentId': current['id'],
                             'rootId': current['rootId'], 'name': spec['name'], 'prompt': spec['prompt'],
                             'cwd': spec['cwd'], 'role': spec.get('role', 'implementer'),
                             'threadId': None, 'turnId': None, 'epoch': 0, 'turnEpoch': 0,
                             'status': 'starting', 'inFlight': True, 'created': time.time(),
                             'worktree': False, 'imageWorkspace': False,
                             'workspaceMode': spec.get('workspace'),
                             'workspaceBackend': None,
                             'remoteWorker': {'server': server, 'link': link_id}, 'remoteReservation': True, 'error': None}
                    for field in ('startAttempt', 'workerDefaults', 'reviewDefaults', 'quickCreate', 'needsTitle',
                                  'executionMove', 'movedTo', 'movedFrom', 'frozenNativeParams', 'executionArchives', 'executionRouteAliases', 'moveReviewPending', 'moveImportPending'):
                        proxy.pop(field, None)
                    self.runtime.put(db, 'agents', proxy)
                    if spec.get('task_id'):
                        work = works[spec['task_id']]
                        work.update(owner=worker_id, version=work['version'] + 1, updated=time.time())
                        self.runtime.put(db, 'work', work)
                self.queue(db, server, 'spawn', payload, remote_key)
        receipt = self.deliver(remote_key)
        return {'requestId': key, 'remoteRequestId': remote_key, 'server': server,
                'outcome': receipt['outcome'], 'status': receipt.get('status'),
                'agents': ([{**summary, 'server': server} for summary in receipt['value']['agents']]
                           if receipt['outcome'] == 'applied' else
                           [{'id': w, 'name': s['name'], 'cwd': s['cwd'], 'server': server} for w, s in zip(workers, specs)]),
                'delivery': 'Remote events wait in a durable queue while the server is offline.'}

    def _spawn_received(self, db: Any, payload: dict[str, Any], value: dict[str, Any]) -> None:
        for summary in value['agents']:
            proxy = self.runtime.agent(summary['id'], db)
            # A late spawn receipt cannot replace a newer worker snapshot.
            if not proxy['autoWake'] or proxy.get('remoteStateSequence'):
                continue
            proxy.update(cwd=summary['cwd'], branch=summary.get('branch'),
                         environment=summary.get('environment', proxy.get('environment')),
                         workspaceMode=summary.get('workspace', proxy.get('workspaceMode')),
                         workspaceBackend=summary.get('workspaceBackend', proxy.get('workspaceBackend')),
                         workerBaseCommit=summary.get('baseCommit'), status='starting')
            self.runtime.put(db, 'agents', proxy)

    def event(self, db: Any, agent: dict[str, Any], kind: str, text: str, key: str) -> str | None:
        remote = agent.get('remoteWorker')
        if agent.get('movedTo') and agent.get('remoteOrigin'):
            origin = agent['remoteOrigin']
            return self.queue(db, origin['home'], 'move_home_event',
                {'agent': agent['id'], 'link': origin['link'], 'kind': kind, 'text': text}, identity('move-home-event', key))
        if remote:
            metadata = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (key,)).fetchone()
            metadata = json.loads(metadata[0]) if metadata else {}
            if metadata.get('assets'):
                raise ValueError('Remote worker input does not support attachments')
            link = self.link(db, remote['link'])
            return self.queue(db, remote['server'], 'input', {'link': remote['link'], 'worker': agent['id'],
                'kind': kind, 'text': text, 'delivery': metadata.get('delivery', 'queue'),
                'parentEpoch': link['parentEpoch'],
                'epoch': agent.get('remoteEpoch', 0), 'controlEpoch': agent['epoch']}, identity('input', key))
        anchor = agent.get('remoteAnchor')
        if anchor:
            link = self.link(db, anchor['link'])
            return self.queue(db, anchor['home'], 'event', {'link': anchor['link'], 'kind': kind,
                'parentEpoch': link['parentEpoch'], 'text': text}, identity('event', key))
        return None

    def admission(self, db: Any, worker: dict[str, Any]) -> bool:
        admission = worker.get('remoteAdmission') or {}
        if admission.get('epoch') == worker['epoch']:
            return admission.get('state') == 'granted'
        key = identity(worker['id'], 'admit', str(worker['epoch']), uuid.uuid4().hex)
        origin = worker['remoteOrigin']
        link = self.link(db, origin['link'])
        self.queue(db, origin['home'], 'admit', {'link': origin['link'], 'worker': worker['id'],
            'parentEpoch': link['parentEpoch'], 'epoch': worker['epoch']}, key)
        worker['remoteAdmission'] = {'id': key, 'epoch': worker['epoch'], 'state': 'pending'}
        self.runtime.put(db, 'agents', worker)
        return False

    def rebind_parent(self, db: Any, agent: dict[str, Any]) -> None:
        if not agent.get('isLead'):
            return
        rows = db.execute("SELECT id,record FROM runtime_server_links WHERE json_extract(record,'$.side')='home' AND json_extract(record,'$.parent')=?", (agent['id'],)).fetchall()
        if not rows:
            return
        for row in rows:
            link = json.loads(row['record'])
            link['parentEpoch'] = agent['epoch']
            db.execute('UPDATE runtime_server_links SET record=? WHERE id=?', (encoded(link), row['id']))
            self.queue(db, link['server'], 'rebind', {'link': row['id'], 'parentEpoch': agent['epoch']},
                identity(row['id'], 'rebind', str(agent['epoch'])))

    def state(self, db: Any, agent: dict[str, Any], previous: dict[str, Any] | None) -> None:
        origin = agent.get('remoteOrigin')
        remote = agent.get('remoteWorker')
        if not (origin or remote or agent.get('remoteAnchor')):
            return
        if origin and agent.get('remoteAdmission') and agent.get('status') in {'completed', 'failed', 'paused', 'interrupted'} and not agent.get('inFlight'):
            agent.pop('remoteAdmission', None)
            self.runtime.put(db, 'agents', agent)
        fields = ('status', 'cwd', 'branch', 'environment', 'workspaceMode', 'workspaceBackend', 'workerBaseCommit', 'autoWake', 'epoch', 'inFlight', 'tokensUsed', 'error')
        if origin and not agent.get('movedTo') and (previous is None or any(agent.get(k) != previous.get(k) for k in fields)
                       or agent.get('startAttempt') != previous.get('startAttempt')):
            key = identity(agent['id'], 'state', uuid.uuid4().hex)
            sequence = db.execute('UPDATE runtime_server_sequence SET value=value+1 WHERE id=1 RETURNING value').fetchone()[0]
            link = self.link(db, origin['link'])
            record = {k: agent.get(k) for k in fields}
            record['inFlight'] = any(a['id'] == agent['id'] for a in self.runtime.dispatch_active_slots(db))
            self.queue(db, origin['home'], 'state', {'link': origin['link'], 'worker': agent['id'],
                'parentEpoch': link['parentEpoch'], 'sequence': sequence, 'record': {**record,
                    'admissionId': (agent.get('remoteAdmission') or {}).get('id') or agent.get('remoteLastAdmission')}}, key)
        if remote and not (origin and agent.get('movedTo')) and previous and previous.get('autoWake') and agent.get('autoWake') is False:
            self.queue(db, remote['server'], 'stop', {'link': remote['link'], 'worker': agent['id'],
                'reason': agent.get('error') or 'Stopped by the parent', 'controlEpoch': agent['epoch']}, identity(agent['id'], 'stop', str(agent['epoch'])))

    def worker_call(self, actor: dict[str, Any], action: str, args: dict[str, Any], key: str) -> dict[str, Any]:
        origin = actor['remoteOrigin']
        key = identity(actor['id'], action, key)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if current['epoch'] != actor['epoch']:
                raise ValueError('The worker was stopped')
            link = self.link(db, origin['link'])
            payload = {'link': origin['link'], 'worker': actor['id'], 'epoch': actor['epoch'],
                'parentEpoch': link['parentEpoch'], 'args': args}
            self.queue(db, origin['home'], action, payload, key)
        receipt = self.deliver(key)
        if receipt['outcome'] == 'not_applied':
            raise ValueError(receipt['error'])
        if receipt['outcome'] == 'unknown':
            return {**receipt, 'detail': 'The request is saved. Do not submit another identity.'}
        return receipt['value']  # type: ignore[no-any-return]

    def receive(self, principal: str, envelope: dict[str, Any]) -> dict[str, Any]:
        # principal is authenticated by the pairing boundary, never by payload.
        if principal != self.server_id and principal not in {s['id'] for s in self.transport.servers()}:
            raise PermissionError('The server is not paired')
        key, action, payload = envelope.get('requestId'), envelope.get('action'), envelope.get('payload')
        if not isinstance(key, str) or not 1 <= len(key) <= 200 or not isinstance(action, str) or not isinstance(payload, dict):
            raise ValueError('Invalid cross-server request')
        signature = hashlib.sha256(encoded([principal, action, payload]).encode()).hexdigest()
        with self._claim_lock, self.runtime.db() as db:
            retired = db.execute('SELECT signature FROM runtime_server_retired WHERE id=? AND direction=?', (key, 'in')).fetchone()
            if retired:
                if retired['signature'] != signature:
                    raise ValueError('This request id has different content')
                return expired_receipt(key)
            row = db.execute('SELECT * FROM runtime_server_inbox WHERE id=?', (key,)).fetchone()
            if row:
                if row['signature'] != signature:
                    raise ValueError('This request id has different content')
                if row['result'] and action not in {'chunk', 'exec_receipt'}:
                    return live_receipt(json.loads(row['result']))
                # Preserve the crash boundary. Only an exact durable effect can
                # reconcile a running receipt; no second execution is allowed.
                if (row['state'] == 'waiting'
                        or action in {'admit', 'projects', 'folders', 'git', 'chunk', 'directory', 'chat_read', 'context', 'exec_read', 'exec_receipt', 'move_validate', 'move_status', 'move_begin', 'move_chunk', 'move_activate', 'move_home_validate', 'move_route_ready', 'move_route_status', 'move_home_event', 'location_matches', 'location_info'}
                        or action == 'task' and payload.get('args', {}).get('action') in {'list', 'get', 'history'}
                        or action == 'complaint' and payload.get('args', {}).get('action') == 'read'):
                    # Reads have no effect. Slot admission is a compare-and-set reservation, with no
                    # native input. A denied reservation has no effect to replay.
                    pass
                else:
                    evidence = self._evidence(db, action, payload, key)
                    if evidence is None:
                        return {'requestId': key, 'outcome': 'unknown'}
                    result = {'requestId': key, 'outcome': 'applied', 'value': evidence}
                    db.execute("UPDATE runtime_server_inbox SET state='complete',result=?,completed_at=? WHERE id=?", (encoded(result), time.time(), key))
                    return result
            db.execute('INSERT OR IGNORE INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor) VALUES (?,?,?,NULL,?,?,?)',
                       (key, signature, 'running', time.time(), principal, payload.get('actor') if isinstance(payload.get('actor'), str) else None))
            if row and row['state'] == 'waiting':
                # Waiting proves that this envelope has not executed an effect.
                # Reserve it again before an input can cross the native boundary.
                db.execute("UPDATE runtime_server_inbox SET state='running' WHERE id=?", (key,))
        try:
            value = self._receive(principal, action, payload, key)
            if action == 'input' and value.get('waiting'):
                with self.runtime.db() as db:
                    db.execute("UPDATE runtime_server_inbox SET state='waiting' WHERE id=?", (key,))
                return {'requestId': key, 'outcome': 'unknown', 'waitingFor': 'The stop or parent update'}
            if action == 'admit' and not value.get('granted'):
                return {'requestId': key, 'outcome': 'unknown'}
            if action == 'stop' and value.get('pending'):
                return {'requestId': key, 'outcome': 'unknown'}
            result = {'requestId': key, 'outcome': 'applied', 'value': value}
        except (ValueError, PermissionError) as error:
            result = {'requestId': key, 'outcome': 'not_applied', 'error': str(error)}
        except Exception as error:
            # Native/OS failures can follow a committed effect. Keep unknown.
            self._unknown_diagnostic(key, action, error)
            return {'requestId': key, 'outcome': 'unknown'}
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='complete',result=?,completed_at=? WHERE id=?",
                (encoded(compact_receipt(action, result)), time.time(), key))
        return result

    def _unknown_diagnostic(self, key: str, action: str, error: Exception) -> None:
        # Arbitrary exception text can include prompts, credentials, or a whole
        # HTTP body. Only fixed native messages and OS errno descriptions are
        # safe to publish. Frames have no source text, locals, or full paths.
        try:
            safe_messages = {'No installed Codex version passed the protocol checks',
                             'Codex compatibility check is in progress'}
            message = str(error)
            if message not in safe_messages:
                message = (os.strerror(error.errno) if isinstance(error, OSError) and error.errno
                           else 'Exception message redacted because it can contain private data')
            diagnostic = {'event': 'cross_server_outcome_unknown', 'at': time.time(),
                'requestId': key, 'action': action, 'errorType': type(error).__name__, 'message': message,
                'traceback': [{'file': Path(frame.filename).name, 'line': frame.lineno,
                               'function': frame.name} for frame in traceback.extract_tb(error.__traceback__)]}
            path = self.runtime.root / 'orchestration-errors.log'
            # Keep diagnostics even when account startup failed before there
            # was an app-server log. No effect or receipt depends on this file.
            from codex_log_rotation import RotatingLog
            with self._diagnostic_lock:
                log = RotatingLog(path, max_bytes=1024 * 1024, backups=2)
                try:
                    path.chmod(0o600)
                    log.write(encoded(diagnostic) + '\n')
                    path.chmod(0o600)
                finally:
                    log.close()
        except Exception:
            pass

    def _evidence(self, db: Any, action: str, payload: dict[str, Any], key: str) -> dict[str, Any] | None:
        if action == 'move_home_relocate':
            row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (payload['agent'],)).fetchone()
            if row and json.loads(row[0]).get('movedTo', {}).get('move') == payload['move']:
                return {'redirected': True}
        if action == 'move_child':
            child_id = payload['args']['child']['id']
            row = db.execute('SELECT record FROM runtime_agents WHERE id=?', (child_id,)).fetchone()
            if row:
                child = json.loads(row[0])
                if child.get('parentId') == payload['worker'] and child.get('remoteWorker', {}).get('link') == payload['link']:
                    return {'registered': True, 'agentId': child_id}
        if action == 'move_prepare':
            row = db.execute('SELECT record FROM runtime_agent_moves WHERE id=?', (identity(payload['move'], 'target'),)).fetchone()
            if row and json.loads(row[0])['phase'] in {'ready', 'active'}:
                return {'phase': json.loads(row[0])['phase']}
        if action in {'add_location', 'remove_location'}:
            row = db.execute('SELECT result FROM runtime_operation_receipts WHERE id=?',
                             (identity('location-effect', key),)).fetchone()
            return json.loads(row[0]) if row else None
        if action == 'exec':
            if not db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_server_exec'").fetchone():
                return None
            row = db.execute('SELECT record FROM runtime_server_exec WHERE id=?', (key,)).fetchone()
            if row:
                record = json.loads(row[0])
                if record['status'] not in {'starting', 'unknown'}:
                    return cast(dict[str, Any], record)
        if action == 'exec_event':
            if db.execute('SELECT 1 FROM runtime_events WHERE id=?', (key,)).fetchone():
                return {'stored': True}
        if action == 'release':
            path = self.runtime.root / 'server-exports' / (str(uuid.UUID(payload['export'])) + '.bundle')
            if not path.exists():
                return {'released': payload['export']}
        if action == 'rebind':
            if self.link(db, payload['link'])['parentEpoch'] >= payload['parentEpoch']:
                return {'rebound': True}
        row = db.execute('SELECT result FROM runtime_operation_receipts WHERE id=?', (key,)).fetchone()
        if row:
            value = json.loads(row[0])
            if action == 'task':
                return {**self.runtime.task_brief(value), 'requestId': key,
                    'detail': {'tool': 'orchestration_task', 'action': 'get', 'task_id': value['id']}}
            return cast(dict[str, Any], value)
        if action == 'spawn':
            spawn_key = 'remote-worker:' + payload['link']
            row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (spawn_key,)).fetchone()
            if row:
                from codex_payloads import resolve_result
                result = resolve_result(self.runtime.root, row[0])
                if result.get('success'):
                    return cast(dict[str, Any], json.loads(result['contentItems'][0]['text']))
        if action in {'input', 'event'}:
            row = db.execute('SELECT status,error FROM runtime_events WHERE id=?', (key,)).fetchone()
            if row:
                return {'id': key, 'status': row['status'], 'error': row['error']}
        if action == 'export':
            export_id = identity(key, 'bundle')
            path = self.runtime.root / 'server-exports' / (export_id + '.bundle')
            if path.is_file():
                return {'export': export_id, 'bytes': path.stat().st_size, 'sha256': file_digest(path), 'branch': payload['branch']}
        if action == 'stop':
            worker = self.runtime.agent(payload['worker'], db)
            if (worker.get('remoteStopRequest') == key and not worker.get('inFlight') and not worker.get('turnId')
                    and not any(a['id'] == worker['id'] for a in self.runtime.dispatch_active_slots(db))):
                return {'stopped': [worker['id']]}
        if action == 'state':
            worker = self.runtime.agent(payload['worker'], db)
            if worker.get('remoteStateSequence', 0) >= payload['sequence']:
                return {'stored': True, 'reconciled': True}
        return None

    def _receive(self, principal: str, action: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if action in {'move_validate', 'move_status', 'move_begin', 'move_chunk', 'move_prepare', 'move_activate', 'move_home_validate', 'move_home_relocate', 'move_home_event', 'move_route_ready', 'move_route_status'}:
            return cast(dict[str, Any], self.moves().receive(principal, action, p, key))
        if action in {'add_location', 'remove_location'}:
            from codex_project_locations import receive as receive_location
            return receive_location(self.runtime, principal, action, p, key)
        if action == 'location_info':
            from codex_project_locations import info
            return info(p['cwd'])
        if action == 'location_matches':
            from codex_project_locations import matches
            return matches(self.runtime, p['origin'], p.get('cwd'))
        if action == 'exec_receipt':
            with self.runtime.db() as db:
                original = db.execute('SELECT * FROM runtime_server_inbox WHERE id=?', (p['handle'],)).fetchone()
                if not original:
                    return {'requestId': p['handle'], 'outcome': 'unknown'}
                if original['principal'] is None and original['actor'] is None:
                    # Receipts from the previous schema still need ownership
                    # proof before a new effect-free probe can read them.
                    job = db.execute('SELECT owner,actor FROM runtime_server_exec WHERE id=?', (p['handle'],)).fetchone()
                    if not job or job['owner'] != principal or job['actor'] != p.get('actor'):
                        raise PermissionError('The command receipt belongs to another actor or server')
                elif original['principal'] != principal or original['actor'] != p.get('actor'):
                    raise PermissionError('The command receipt belongs to another actor or server')
                if original['signature'] != p.get('digest'):
                    raise PermissionError('The receipt fingerprint belongs to another request')
                if original['result']:
                    return live_receipt(json.loads(original['result']))
                evidence = self._evidence(db, 'exec', {}, p['handle'])
                return ({'requestId': p['handle'], 'outcome': 'applied', 'value': evidence} if evidence
                        else {'requestId': p['handle'], 'outcome': 'unknown'})
        if action == 'exec':
            return cast(dict[str, Any], self.commands().start(principal, p, key))
        if action == 'exec_read':
            return cast(dict[str, Any], self.commands().read(principal, p))
        if action in {'exec_input', 'exec_cancel'}:
            return cast(dict[str, Any], self.commands().control(principal, action, p, key))
        if action == 'exec_event':
            return self._exec_event(principal, p, key)
        if action == 'spawn':
            return self._remote_spawn(principal, p, key)
        if action in {'projects', 'folders', 'git', 'export', 'chunk', 'release'}:
            return self._git_action(action, p, key)
        with self.runtime.lock, self.runtime.db() as db:
            link_id = p.get('link')
            if not isinstance(link_id, str):
                raise ValueError('Supply a remote-parent link')
            link = self.link(db, link_id)
            expected = link['server'] if link['side'] == 'home' else link['home']
            if principal != expected:
                raise PermissionError('The remote-parent link belongs to another server')
            if action == 'rebind':
                if link['side'] != 'remote' or type(p.get('parentEpoch')) is not int or p['parentEpoch'] < link['parentEpoch']:
                    raise ValueError('The parent epoch was superseded')
                link['parentEpoch'] = p['parentEpoch']
                db.execute('UPDATE runtime_server_links SET record=? WHERE id=?', (encoded(link), link_id))
                return {'rebound': True}
            parent_superseded = link['side'] == 'home' and p.get('parentEpoch', link['parentEpoch']) != link['parentEpoch']
            if parent_superseded and action != 'state':
                if action == 'event':
                    return {'stored': False, 'reason': 'The parent epoch was superseded'}
                raise ValueError('The parent epoch was superseded')
            if action in {'input', 'stop'}:
                if link['side'] != 'remote' or p.get('worker') not in link['workers']:
                    raise PermissionError('Worker is outside this remote-parent link')
                worker = self.runtime.agent(p['worker'], db)
            elif action in {'state', 'task', 'message', 'admit', 'directory', 'complaint', 'chat_read', 'context', 'move_spawn', 'move_send', 'move_interrupt', 'move_manage', 'move_child'}:
                if link['side'] != 'home' or p.get('worker') not in link['workers']:
                    raise PermissionError('Worker is outside this home team')
                worker = self.runtime.agent(p['worker'], db)
                if (worker.get('movedTo') or worker.get('executionRouteAliases')) and worker.get('remoteWorker') != {'server': principal, 'link': link_id}:
                    raise PermissionError('The worker has moved to another execution route')
            elif action == 'event':
                if link['side'] != 'home':
                    raise PermissionError('The event is not addressed to this home team')
            else:
                raise ValueError('Unknown cross-server operation')
            if action == 'admit':
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch'] or not parent['autoWake'] or not worker['autoWake']:
                    raise ValueError('The home team was stopped')
                if p['epoch'] < worker.get('remoteEpoch', 0):
                    raise ValueError('The remote worker epoch changed')
                worker['remoteEpoch'] = p['epoch']
                root = self.runtime.agent(worker['rootId'], db)
                from codex_budget import budget_admission
                budget_admission(self.runtime, db, worker)
                if root['concurrency'] == 0:
                    return {'granted': False}
                slots = [a for a in self.runtime.dispatch_active_slots(db) if a['rootId'] == root['id'] and a['id'] != root['id']]
                if worker.get('remoteReservation') and worker.get('remoteAdmissionRequest') == key:
                    return {'granted': True}
                initial = bool(worker.get('remoteReservation') and not worker.get('remoteAdmissionRequest')
                    and any(a['id'] == worker['id'] for a in slots))
                if len(slots) - int(initial) >= root['concurrency']:
                    return {'granted': False}
                worker.update(remoteReservation=True, remoteAdmissionRequest=key, inFlight=True, status='starting')
                self.runtime.put(db, 'agents', worker)
                return {'granted': True}
            if action == 'state':
                record = p['record']
                if p['sequence'] <= worker.get('remoteStateSequence', 0):
                    return {'stored': False, 'reason': 'A newer remote state already exists'}
                worker['remoteStateSequence'] = p['sequence']
                worker['remoteEpoch'] = max(worker.get('remoteEpoch', 0), record['epoch'])
                terminal = record.get('status') in {'completed', 'failed', 'paused', 'idle', 'interrupted'} and not record.get('inFlight')
                if (terminal
                        and worker.get('remoteAdmissionRequest')
                        and record.get('admissionId') != worker['remoteAdmissionRequest']):
                    self.runtime.put(db, 'agents', worker)
                    return {'stored': False, 'reason': 'This snapshot belongs to an earlier admission'}
                if terminal:
                    # Parent resume must not strand the previous native slot.
                    # Admission identity still prevents releasing a newer turn.
                    worker.update(inFlight=False, remoteReservation=False)
                if parent_superseded or not worker['autoWake'] or self.runtime.agent(link['parent'], db)['epoch'] != link['parentEpoch']:
                    self.runtime.put(db, 'agents', worker)
                    return {'stored': False, 'reason': 'The parent or worker was stopped or superseded'}
                worker.update({k: record[k] for k in ('status', 'cwd', 'branch', 'workerBaseCommit', 'inFlight', 'tokensUsed', 'error') if k in record})
                worker['inFlight'] = bool(worker.get('remoteReservation') or record.get('inFlight'))
                worker['remoteEpoch'] = record['epoch']
                self.runtime.put(db, 'agents', worker)
                return {'stored': True}
            if action == 'event':
                if p['kind'] not in {'child_result', 'child_stopped', 'agent_message', 'work_review'}:
                    raise ValueError('Unsupported remote event kind')
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch']:
                    return {'stored': False, 'reason': 'The parent was stopped'}
                if p['kind'] == 'child_result':
                    details = json.loads(p['text'])
                    if details.get('agent_id') not in link['workers']:
                        raise PermissionError('The result is outside this remote-parent link')
                    proxy = self.runtime.agent(details['agent_id'], db)
                    proxy['lastAnswer'] = details.get('result', details.get('reason', ''))
                    proxy['lastCompletedTurn'] = key
                    self.runtime.put(db, 'agents', proxy)
                self.runtime.enqueue_recovery_event(db, parent, p['kind'], p['text'], key)
                return {'eventId': key}
            if action in {'task', 'message', 'directory', 'complaint', 'chat_read', 'context', 'move_spawn', 'move_send', 'move_interrupt', 'move_manage', 'move_child'}:
                parent = self.runtime.agent(link['parent'], db)
                if parent['epoch'] != link['parentEpoch'] or not parent['autoWake'] or not worker['autoWake']:
                    raise ValueError('The home team was stopped')
                if p['epoch'] < worker.get('remoteEpoch', 0):
                    raise ValueError('The worker epoch changed')
                worker['remoteEpoch'] = p['epoch']
                self.runtime.put(db, 'agents', worker)
            if action == 'input':
                if (p.get('parentEpoch', link['parentEpoch']) > link['parentEpoch']
                        or p.get('controlEpoch', 0) > worker.get('remoteControlEpoch', 0)):
                    return {'waiting': True}
                if p.get('parentEpoch', link['parentEpoch']) != link['parentEpoch']:
                    raise ValueError('The parent epoch was superseded')
                if p.get('controlEpoch', 0) != worker.get('remoteControlEpoch', 0):
                    raise ValueError('The remote worker control epoch changed')
                if worker.get('moveImportPending') or worker.get('movedTo'):
                    # A queued input can arrive after the source becomes read-only.
                    # Route it through the canonical home without reopening native input here.
                    self.runtime.enqueue_recovery_event(db, worker, p['kind'], p['text'], key)
                    return {'eventId': key}
                if p['kind'] in {'user', 'followup'}:
                    # send handles explicit resumption and preserves native input
                    # receipts. Other events never reopen a stopped worker.
                    pass
                else:
                    self.runtime.enqueue_recovery_event(db, worker, p['kind'], p['text'], key)
                    return {'eventId': key}
        if action == 'input':
            return cast(dict[str, Any], self.runtime.send(worker['id'], p['text'], key, manual=p['kind']=='user',
                resume=True, delivery=p.get('delivery', 'queue'), _expected_epoch=worker['epoch']))
        if action == 'stop':
            value = self.runtime.stop(worker['id'], True, reason=p['reason'],
                _remote_request={'id': key, 'controlEpoch': p['controlEpoch']})
            with self.runtime.read_db() as db:
                if self._evidence(db, 'stop', p, key) is None:
                    return {'pending': True}
            return cast(dict[str, Any], value)
        if action in {'move_spawn', 'move_send', 'move_interrupt', 'move_manage', 'move_child'}:
            if not worker.get('movedTo'):
                raise PermissionError('This agent has no move redirect')
            if worker.get('remoteOrigin'):
                return self.worker_call(worker, action, p['args'], key)
            if action == 'move_child':
                summary = p['args']['child']
                if set(summary) - {'id', 'name', 'cwd', 'model', 'effort', 'nativeEffort', 'fastMode', 'provider', 'role', 'epoch', 'created', 'prompt'}:
                    raise ValueError('The review child contains unsupported fields')
                child_id = summary['id']
                uuid.UUID(child_id)
                with self.runtime.lock, self.runtime.db() as db:
                    link = self.link(db, p['link'])
                    existing = db.execute('SELECT record FROM runtime_agents WHERE id=?', (child_id,)).fetchone()
                    if existing:
                        proxy = json.loads(existing[0])
                        if proxy.get('parentId') != worker['id'] or proxy.get('remoteWorker') != {'server': principal, 'link': p['link']}:
                            raise PermissionError('The review child identity belongs to another parent')
                    else:
                        proxy = {**worker, **summary, 'id': child_id, 'parentId': worker['id'], 'rootId': worker['rootId'],
                                 'isLead': False, 'threadId': None, 'turnId': None, 'status': 'starting', 'inFlight': True,
                                 'autoWake': True, 'remoteReservation': True, 'remoteWorker': {'server': principal, 'link': p['link']}}
                        for field in list(proxy):
                            if field.startswith(('imageWorkspace', 'workerBase', 'worktree', 'remote')) and field not in {'remoteReservation', 'remoteWorker'} or field in {'executionMove', 'movedFrom', 'movedTo', 'frozenNativeParams', 'startAttempt', 'executionArchives', 'executionRouteAliases', 'moveImportPending'}:
                                proxy.pop(field, None)
                        self.runtime.put(db, 'agents', proxy)
                    if child_id not in link['workers']:
                        link['workers'].append(child_id)
                        db.execute('UPDATE runtime_server_links SET record=? WHERE id=?', (encoded(link), p['link']))
                return {'registered': True, 'agentId': child_id}
            if action == 'move_spawn':
                args = copy.deepcopy(p['args'])
                if not args.get('server'):
                    args['server'] = principal
                return cast(dict[str, Any], self.runtime.spawn_agents(worker, args, key))
            args = copy.deepcopy(p['args'])
            if args.get('agent_id'):
                args['agent_id'] = self.runtime.resolve_visible_agent_id(worker['id'], args['agent_id'], include_archived=action == 'move_manage')
            if action == 'move_manage':
                from codex_agent_management import manage_agent
                return manage_agent(self.runtime, worker['id'], args, worker['epoch'])
            target = self.runtime.agent(args['agent_id'])
            cursor = target
            while cursor.get('parentId') and cursor['parentId'] != worker['id']:
                cursor = self.runtime.agent(cursor['parentId'])
            if cursor.get('parentId') != worker['id']:
                raise PermissionError('You can control only your descendants')
            if action == 'move_interrupt':
                return cast(dict[str, Any], self.runtime.stop(target['id'], True,
                    reason='Stopped by agent ' + worker['name'], sender=worker['id'], sender_epoch=worker['epoch']))
            return cast(dict[str, Any], self.runtime.send(target['id'], p['args']['text'], key, manual=False,
                resume=True, delivery=p['args'].get('delivery', 'queue'), sender=worker['id'], sender_epoch=worker['epoch']))
        if worker.get('movedTo') and worker.get('remoteOrigin') and action in {'task', 'message', 'directory', 'complaint', 'chat_read', 'context'}:
            return self.worker_call(worker, action, p['args'], key)
        if action == 'chat_read':
            return cast(dict[str, Any], self.runtime.chat_read(p['args']['room_id'], worker['id'], p['args'].get('before'), model=True))
        if action == 'context':
            if p['args'].get('topic') not in {'plan','complaints'}:
                raise ValueError('This context belongs to the destination server')
            return cast(dict[str, Any], self.runtime.model_context(worker['id'], p['args']))
        if action == 'directory':
            if p['args'].get('tool') not in {'orchestration_status','orchestration_peers'}:
                raise ValueError('Unknown remote directory tool')
            return cast(dict[str, Any], self.runtime.model_directory(worker['id'], p['args']['tool'], p['args'].get('arguments', {})))
        if action == 'complaint':
            return cast(dict[str, Any], self.runtime.complaint(worker['id'], p['args'], key, worker['epoch']))
        if action == 'task':
            return cast(dict[str, Any], self.runtime.model_work(worker['id'], p['args'], key, worker['epoch']))
        if action == 'message':
            return cast(dict[str, Any], self.runtime.chat_message(worker['id'], p['args']['target'], p['args']['text'], key, worker['epoch'],
                importance=p['args'].get('importance', 'message'), progress_key=p['args'].get('progress_key'),
                progress_version=p['args'].get('progress_version')))
        raise ValueError('Unknown cross-server operation')

    def _remote_spawn(self, principal: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if p.get('home') != principal:
            raise PermissionError('Remote-parent identity differs from the paired server')
        specs = p['specs']
        if not specs or len(specs) != len(p['workers']):
            raise ValueError('Invalid remote worker batch')
        anchor_id = identity(p['link'], 'anchor')
        # Use the destination's account catalog and project defaults. Local
        # account keys from the home server are never inferred to exist here.
        with self.runtime.read_db() as db:
            existing = db.execute('SELECT record FROM runtime_agents WHERE id=?', (p['parent'],)).fetchone()
            moved_parent = json.loads(existing[0]) if existing else None
        if (moved_parent and moved_parent.get('movedFrom') and not moved_parent.get('movedTo')
                and moved_parent.get('remoteOrigin', {}).get('home') == principal
                and moved_parent.get('isLead') and moved_parent.get('autoWake')):
            anchor = moved_parent
        else:
            anchor = self.runtime.create({'id': anchor_id, 'name': 'Remote parent ' + p['parent'][:8],
                'prompt': '', 'cwd': specs[0]['cwd'], 'yolo_mode': p['yoloMode'],
                'concurrency': p['concurrency']}, draft=True)
            anchor['remoteAnchor'] = {'link': p['link'], 'home': principal}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', anchor)
            db.execute('INSERT OR IGNORE INTO runtime_server_links VALUES (?,?)', (p['link'], encoded({**p, 'side': 'remote'})))
        # Existing spawn owns account/base validation and atomic worker + input
        # commits. Its UUID derivation is used on both servers.
        spawn_key = 'remote-worker:' + p['link']
        expected_ids = [str(uuid.uuid5(uuid.NAMESPACE_URL, spawn_key + ':' + str(i))) for i in range(len(specs))]
        if expected_ids != p['workers']:
            raise ValueError('Remote worker identities differ')
        resolved = []
        for spec in specs:
            defaults = p['defaults']
            clean = {'model': defaults.get('model') or p['leadModel'], 'effort': defaults.get('effort'),
                     'fast_mode': defaults.get('fastMode', False),
                     **{k: v for k, v in spec.items() if k != 'task_id'}}
            if spec.get('task_id'):
                task = p['tasks'][spec['task_id']]
                clean['prompt'] += '\n\n[Remote Studio task ' + task['id'] + '] ' + task['title'] + '\nSubmit evidence through orchestration_task.'
            clean['_remoteOrigin'] = {'home': principal, 'link': p['link']}
            resolved.append(clean)
        return cast(dict[str, Any], self.runtime.spawn_agents(anchor, {'agents': resolved}, spawn_key))

    def tools(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        if not actor.get('isLead'):
            raise ValueError('Only the lead can use remote server tools')
        action = args.get('action', 'list')
        if action == 'list':
            with self.runtime.read_db() as db:
                pending = {r[0] for r in db.execute("SELECT DISTINCT server FROM runtime_server_outbox WHERE state='queued' AND error IS NOT NULL")}
            return {'localServer': self.server_id, 'servers': [{**s, **({'status': 'offline'} if s['id'] in pending else {})} for s in self.transport.servers()]}
        if action in {'add_location', 'remove_location'}:
            from codex_project_locations import request as location_request
            return location_request(self.runtime, {**args, 'request_id': key}, actor=actor)
        if action == 'location_matches':
            from codex_project_locations import suggestions
            return suggestions(self.runtime, args['project'], args['server'], identity(actor['id'], action, key))
        if action in {'exec', 'exec_read', 'exec_input', 'exec_cancel'}:
            return self._exec_tool(actor, args, key)
        server = args.get('server', self.server_id if action == 'receipt' else None)
        if action == 'receipt' and server == 'local':
            server = self.server_id
        if not isinstance(server, str):
            raise ValueError('Select a paired server')
        if (server not in {s['id'] for s in self.transport.servers()}
                and not (action == 'receipt' and server == self.server_id)):
            raise ValueError('Select a paired server')
        if action == 'fetch':
            return self._fetch(actor, args, key)
        if action not in {'projects', 'folders', 'git', 'receipt'}:
            raise ValueError('Select list, projects, folders, git, fetch, or receipt')
        if action == 'receipt':
            with self.runtime.read_db() as db:
                from codex_project_locations import tool_receipt_key
                request_id = tool_receipt_key(db, actor['id'], args['request_id'], server)
                row = db.execute('SELECT server,state,result,error,body FROM runtime_server_outbox WHERE id=?', (request_id,)).fetchone()
                if row:
                    envelope = json.loads(row['body'])
                    if envelope['action'].startswith('exec') and envelope['payload'].get('actor') != actor['id']:
                        raise PermissionError('The command receipt belongs to another lead')
                if not row:
                    row = db.execute('SELECT server,state,result,NULL AS error FROM runtime_server_fetch WHERE id=?', (request_id,)).fetchone()
                if not row:
                    retired = db.execute('SELECT actor FROM runtime_server_retired WHERE id=? AND direction=? AND server=?',
                        (request_id, 'out', server)).fetchone()
                    if retired:
                        if retired['actor'] and retired['actor'] != actor['id']:
                            raise PermissionError('The command receipt belongs to another lead')
                        return {'state': 'expired', 'result': expired_receipt(request_id), 'error': None}
            if not row or row['server'] != server:
                raise ValueError('Unknown remote request receipt')
            return {'state': row['state'], 'result': live_receipt(json.loads(row['result'])) if row['result'] else None, 'error': row['error']}
        payload: dict[str, Any] = {k: args[k] for k in ('cwd', 'argv') if k in args}
        key = identity(actor['id'], action, key)
        with self.runtime.db() as db:
            self.queue(db, server, action, payload, key)
        return self.deliver(key)

    def _exec_tool(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        action = args['action']
        server = args.get('server') or self.server_id
        if server == 'local':
            server = self.server_id
        if server != self.server_id and server not in {s['id'] for s in self.transport.servers()}:
            raise ValueError('Select local or a paired server')
        from codex_server_exec import validate
        payload: dict[str, Any] = {k: args[k] for k in ('cwd', 'command', 'env', 'timeout', 'output_limit',
                   'handle', 'input', 'close_stdin', 'stdout_offset', 'stderr_offset') if k in args}
        payload['actor'] = actor['id']
        if action == 'exec':
            payload = validate(payload, allow_foreign_windows_path=server != self.server_id)
        key = identity(actor['id'], action, key)
        if len(encoded({'requestId': key, 'action': action, 'payload': payload}).encode()) > 256 * 1024:
            raise ValueError('The encoded command request exceeds 256 KiB')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.checked_actor(db, actor['id'], actor['id'])
            if not current.get('isLead') or current['epoch'] != actor['epoch']:
                raise PermissionError('Only the active lead can use server commands')
            if action != 'exec':
                handle = payload.get('handle')
                if not isinstance(handle, str):
                    raise ValueError('Supply a command handle')
                self._exec_owner(db, server, actor['id'], handle)
            self.queue(db, server, action, payload, key)
        return self.deliver(key)

    def _git_action(self, action: str, p: dict[str, Any], key: str) -> dict[str, Any]:
        if action == 'projects':
            return cast(dict[str, Any], self.runtime.projects())
        if action == 'folders':
            from codex_project_locations import folders
            return folders(p.get('cwd'))
        if action == 'release':
            path = self.runtime.root / 'server-exports' / (str(uuid.UUID(p['export'])) + '.bundle')
            path.unlink(missing_ok=True)
            return {'released': p['export']}
        if action == 'chunk':
            path = self.runtime.root / 'server-exports' / (str(uuid.UUID(p['export'])) + '.bundle')
            if not path.is_file():
                raise ValueError('The Git bundle has expired or was released')
            size = path.stat().st_size
            offset = p.get('offset')
            if type(offset) is not int or not 0 <= offset < size:
                raise ValueError('Invalid bundle offset')
            with path.open('rb') as stream:
                stream.seek(offset)
                data = stream.read(CHUNK)
            return {'offset': offset, 'nextOffset': offset + len(data),
                    'data': base64.b64encode(data).decode(), 'sha256': hashlib.sha256(data).hexdigest()}
        directory = Path(p.get('cwd', ''))
        if not directory.is_absolute() or not directory.is_dir():
            raise ValueError('Supply an existing absolute repository folder')
        if action == 'export':
            branch = p.get('branch')
            if not isinstance(branch, str) or branch.startswith('-'):
                raise ValueError('Supply a worker branch')
            if git_read(directory, 'check-ref-format', '--branch', branch)[1] != 0:
                raise ValueError('Invalid worker branch')
            known = p.get('known', [])
            if not isinstance(known, list) or len(known) > 16 or any(not commit_hash(c) for c in known):
                raise ValueError('Supply at most 16 complete commit hashes')
            exclusions = [c for c in known if git_read(directory, 'cat-file', '-e', c + '^{commit}')[1] == 0]
            tip, code = git_read(directory, 'rev-parse', '--verify', 'refs/heads/' + branch)
            if code or not commit_hash(tip.decode().strip()):
                raise ValueError('Unknown worker branch')
            if any(git_read(directory, 'merge-base', '--is-ancestor', tip.decode().strip(), commit)[1] == 0 for commit in exclusions):
                return {'alreadyHave': tip.decode().strip(), 'bytes': 0, 'branch': branch}
            export_id = identity(key, 'bundle')
            folder = self.runtime.root / 'server-exports'
            folder.mkdir(exist_ok=True)
            path = folder / (export_id + '.bundle')
            if not path.exists():
                if shutil.disk_usage(folder).free < MAX_BUNDLE:
                    raise RuntimeError('Insufficient disk space for a Git bundle')
                temporary = folder / (export_id + '.partial')
                export_bundle(directory, branch, temporary, exclusions)
                temporary.replace(path)
            return {'export': export_id, 'bytes': path.stat().st_size, 'sha256': file_digest(path), 'branch': branch}
        argv = p.get('argv')
        if not isinstance(argv, list) or not argv or len(argv) > 8 or any(not isinstance(a, str) or len(a) > 1024 for a in argv):
            raise ValueError('Supply a bounded Git argument list')
        # Allow complete command forms, not arbitrary Git switches. This blocks
        # aliases, external diff/textconv, config writers and option injection.
        allowed = (argv in [['status', '--porcelain=v1'], ['branch', '--list'],
                            ['rev-parse', 'HEAD'], ['rev-parse', '--show-toplevel']]
                   or argv[:2] in [['log', '-n'], ['show', '--no-patch']])
        if argv[:2] == ['log', '-n']:
            allowed = len(argv) == 3 and argv[2].isdigit() and 1 <= int(argv[2]) <= 50
        elif argv[:2] == ['show', '--no-patch']:
            allowed = len(argv) == 3 and len(argv[2]) in {40, 64} and all(c in '0123456789abcdef' for c in argv[2])
        if not allowed:
            raise ValueError('Unsupported read-only Git command')
        if argv[0] in {'log', 'show'}:
            argv = [argv[0], '--no-show-signature', '--no-ext-diff', '--no-textconv', *argv[1:]]
        elif argv[0] == 'status':
            argv = [*argv, '--no-renames', '--ignore-submodules=all']
        command = git_command(directory, *argv)
        raw, code, truncated = bounded_command(command)
        return {'exitCode': code, 'output': raw.decode('utf-8', errors='replace'), 'truncated': truncated}

    def _fetch(self, actor: dict[str, Any], args: dict[str, Any], key: str) -> dict[str, Any]:
        worker = self.runtime.agent(args.get('agent_id'))
        if worker['rootId'] != actor['rootId'] or not worker.get('remoteWorker') or worker['remoteWorker']['server'] != args['server']:
            raise ValueError('Select a remote worker in this team')
        branch, destination = args.get('branch'), args.get('destination', actor['cwd'])
        if not isinstance(branch, str) or not branch or branch.startswith('-'):
            raise ValueError('Supply a worker branch')
        destination = str(Path(destination).expanduser().resolve())
        if git_read(destination, 'check-ref-format', '--branch', branch)[1] != 0:
            raise ValueError('Invalid worker branch')
        if git_read(destination, 'rev-parse', '--git-dir')[1] != 0:
            raise ValueError('Supply an existing local Git repository')
        known = []
        for reference in ('HEAD', 'origin/main', 'FETCH_HEAD'):
            output, code = git_read(destination, 'rev-parse', '--verify', reference + '^{commit}')
            known_commit = output.decode().strip()
            if code == 0 and commit_hash(known_commit):
                known.append(known_commit)
        base = worker.get('workerBaseCommit')
        if commit_hash(base) and git_read(destination, 'cat-file', '-e', base + '^{commit}')[1] == 0:
            known.append(base)
        with self.runtime.lock, self.runtime.db() as db:
            signature, previous = self.runtime.operation_receipt(db, key, {'worker': worker['id'], 'branch': branch, 'destination': destination})
            if previous is not None:
                return previous  # type: ignore[no-any-return]
            prior_fetch = db.execute('SELECT * FROM runtime_server_fetch WHERE id=?', (key,)).fetchone()
            if prior_fetch and prior_fetch['signature'] != signature:
                raise ValueError('This request id has different content')
            if prior_fetch and prior_fetch['state'] == 'fetching':
                return {'requestId': key, 'outcome': 'unknown', 'detail': 'The local Git fetch has no final receipt. Inspect FETCH_HEAD; do not repeat it.'}
            db.execute('INSERT OR IGNORE INTO runtime_server_fetch VALUES (?,?,?,?,NULL)',
                (key, args['server'], signature, 'preparing'))
            export_key = identity(key, 'export')
            prior_export = db.execute('SELECT body FROM runtime_server_outbox WHERE id=?', (export_key,)).fetchone()
            export = json.loads(prior_export['body'])['payload'] if prior_export else {
                'cwd': worker['cwd'], 'branch': branch, 'known': sorted(set(known))}
            self.queue(db, args['server'], 'export', export, export_key)
        receipt = self.deliver(export_key)
        if receipt['outcome'] != 'applied':
            return receipt
        metadata = receipt['value']
        already = metadata.get('alreadyHave')
        if already and (not commit_hash(already) or git_read(destination, 'cat-file', '-e', already + '^{commit}')[1] != 0):
            raise ValueError('The advertised commit is absent from the local repository')
        if type(metadata.get('bytes')) is not int or not (metadata['bytes'] == 0 and already or 0 < metadata['bytes'] <= MAX_BUNDLE):
            raise ValueError('Invalid remote bundle size')
        with tempfile.TemporaryDirectory(prefix='studio-server-fetch-') as temporary:
            path = Path(temporary) / 'worker.bundle'
            if shutil.disk_usage(temporary).free < metadata['bytes'] * 2:
                raise RuntimeError('Insufficient disk space for the remote Git bundle')
            offset = 0
            digest = hashlib.sha256()
            with path.open('wb') as stream:
                while offset < metadata['bytes']:
                    chunk_key = identity(key, 'chunk', str(offset))
                    with self.runtime.db() as db:
                        self.queue(db, args['server'], 'chunk', {'export': metadata['export'], 'offset': offset}, chunk_key)
                    chunk_receipt = self.deliver(chunk_key)
                    if chunk_receipt['outcome'] != 'applied':
                        return chunk_receipt
                    chunk = chunk_receipt['value']
                    data = base64.b64decode(chunk['data'], validate=True)
                    if (not data or len(data) > CHUNK or chunk['offset'] != offset or chunk['nextOffset'] != offset + len(data)
                            or offset + len(data) > metadata['bytes'] or hashlib.sha256(data).hexdigest() != chunk['sha256']):
                        raise ValueError('Invalid remote bundle chunk')
                    stream.write(data)
                    digest.update(data)
                    offset += len(data)
            if not already and digest.hexdigest() != metadata['sha256']:
                raise ValueError('The remote bundle checksum differs')
            if not already:
                command = git_command(destination, 'bundle', 'verify', str(path))
                subprocess.run(command.argv, env=command.environment, check=True, capture_output=True, timeout=30)
            source = destination if already else str(path.resolve())
            # FETCH_HEAD is the only ref changed; branch integration belongs to
            # the orchestrator's separate review and merge action.
            with self.runtime.db() as db:
                claimed = db.execute("UPDATE runtime_server_fetch SET state='fetching' WHERE id=? AND state='preparing'", (key,)).rowcount
            if not claimed:
                return {'requestId': key, 'outcome': 'unknown', 'detail': 'The original local Git fetch is already reserved.'}
            command = git_command(destination, 'fetch', '--no-tags', source,
                already or 'refs/heads/' + branch, file_transport=True)
            subprocess.run(command.argv, env=command.environment, check=True, capture_output=True, timeout=60)
            output, code = git_read(destination, 'rev-parse', 'FETCH_HEAD')
            commit = output.decode().strip()
            if code or not commit_hash(commit):
                raise RuntimeError('Git did not return a final fetch commit')
        value = {'requestId': key, 'server': args['server'], 'branch': branch, 'commit': commit,
                 'bytes': metadata['bytes'], 'destination': destination}
        with self.runtime.db() as db:
            self.runtime.save_receipt(db, key, signature, value)
            db.execute("UPDATE runtime_server_fetch SET state='complete',result=? WHERE id=?", (encoded(value), key))
            if not already:
                release_key = identity(key, 'release')
                self.queue(db, args['server'], 'release', {'export': metadata['export']}, release_key)
        if not already:
            self.deliver(release_key)
        return value


def commit_hash(value: Any) -> bool:
    return isinstance(value, str) and len(value) in {40, 64} and all(c in '0123456789abcdef' for c in value)


def export_bundle(directory: Path, branch: str, path: Path, exclusions: list[str]) -> None:
    command = git_command(directory, 'bundle', 'create', '-', 'refs/heads/' + branch, *('^' + c for c in exclusions))
    process = subprocess.Popen(command.argv, env=command.environment, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    assert process.stdout is not None
    size = 0
    deadline = time.monotonic() + 60
    try:
        with path.open('wb') as output, selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command.argv, 60)
                if not selector.select(remaining):
                    continue
                chunk = os.read(process.stdout.fileno(), 128 * 1024)
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_BUNDLE:
                    raise ValueError('The Git bundle exceeds 256 MiB')
                output.write(chunk)
        if process.wait(timeout=2) != 0:
            raise ValueError('Git could not export the worker branch')
    except BaseException:
        path.unlink(missing_ok=True)
        raise
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=2)
        process.stdout.close()


def bounded_command(command: GitCommand) -> tuple[bytes, int, bool]:
    process = subprocess.Popen(command.argv, env=command.environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    assert process.stdout is not None
    data = bytearray()
    deadline = time.monotonic() + 10
    truncated = False
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command.argv, 10)
                if not selector.select(remaining):
                    continue
                chunk = os.read(process.stdout.fileno(), 8192)
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > 64 * 1024:
                    truncated = True
                    process.kill()
                    break
        return bytes(data[:64 * 1024]), process.wait(timeout=2), truncated
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=2)
        process.stdout.close()


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while data := stream.read(1024 * 1024):
            digest.update(data)
    return digest.hexdigest()


def server_tools(tool: Any, text: Any) -> list[dict[str, Any]]:
    return [tool('orchestration_servers', 'Lead only. add_location(project, server, path) registers a project folder on that server. remove_location removes the association and keeps files and chats. location_matches suggests registered folders with the same Git origin. No credentials move. List paired servers, run Git reads, fetch branches, or run commands as the Studio user on local or paired servers. exec takes command argv or shell text, absolute cwd, env additions, timeout (120 seconds, max 1800), output_limit (256 KiB, max 4 MiB), and request_id. Above 5 seconds it returns a handle; exit events reach the lead. exec_read reads bounded head/tail output with stdout_offset and stderr_offset cursors. exec_input sends input or closes stdin (32 requests per command, 64 KiB each); exec_cancel stops the process group. Exact retries never repeat execution. Unknown outcomes require inspection.',
        {'action': {'type': 'string', 'enum': ['list', 'projects', 'folders', 'git', 'fetch', 'receipt', 'add_location', 'remove_location', 'location_matches', 'exec', 'exec_read', 'exec_input', 'exec_cancel']},
         'server': text, 'project': text, 'path': text, 'name': text, 'cwd': text, 'agent_id': text, 'branch': text, 'destination': text,
         'argv': {'type': 'array', 'items': text, 'maxItems': 8}, 'request_id': text,
         'command': {'oneOf': [text, {'type': 'array', 'items': text, 'minItems': 1, 'maxItems': 256}]},
         'env': {'type': 'object', 'additionalProperties': text},
         'timeout': {'type': 'integer', 'minimum': 1, 'maximum': 1800},
         'output_limit': {'type': 'integer', 'minimum': 2, 'maximum': 4194304},
         'handle': text, 'input': text, 'close_stdin': {'type': 'boolean'},
         'stdout_offset': {'type': 'integer', 'minimum': 0},
         'stderr_offset': {'type': 'integer', 'minimum': 0}}, ['action'])]
