"""Restore exact input receipts and surviving turns without a backend restart."""
from codex_historical_input_wait_update import apply as apply_inputs
from codex_queued_restart_update import apply as apply_turns


def apply(runtime):
    with runtime.lock:
        inputs = apply_inputs(runtime)
        turns = apply_turns(runtime)
    return {'status': 'applied', 'inputs': inputs, 'turns': turns}
