"""Apply reviewed send and worker fixes without replacing native transports."""
import hashlib
import importlib.util
from pathlib import Path
import sys
from contextlib import contextmanager

from codex_source import signature, source_function

SOURCES = {'codex_runtime': {'source': 'codex_runtime.py',
                   'sha256': 'ddbecc02e7ac6435226c535a20a40c30f66663eed4600eaa70502ad20e6861a3',
                   'functions': [{'path': ['sqlite_busy'],
                                  'before': None,
                                  'after': '024fe70961cf9c7425882bdc4aa9c32dbaf4d611013de4a4cc68e6523a42007d',
                                  'manager': False},
                                 {'path': ['AppServer', 'persistence_retry'],
                                  'before': None,
                                  'after': '1bf9609a53f784165f1cc940f78f2407dc68d3a18e08e66dfc6b33499a797a4b',
                                  'manager': False},
                                 {'path': ['AppServer', 'dispatch'],
                                  'before': '6412c6ac3872873146c97f44f8872d2c59d7e79e01e9da69a58043b3ada7a193',
                                  'after': '0ae288e1a056da5deb9f69bdef861637a6f715aeb4ecdd07bfc2d18713084e27',
                                  'manager': False},
                                 {'path': ['Runtime', '_cleanup_failed_initialization'],
                                  'before': '54f4f1572844e31ef26f0e5ad8d9262422cf9fcba50651b6f080b036baad98b5',
                                  'after': '3a87cf789fb3c9ada0c5b25eaf917cd59eeeda6ee40dee88421d7fb0728f18b1',
                                  'manager': False},
                                 {'path': ['Runtime', 'db'],
                                  'before': 'c151051fbfbe700b10530b4c3fa38d21234689f5d3d2845180576bff7584a5f1',
                                  'after': 'f0f0f3ad4048a2b09ec6a7acd47e211440db3e02e1a475bb9acd712998539da8',
                                  'manager': True},
                                 {'path': ['Runtime', 'notification_db'],
                                  'before': None,
                                  'after': '5dbc88a5401c8fae4d504452854f6de4d60526cb4df82bef1ce216f84eab4ea1',
                                  'manager': True},
                                 {'path': ['Runtime', 'connect'],
                                  'before': '6ed745cdddce11a0a384c8a0101a8effbb306e63b4b389f76927610332ec1cab',
                                  'after': '1e90bacab3c3434a07f520e77ff5d1889f8c0a4e118d0cb8d1dd28777835ca41',
                                  'manager': False},
                                 {'path': ['Runtime', 'supervisor_event_applied'],
                                  'before': 'b1626f3a02600f6c66e5b69cd3fa5185027ae847357d3e8029f0d4c24abb88be',
                                  'after': 'c0f83ada12f833857eaf6db5e263f30b41ab90fc8b889f78a8d182e658c2bb52',
                                  'manager': False},
                                 {'path': ['Runtime', 'commit_supervisor_event'],
                                  'before': '736987794cc5e36061966f74326eea3331f9e9ec29b52a31cf682b809b26b65f',
                                  'after': '8388d2a8b0dbfe7b9c8944068cb9e3c80cd872f5e04a96b8c28048158985b887',
                                  'manager': False},
                                 {'path': ['Runtime', 'capture_stream_analytics'],
                                  'before': None,
                                  'after': 'dd95ec2c7074ae756a22f2c84bb9e3370b51fe16f42ac32e7755264cdcfdfb6b',
                                  'manager': False},
                                 {'path': ['Runtime', 'disconnected'],
                                  'before': 'd0a8b67c30478d2be20e8abc7d40de7c5a1f73f406b8fc55fa2684ff5979162f',
                                  'after': '752d57cc277fd85c4db789e19f2abef9a1d3fe6a90dd5f791d9e91f9fbb6301e',
                                  'manager': False},
                                 {'path': ['Runtime', 'notification'],
                                  'before': '2348465223375e9454cbef39f5ec63ca9b22ae9380faa0adb6ac7fa8ec26a002',
                                  'after': '0b442b78ae01be883b3486a468bbad790d595310ae4f6a7eb53f4b78980e6655',
                                  'manager': False},
                                 {'path': ['Runtime', 'close'],
                                  'before': 'cde0cc16702a4fd0a0eee11aaab8e4cf1a6f8d380109aa3f08daba2ecc650cc7',
                                  'after': '4c76528fe1155869406bf9940a7786abaad038b8094fee8a68eeae37b128cdf9',
                                  'manager': False}]},
 'codex_streaming': {'source': 'codex_streaming.py',
                     'sha256': '53e57815169f2cd468a817212d9e4a41f1d6225dd6effa5cc33f71a9a47af01a',
                     'functions': [{'path': ['StreamBuffer', 'flush_locked'],
                                    'before': '1d78155a4cffe7124faf684ebb6d057562bfab38bf0077e50f8557d3382ad425',
                                    'after': '91ba513d6f29edb74605cf5c98c43d6f7f482031cbbd70c7d3cdf39d9194fc87',
                                    'manager': False}]},
 'codex_connection_recovery': {'source': 'codex_connection_recovery.py',
                               'sha256': 'bf898eb8816c8c18af7325b5609824f80ae79fd1a6f722ad06682c791d992107',
                               'functions': [{'path': ['supervisor_identity'],
                                              'before': None,
                                              'after': '495401a605336d7793c4b22727047c100a40e6b536ef2e6c582a436108aca80b',
                                              'manager': False},
                                             {'path': ['transport_turn_eligible'],
                                              'before': None,
                                              'after': 'ffa21d497b32b3e46340da8022ce098974f697adc3b0b220d61d3b87d1822573',
                                              'manager': False},
                                             {'path': ['eligible'],
                                              'before': 'bb3a0940311877abc0e21117dd350ecdf0dab724971ce89ae74c9e34e6411d18',
                                              'after': 'dbfc3ac6ddec1cf281e7ca55efd2f3250a8a0e59bfc7f8fff5b32ecf3df1b22d',
                                              'manager': False},
                                             {'path': ['queued_active_wait_eligible'],
                                              'before': None,
                                              'after': '377c1bdc1f2287909532e45156fc020d8666638ad94016fb7732fe567adb718f',
                                              'manager': False},
                                             {'path': ['recover'],
                                              'before': '0254c8ed9c679c1d441908943fb2d14ec6e59eb240309b9aa87938c2f2c27ae0',
                                              'after': 'ef52b9cc675700adefe567843ddb3ab193460d15c6e47dab0e0b685f0dddbd3a',
                                              'manager': False},
                                             {'path': ['recover_queued_active_wait'],
                                              'before': None,
                                              'after': 'f1d7ab5c66fbb0e7eb97a38f2107019b84c66fdef9e29343e30dfe47fd38fdf0',
                                              'manager': False},
                                             {'path': ['adopt_transport_turn'],
                                              'before': None,
                                              'after': 'b34e21051e13aadf82617d256b2ec091d1db4b9e3b07dc0868f9a36bd7c79056',
                                              'manager': False},
                                             {'path': ['can_deliver_completion'],
                                              'before': '97b26edb3ef46413fed11a91abb45f05b5e75a1ae4523b0871f1d75d0281d290',
                                              'after': 'e13d2a4b30a4a4de76bc9d0fb7dce0617874713376ad08bb77f49fff252abe5a',
                                              'manager': False},
                                             {'path': ['unresolved_turn_receipts'],
                                              'before': None,
                                              'after': '76558661f8334fc7ea46c49d02f144f5511c6595e948a9a0793821427cac52c9',
                                              'manager': False},
                                             {'path': ['tick'],
                                              'before': 'daa085f4dbe88ee5821525e804999cbcd9d727ceb9635a9395bbac97e3b1b2cc',
                                              'after': '96d76e7014c2bc0beb3255d75d7745413ec153aa796e352d6253c5470aa095b2',
                                              'manager': False},
                                             {'path': ['run'],
                                              'before': '5326878da985dc64f16aa4b611a709db00576991fef27e44d6cf171d43ef8834',
                                              'after': '4b081689bbc82a82054c706519b106dec989fca018e3d34bda4c5b54a25a19d1',
                                              'manager': False},
                                             {'path': ['ConnectionRecovery', '__init__'],
                                              'before': None,
                                              'after': 'cc0b40e8e616a262f65c2885955f9e203ff39a087ab235ac1c0eff065ade3cfb',
                                              'manager': False},
                                             {'path': ['ConnectionRecovery', 'run'],
                                              'before': None,
                                              'after': 'b161ea2a02e577931c00fddc61485811f2f5648709fcd56a9593e8aa0cb93849',
                                              'manager': False},
                                             {'path': ['ConnectionRecovery', 'close'],
                                              'before': None,
                                              'after': 'a8e61459ecbbfc6ccae243dde638d625e271d981e438bd6b09b937099b3be7d2',
                                              'manager': False},
                                             {'path': ['start'],
                                              'before': None,
                                              'after': 'df050f203f7e191a84377e07081e41463ccdfda8bdc22873a5da79e014c6814a',
                                              'manager': False},
                                             {'path': ['close'],
                                              'before': None,
                                              'after': '4fa29d63eb77244c011c3dd174caf17d385920679f327b6f97cdc5c6f7f0b73e',
                                              'manager': False}]},
 'codex_analytics': {'source': 'codex_analytics.py',
                     'sha256': 'e0ae2ae93ee4de5898e5611fc88552be381e31e6d7bdac64f38ca351199485b7',
                     'functions': [{'path': ['AnalyticsMixin', 'analytics_event'],
                                    'before': '17c3bf418b6308004fb60aba930efd3a0515d7414d36e2934c6e4b5701a02cd8',
                                    'after': 'b6e20935097ed7f9be6c0b54948c03d139b837ad37c1538e5cde1f6419e740d1',
                                    'manager': False}]},
 'codex_analytics_history': {'source': 'codex_analytics_history.py',
                             'sha256': '7bed9200317322f79f6a763151cfba103483276b77cf19ea6eb95513a411b2a0',
                             'functions': [{'path': ['AnalyticsHistoryMixin',
                                                     'analytics_history_step'],
                                            'before': '8fb05f6807159384d2ba65a2d7c850a1d3d697e5e5a1cada52db2c08a4f7472d',
                                            'after': '88da62bab3b13431cbcf4f54c4393f1c689b5136d88a47fb781549b90c9b98f5',
                                            'manager': False}]},
 'native_notifications.dispatch': {'source': 'native_notifications/dispatch.py',
                                   'sha256': '860a35786b551bc3ce389a0a37a7c159d6f33f994098dc504a541358d5c774ac',
                                   'functions': [{'path': ['consume_native_notification'],
                                                  'before': '7071389a47d956446013f3664456ae3bcb8ec0428e2f1301f950252fdb533dd1',
                                                  'after': '6e0f7e610ec7e3116c914dad2ec5928fdb984a9206455ee18b550f423a4a46d4',
                                                  'manager': False}]}}

def _target(module, path):
    owner = module
    for name in path[:-1]:
        owner = getattr(owner, name, None)
        if owner is None:
            return None, None
    return owner, (vars(owner).get(path[-1]) if isinstance(owner, type) else getattr(owner, path[-1], None))


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    runtime_module = sys.modules.get('codex_runtime')
    if (runtime_module is None or not isinstance(runtime, runtime_module.Runtime)
            or Path(runtime_module.__file__).resolve().parent != scripts):
        raise RuntimeError('The running backend source identity differs')
    replacements = []
    for name, spec in SOURCES.items():
        module = sys.modules.get(name)
        path = scripts / spec['source']
        raw = path.read_bytes()
        if (module is None or Path(module.__file__).resolve() != path.resolve()
                or hashlib.sha256(raw).hexdigest() != spec['sha256']):
            raise RuntimeError('The reviewed backend source differs: ' + name)
        for item in spec['functions']:
            desired, _ = source_function(raw, item['path'], vars(module), str(path),
                                         allow_contextmanager=True)
            if signature(desired) != item['after']:
                raise RuntimeError('The reviewed function differs: ' + '.'.join(item['path']))
            replacements.append((module, item, desired))
    # The old history frame can hold analytics then request the runtime writer.
    # Drain it before installing the consistent writer order.
    guard = getattr(runtime, '_analytics_history_guard', None)
    if guard is not None and not guard.acquire(timeout=30):
        raise BlockingIOError('The history writer has not drained')
    try:
        with runtime.lock:
            for module, item, _ in replacements:
                _, current = _target(module, item['path'])
                actual = getattr(current, '__wrapped__', current) if item['manager'] else current
                if actual is None:
                    if item['before'] is not None:
                        raise RuntimeError('The running function is missing: ' + '.'.join(item['path']))
                elif signature(actual) not in {item['before'], item['after']}:
                    raise RuntimeError('The running function differs: ' + '.'.join(item['path']))
            with runtime.db() as db:
                db.execute('CREATE TABLE IF NOT EXISTS runtime_supervisor_cursor '
                           '(handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL)')
            recovery = sys.modules['codex_connection_recovery']
            # This reviewed module only defines constants, functions and a timer class.
            spec = importlib.util.spec_from_file_location('_studio_recovery_patch_source', recovery.__file__)
            fresh = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(fresh)
            recovery.threading = fresh.threading
            recovery.IDENTITY = fresh.IDENTITY
            recovery.MAX_ACCOUNT_RECOVERIES = fresh.MAX_ACCOUNT_RECOVERIES
            if not hasattr(recovery, '_START_LOCK'):
                recovery._START_LOCK = fresh._START_LOCK
            if not hasattr(recovery, 'ConnectionRecovery'):
                recovery.ConnectionRecovery = type('ConnectionRecovery', (), {'__module__': recovery.__name__})
            for module, item, desired in replacements:
                owner, current = _target(module, item['path'])
                actual = getattr(current, '__wrapped__', current) if item['manager'] else current
                if actual is None:
                    setattr(owner, item['path'][-1], contextmanager(desired) if item['manager'] else desired)
                else:
                    actual.__code__ = desired.__code__
                    actual.__defaults__ = desired.__defaults__
                    actual.__kwdefaults__ = desired.__kwdefaults__
            # Replacing function code cannot change an already-running dispatch frame.
            # Give those existing frames the same two safe persistence boundaries.
            frames = sys._current_frames()
            for server in runtime.servers.values():
                if not isinstance(server, runtime_module.AppServer) or not server.supervisor_mode:
                    continue
                frame = frames.get(getattr(server.dispatcher, 'ident', None))
                while frame is not None and frame.f_code.co_name != 'dispatch':
                    frame = frame.f_back
                if (frame is not None and frame.f_code is runtime_module.AppServer.dispatch.__code__):
                    continue
                if getattr(server, '_studio_persistence_retry_installed', False):
                    continue
                commit, lookup = server.supervisor_commit, server.supervisor_event_applied
                if commit is not None:
                    server.supervisor_commit = (lambda message, sequence, commit=commit, server=server:
                        server.persistence_retry(lambda: commit(message, sequence)))
                if lookup is not None:
                    server.supervisor_event_applied = (lambda sequence, lookup=lookup, server=server:
                        server.persistence_retry(lambda: lookup(sequence)))
                server._studio_persistence_retry_installed = True
    finally:
        if guard is not None:
            guard.release()
    recovery.start(runtime)
    runtime.changed.set()
    return {'status': 'applied'}
