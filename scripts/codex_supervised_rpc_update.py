"""Apply exact supervisor response and clock fixes without replacing transports."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

HASHES = {'codex_process_supervisor': '4b607cb64e2e8c14336510370b3dac67724fe37ec0a72e96ef53270733ff09a3',
 'codex_runtime': '29187ff167a992655253268b5458d08c7558744cd159893ab4836800bb2f007f'}
FUNCTIONS = (('codex_process_supervisor',
  'ProcessProxy',
  'send_write',
  '6d8d383132c9d93e875389e0bfe0d70f003f223aecb6c166ce6f6da6836098a2',
  '8cf63fbdaa08592ce4db7907c220fb7f5d880b708a57a173e26002ec3ca8284f'),
 ('codex_runtime',
  'AppServer',
  'write',
  'd128ce66c0bea0ec43720af4a60f99039b3503108dd6c9da9b4c0a3982c89d46',
  '142af6e3c5138efa5a26320b64d0083895bd62eab3529c449a9a44355dfc9eff'),
 ('codex_runtime',
  'AppServer',
  'enqueue_clock',
  'bd509a7e2a6119b2718f852c965b68f79c381e1dc704c6949dd84510fb4f7d0c',
  'b8ae159547b2bb975bbb382fe7392782fea7f8933a57e32800b15d27f3de1599'),
 ('codex_runtime',
  'AppServer',
  'enqueue',
  'd22a54d884c3809715c5cc72f0d8eb5081ccdfb453eb01c2b4940d339053abb7',
  'c2381ad2b2e27c32316d49a35f22f736db8d2360aa063eb72816fd4ef90c0a41'))
DEPENDENCIES = (('codex_process_supervisor',
  'ProcessProxy',
  'call',
  '969a43314b8c0b535fedf98c900b4533b47d846ba48303282321e76f4ae99eb7'),
 ('codex_process_supervisor',
  'ProcessProxy',
  'ack',
  '8e58841521f7f7033ea93f5531b20646d94ac87a985d158b38684a50224af04a'),
 ('codex_process_supervisor',
  'ProcessProxy',
  'next_event',
  'b17332d1f43e308fc872e503d078a926646ad3204e3384ad0c12f499499c36ee'),
 ('codex_process_supervisor',
  'ProcessProxy',
  '_operation_identity',
  'e0a0460e0ea9a59a310cf25a1dc5b0efbfeb1f403c160e3100ab63078276c156'),
 ('codex_runtime', 'AppServer', 'read', '684fc7c811dfc3307b9f4e7d3a924f8918f130ba6b4fecf8fc9f7d985ad88ecd'),
 ('codex_runtime',
  'AppServer',
  'write_clocks',
  '2ebcae9d92d755fb3b4377a163efd63f42f8ef23601898adf78fcf332ea37ceb'))


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    modules = {name: sys.modules.get(name) for name in HASHES}
    module = modules['codex_runtime']
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or any(loaded is None or Path(loaded.__file__).resolve() != scripts / (name + '.py')
                   for name, loaded in modules.items())):
        raise RuntimeError('The running backend source identity differs')
    sources = {}
    for name, expected in HASHES.items():
        raw = (scripts / (name + '.py')).read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise RuntimeError('The reviewed supervised RPC source differs: ' + name)
        sources[name] = raw
    plans = []
    for name, owner, method, before, after in FUNCTIONS:
        desired, _ = source_function(sources[name], [owner, method], vars(modules[name]),
                                     str(scripts / (name + '.py')))
        if signature(desired) != after:
            raise RuntimeError('The reviewed supervised RPC function differs: ' + method)
        plans.append((name, owner, method, before, after, desired))
    with runtime.lock:
        if module.AppServer.CLOCK_QUEUE_LIMIT != 128:
            raise RuntimeError('The running clock queue limit differs')
        proxy = modules['codex_process_supervisor'].ProcessProxy
        for server in runtime.servers.values():
            if getattr(server, 'supervisor_mode', False):
                if (not isinstance(server, module.AppServer) or not isinstance(server.proc, proxy)
                        or any(name in vars(server) for name in ('write', 'enqueue', 'enqueue_clock'))
                        or 'send_write' in vars(server.proc)):
                    raise RuntimeError('The running supervised transport differs')
        for name, owner, method, expected in DEPENDENCIES:
            current = getattr(getattr(modules[name], owner), method)
            if signature(current) != expected:
                raise RuntimeError('The running supervised RPC dependency differs: ' + method)
        already = True
        for name, owner, method, before, after, desired in plans:
            current = getattr(getattr(modules[name], owner), method)
            actual = signature(current)
            if actual not in {before, after}:
                raise RuntimeError('The running supervised RPC function differs: ' + method)
            already = already and actual == after
        if already:
            return {'status': 'already_applied'}
        # Install the write ledger before the reader can route a new clock.
        # The existing read and clock writer loop frames call these methods.
        for name, owner, method, before, after, desired in plans:
            current = getattr(getattr(modules[name], owner), method)
            current.__code__ = desired.__code__
            current.__defaults__ = desired.__defaults__
            current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
