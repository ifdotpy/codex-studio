"""Apply user-assigned review pairs without opening other team boundaries."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_chat_reviews.review_schedule': ('506caa3cbe2e21c813c92216ee0f250a267cc52874edc4b6f08185e1e8438852',
                                        '9589b0d087ca7b72f5d3f8c84a59dfc3a5f71d7bdd6c2766acf1e824ca259b78'),
 'codex_chat_reviews.review_tick': ('992a45d7b495abb450d54afb494e34ad70bc0c11c977d5d3e3aa1aec2b447579',
                                    'ae1ae39a20b0dcf139e3cb5981cfa5be7aa5e8c7c2a7e96a5713e4de7448dbed'),
 'codex_chat_reviews.review_pair_allowed': (None,
                                            '68218730e476e82c3616c85210738278b9ca37d558c53754831c291c83cc0dbc'),
 'codex_runtime.chat_rooms': ('58359b4cd138fbcd4500277d96ea4115cd24e3e21a352b42fbf5c201ba7db450',
                              'cfa14b270ab4aeef7b60247337de132d18d661b411c0e14544f71ff37ebb15cc'),
 'codex_runtime.chat_message': ('707dce71f666f4b70dfa7e9debf5494072a00f61d2e9806266a4b487ed6cd315',
                                '792ec294217e621fce39099ed9e825913a68231eb059a492f498535fb6dc4ba3'),
 'codex_team_isolation.validate_event': ('ca98828c99a067e540fece82b8442491b8d907e52391f1dc8b644998fe12f072',
                                         'c2d05be3bd8676421f93272adef47a38e298367f55fc49e147d0e3a7c5b55721')}
INSTRUCTIONS = ('eb63c2ee7e9e7b0e3af0a6c0ff36018f28a45cb657218d02e65a0663a2b6bcf5', '8b48f15da523f9241eddb1e8905662e5fb59f88c11e455964f6c0cfa223d91f7')
TOOLS = ('a76bf69476cdf347a932ced5f5a6a2f0ab2c5f7abad4124f6a0b271e468ce0a4', 'f4858296d91ac7cb7ccf2a85c7d9ab7e1f6c23572ecce6061e1bf3b85fae36c5')


def apply(runtime):
    import codex_runtime
    import codex_chat_reviews
    import codex_team_isolation
    from codex_progress_update import source_instructions, digest
    from codex_team_isolation_update import _tools
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for review pair update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_chat_reviews, None, 'review_schedule'),
        (codex_chat_reviews, None, 'review_tick'),
        (codex_chat_reviews, None, 'review_pair_allowed'),
        (codex_runtime, 'Runtime', 'chat_rooms'),
        (codex_runtime, 'Runtime', 'chat_message'),
        (codex_team_isolation, None, 'validate_event'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected review pair module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed review pair replacement')
        staged.append((module, owner, name, desired, allowed))
    raw = (directory / 'codex_runtime.py').read_text()
    instructions = source_instructions(raw)
    tools = _tools(raw, codex_runtime.TOOLS)
    if digest(instructions) != INSTRUCTIONS[1] or digest(tools) != TOOLS[1]:
        raise RuntimeError('Unreviewed review pair instructions')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no review pair update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        previous_instructions = codex_runtime.INSTRUCTIONS
        previous_tools = list(codex_runtime.TOOLS)
        if digest(previous_instructions) not in INSTRUCTIONS or digest(previous_tools) not in TOOLS:
            raise RuntimeError('Unknown live review instructions')
        additions = []
        changes = []
        for module, owner, name, desired, allowed in staged:
            live = vars(owner).get(name)
            if live is None and allowed[0] is None:
                additions.append((owner, name, desired))
                continue
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live review pair function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, desired.__code__))
        changed = bool(changes or additions or digest(previous_instructions) != INSTRUCTIONS[1]
                       or digest(previous_tools) != TOOLS[1])
        try:
            for owner, name, desired in additions:
                setattr(owner, name, desired)
            for live, previous, desired in changes:
                live.__code__ = desired
            codex_runtime.INSTRUCTIONS = instructions
            codex_runtime.TOOLS[:] = tools
        except BaseException:
            for live, previous, desired in reversed(changes):
                live.__code__ = previous
            for owner, name, desired in additions:
                if getattr(owner, name, None) is desired:
                    delattr(owner, name)
            codex_runtime.INSTRUCTIONS = previous_instructions
            codex_runtime.TOOLS[:] = previous_tools
            raise
        return {'status': 'applied' if changed else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
