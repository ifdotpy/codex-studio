"""Apply the account migration boundary without restarting active chats."""
import hashlib
import sys
from pathlib import Path
from types import FunctionType

from codex_active_task_update import compile_function, signature


SOURCE_SHA = '7da253c94769eb7dd54dab0faf915437528ad7f23b1cb6dc6f8ce7d5d5782443'
EXPECTED = {
    'save': (
        '63466821529bda2c746245e1e44848e9f27fece3708b74d558d4038580778251',
        '743a404c8319d1ca957c28eb1a7bb2f6cb7532bf06ba6c5f4e913bdb772d433c',
    ),
    'request': (
        'c30af1f46c7999ae1001c5b9589faf73a8850a5b119726109dbf447251bca662',
        '3d85e0b463d45dd458ab7289f9bbabbfd2abf2a37a4063327190e4a051cf9c6a',
    ),
    'adopt': (
        'e44991039703fec31420ed92833c4ffdbb7c13032be4c37aaf69f817118b9ead',
        'beebd001b9f5fbb480d731b1b8d5446e0329446669409c7dc07130109b8185f3',
    ),
    'tick': (
        '566c776adab85e7359e42459279cde5f8a88744cab3e2c69bc081b5a59542289',
        '87d0b6776108cfda85f055aeb310e9d984ec3b8eb6daa386eff1f4e921981d3a',
    ),
}


def apply(runtime):
    import codex_account_transfer as transfer
    import codex_runtime

    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for account transfer update')
    source = Path(__file__).resolve().parent / 'codex_account_transfer.py'
    if hashlib.sha256(source.read_bytes()).hexdigest() != SOURCE_SHA:
        raise RuntimeError('Unreviewed account transfer source')
    desired = {
        name: compile_function(source.read_text(), 'AccountTransfers', name,
                               vars(transfer), str(source))
        for name in EXPECTED
    }
    for name, function in desired.items():
        if signature(function) != EXPECTED[name][1]:
            raise RuntimeError('Unreviewed account transfer replacement: ' + name)
    owner = transfer.AccountTransfers
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; account transfer update not applied')
    originals = []
    try:
        for name, allowed in EXPECTED.items():
            live = vars(owner).get(name)
            if not isinstance(live, FunctionType) or live.__globals__ is not vars(transfer):
                raise RuntimeError('Unknown live account transfer method: ' + name)
            if signature(live) not in allowed:
                raise RuntimeError('Unknown live account transfer method: ' + name)
            originals.append((live, live.__code__, desired[name].__code__))
        if all(signature(live) == EXPECTED[name][1]
               for (live, _, _), name in zip(originals, EXPECTED)):
            return {'status': 'already_applied', 'methods': list(EXPECTED)}
        try:
            for live, _, replacement in originals:
                live.__code__ = replacement
        except BaseException:
            for live, previous, _ in originals:
                live.__code__ = previous
            raise
        return {'status': 'applied', 'methods': list(EXPECTED)}
    finally:
        runtime.lock.release()
