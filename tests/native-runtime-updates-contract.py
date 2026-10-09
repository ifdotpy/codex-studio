#!/usr/bin/env python3
"""Native executable changes must preserve work and wait for confirmed idleness."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import hashlib
import importlib.util
import json
import os
from contextlib import contextmanager
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_native_tools as native_tools
from codex_native_binary import APPROVAL_REVISION, REQUIRED_COMPANIONS, bundle_digest
from codex_native_runtime import NativeRuntimeUpdates, executable_for
from codex_runtime import Runtime as ProductionRuntime

spec = importlib.util.spec_from_file_location("native_tools_fixture", ROOT / "tests/native-tools-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Native(fixture.Native):
    def __init__(self, runtime, *, replacement=False):
        super().__init__(runtime)
        self.replacement = replacement
        self.native_binary = {"path": str(runtime.root / "old-codex"), "version": "0.1.0",
                              "sha256": "old", "bundleSha256": "old-bundle"}
        self.initialize_result = {"userAgent": "codex-cli/0.1.0"}
        self.callbacks_bound = None
        self.after_call = None

    def call(self, method, params, timeout=60):
        if method == "model/list":
            self.calls.append((method, copy.deepcopy(params)))
            if self.fail_method == method:
                raise TimeoutError("Replacement model catalog probe failed")
            result = {"data": [{"id": "gpt-5.6-sol"}]}
        elif self.replacement and method == "thread/loaded/list":
            self.calls.append((method, copy.deepcopy(params)))
            if self.fail_method == method:
                raise TimeoutError("Replacement loaded-thread probe failed")
            result = {"data": [], "nextCursor": None}
        else:
            result = super().call(method, params, timeout)
        if self.after_call:
            self.after_call(method)
        return result

    def close(self):
        assert not self.rt.lock._is_owned(), "Process close must not hold Runtime.lock"
        if not self.replacement:
            assert native_tools.account_reserved(self.rt, "default"), "Retirement must hold the account reservation"
            assert self.rt.servers["default"] is not self, "Publish a verified replacement before retirement"
            assert self.rt.connection_ids["default"] != "connection-1", "Invalidate old callbacks before retirement"
        self.close_count += 1


class Runtime(fixture.Runtime):
    connection_current = ProductionRuntime.connection_current
    notification = ProductionRuntime.notification
    request = ProductionRuntime.request
    disconnected = ProductionRuntime.disconnected

    def refresh_workspace_volatile(self):
        with self.db() as db:
            self.sync_workspace_volatile(db)

    def __init__(self, root):
        super().__init__(root)
        self.factory = Native
        self.server = Native(self)
        self.servers = {"default": self.server}
        self.offline_accounts = set()
        self.offline = False
        self._publish_desktop_resource = lambda: None
        home = self.home
        self.providers = {"default": "codex"}
        self.accounts = type("Accounts", (), {
            "get": lambda _, key: {"id": key, "provider": self.providers.get(key, "codex")},
            "home": lambda _, key, **_kwargs: home,
        })()


class Contract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-native-runtime-")
        self.rt = Runtime(Path(self.temp.name))
        self.old = self.rt.server
        self.source = self.rt.root / "new-codex"
        self.source.write_text("fixture executable")
        self.source.chmod(0o700)
        self.candidate = {"path": str(self.source), "version": "0.2.0", "identity": {"path": str(self.source), "size": 18}}
        self.selected = {"path": str(self.source), "sourcePath": str(self.source), "version": "0.2.0",
                         "sha256": "new", "bundleSha256": "new-bundle", "validatedAt": 1, "checks": {"protocol": True},
                         "sourceIdentity": copy.deepcopy(self.candidate["identity"])}
        self.approvals, self.spawned = [], []
        self.approval_error = None
        self.spawn_error = None
        self.probe_error = None
        self.on_spawn = None
        self.manager = NativeRuntimeUpdates(self.rt, discover=lambda: [self.candidate],
                                            approve=self.approve, spawn=self.spawn, interval=3600)

    def tearDown(self):
        self.manager.close()
        self.temp.cleanup()

    def approve(self, path, root):
        self.approvals.append((path, root))
        if self.approval_error:
            raise self.approval_error
        return copy.deepcopy(self.selected)

    def spawn(self, account, selected, callbacks):
        self.assertEqual(account, "default")
        self.assertTrue(native_tools.account_reserved(self.rt, account))
        if self.spawn_error:
            raise self.spawn_error
        native = Native(self.rt, replacement=True)
        native.native_binary = copy.deepcopy(selected)
        native.callbacks_bound = callbacks
        native.fail_method = self.probe_error
        self.spawned.append(native)
        if self.on_spawn:
            self.on_spawn(native)
        return native

    def change_agent(self, **changes):
        with self.rt.db() as db:
            agent = self.rt.agent("chat-1", db)
            agent.update(changes)
            self.rt.put(db, "agents", agent)

    def assert_waiting(self):
        self.manager.check()
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.rt.connection_ids["default"], "connection-1")
        self.assertIn("chat-1", self.rt.loaded)
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))
        self.assertIn("waiting", str(self.manager.status()).lower())

    @contextmanager
    def supervised(self, *, close_error=None):
        self.old.pid = 101
        state = {'signature': 'accepted-launch', 'pid': 101, 'identity_pid': 101,
                 'start_time': 'exact-start', 'generation': 1, 'closed_at': None,
                 'sequence': 0, 'acknowledged': 0}
        closes = []

        def close(root, handle, pid, started, signature, **kwargs):
            self.assertEqual((root, handle, pid, started, signature),
                             (self.rt.root, 'account:default', 101, 'exact-start', 'accepted-launch'))
            self.assertFalse(self.rt.lock._is_owned())
            self.assertTrue(native_tools.account_reserved(self.rt, 'default'))
            self.assertNotEqual(self.rt.connection_ids['default'], 'connection-1')
            closes.append(kwargs)
            state['closed_at'] = 1
            if close_error:
                raise close_error
            return {'closed': True}

        def detach():
            self.assertFalse(self.rt.lock._is_owned())
            self.old.close_count += 1

        with patch.dict(os.environ, {'CODEX_AGENTS_SUPERVISOR_MODE': '1'}), \
                patch('codex_process_supervisor.supervisor_launch_snapshot', side_effect=lambda *_: dict(state)), \
                patch('codex_process_supervisor.process_start_time',
                      side_effect=lambda _: None if state['closed_at'] else 'exact-start'), \
                patch('codex_process_supervisor.admin_close_handle', side_effect=close), \
                patch.object(self.old, 'close', side_effect=detach):
            yield state, closes

    def test_supervised_idle_rotation_preserves_history_and_reports_versions(self):
        before, history = self.rt.agent('chat-1'), self.rt.path.read_bytes()
        with self.supervised() as (_, closes):
            self.manager.check()
        self.assertEqual(len(closes), 1)
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.rt.agent('chat-1'), before)
        self.assertEqual(self.rt.path.read_bytes(), history)
        account = self.manager.status()['accounts']['default']
        self.assertEqual((account['status'], account['version'], account['targetVersion']),
                         ('current', '0.2.0', '0.2.0'))
        from studio_api.system.models import NativeAccountUpdate
        NativeAccountUpdate.model_validate(account)
        logged = json.loads(self.manager.path.with_name('rotations.jsonl').read_text())
        self.assertEqual((logged['account'], logged['sourceVersion'], logged['targetVersion']),
                         ('default', '0.1.0', '0.2.0'))

    def test_supervised_active_turn_and_unmanaged_native_work_refuse_rotation(self):
        with self.supervised() as (_, closes):
            self.change_agent(status='running', inFlight=True)
            self.assert_waiting()
            self.change_agent(status='complete', inFlight=False)
            self.old.extra['unmanaged'] = 'active'
            self.assert_waiting()
            self.assertEqual(closes, [])
            self.old.extra['unmanaged'] = 'idle'
            self.manager.check()
        self.assertEqual(len(closes), 1)

    def test_supervised_admission_is_blocked_until_the_replacement_is_published(self):
        def check_admission(_):
            with self.assertRaisesRegex(ValueError, 'update is in progress'):
                ProductionRuntime.connect(self.rt, 'default')
            with self.assertRaisesRegex(ValueError, 'Input remains queued'):
                ProductionRuntime.prepare_locked(self.rt, self.rt.agent('chat-1'))
        self.on_spawn = check_admission
        with self.supervised() as (_, closes):
            self.manager.check()
        self.assertEqual(len(closes), 1)
        self.assertFalse(native_tools.account_reserved(self.rt, 'default'))
        self.assertEqual(self.manager.status()['accounts']['default']['status'], 'current')

    def test_supervised_identity_or_activity_change_at_event_barrier_refuses(self):
        with self.supervised() as (state, closes):
            def barrier(callback):
                state['signature'] = 'different-launch'
                callback()
            self.old.after_events = barrier
            self.manager.check()
            self.assertEqual(closes, [])
            state['signature'] = 'accepted-launch'
            self.old.after_events = lambda callback: (self.change_agent(inFlight=True), callback())
            self.manager.check()
            self.assertEqual(closes, [])
        self.assertEqual(self.spawned, [])

    def test_supervised_lost_close_reply_reads_the_receipt_without_another_close(self):
        with self.supervised(close_error=TimeoutError('Reply lost')) as (_, closes):
            self.manager.check()
            self.manager.check()
        self.assertEqual(len(closes), 1)
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.manager.status()['accounts']['default']['status'], 'current')

    def test_supervised_crash_after_close_reconnects_without_repeating_the_close(self):
        class BackendCrash(BaseException):
            pass
        self.spawn_error = BackendCrash()
        with self.supervised() as (_, closes):
            with self.assertRaises(BackendCrash):
                self.manager.check()
            self.assertEqual(len(closes), 1)
            saved = json.loads(self.manager.path.read_text())
            self.assertEqual(saved['accounts']['default']['rotation']['phase'], 'closed')
            self.spawn_error = None
            self.manager = NativeRuntimeUpdates(self.rt, discover=lambda: [self.candidate],
                                                approve=self.approve, spawn=self.spawn)
            self.manager.check()
        self.assertEqual(len(closes), 1)
        self.assertEqual(len(self.spawned), 1)
        allowed = {'model/list', 'thread/loaded/list', 'thread/read',
                   'thread/queue/list', 'thread/backgroundTerminals/list'}
        self.assertTrue(all(method in allowed for native in (self.old, *self.spawned)
                            for method, _ in native.calls))

    def test_supervised_unknown_close_blocks_input_across_backend_restart(self):
        with self.supervised() as (state, closes):
            with patch('codex_process_supervisor.admin_close_handle', side_effect=TimeoutError('No receipt')):
                self.manager.check()
            self.assertEqual(closes, [])
            self.assertTrue(native_tools.account_reserved(self.rt, 'default'))
            with self.assertRaisesRegex(ValueError, 'update is in progress'):
                ProductionRuntime.connect(self.rt, 'default')
            restarted = NativeRuntimeUpdates(self.rt)
            restarted._load_selected()
            self.rt.native_runtime_updates = restarted
            with patch.object(restarted, 'maybe_check'), patch.object(restarted, 'selected', return_value=self.selected), \
                    patch('codex_native_runtime.retained_executable', return_value=None) as retain:
                with self.assertRaisesRegex(ValueError, 'close outcome is unknown'):
                    executable_for(self.rt, account_key='default', home=self.rt.home)
                retain.assert_not_called()
                state['closed_at'] = 1
                self.assertEqual(executable_for(self.rt, account_key='default', home=self.rt.home), self.selected)
            self.assertEqual(json.loads(restarted.path.read_text())['accounts']['default']['rotation']['phase'], 'closed')

    def test_supervised_unknown_close_retry_reads_only_without_repeating_the_close(self):
        with self.supervised():
            with patch('codex_process_supervisor.admin_close_handle', side_effect=TimeoutError('No receipt')) as close:
                self.manager.check()
                self.assertEqual(close.call_count, 1)
                self.manager.check()
                self.assertEqual(close.call_count, 1)
            self.assertTrue(native_tools.account_reserved(self.rt, 'default'))
            self.assertEqual(self.spawned, [])

    def test_supervised_second_native_check_refuses_work_started_after_the_barrier(self):
        def barrier(callback):
            self.old.extra['unmanaged-after-barrier'] = 'active'
            callback()
        self.old.after_events = barrier
        with self.supervised() as (_, closes):
            self.manager.check()
        self.assertEqual(closes, [])
        self.assertEqual(self.spawned, [])
        self.assertIs(self.rt.servers['default'], self.old)

    def test_supervised_named_account_spawn_uses_the_shared_supervisor_root(self):
        self.rt.commit_supervisor_event = Mock()
        self.rt.supervisor_event_applied = Mock()
        with patch('codex_runtime.AppServer') as factory:
            self.manager._spawn('named', self.selected, (Mock(), Mock(), Mock()))
        self.assertEqual(factory.call_args.args[0], self.rt.root / 'account-servers' / 'named')
        self.assertEqual(factory.call_args.kwargs['supervisor_root'], self.rt.root)
        self.assertEqual(factory.call_args.kwargs['supervisor_handle'], 'account:named')

    def test_supervised_reconnect_after_crash_reports_current_during_an_active_turn(self):
        self.spawn_error = RuntimeError('The backend stopped after close')
        with self.supervised() as (state, closes):
            self.manager.check()
            self.assertEqual(len(closes), 1)
            current = Native(self.rt, replacement=True)
            current.pid, current.generation = 202, 2
            current.native_binary = copy.deepcopy(self.selected)
            self.rt.servers['default'] = self.rt.server = current
            state.update(pid=202, identity_pid=202, generation=2, closed_at=None)
            self.rt.offline_accounts.clear()
            self.change_agent(status='running', inFlight=True)
            before = self.rt.agent('chat-1')
            self.manager.check()
        self.assertEqual(self.manager.status()['accounts']['default']['status'], 'current')
        self.assertEqual(self.rt.agent('chat-1'), before)
        self.assertEqual(current.close_count, 0)
        self.assertEqual(current.calls, [])
        self.assertEqual(len(closes), 1)

    def test_review_final_journal_fence_refuses_unread_or_unacknowledged_events(self):
        for sequence, acknowledged in ((2, 0), (2, 1)):
            with self.subTest(sequence=sequence, acknowledged=acknowledged), self.supervised() as (state, closes):
                self.old.after_events = lambda callback: (state.update(sequence=sequence, acknowledged=acknowledged), callback())
                self.manager.check()
                self.assertEqual(closes, [])
                self.assertEqual(self.spawned, [])
                self.assertEqual(self.rt.connection_ids['default'], 'connection-1')
                self.assertIs(self.rt.server, self.old)
                self.assertIn('journal', self.manager.status()['accounts']['default']['reason'])

    def test_review_close_intent_save_failure_preserves_the_old_connection(self):
        save = self.manager._save
        failed = []
        def fail_intent():
            rotation = self.manager.state['accounts'].get('default', {}).get('rotation', {})
            if rotation.get('phase') == 'closing' and not failed:
                failed.append(True)
                raise OSError('The close intent could not be saved')
            return save()
        with self.supervised() as (_, closes), patch.object(self.manager, '_save', side_effect=fail_intent):
            self.manager.check()
        self.assertEqual(failed, [True])
        self.assertEqual(self.rt.connection_ids['default'], 'connection-1')
        self.assertEqual(closes, [])
        self.assertEqual(self.spawned, [])
        self.assertFalse(native_tools.account_reserved(self.rt, 'default'))
        self.assertIs(ProductionRuntime.connect(self.rt, 'default'), self.old)

    def test_review_concurrent_save_cannot_restore_an_older_phase(self):
        entered, release, second_attempt, second_done = (threading.Event() for _ in range(4))
        errors = []
        replace = os.replace
        self.manager.state['accounts']['default'] = {'rotation': {'phase': 'closing'}}
        def delayed(source, destination):
            if Path(destination) == self.manager.path and threading.current_thread().name == 'older-save':
                entered.set()
                if not release.wait(5):
                    raise TimeoutError('The test did not release the older save')
            return replace(source, destination)
        def older():
            try:
                self.manager._save()
            except BaseException as error:
                errors.append(error)
        def newer():
            second_attempt.set()
            try:
                with self.manager.lock:
                    self.manager.state['accounts']['default']['rotation']['phase'] = 'closed'
                self.manager._save()
            except BaseException as error:
                errors.append(error)
            finally:
                second_done.set()
        with patch('codex_native_runtime.os.replace', side_effect=delayed):
            first = threading.Thread(target=older, name='older-save')
            second = threading.Thread(target=newer, name='newer-save')
            first.start()
            try:
                self.assertTrue(entered.wait(2))
                second.start()
                self.assertTrue(second_attempt.wait(2))
                second_done.wait(1)
            finally:
                release.set()
                first.join(3)
                if second.ident is not None:
                    second.join(3)
        self.assertFalse(first.is_alive())
        self.assertFalse(second.is_alive())
        self.assertEqual(errors, [])
        saved = json.loads(self.manager.path.read_text())
        self.assertEqual(saved['accounts']['default']['rotation']['phase'], 'closed')

    def test_idle_swap_preserves_chat_native_identity_history_and_receipts(self):
        before = self.rt.agent("chat-1")
        history = self.rt.path.read_bytes()
        self.manager.check()
        self.assertEqual(len(self.spawned), 1)
        self.assertIs(self.rt.servers["default"], self.spawned[0])
        self.assertIs(self.rt.server, self.spawned[0])
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(self.rt.agent("chat-1"), before)
        self.assertEqual(self.rt.path.read_bytes(), history)
        self.assertNotIn("chat-1", self.rt.loaded)
        self.assertNotEqual(self.rt.connection_ids["default"], "connection-1")
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))
        self.assertTrue(any(method == "thread/read" for method, _ in self.old.calls))
        self.assertTrue(any(method == "model/list" for method, _ in self.spawned[0].calls))
        allowed = {"model/list", "thread/read", "thread/loaded/list", "thread/queue/list", "thread/backgroundTerminals/list"}
        self.assertTrue(all(method in allowed for native in (self.old, self.spawned[0]) for method, _ in native.calls))

    def test_same_approved_binary_does_not_restart_again(self):
        self.manager.check()
        self.manager.check()
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(self.spawned[0].close_count, 0)

    def test_changed_companion_bundle_rotates_even_when_codex_hash_is_unchanged(self):
        self.old.native_binary["sha256"] = self.selected["sha256"]
        self.old.native_binary["version"] = self.selected["version"]
        self.manager.check()
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(self.rt.server.native_binary["sha256"], self.old.native_binary["sha256"])
        self.assertEqual(self.rt.server.native_binary["bundleSha256"], self.selected["bundleSha256"])

    def persist_bundle(self):
        content = {name: ("fixture " + name).encode() for name in ("codex", *REQUIRED_COMPANIONS)}
        hashes = {name: hashlib.sha256(data).hexdigest() for name, data in content.items()}
        bundle_hash = bundle_digest(hashes)
        directory = self.manager.path.parent / "builds" / bundle_hash
        directory.mkdir(parents=True)
        for name, data in content.items():
            path = directory / name
            path.write_bytes(data)
            path.chmod(0o700)
        info = {**self.selected, "path": str(directory / "codex"), "sha256": hashes["codex"],
                "bundleSha256": bundle_hash, "approvalRevision": APPROVAL_REVISION,
                "companions": {name: {"path": str(directory / name), "sha256": hashes[name]}
                               for name in REQUIRED_COMPANIONS}}
        self.manager.path.write_text(json.dumps({"selected": info, "checkedAt": 1}))
        return info

    def test_persisted_bundle_loads_only_after_main_and_companion_hash_checks(self):
        info = self.persist_bundle()
        self.manager._load_selected()
        self.assertEqual(self.manager.selected(), info)
        self.assertTrue(self.manager.ready.is_set())

    def test_failed_persisted_null_selection_recovers_from_valid_discovery(self):
        self.manager.path.parent.mkdir(parents=True)
        self.manager.path.write_text(json.dumps({"status": "failed", "selected": None,
                                                "error": "No installed version passed the checks"}))
        self.manager.check()
        self.assertEqual(self.manager.selected(), self.selected)
        self.assertEqual(self.manager.status()["status"], "ready")
        self.assertEqual(len(self.approvals), 1)
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(json.loads(self.manager.path.read_text())["selected"], self.selected)

    def test_persisted_bundle_with_missing_code_mode_host_is_rejected(self):
        info = self.persist_bundle()
        Path(info["companions"]["codex-code-mode-host"]["path"]).unlink()
        self.manager._load_selected()
        self.assertIsNone(self.manager.selected())
        self.assertFalse(self.manager.ready.is_set())

    def test_persisted_bundle_with_corrupted_code_mode_host_is_rejected(self):
        info = self.persist_bundle()
        Path(info["companions"]["codex-code-mode-host"]["path"]).write_bytes(b"modified host")
        self.manager._load_selected()
        self.assertIsNone(self.manager.selected())
        self.assertFalse(self.manager.ready.is_set())

    def test_active_turn_waits_without_native_calls(self):
        self.change_agent(status="running", inFlight=True)
        self.assert_waiting()
        self.assertEqual(self.old.calls, [])
        self.assertEqual(self.spawned, [])

    def test_pending_approval_waits(self):
        with self.rt.db() as db:
            self.rt.put(db, "requests", {"id": "approval-1", "agent": "chat-1", "status": "pending", "method": "item/commandExecution/requestApproval"})
        self.assert_waiting()

    def test_active_monitor_waits(self):
        with self.rt.db() as db:
            self.rt.put(db, "monitors", {"id": "monitor-1", "agent": "chat-1", "status": "running"})
        self.assert_waiting()

    def test_unknown_pending_tool_receipt_waits(self):
        with self.rt.db() as db:
            self.rt.put(db, "tool_requests", {"id": "tool-1", "agent": "chat-1", "stage": "running", "outcome": "unknown"})
        self.assert_waiting()

    def test_native_request_receipt_waits(self):
        self.old.pending[17] = object()
        self.assert_waiting()

    def test_native_callback_waits(self):
        self.old.callbacks.put(object())
        self.assert_waiting()

    def test_clock_reply_waits(self):
        self.old.clock_replies.put(object())
        self.assert_waiting()

    def test_dispatch_receipt_waits(self):
        with self.rt.db() as db:
            db.execute("INSERT INTO runtime_events VALUES ('input-1','chat-1','dispatching')")
        self.assert_waiting()

    def test_native_child_outside_studio_waits(self):
        self.old.extra["unmanaged-child"] = "active"
        self.assert_waiting()
        self.assertTrue(any(params.get("threadId") == "unmanaged-child" for _, params in self.old.calls))

    def test_native_queue_waits(self):
        self.old.native_queue = [{"id": "queued-input"}]
        self.assert_waiting()

    def test_native_background_command_waits(self):
        self.old.background = [{"id": "background-command"}]
        self.assert_waiting()

    def test_unknown_native_state_waits(self):
        self.old.fail_method = "thread/backgroundTerminals/list"
        self.assert_waiting()

    def test_idle_account_rotates_after_busy_work_finishes(self):
        self.change_agent(status="running", inFlight=True)
        self.assert_waiting()
        self.change_agent(status="complete", inFlight=False)
        self.manager.check()
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(len(self.spawned), 1)

    def test_candidate_validation_failure_preserves_current_process(self):
        self.approval_error = ValueError("Required native method is missing")
        self.manager.check()
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned, [])
        self.assertIn("missing", str(self.manager.status()))

    def test_transient_validation_failure_retries_unchanged_candidate_after_one_minute(self):
        self.approval_error = TimeoutError("Validation timed out")
        with patch("codex_native_runtime.time.monotonic", return_value=100) as clock:
            self.manager.check()
            self.assertEqual(len(self.approvals), 1)
            self.assertIs(self.rt.servers["default"], self.old)
            self.approval_error = None
            clock.return_value = 159
            self.manager.check()
            self.assertEqual(len(self.approvals), 1)
            self.assertEqual(self.spawned, [])
            clock.return_value = 161
            self.manager.check()
        self.assertEqual(len(self.approvals), 2)
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(self.manager.status()["accounts"]["default"]["status"], "current")

    def test_replacement_start_failure_preserves_current_process(self):
        self.spawn_error = RuntimeError("Replacement failed to initialize")
        self.manager.check()
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.rt.connection_ids["default"], "connection-1")
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))

    def test_replacement_probe_failure_closes_only_replacement(self):
        self.probe_error = "model/list"
        self.manager.check()
        self.assertEqual(len(self.spawned), 1)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertEqual(self.old.close_count, 0)
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.rt.connection_ids["default"], "connection-1")
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))

    def test_retirement_failure_keeps_account_reserved_until_old_process_exits(self):
        self.old.refuse_exit = True
        self.manager.check()
        self.assertEqual(self.old.close_count, 1)
        self.assertEqual(len(self.spawned), 1)
        self.assertTrue(native_tools.account_reserved(self.rt, "default"))
        self.assertIn("failed", str(self.manager.status()).lower())
        self.manager.check()
        self.assertTrue(native_tools.account_reserved(self.rt, "default"))
        self.assertIn(self.manager.status()["accounts"]["default"]["status"], {"waiting", "failed"})
        self.assertEqual(len(self.spawned), 1)
        self.old.refuse_exit = False
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))

    def test_failed_replacement_that_will_not_exit_keeps_account_reserved(self):
        self.probe_error = "model/list"
        self.on_spawn = lambda native: setattr(native, "refuse_exit", True)
        self.manager.check()
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertTrue(native_tools.account_reserved(self.rt, "default"))
        self.spawned[0].refuse_exit = False
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))

    def test_agent_identity_change_during_preflight_cancels_swap(self):
        self.on_spawn = lambda _: self.change_agent(threadId="different-native-thread", epoch=4)
        self.manager.check()
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.rt.agent("chat-1")["threadId"], "different-native-thread")

    def test_new_work_during_preflight_cancels_swap(self):
        self.on_spawn = lambda _: self.change_agent(status="running", inFlight=True)
        self.manager.check()
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertIs(self.rt.servers["default"], self.old)

    def test_account_disconnect_during_preflight_cancels_swap(self):
        before = self.rt.agent("chat-1")
        self.on_spawn = lambda _: self.rt.offline_accounts.add("default")
        self.manager.check()
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.rt.connection_ids["default"], "connection-1")
        self.assertEqual(self.rt.agent("chat-1"), before)
        self.assertIn("default", self.rt.offline_accounts)
        self.assertIn(self.manager.status()["accounts"]["default"]["status"], {"waiting", "failed"})

    def test_connect_selects_approved_executable_when_account_disconnects_before_start_lock(self):
        old = Mock()
        self.rt.server = self.rt.servers["default"] = old
        replacement = Native(self.rt, replacement=True)
        factory = Mock(return_value=replacement)
        self.rt.factory = factory

        runtime = self.rt

        class DisconnectBeforeAcquire:
            held = False
            entries = 0

            def __enter__(self):
                self.entries += 1
                runtime.offline_accounts.add("default")
                self.held = True

            def __exit__(self, *_):
                self.held = False

        self.rt.start_lock = DisconnectBeforeAcquire()

        def select(runtime, *, account_key, home):
            self.assertIs(runtime, self.rt)
            self.assertIn("default", runtime.offline_accounts)
            self.assertFalse(runtime.start_lock.held, "Resolve executable outside the connection lock")
            self.assertEqual(account_key, "default")
            self.assertEqual(home, runtime.home)
            return copy.deepcopy(self.selected)

        with patch("codex_runtime.AppServer", factory), patch("codex_native_runtime.executable_for", side_effect=select) as choose:
            result = ProductionRuntime.connect(self.rt, "default")
        choose.assert_called_once_with(self.rt, account_key="default", home=self.rt.home)
        old.close.assert_called_once_with()
        self.assertIs(result, replacement)
        self.assertIs(self.rt.servers["default"], replacement)
        self.assertEqual(factory.call_args.kwargs["executable"], self.selected["path"])
        self.assertEqual(replacement.native_binary, self.selected)
        self.assertNotIn("default", self.rt.offline_accounts)
        self.assertEqual(self.rt.start_lock.entries, 2)

    def test_healthy_existing_connection_does_not_require_available_validation(self):
        factory = Mock()
        self.rt.factory = factory
        with patch("codex_runtime.AppServer", factory), patch("codex_native_runtime.executable_for", side_effect=RuntimeError("No candidate passed validation")) as choose:
            result = ProductionRuntime.connect(self.rt, "default")
        self.assertIs(result, self.old)
        choose.assert_not_called()
        factory.assert_not_called()
        self.assertEqual(self.old.close_count, 0)

    def test_busy_connection_lock_does_not_block_publication_or_retire_old_process(self):
        occupied, completed = threading.Event(), threading.Event()
        errors = []

        def occupy(_):
            self.rt.start_lock.acquire()
            occupied.set()

        def check():
            try:
                self.manager.check()
            except BaseException as error:
                errors.append(error)
            finally:
                completed.set()

        self.on_spawn = occupy
        worker = threading.Thread(target=check, daemon=True)
        worker.start()
        try:
            self.assertTrue(occupied.wait(2))
            self.assertTrue(completed.wait(1), "Publication blocked on an occupied connection lock")
        finally:
            if self.rt.start_lock.locked():
                self.rt.start_lock.release()
            worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned[0].close_count, 1)
        self.assertFalse(native_tools.account_reserved(self.rt, "default"))
        self.assertIn(self.manager.status()["accounts"]["default"]["status"], {"waiting", "failed"})

    def test_claude_process_is_not_rotated(self):
        self.rt.providers["default"] = "claude"
        self.change_agent(provider="claude")
        self.manager.check()
        self.assertIs(self.rt.servers["default"], self.old)
        self.assertEqual(self.old.calls, [])
        self.assertEqual(self.old.close_count, 0)
        self.assertEqual(self.spawned, [])

    def test_old_connection_callbacks_cannot_affect_replacement(self):
        self.manager.check()
        before = self.rt.agent("chat-1")
        self.rt.notification({"method": "thread/started", "params": {}}, "default", "connection-1")
        with self.assertRaisesRegex(ConnectionError, "not admitted"):
            self.rt.request({"id": 9, "method": "currentTime/read", "params": {}}, "default", "connection-1")
        self.rt.disconnected("default", "connection-1")
        self.assertEqual(self.rt.agent("chat-1"), before)
        self.assertEqual(self.rt.offline_accounts, set())
        self.assertIs(self.rt.servers["default"], self.spawned[0])

    def test_close_waits_for_validation_worker_and_prevents_rotation(self):
        entered, release, closed = threading.Event(), threading.Event(), threading.Event()
        approve = self.manager.approve

        def blocked_approve(path, root):
            entered.set()
            if not release.wait(3):
                raise TimeoutError("Fixture validation was not released")
            return approve(path, root)

        self.manager.approve = blocked_approve
        self.manager.maybe_check()
        self.assertTrue(entered.wait(2))
        closer = threading.Thread(target=lambda: (self.manager.close(), closed.set()))
        closer.start()
        try:
            self.assertFalse(closed.wait(.05), "close returned while validation still held runtime state")
        finally:
            release.set()
            closer.join(3)
        self.assertTrue(closed.is_set())
        self.assertFalse(self.manager.worker.is_alive())
        self.assertEqual(self.spawned, [])
        self.assertEqual(self.old.close_count, 0)


if __name__ == "__main__":
    unittest.main()
