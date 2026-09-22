"""Apply reviewed chat controls without interrupting active agents."""
import sys
from pathlib import Path
from types import FunctionType
from codex_active_task_update import signature
from codex_progress_update import source_function
from codex_resource_removal_update import _find_handler

EXPECTED = {'codex_runtime.Runtime.send': ['56f5b33967163ff41b1a26d21f8a9495b10b797a1655bf57c5e6698b4a82af82', 'fc88db151db647549f1d899ef884dddf943dda9a51e2ac7b3f6b18e1e9604f24'], 'codex_runtime.Runtime.answer': ['acdf12f1e0dc49cef1461d57ef2200015081f6c58ae3145efd161a4bb41c3de4', '45e6cd61058b0ac22037465c886f3f6a1bfaacdcbea6e177660c1b71d63f9e88'], 'codex_work.WorkMixin.queue_action': ['4e263ff8b08c41c23b54cfa5f108ab60cebcf107c45bcb8c3faad1cfb9c34762', '5150911f9d9101a1f3a7249873a4378bae47c890d5d1cb47b9612c010fc6f023'], 'codex_questions.QuestionsMixin.delete_question': [None, '01c9329bedf2ef8471eca6f6c2a8517fe499a7bdc94901dda245d27e59e24789'], 'codex_questions.QuestionsMixin.question_history': ['a983cda873e25a1aec653cc82c22e934557f7e34fc064950a70b6b6eb1a1e38d', '93b5cb7540ea9f58bdc00101ef8800e0ae6ddb085bca8cdf568310c7ff6dcf40'], 'codex_canvas.make_server.Handler.do_POST': ['d5c6d1ae17c0c3a07d8017dd6b03914c8df9eae076ae24be2e530a3b5c1e776c', 'b412e08c14870c9a3a36e016760e10ffe59f332eb44cb466604690f604ed13e8']}


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError("Unknown chat controls runtime")
    handler = _find_handler(runtime)
    replacements = []
    directory = Path(__file__).resolve().parent
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split(".")
        module = sys.modules[module_name]
        if Path(module.__file__).resolve().parent != directory:
            raise RuntimeError("Unexpected chat controls module: " + module_name)
        desired, static = source_function((directory / (module_name + ".py")).read_text(), path, vars(module))
        if static or signature(desired) != allowed[1]:
            raise RuntimeError("Unreviewed chat controls source: " + target)
        owner = handler if path[0] == "make_server" else getattr(module, path[0])
        replacements.append((owner, path[-1], desired, allowed, target))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime busy; chat update waits")
    originals = []
    try:
        if runtime.closed:
            raise RuntimeError("Runtime closed")
        for owner, name, desired, allowed, target in replacements:
            live = vars(owner).get(name)
            if live is None:
                if allowed[0] is not None:
                    raise RuntimeError("Missing live chat method: " + target)
            elif (not isinstance(live, FunctionType) or signature(live) not in allowed
                  or live.__globals__ is not desired.__globals__
                  or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError("Unknown live chat method: " + target)
            originals.append((owner, name, live, None if live is None else
                              (live.__code__, live.__defaults__, live.__kwdefaults__)))
        try:
            for (owner, name, live, _), (_, _, desired, _, _) in zip(originals, replacements):
                if live is None:
                    setattr(owner, name, desired)
                else:
                    live.__code__ = desired.__code__
                    live.__defaults__ = desired.__defaults__
                    live.__kwdefaults__ = desired.__kwdefaults__
        except BaseException:
            for owner, name, live, previous in reversed(originals):
                if live is None:
                    if name in vars(owner): delattr(owner, name)
                else:
                    live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            raise
        return {"status": "applied", "methods": list(EXPECTED)}
    finally:
        runtime.lock.release()
