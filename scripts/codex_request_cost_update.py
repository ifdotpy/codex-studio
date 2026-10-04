"""Apply retained request admission and one-pass session costs without a restart."""
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import threading
import time

from codex_source import signature, source_function

SOURCES = {'codex_runtime': ('ec05e1d6b43d80a36a0548db1cd6baf185af8ca4a8f048998425f6c3d4312d51',
                   '0855e2e41b6652a4a7c3f8e472855d4b171c920b0fea3ad91cc961b3dfea51d2'),
 'codex_session_costs': ('daa6190890bae0c94c8762607ada2f0134779006cdfb53f0ca01dce24a8d7001',
                         'daa6190890bae0c94c8762607ada2f0134779006cdfb53f0ca01dce24a8d7001')}
FUNCTIONS = (('codex_runtime',
  'Runtime',
  'request',
  'ebeb1becaa72d5b222e55aec4178ff5ecd2e017774a670e0a965b974a49a5fd5',
  '46cefba3a58df3e5726807825b7803e18dd56e2c21158da2b1c767e85e4155b6'),
 ('codex_runtime',
  'Runtime',
  'dynamic',
  'e5d0dd7874083a097734ff4cff495d2e80ecf893279db3ed2c65f67946f3bb9d',
  '5848414f9fa1e6feb15f41e3011fd4cc651231f85076eaea6f6f40b20db730dd'),
 ('codex_session_costs',
  'SessionCostReader',
  '_cost_usage_groups',
  '7dcd36d5aefd75fde2a47531625ab713a008a1cc8a20840fadd4429d2c747aee',
  'd40c0b66bd52dda0e69a99ac7253d7bae87023595f97fa83d84941a033af07f5'),
 ('codex_session_costs',
  'SessionCostReader',
  '_compute',
  'd899cc7a214c245ec11682a6a057af0b723cfe8a26a4b74ba0b36c12933bbdbd',
  '16d198433626fa2202cab3e2c17ddebbe58ae8c1412af004e24d0357d4b19fef'),
 ('codex_runtime',
  'Runtime',
  'dispatch_active_slots',
  'cddaeed9bf87a034810e2c6a61a2ef4255a3a09e3d128cd5f0875387ae094f13',
  'e3649e01f93da1883fb58dc7139f0fda1256f95ff874f881b86cad985843b216'),
 ('codex_runtime',
  'AppServer',
  'enqueue',
  'c2381ad2b2e27c32316d49a35f22f736db8d2360aa063eb72816fd4ef90c0a41',
  '2693d9684a9340a64a3a928b570f929c7881cc803f6c26c5a6d65cc241ad41ce'))
DEPENDENCIES = (('codex_runtime',
  'Runtime',
  'connection_current',
  ('f8ea88adeb09f9e5b2fe314c212843d8f3f109838b6741c0c5006533e2015174',)),
 ('codex_runtime',
  'Runtime',
  'reply',
  ('4f94c6851a22e622071beb18bacb5f3a7b0cbdce4c6ae952814768f26ee2d360',)),
 ('codex_runtime',
  'AppServer',
  'dispatch',
  ('0ae288e1a056da5deb9f69bdef861637a6f715aeb4ecdd07bfc2d18713084e27',)),
 ('codex_runtime',
  'AppServer',
  'fail_transport',
  ('71246cb06f7897b8149eb9e44b00851506aab01e4331a90b6e06885c10aa6bab',)),
 ('codex_process_supervisor',
  'ProcessProxy',
  'call',
  ('969a43314b8c0b535fedf98c900b4533b47d846ba48303282321e76f4ae99eb7',)),
 ('codex_process_supervisor',
  'ProcessProxy',
  'ack',
  ('8e58841521f7f7033ea93f5531b20646d94ac87a985d158b38684a50224af04a',)),
 ('codex_process_supervisor',
  'ProcessProxy',
  'terminate',
  ('72b63e9ee71459857937eac9a195c6aab143c850e8a01e21b36bb626f46fdf91',)),
 ('codex_tool_requests',
  'RequestMixin',
  'tool_request_key',
  ('3faceae71c6e2cccfe8b6f33fc87efe3d59d5c58b26c6069e9e47b49fd2f4035',)),
 ('codex_tool_requests',
  'RequestMixin',
  'reserve_tool_request',
  ('1012c9288c90897fb79a4e23cc070ce14cdca5a2ffd0c4cf5c6b45e326b957d6',)),
 ('codex_tool_requests',
  'RequestMixin',
  'begin_tool_request',
  ('e3d12077e5213aee7b238c1b42a27bb2bd0946ad4620c6ca7d2f0520473d3976',)),
 ('codex_tool_requests',
  'RequestMixin',
  'finish_tool_request',
  ('88ae8e105b8ec16cdf70073533ee8cc0d89dd81e98574475cf244e77015a7628',)),
 ('codex_tool_requests',
  'RequestMixin',
  'tool_request_actor',
  ('fc47026d099e704b5faef92666d5f058a54c54afb483561a1e25fc57d982854b',)),
 ('codex_session_costs',
  'SessionCostReader',
  '_usage_state',
  ('e75e472beee015302d92264906e4cf541dcd5c4ef80f55f8a034d427b71f14f3',)),
 ('codex_session_costs',
  'SessionCostReader',
  '_global_usage_generation',
  ('f4ee78b8f70b592e03996637a8ad72a57dadb37afe06efbd92160c6f0aa716b4',)),
 ('codex_session_costs',
  'SessionCostReader',
  '_log_rows',
  ('e2942bffa1659222d1b6e825730fb9e447174e29301f9e78c3ca6c90863a5829',)),
 ('codex_session_costs',
  'SessionCostReader',
  '_claude_signature',
  ('7a7a7933118c5e2a5a3e1ba13cf4a8c1e326c84bc4079e7173c0d343f043c508',)),
 ('codex_tool_requests',
  None,
  'operation_receipt_evidence',
  ('8753c842c2b36e0c108f25d11937c166b6f211fbe651375b01649b02910d8835',)),
 ('codex_time', None, 'stamp_tool_result',
  ('b2d1b043818b7b431a9ebb59d3d280b2e209a2c4bdb2d5347bdbc47ae8f94042',)))


def _callback_account(callback, runtime):
    if getattr(callback, '__self__', None) is runtime:
        return 'default', None
    code = getattr(callback, '__code__', None)
    cells = getattr(callback, '__closure__', None)
    if code is None or cells is None:
        return None
    try:
        values = {name: cell.cell_contents for name, cell in zip(code.co_freevars, cells)}
    except ValueError:
        return None
    if not any(value is runtime for value in values.values()):
        return None
    if 'account_key' not in values or 'connection_id' not in values:
        return None
    return values['account_key'], values['connection_id']


def _request_ack_frames(runtime, module, old_request_code):
    """Read only callback identities, never foreign SQLite properties."""
    proxies, requests = {}, {}
    proxy_type = sys.modules['codex_process_supervisor'].ProcessProxy
    for server in [*runtime.servers.values(), *getattr(runtime, '_late_servers', ())]:
        proxy = getattr(server, 'proc', None)
        if isinstance(proxy, proxy_type):
            proxies[id(proxy)] = proxy
    for frame in sys._current_frames().values():
        active_request = None
        while frame is not None:
            if frame.f_code is old_request_code:
                local = frame.f_locals
                if local.get('self') is runtime:
                    active_request = (local.get('message'), local.get('account_key', 'default'),
                                      local.get('connection_id'))
            dispatch = module.AppServer.dispatch
            code = frame.f_code
            filename = code.co_filename
            historical_dispatch = (
                code.co_name == 'dispatch' and code.co_argcount == 1
                and code.co_kwonlyargcount == 0 and not code.co_freevars
                and (frame.f_globals is dispatch.__globals__
                     or (frame.f_globals.get('AppServer') is module.AppServer
                         and frame.f_globals.get('__name__') == dispatch.__globals__.get('__name__')
                         and frame.f_globals.get('__file__') == dispatch.__globals__.get('__file__')))
                and {'callbacks', 'proc', 'ack'}.issubset(code.co_names)
                and (filename == dispatch.__code__.co_filename
                     or Path(filename).name == 'codex_runtime.py'
                     or filename.startswith('<') and filename.endswith('>')))
            if historical_dispatch:
                local = frame.f_locals
                server = local.get('self')
                if not isinstance(server, module.AppServer):
                    frame = frame.f_back
                    continue
                account = _callback_account(getattr(server, 'request', None), runtime)
                if active_request is not None and (account is None or active_request[1:] != account):
                    raise RuntimeError('An active native request has a different account binding')
                if account is not None:
                    proxy = getattr(server, 'proc', None)
                    if isinstance(proxy, proxy_type):
                        proxies[id(proxy)] = proxy
                        request = (active_request if active_request is not None
                                   and active_request[1:] == account else None)
                        if (request is None and account is not None
                                and local.get('callback') == server.request):
                            request = (local.get('callback_message', local.get('message')), *account)
                        if request is not None:
                            message, account_key, connection_id = request
                            sequence = (message.get('_studioSupervisorSequence')
                                        if isinstance(message, dict) else None)
                            if (type(sequence) is int and sequence > proxy.cursor
                                    and message.get('method') == 'item/tool/call'):
                                requests[(id(proxy), sequence)] = (proxy, message, account_key, connection_id)
            frame = frame.f_back
    return proxies, requests


def _request_ack_identity(runtime, message, account_key, connection_id):
    params = message.get('params') or {}
    if not isinstance(params, dict):
        return None
    args = params.get('arguments', {})
    try:
        if isinstance(args, str):
            args = json.loads(args)
        if not isinstance(args, dict):
            return None
        key = runtime.tool_request_key(message, account_key)
        content = hashlib.sha256(json.dumps(
            {'tool': params.get('tool'), 'arguments': args}, sort_keys=True,
            separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()).hexdigest()
    except (ValueError, TypeError, OverflowError):
        return None
    return {'key': key, 'signature': content, 'account': account_key,
            'rpcId': message.get('id'), 'thread': params.get('threadId'),
            'connection': connection_id, 'admitted': False, 'held': False}


def _request_was_admitted(runtime, entry):
    uri = runtime.db_path.absolute().as_uri() + '?mode=ro'
    db = sqlite3.connect(uri, uri=True, timeout=.05)
    try:
        row = db.execute(
            "SELECT json_extract(record,'$.signature'),"
            "CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
            "ELSE json_extract(record,'$.accountKey') END,"
            "json_extract(record,'$.rpcId'),json_extract(record,'$.threadId') "
            "FROM runtime_tool_requests WHERE id=?", (entry['key'],)).fetchone()
    finally:
        db.close()
    return row == (entry['signature'], entry['account'], entry['rpcId'], entry['thread'])


def _stale_claim(runtime, db, key):
    row = db.execute(
        "SELECT r.record,json_extract(a.record,'$.epoch'),json_extract(a.record,'$.turnEpoch') "
        "FROM runtime_tool_requests r LEFT JOIN runtime_agents a "
        "ON a.id=json_extract(r.record,'$.agent') WHERE r.id=?", (key,)).fetchone()
    if row is None:
        return None
    record = json.loads(row[0])
    if record.get('id') != key:
        return record, False
    stale = (record.get('epoch') != row[1]
             or (record.get('turnId') and (row[2] if row[2] is not None else row[1]) != row[1]))
    if record.get('stage') != 'queued':
        return None
    # Unknown, saved, and started receipts must never become a new claim.
    safe_rejection = (record.get('outcome') == 'pending' and record.get('started') is None
                      and record.get('result') is None
                      and not record.get('_payloadBlobs', {}).get('result')
                      and db.execute('SELECT 1 FROM runtime_tool_results WHERE id=?', (key,)).fetchone() is None)
    if safe_rejection and db.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_operation_receipts'").fetchone():
        safe_rejection = db.execute(
            'SELECT 1 FROM runtime_operation_receipts WHERE id=?', (key,)).fetchone() is None
    if not safe_rejection:
        return record, False
    return (record, True) if stale else None


class _OldDynamicClaimFence:
    """Recheck the exact receipt when an old dynamic frame reaches its claim."""
    def __init__(self, runtime):
        self.runtime, self.installed = runtime, False
        state = vars(runtime).get('_studio_old_dynamic_claim_guard')
        if state is None:
            if any(name in vars(runtime) for name in ('begin_tool_request', '_studio_old_dynamic_claim_guard')):
                raise RuntimeError('The running tool claim callback differs')
        elif (not isinstance(state, dict) or state.get('runtime') is not runtime
              or runtime.begin_tool_request is not state.get('begin')):
            raise RuntimeError('The running tool claim callback differs')
        self.state = state

    def install(self):
        if self.state is not None:
            return
        runtime, original = self.runtime, self.runtime.begin_tool_request

        def begin(key):
            with runtime.lock:
                uri = runtime.db_path.absolute().as_uri() + '?mode=ro'
                db = sqlite3.connect(uri, uri=True, timeout=.05)
                try:
                    pending = _stale_claim(runtime, db, key)
                finally:
                    db.close()
                if pending is None:
                    return original(key)
                if pending[1]:
                    # Never keep the reader open while acquiring the writer.
                    with runtime.db(busy_timeout=50) as db:
                        pending = _stale_claim(runtime, db, key)
                        if pending is not None and pending[1]:
                            result = sys.modules['codex_time'].stamp_tool_result({
                                'success': False, 'contentItems': [{'type': 'inputText',
                                'text': 'This tool call belongs to an earlier turn'}]}, time.time())
                            runtime.finish_tool_request(key, result, outcome='not_applied', db=db)
                return False

        self.state = {'runtime': runtime, 'begin': begin}
        runtime._studio_old_dynamic_claim_guard = self.state
        runtime.begin_tool_request = begin
        self.installed = True

    def close(self, failed):
        if failed and self.installed:
            vars(self.runtime).pop('begin_tool_request', None)
            vars(self.runtime).pop('_studio_old_dynamic_claim_guard', None)


class _RequestAckBarrier:
    """Fence exact callbacks while their old Python frames can still return."""
    def __init__(self, runtime, module, old_request_code):
        self.runtime, self.module, self.old_request_code = runtime, module, old_request_code
        self.locks, self.restores = {}, []
        self.deadline = time.monotonic() + 1

    def protect(self):
        while True:
            proxies, requests = _request_ack_frames(self.runtime, self.module, self.old_request_code)
            fresh = [proxy for identity, proxy in proxies.items() if identity not in self.locks]
            for proxy in fresh:
                lock = vars(proxy).setdefault('_ack_lock', threading.RLock())
                if not lock.acquire(timeout=max(0, self.deadline - time.monotonic())):
                    raise RuntimeError('A supervisor ACK is active; retry the update')
                self.locks[id(proxy)] = lock
            if not fresh:
                break
        planned = {}
        for (_, sequence), (proxy, message, account_key, connection_id) in requests.items():
            entry = _request_ack_identity(self.runtime, message, account_key, connection_id)
            if entry is not None:
                entry['admitted'] = _request_was_admitted(self.runtime, entry)
                planned.setdefault(id(proxy), (proxy, {}))[1][sequence] = entry
        # Validate every instance before installing any wrapper.
        for proxy, entries in planned.values():
            state = vars(proxy).get('_studio_request_ack_guard')
            if state is None:
                valid = not any(name in vars(proxy) for name in ('ack', 'call', '_studio_request_ack_guard'))
            else:
                valid = (isinstance(state, dict) and state.get('runtime') is self.runtime
                         and isinstance(state.get('entries'), dict)
                         and proxy.ack is state.get('ack') and proxy.call is state.get('call'))
                if valid:
                    fields = ('key', 'signature', 'account', 'rpcId', 'thread', 'connection')
                    valid = all(sequence not in state['entries'] or
                                all(state['entries'][sequence][field] == entry[field] for field in fields)
                                for sequence, entry in entries.items())
            if not valid:
                raise RuntimeError('The supervisor ACK callback differs')
        for proxy, entries in planned.values():
            state = vars(proxy).get('_studio_request_ack_guard')
            if state is None:
                state = {'entries': {}, 'runtime': self.runtime}
                original_ack, original_call = proxy.ack, proxy.call
                lock = self.locks[id(proxy)]

                def denied(sequence, *, state=state, proxy=proxy):
                    held = set()
                    runtime = state['runtime']
                    for exact, entry in state['entries'].items():
                        if exact <= proxy.cursor or exact > sequence or entry['admitted']:
                            continue
                        if (not entry['held'] and (runtime.closed
                                or not runtime.connection_current(entry['account'], entry['connection']))):
                            entry['admitted'] = _request_was_admitted(runtime, entry)
                            entry['held'] = not entry['admitted']
                        if entry['held']:
                            held.add(exact)
                    proxy.ack_pending.difference_update(held)
                    return held

                def ack(sequence, *, original=original_ack, lock=lock, denied=denied):
                    with lock:
                        if sequence in denied(sequence):
                            raise ConnectionError('The native request was not durably admitted before its connection ended')
                        return original(sequence)

                def call(action, *, original=original_call, lock=lock, denied=denied, **values):
                    if action != 'ack':
                        return original(action, **values)
                    with lock:
                        if denied(values['sequence']):
                            raise ConnectionError('The native request was not durably admitted before its connection ended')
                        return original(action, **values)

                self.restores.append((proxy, {name: vars(proxy).get(name) for name in
                                             ('ack', 'call', '_studio_request_ack_guard')}))
                state.update(ack=ack, call=call)
                proxy._studio_request_ack_guard, proxy.ack, proxy.call = state, ack, call
            else:
                self.restores.append((proxy, {'entries': state['entries'].copy()}))
            for sequence, entry in entries.items():
                state['entries'].setdefault(sequence, entry)

    def close(self, failed):
        if failed:
            for proxy, previous in reversed(self.restores):
                for name, value in previous.items():
                    if name == 'entries':
                        proxy._studio_request_ack_guard['entries'] = value
                    elif value is None:
                        vars(proxy).pop(name, None)
                    else:
                        setattr(proxy, name, value)
        for lock in reversed(list(self.locks.values())):
            lock.release()


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    modules = {name: sys.modules.get(name) for name in SOURCES}
    module = modules['codex_runtime']
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or any(loaded is None or Path(loaded.__file__).resolve() != scripts / (name + '.py')
                   for name, loaded in modules.items())):
        raise RuntimeError('The running backend source identity differs')
    sources = {}
    for name, accepted in SOURCES.items():
        raw = (scripts / (name + '.py')).read_bytes()
        if hashlib.sha256(raw).hexdigest() not in accepted:
            raise RuntimeError('The reviewed request or cost source differs: ' + name)
        sources[name] = raw
    plans = []
    for name, owner, method, before, after in FUNCTIONS:
        desired, _ = source_function(sources[name], [owner, method], vars(modules[name]),
                                     str(scripts / (name + '.py')))
        if signature(desired) != after:
            raise RuntimeError('The reviewed request or cost function differs: ' + method)
        current = getattr(getattr(modules[name], owner), method)
        plans.append((current, desired, before, after, method))
    with runtime.lock:
        if any(name in vars(runtime) for name in ('request', 'dynamic')):
            raise RuntimeError('The running request callback differs')
        for name, owner, method, accepted in DEPENDENCIES:
            dependency = sys.modules.get(name)
            current = (getattr(dependency, method, None) if owner is None
                       else getattr(getattr(dependency, owner, None), method, None))
            if signature(current) not in accepted:
                raise RuntimeError('The running request or cost guard differs: ' + method)
        actual = [signature(current) for current, *_ in plans]
        for observed, (_, _, before, after, method) in zip(actual, plans):
            if observed not in {before, after}:
                raise RuntimeError('The running request or cost function differs: ' + method)
        already_applied = all(observed == plan[3] for observed, plan in zip(actual, plans))
        if runtime.closed:
            raise RuntimeError('The running backend stopped before the update')
        barrier = _RequestAckBarrier(runtime, module, plans[0][0].__code__)
        claim_fence = _OldDynamicClaimFence(runtime)
        original = [(current, current.__code__, current.__defaults__, current.__kwdefaults__)
                    for current, *_ in plans]
        failed = True
        try:
            barrier.protect()
            claim_fence.install()
            # An old _compute frame creates the Claude receipt table required
            # by both projections. Preserve bound callback function identities.
            for current, desired, _, _, _ in plans:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
            # A constructor can publish a transport without Runtime.lock.
            # No new old request frame can start after the code replacement.
            barrier.protect()
            failed = False
        finally:
            if failed:
                for current, code, defaults, kwdefaults in original:
                    current.__code__, current.__defaults__, current.__kwdefaults__ = code, defaults, kwdefaults
            claim_fence.close(failed)
            barrier.close(failed)
        if already_applied and not claim_fence.installed:
            return {'status': 'already_applied'}
    runtime.changed.set()
    return {'status': 'applied'}
