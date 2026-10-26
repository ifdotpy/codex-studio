"""Run existing Python client contracts against an explicitly selected disposable supervisor.

Only the fixture's supervisor launch is substituted. Application code and test
assertions are imported unchanged. No Runtime-based tests or real providers run.
"""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[5]
SOURCE = ROOT / "workspaces/runtime/apps/server/src"
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
sys.path.insert(0, str(SOURCE))
sys.path.insert(0, str(TESTS))
binary = Path(os.environ["STUDIO_SUPERVISOR_BINARY"]).resolve(strict=True)
assert Path(os.environ["TMPDIR"]).is_dir()
assert all(os.environ.get(key) for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"))

spec = importlib.util.spec_from_file_location("supervisor_client_contract", TESTS / "process-supervisor-contract.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
launch = subprocess.Popen

def disposable_launch(command, *args, **kwargs):
    if isinstance(command, list) and str(SOURCE / "codex_process_supervisor.py") in command:
        marker = command.index(str(SOURCE / "codex_process_supervisor.py"))
        command = [str(binary), *command[marker + 1:]]
        state = Path(command[command.index("--state") + 1]).resolve()
        assert state.is_relative_to(Path(os.environ["TMPDIR"]).resolve()), "refuse non-disposable supervisor"
    return launch(command, *args, **kwargs)

NAMES = [
    "test_live_child_rejects_changed_signature_with_operator_guidance",
    "test_supervisor_rpc_reply_passes_a_blocked_notification",
    "test_exited_child_is_replaced_in_the_same_supervisor_generation",
    "test_operator_close_requires_exact_identity_and_closes_verified_fixture",
    "test_backend_detach_reattaches_to_same_fake_turn_and_replays_output",
    "test_backend_identity_changes_reattach_the_same_native_process",
    "test_reattach_completes_initialized_if_backend_dies_after_initialize_result",
    "test_reopened_handle_completes_handshake_for_new_child_generation",
    "test_operation_receipt_and_cursor_errors_are_fail_closed",
    "test_competing_backend_attach_is_refused",
    "test_supervisor_reboot_closes_dead_handles_and_preserves_receipts",
    "test_supervisor_sigkill_terminates_verified_live_child_group",
    "test_reused_child_pid_closes_old_handle_without_signalling_new_process",
]
if __name__ == "__main__":
    selected = sys.argv[1:] or NAMES
    assert set(selected) <= set(NAMES)
    suite = unittest.TestSuite(module.ProcessSupervisorContract(name) for name in selected)
    with patch.object(subprocess, "Popen", disposable_launch):
        result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
