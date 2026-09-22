"""Apply reviewed Claude integration code without restarting active sessions."""
import hashlib
import re
import shlex
import sys
from pathlib import Path
from types import FunctionType

from codex_active_task_update import signature
from codex_progress_update import source_function, _http_closure
from codex_resource_removal_update import _find_handler

BASE_COMMIT = 'c4af1ae'
# Python 3.14 signatures for the installed baseline and reviewed replacement.
EXPECTED = {'codex_runtime.AppServer.__init__': ['db501ad9f19f742f958e0cc4888e3ecb6d98d464ba424f94ea4716f4c4a966f0',
                                      'be1dbfc2f4df805363aa071fdce3f9edc3434cabaa71be106369f8abc7d319cf'],
 'codex_runtime.Runtime.connect': ['a36f580c23835d0dc226d2992f3d44cb76a22e53c6cc42541f04d575942ba393',
                                   'a58f8127084c2d46eec923ac4d288f98e89de561f21475dec0ffbca98f5f0f45'],
 'codex_runtime.Runtime.send': ['04eccb45271a3a3d8d355b3d5fa28d01e66f230e8e843c7f7b5f0491bdca1e0a',
                                'fc88db151db647549f1d899ef884dddf943dda9a51e2ac7b3f6b18e1e9604f24'],
 'codex_runtime.Runtime.new_thread_params': ['d4186afae1407e76eb1cabc5ec39fb32de810ba00f3cb4728bc751f9882431c7',
                                             'edb3cc2c27e066a16eb891107563c9896f3eea3543c591adeeb87102ebfb6876'],
 'codex_runtime.Runtime.dispatch': ['5663ae51be5b9f938dde85042da686224986109e8304e2950cdcfb2b10b9ebf8',
                                    'f292cc479e57713129bf6b357734ccef4376830af12330d39c9bdfac5e387ace'],
 'codex_runtime.Runtime.start': ['51cadbaa97832c5563a39c3f6ad2cfe7be888a808c69cf7dd98e5eb8faf7b1ab',
                                 '5c1fe6b639d2b5d92f9b47ce7ab07349f49156c5d48bb87097f5e3a6af1c586b'],
 'codex_runtime.Runtime.limits': ['d40048f79288684ae82ffff8702a83538b128a93d4b5c24c2032f6cb2a2f32dc',
                                  'b85262a88e9330e0ee2887c0eaf9253e88dd2eb118e1d62df49aeb17d57d6d37'],
 'codex_accounts.AccountStore.refresh': ['65609a7704a4477fe678e45cb7ac9d0f5ae78db743c8373aae813487c3001065',
                                         '939e521fad5ef9649e403d794a9e7994e432000f42ea11c07046c9276929cfb1'],
 'codex_accounts.AccountStore.register_claude': [None,
                                                 '7682e679549d768f99424d2998ed96b14d02115fba61f3e32e62b8cc3f19750c'],
 'codex_accounts.AccountStore.update_claude': [None,
                                               'a389171f3517c17d865b78b14b521662a0724295c50bec61eb14646608837a3d'],
 'codex_claude.profile_options': [None, 'bd76dacbd091822995ebcb6a68f1edc7d6f0c2e5c51075532ce5e2771ad9ebe7'],
 'codex_claude.bridge_options': [None, 'a0940c50582b138bf331e648f6acbefae69d6f1084b5de4c336ee2801a99879d'],
 'codex_claude.installed': ['e299c3aa08d1fa6504bb08fb3df6162bdc55252d72176dd6735a01c69acac881',
                            '64994c3adaf98866bf6929e23e67bcde05733b8938f4ac2119dc813d8a5d47f0'],
 'codex_claude.subscription_env': ['0d6c9d721e7b02dd828194025ab853539541ed443c716a7575dd55ef67fd51e5',
                                   'e457de0bef0d995fb9796bd87732a77854448369550d1b0d950cea9dc9a84c0a'],
 'codex_claude.auth_metadata': ['f2f4e1bf92365266d63d54e5b3c01b6d8aa4adc21abed7bea9b0b82a17de5801',
                                'e57adba8d0f06d78b90cca2a220632e965a93e675e19097930ca32ab77dc8514'],
 'codex_claude.transport': ['ef0be5ad1e88c84f19af9e80c46be7da47c41e25a57b23e70ee57c35b5b213c2',
                            '7add69e68077a9810878c785b9c74c9946fd59d5a8131ab42ae9de1daaefbb62'],
 'codex_canvas.make_server.Handler.do_POST': ['b412e08c14870c9a3a36e016760e10ffe59f332eb44cb466604690f604ed13e8',
                                              'f24b8e4bf756ce7967c0333bb2025d4985c19d61a614313548c40d6a3d0a07d6']}
FILES = {'claude_bridge/bridge.mjs': '82ff0a019cd874332d244207117ad908b37fbcaacd5049c177e13ac09ea9d640',
 'claude_bridge/features.mjs': 'f876173417fa5d4a7f3bba6e4affd39f47141f66b230e3358def5e4617556aa5',
 'claude_bridge/controls.mjs': '565f0722e556697fa4cfc6956bfb335158293c9e311f151f04558839d0811400',
 'claude_bridge/package.json': 'b16f3c40492c75a0b564a87db121442b4fa3c42609b91d54d11f5be7bb9285f8',
 'claude_bridge/package-lock.json': '4c49940618ae1e94e846ba59f7813ce5d118cdbc679bb72bcc8e4c79519d306a',
 'codex_claude_controls.py': 'd2c5b7c0ef73a9b8eb75bcd689041f9f9472a14aa0aa983eb48f003ca58499f8'}
_MISSING = object()


def _install(owner, name, live, desired, static):
    if live is None:
        setattr(owner, name, staticmethod(desired) if static else desired)
    else:
        live.__code__ = desired.__code__
        live.__defaults__ = desired.__defaults__
        live.__kwdefaults__ = desired.__kwdefaults__


def apply(runtime):
    import codex_runtime
    import codex_accounts
    import codex_claude
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown Claude parity runtime')
    if not EXPECTED or not FILES:
        raise RuntimeError('Claude parity source review is incomplete')
    directory = Path(__file__).resolve().parent
    for name, digest in FILES.items():
        if hashlib.sha256((directory / name).read_bytes()).hexdigest() != digest:
            raise RuntimeError('Unreviewed Claude parity file: ' + name)
    handler = _find_handler(runtime)
    _http_closure(handler.do_GET, runtime, handler)
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split('.')
        module = sys.modules.get(module_name)
        if module is None or Path(module.__file__).resolve() != directory / (module_name + '.py'):
            raise RuntimeError('Unexpected Claude parity module: ' + module_name)
        desired, static = source_function((directory / (module_name + '.py')).read_text(), path, vars(module))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed Claude parity source: ' + target)
        owner = handler if path[0] == 'make_server' else getattr(module, path[0]) if len(path) > 1 else module
        replacements.append((owner, path[-1], desired, static, allowed, target))
    acquired = []
    originals = []
    previous_globals = {}
    try:
        # Protect account refresh and native auth reads while their helpers change.
        for lock in (runtime.lock, runtime.accounts.lock, codex_claude._lock):
            if not lock.acquire(timeout=10):
                raise RuntimeError('Runtime busy; Claude parity update waits')
            acquired.append(lock)
        if runtime.closed:
            raise RuntimeError('Runtime closed')
        for owner, name, desired, static, allowed, target in replacements:
            descriptor = vars(owner).get(name)
            live = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            if live is None:
                if allowed[0] is not None or hasattr(owner, name):
                    raise RuntimeError('Missing live Claude parity method: ' + target)
            elif (isinstance(descriptor, staticmethod) != static
                  or not isinstance(live, FunctionType) or signature(live) not in allowed
                  or live.__globals__ is not desired.__globals__
                  or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live Claude parity method: ' + target)
            originals.append((owner, name, live, descriptor, None if live is None else
                              (live.__code__, live.__defaults__, live.__kwdefaults__)))
        additions = {'re': re, 'shlex': shlex}
        for name, module in additions.items():
            value = vars(codex_claude).get(name, _MISSING)
            if value is not _MISSING and value is not module:
                raise RuntimeError('Unknown Claude parity global: ' + name)
        cache = vars(codex_claude).get('_cache', _MISSING)
        if cache is not _MISSING and type(cache) is not dict:
            raise RuntimeError('Unknown Claude parity auth cache')
        additions['_cache'] = {} if cache is _MISSING else cache
        previous_globals = {name: vars(codex_claude).get(name, _MISSING) for name in additions}
        previous_discovered = runtime.accounts.discovered
        try:
            vars(codex_claude).update(additions)
            for original, replacement in zip(originals, replacements):
                owner, name, live, _, _ = original
                desired, static = replacement[2:4]
                _install(owner, name, live, desired, static)
            runtime.accounts.discovered = False
        except BaseException:
            for owner, name, live, descriptor, previous in reversed(originals):
                if live is None:
                    if name in vars(owner):
                        delattr(owner, name)
                else:
                    live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            for name, previous in previous_globals.items():
                if previous is _MISSING:
                    vars(codex_claude).pop(name, None)
                else:
                    setattr(codex_claude, name, previous)
            runtime.accounts.discovered = previous_discovered
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
    finally:
        for lock in reversed(acquired):
            lock.release()
