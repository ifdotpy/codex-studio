"""Restore one paused worker's exact native failure without repeating its send."""
import hashlib
from pathlib import Path
from types import ModuleType

HELPER_SHA256 = '3e1f4d1733344f70326aad58c619f81f014a8c0d291557cb751f424bf110d7f2'

PLAN = {
    'id': 'worker-1aee-terminal-receipt-20260914-v1',
    'evidence': 'evidence/known-bugs-20260914/worker-1aee-native-terminal-receipt.json',
    'sha256': 'd82164e9fd430acff6606a9a261ed70db0bdf0602f83368dda0a128c88daaf12',
    'agent': '1aee97c4-430a-5f7a-8216-86111f4f2883',
    'accountKey': 'default',
    'threadId': '01a08a9c-626c-7ea1-800c-d6d5d56fcb36',
    'turnId': '01a0969c-45b8-70c2-bf44-74d9f391901f',
    'callId': 'exec-9c68a17b-5bd2-4539-a481-6cb97ecf8d57',
    'tool': 'orchestration_message',
    'epoch': 0,
    'currentEpoch': 1,
    'requirePaused': True,
    'accountHomePolicy': 'managed',
}


def _load_helper():
    path = Path(__file__).with_name('codex_terminal_receipt_recovery_update.py')
    with path.open('rb') as source:
        content = source.read(65537)
    if len(content) > 65536 or hashlib.sha256(content).hexdigest() != HELPER_SHA256:
        raise RuntimeError('Terminal receipt recovery refused: unreviewed helper source')
    helper = ModuleType('_studio_worker_terminal_receipt_helper')
    helper.__file__ = str(path)
    helper.__package__ = ''
    exec(compile(content, str(path), 'exec', dont_inherit=True), vars(helper))
    return helper


def apply(runtime):
    return _load_helper().apply_plan(runtime, PLAN)
