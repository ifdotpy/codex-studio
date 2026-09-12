"""Apply reviewed execution-setting fixes without replacing live connections."""
import hashlib
import importlib
from pathlib import Path
import sys

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_runtime.Runtime.conversation_settings': ['fe42afee750a93612e74d72c16a373d523e9df8e088fe6fe75adc6df0360fc27',
                                                 '185b567877fa4619d3c6c5f564bd235a9f6cfd57b283cac6ae751041ffbf892e'],
 'codex_runtime.Runtime.native_action': ['cff06b16172a55b341f7b2d62cf8c3bea13568cb8e0487666263a9d649fa72c6',
                                         'd5902623c1f95b33616838b7235c78106bbb468335c2b6d200c7926b5d3538e0'],
 'codex_runtime.Runtime.run_native_action': ['9756e74ea13ba1d381f540bd52ab249149a4a6cfb290868f4b372b9ef67e85e0',
                                             '086804757634cf287f8c57bf52238bdf0788c80a3fafa2326af838a14a3c3b84'],
 'codex_capacity_retry.CapacityRetryMixin.capacity_retry': ['2088bfbf93a07c47babc319e30ece4cc8df13e5b641796f2b4b817165f864fcd',
                                                            '320183e7a032ebe7535f69d277b0c0cac7592ae864d77a649412265695763477'],
 'codex_account_transfer.AccountTransfers.run': ['97f9a12b51fdc811053727d049101e7faee17b707ef764f385ca2c592d06b812',
                                                 'cbfd1156b08a1946b4b149293fecdbd67f4b778e87a2d025b9403708286b06fb'],
 'codex_account_transfer.AccountTransfers.received': ['ae174f0052b80c4acde2db40af5c60a303a83146dbc65b5350423bd7ad7a8995',
                                                      '7a1369afa24951d22bd1ce775c3e091cc205c22ba7202b91261b5df71cc70133'],
 'codex_account_transfer.AccountTransfers.commit': ['e000ac262f6725bbeeac507f8be2d1e70927e9dcc29fbd698dd34c544da9ea9c',
                                                    '8fec45515ce9e171dbcb716d9d60e7365c60eaa24c57159a3897fddcc1e4ad23']}
TRANSFER_SHA = 'e952e41f3ea3ae09e92ee2de341471a61332327f4968c56f54e6545529fe8dd8'
ACTION_SHA = '33db9ebb32d65fec14627378eb35c3b8a1798ddf3aa8d3dda8449ea70dd1bf33'
ASSERT_SIGNATURE = '29addd9da8af12a9505c9e42ce2863d367271a02dd1be4f5de48403fa56be2c0'


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError("Unknown runtime for execution settings update")
    directory = Path(__file__).resolve().parent
    for name, digest in (("codex_account_transfer.py", TRANSFER_SHA), ("codex_native_action_settings.py", ACTION_SHA)):
        if hashlib.sha256((directory / name).read_bytes()).hexdigest() != digest:
            raise RuntimeError("Unreviewed execution settings source: " + name)
    replacements = []
    modules = {}
    for target, allowed in EXPECTED.items():
        module_name, owner_name, name = target.split(".")
        module = importlib.import_module(module_name)
        modules[module_name] = module
        path = directory / (module_name + ".py")
        if Path(module.__file__).resolve() != path:
            raise RuntimeError("Unexpected settings module location")
        owner = getattr(module, owner_name)
        desired = compile_function(path.read_text(), owner_name, name, vars(module), str(path))
        if signature(desired) != allowed[1]:
            raise RuntimeError("Unreviewed settings replacement: " + target)
        replacements.append((owner, name, desired, module, allowed))
    transfer = modules["codex_account_transfer"]
    check = compile_function((directory / "codex_account_transfer.py").read_text(),
                             "AccountTransfers", "assert_settings", vars(transfer))
    if signature(check) != ASSERT_SIGNATURE:
        raise RuntimeError("Unknown transfer settings guard")
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime remains busy; settings update not applied")
    originals = []
    missing = object()
    previous_check = vars(transfer.AccountTransfers).get("assert_settings", missing)
    previous_error = vars(transfer).get("TransferSettingsConflict", missing)
    try:
        if runtime.closed:
            raise RuntimeError("Runtime is closed")
        for owner, name, desired, module, allowed in replacements:
            live = vars(owner).get(name)
            if signature(live) not in allowed or live.__globals__ is not vars(module):
                raise RuntimeError("Unknown live settings method: " + name)
            if name in vars(runtime) or (hasattr(runtime, name) and getattr(runtime, name).__func__ is not live):
                raise RuntimeError("Unexpected runtime settings override: " + name)
            store = getattr(runtime, "_account_transfers", None)
            if owner is transfer.AccountTransfers and store is not None:
                if type(store) is not owner or name in vars(store):
                    raise RuntimeError("Unexpected transfer settings override")
            originals.append((live, live.__code__))
        if previous_check is not missing and (signature(previous_check) != ASSERT_SIGNATURE
                or previous_check.__globals__ is not vars(transfer)):
            raise RuntimeError("Unknown existing transfer settings guard")
        store = getattr(runtime, "_account_transfers", None)
        if store is not None and (type(store) is not transfer.AccountTransfers or "assert_settings" in vars(store)):
            raise RuntimeError("Unexpected transfer settings guard override")
        if previous_error is not missing and (not isinstance(previous_error, type)
                or previous_error.__bases__ != (ValueError,) or previous_error.__module__ != transfer.__name__):
            raise RuntimeError("Unknown existing transfer settings error")
        already = all(signature(live) == row[4][1] for (live, _), row in zip(originals, replacements))
        if already and previous_check is not missing and previous_error is not missing:
            return {"status": "already_applied", "baseCommit": "5217bb9"}
        if "codex_native_action_settings" in sys.modules:
            raise RuntimeError("Native action settings helper was loaded before update")
        try:
            if previous_error is missing:
                transfer.TransferSettingsConflict = type("TransferSettingsConflict", (ValueError,), {"__module__": transfer.__name__})
            if previous_check is missing:
                transfer.AccountTransfers.assert_settings = check
            for (live, _), (_, _, desired, _, _) in zip(originals, replacements):
                live.__code__ = desired.__code__
        except BaseException:
            for live, code in originals:
                live.__code__ = code
            if previous_check is missing:
                delattr(transfer.AccountTransfers, "assert_settings")
            if previous_error is missing:
                delattr(transfer, "TransferSettingsConflict")
            raise
        return {"status": "applied", "baseCommit": "5217bb9", "methods": list(EXPECTED)}
    finally:
        runtime.lock.release()
