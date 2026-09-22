"""Validate installed Codex updates and replace only proven-idle account processes."""
from __future__ import annotations

import copy
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import threading
import time
import uuid

from codex_native_binary import (APPROVAL_REVISION, REQUIRED_COMPANIONS, ProtocolIncompatible, bundle_digest,
                                 approve_candidate, discover_candidates, version_key)


def _version(server):
    info = getattr(server, 'native_binary', None) or {}
    if info.get('version'):
        return info['version']
    agent = (getattr(server, 'initialize_result', None) or {}).get('userAgent', '')
    match = re.search(r'/([0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?)', agent)
    return match.group(1) if match else None


class _BoundedReader:
    def __init__(self, server, seconds=30):
        self.server, self.deadline = server, time.monotonic() + seconds

    def call(self, method, params, timeout=10):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('Native idle check exceeded its time limit')
        return self.server.call(method, params, timeout=min(timeout, remaining))


class NativeRuntimeUpdates:
    def __init__(self, runtime, *, discover=None, approve=None, spawn=None, interval=60):
        self.rt = runtime
        self.discover = discover or discover_candidates
        self.approve = approve or approve_candidate
        self.spawn = spawn or self._spawn
        self.interval = interval
        self.lock = threading.RLock()
        self.check_lock = threading.Lock()
        self.stopped = threading.Event()
        self.ready = threading.Event()
        self.worker = None
        self.next_check = self.next_scan = 0
        self.cache = {}
        self.path = Path(runtime.root) / 'native-runtime' / 'status.json'
        self.state = {'status': 'checking', 'checkedAt': None, 'selected': None,
                      'candidates': [], 'accounts': {}}
        self.loaded = False

    def _load_selected(self):
        try:
            prior = json.loads(self.path.read_text())
            info = prior['selected']
            if not isinstance(info, dict):
                return
            if (info.get('approvalRevision') != APPROVAL_REVISION
                    or not re.fullmatch('[0-9a-f]{64}', info.get('sha256', ''))
                    or not re.fullmatch('[0-9a-f]{64}', info.get('bundleSha256', ''))):
                return
            path = Path(info['path'])
            expected = self.path.parent / 'builds' / info['bundleSha256'] / 'codex'
            if path.resolve() != expected.resolve() or not os.access(path, os.X_OK):
                return
            with path.open('rb') as stream:
                digest = hashlib.file_digest(stream, 'sha256').hexdigest()
            if digest != info['sha256'] or not info.get('checks'):
                return
            hashes = {'codex': digest}
            for name in REQUIRED_COMPANIONS:
                helper = info['companions'][name]
                helper_path = expected.with_name(name)
                if (Path(helper['path']).resolve() != helper_path.resolve()
                        or not os.access(helper_path, os.X_OK)):
                    return
                with helper_path.open('rb') as stream:
                    hashes[name] = hashlib.file_digest(stream, 'sha256').hexdigest()
                if hashes[name] != helper['sha256']:
                    return
            if bundle_digest(hashes) != info['bundleSha256']:
                return
            with self.lock:
                self.state.update(selected=info, status='ready', checkedAt=prior.get('checkedAt'))
            self.ready.set()
        except (OSError, ValueError, KeyError, TypeError):
            pass

    def status(self):
        with self.lock:
            return copy.deepcopy(self.state)

    def selected(self):
        with self.lock:
            return copy.deepcopy(self.state['selected'])

    def _save(self):
        state = self.status()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(mode='w', dir=self.path.parent, prefix='.status-', delete=False) as stream:
                temporary = stream.name
                json.dump(state, stream)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            temporary = None
        finally:
            if temporary:
                Path(temporary).unlink(missing_ok=True)

    def maybe_check(self):
        with self.lock:
            if self.stopped.is_set() or self.rt.closed or time.monotonic() < self.next_check:
                return
            if self.worker and self.worker.is_alive():
                return
            self.next_check = time.monotonic() + self.interval
            self.worker = threading.Thread(target=self._background, name='codex-runtime-updates', daemon=True)
            self.worker.start()

    def _background(self):
        try:
            self.check(force=False)
        except Exception as error:
            with self.lock:
                self.state.update(status='failed', error=str(error)[:500])
            self.ready.set()
            try:
                self._save()
            except OSError:
                pass

    def check(self, *, force=True):
        if not self.check_lock.acquire(blocking=False):
            return self.status()
        try:
            if self.stopped.is_set() or self.rt.closed:
                return self.status()
            if not self.loaded:
                self._load_selected()
                self.loaded = True
            if force or time.monotonic() >= self.next_scan:
                self._scan()
                self.next_scan = time.monotonic() + self.interval
            selected = self.selected()
            if selected:
                with self.rt.lock:
                    accounts = list(self.rt.servers)
                for account in accounts:
                    if self.stopped.is_set() or self.rt.closed:
                        break
                    if self.rt.accounts.get(account).get('provider', 'codex') == 'codex':
                        self._rotate(account, selected)
            with self.lock:
                waiting = any(row['status'] == 'waiting' for row in self.state['accounts'].values())
                self.next_check = time.monotonic() + (10 if waiting else self.interval)
            self._save()
            return self.status()
        finally:
            self.check_lock.release()

    def _scan(self):
        rows, selected = [], self.selected()
        for candidate in self.discover():
            row = dict(candidate)
            if row.get('error'):
                rows.append({**row, 'status': 'rejected'})
                continue
            identity = json.dumps([row['path'], row.get('identity')], sort_keys=True)
            cached = self.cache.get(identity)
            if cached is None or cached.get('retryAt', float('inf')) <= time.monotonic():
                try:
                    info = self.approve(row['path'], self.rt.root)
                    cached = {'info': info}
                except Exception as error:
                    cached = {'error': str(error)[:500]}
                    if not isinstance(error, ProtocolIncompatible):
                        cached['retryAt'] = time.monotonic() + 60
                self.cache[identity] = cached
            if 'error' in cached:
                rows.append({**row, 'status': 'rejected', 'error': cached['error']})
                continue
            info = cached['info']
            rows.append({**row, 'status': 'approved'})
            if selected is None or version_key(info['version']) >= version_key(selected['version']):
                selected = info
            # The discoverer returns newest first. Older binaries need no validation.
            break
        with self.lock:
            changed = (selected or {}).get('bundleSha256') != (self.state['selected'] or {}).get('bundleSha256')
            self.state.update(status='ready' if selected else 'failed', checkedAt=time.time(),
                              selected=selected, candidates=rows)
            self.state.pop('error', None)
            if not selected:
                self.state['error'] = 'No installed Codex version passed the protocol checks'
        self._save()  # Approved immutable identity survives a backend restart.
        self.ready.set()
        if changed:
            from codex_catalog import ModelCatalogCache
            with self.rt.lock:
                self.rt._catalog_cache = ModelCatalogCache()

    def _account(self, key, status, server, selected, reason=None):
        with self.lock:
            self.state['accounts'][key] = {
                'status': status, 'version': _version(server) if server else None,
                'pid': getattr(getattr(server, 'proc', None), 'pid', None),
                'targetVersion': selected['version'], 'reason': reason, 'updatedAt': time.time()}

    def _spawn(self, account, selected, callbacks):
        from codex_runtime import AppServer
        root = self.rt.root if account == 'default' else self.rt.root / 'account-servers' / account
        root.mkdir(parents=True, exist_ok=True)
        return AppServer(root, *callbacks, home=self.rt.accounts.home(account), isolated=account != 'default',
                         executable=selected['path'])

    def _rotate(self, account, selected):
        from codex_native_tools import _local_idle, _native_idle, _same_source, account_reserved
        rt = self.rt
        replacement = None
        installed = False
        spawning = False
        token = uuid.uuid4().hex
        with rt.lock, rt.db() as db:
            server = rt.servers.get(account)
            if server is None:
                return
            if account_reserved(rt, account):
                self._account(account, 'waiting', server, selected, 'An account process update is in progress')
                return
            if (getattr(server, 'native_binary', None) or {}).get('bundleSha256') == selected['bundleSha256']:
                self._account(account, 'current', server, selected)
                return
            agents, reason = _local_idle(rt, db, account, server)
            if reason:
                self._account(account, 'waiting', server, selected, reason or 'An account update is already in progress')
                return
            connection = rt.connection_ids.get(account)
            reservations = rt.__dict__.setdefault('_native_runtime_reservations', {})
            reservations[account] = token
        try:
            self._account(account, 'updating', server, selected)
            _native_idle(_BoundedReader(server))
            barrier = threading.Event()
            server.after_events(barrier.set)
            if not barrier.wait(10):
                raise ValueError('Waiting for native events to finish')
            if rt.closed or self.stopped.is_set():
                raise ValueError('Studio is stopping')
            next_connection = str(uuid.uuid4())
            callbacks = (
                lambda message: rt.notification(message, account, next_connection),
                lambda message: rt.request(message, account, next_connection),
                lambda: rt.disconnected(account, next_connection),
            )
            spawning = True
            replacement = self.spawn(account, selected, callbacks)
            models = replacement.call('model/list', {'limit': 100}, timeout=10)
            if not isinstance(models.get('data'), list):
                raise ValueError('Replacement app-server returned an invalid model catalog')
            if replacement.proc.poll() is not None:
                raise ValueError('Replacement app-server stopped during its check')
            replacement.native_binary = dict(selected)
            with rt.lock:
                # A concurrent caller can hold Runtime.lock before connect().
                # Never block on the opposite order used by native tool refresh.
                if not rt.start_lock.acquire(blocking=False):
                    raise ValueError('Another account connection is changing')
                try:
                    with rt.db() as db:
                        current, reason = _local_idle(rt, db, account, server)
                        if (rt.closed or self.stopped.is_set() or reason or rt.servers.get(account) is not server
                                or rt.connection_ids.get(account) != connection
                                or account in rt.offline_accounts or server.proc.poll() is not None
                                or replacement.proc.poll() is not None
                                or [_same_source(a) for a in current] != [_same_source(a) for a in agents]):
                            raise ValueError(reason or 'The account changed during its update check')
                        # New callbacks belong to the new connection. Old callbacks cannot
                        # mark resumed chats as interrupted after the old process exits.
                        rt.connection_ids[account] = next_connection
                        rt.servers[account] = replacement
                        rt.loaded.difference_update(a['id'] for a in agents)
                        if account == 'default':
                            rt.server = replacement
                        installed = True
                finally:
                    rt.start_lock.release()
            with rt.lock:
                rt.__dict__.setdefault('_native_tools_retiring', {})[account] = {'server': server}
            server.close()
            if server.proc.poll() is None:
                raise RuntimeError('The previous idle app-server did not stop')
            with rt.lock:
                rt._native_tools_retiring.pop(account, None)
            self._account(account, 'current', replacement, selected)
        except Exception as error:
            self._account(account, 'failed' if spawning else 'waiting',
                          replacement if installed else server, selected, str(error)[:500])
        finally:
            if replacement is not None and not installed:
                try:
                    replacement.close()
                finally:
                    if replacement.proc.poll() is None:
                        with rt.lock:
                            rt.__dict__.setdefault('_native_tools_retiring', {})[account] = {'server': replacement}
            with rt.lock:
                if rt.__dict__.get('_native_runtime_reservations', {}).get(account) == token:
                    rt._native_runtime_reservations.pop(account)
            rt.changed.set()

    def close(self):
        self.stopped.set()
        worker = self.worker
        if worker and worker is not threading.current_thread():
            worker.join(timeout=45)
            if worker.is_alive():
                raise RuntimeError('Codex runtime update is still checking native state; runtime lease retained')


def manager(runtime):
    with runtime.lock:
        current = getattr(runtime, 'native_runtime_updates', None)
        if current is None:
            current = runtime.native_runtime_updates = NativeRuntimeUpdates(runtime)
        return current


def tick(runtime):
    from codex_runtime import AppServer
    if runtime.factory is AppServer:
        manager(runtime).maybe_check()


def executable_for(runtime):
    updates = manager(runtime)
    updates.maybe_check()
    selected = updates.selected()
    if selected is None:
        updates.ready.wait(5)
        selected = updates.selected()
    if selected is None:
        raise RuntimeError(updates.status().get('error') or 'Codex compatibility check is in progress')
    return selected


def status(runtime):
    updates = getattr(runtime, 'native_runtime_updates', None)
    return updates.status() if updates else None
