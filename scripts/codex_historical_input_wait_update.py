"""Apply exact historical input recovery without stopping the backend."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

BASE_SOURCE_SHA256 = '22fe678dd0d47eb9ba7c8117223e7fce6995ed9ffb0c2cb2593af83f37acbb2f'
SOURCE_SHA256 = '60ba654897fa1d979566ae66067d3c0efde4ec7dfad7373312ef26b4d7360442'
FUNCTIONS = (('recover_unconfirmed_inputs',
  '13828ec4967e6b458241ccf3587fb19330923a6d01d3fedb68302edbcbf3d8f3',
  '761236ec797e70c42714512333c43a9ae2daadd41ef2a1ff6d898fc610140840'),
 ('claim_context_wait',
  '621d6bb41721ba22fb8b90740d5e6245da27e9219ef2616a5154c120fad92cdb',
  '3dfbb0251949fb4e49bb8e1499c184457c7d2e7b2904fbdb532a3a77ce239452'),
 ('_historical_input_wait',
  None,
  '74f6cc9c8e86caaf802a454605d95f475ac1565a5e9167b4df381b31abf8cae3'),
 ('_historical_input_rows',
  None,
  '44d7ec4f288bda864bb5f5fae1ba69214ee7afc6cf0c92bbfec8452834cb19ef'),
 ('_accepted_input_turn', None, '742145d694a26e9819f112dcac600d1e94f01e8f1aefe48b44258baf268c5fe8'),
 ('_schedule_input_wait_check',
  None,
  'a42297062da1b3cd642fa9233f12c5671d80a6f9fa2bc6dc702791a111805d03'))
DEPENDENCIES = (('_identity', '80fa0af556ae6361072cae6d76af1ed7deedcbda905380d3639ec84e370f2da4'),
 ('_attempt', 'd235ed309cbede48a8cd92b00fb84da2631c36d53be16df57d226634c5285e26'),
 ('_unsubmitted', 'cc91cb6045b9e3351683147bee5be3307cd4d755f045402a9d1d99e7ea506749'),
 ('_held_restart_marker', '5a0f047e71f9cec27e95da3a3abd046e60afbd43b6b7a78c24122d1fa290ea9a'),
 ('_unsettled_inputs', '3ea0b1159805deb91ba009990d63dcd39c68de615eef4a03a7b44b081b330679'),
 ('_local_idle', '4b3729c083b641da7db9236237314612138fa72efd85a186ecb5a411d646cc70'),
 ('_run_restart_input_check', 'b6c3d2e9a849479d343150956b2bc71718c63fd32c44e9d66ab36bdde5a0a74f'))


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    context = sys.modules.get('codex_context_repair')
    path = scripts / 'codex_context_repair.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or context is None or Path(context.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed historical input source differs')
    plans = []
    for name, before, after in FUNCTIONS:
        desired, _ = source_function(raw, [name], vars(context), str(path))
        if signature(desired) != after:
            raise RuntimeError('The reviewed historical input function differs: ' + name)
        plans.append((name, before, after, desired))
    with runtime.lock:
        already = True
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            actual = None if current is None else signature(current)
            if actual not in {before, after}:
                raise RuntimeError('The running historical input function differs: ' + name)
            already = already and actual == after
        for name, expected in DEPENDENCIES:
            current = vars(context).get(name)
            if current is None or signature(current) != expected:
                raise RuntimeError('The running input recovery dependency differs: ' + name)
        if context.TASK_CHECK_SECONDS != 20:
            raise RuntimeError('The running input history deadline differs')
        if already:
            return {'status':'already_applied'}
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            if current is None:
                vars(context)[name] = desired
            else:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status':'applied'}
