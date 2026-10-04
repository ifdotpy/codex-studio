"""Apply clock response and compact historical receipt reads without a restart."""
from codex_supervised_rpc_update import apply as apply_rpc
from codex_saved_input_receipts_update import apply as apply_inputs


def apply(runtime):
    with runtime.lock:
        rpc = apply_rpc(runtime)
        inputs = apply_inputs(runtime)
    return {'status': 'applied', 'rpc': rpc, 'inputs': inputs}
