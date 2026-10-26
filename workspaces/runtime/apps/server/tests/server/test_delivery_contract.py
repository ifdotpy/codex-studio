"""Fast delivery timing witnesses using disposable SQLite and fake native input."""
from __future__ import annotations

import base64
import importlib.util
import json
import re
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

TESTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TESTS.parent / "src"))
sys.path.insert(0, str(TESTS))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
from codex_delivery import DeliveryMoment, event_delivery, normalize_delivery
from codex_layout import REPOSITORY_ROOT
from codex_shared_values import MAX_TURN_ATTACHMENTS
from codex_work_limits import TASK_SUBMISSION_MAX_FILES, TASK_SUBMISSION_PATH_MAX_CODEPOINTS
from codex_runtime import TOOLS
from codex_wakeups import pending_batch
from generate_delivery_tools import BUNDLED_RESOURCES, DELIVERY_TOOL_NAMES, stale_delivery_tools
from studio_api.work.models import ManagedMessageBody, QueueItem

SPEC = importlib.util.spec_from_file_location("delivery_workspace_fixture", TESTS / "workspace-contract.py")
f = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(f)


class DeliveryContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    start = f.WorkspaceContract.start

    def test_task_submission_limits_match_rust_owner_and_tool_schema(self):
        owner = REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/shared_values.rs"
        for name, value in (
            ("TASK_SUBMISSION_MAX_FILES", TASK_SUBMISSION_MAX_FILES),
            ("TASK_SUBMISSION_PATH_MAX_CODEPOINTS", TASK_SUBMISSION_PATH_MAX_CODEPOINTS),
        ):
            self.assertIn(f"pub const {name}: usize = {value};", owner.read_text())
            self.assertIn(name, (REPOSITORY_ROOT / "docs/orchestration.md").read_text())
        task = next(tool for tool in TOOLS if tool["name"] == "orchestration_task")
        files = task["inputSchema"]["properties"]["files"]
        self.assertEqual(files["maxItems"], TASK_SUBMISSION_MAX_FILES)
        self.assertEqual(files["items"]["maxLength"], TASK_SUBMISSION_PATH_MAX_CODEPOINTS)
        self.assertEqual(stale_delivery_tools(json.loads(BUNDLED_RESOURCES.read_text())), [])

    def test_task_submission_manifest_boundary_and_path_limit(self):
        lead = self.lead()
        for count in (51, 73, TASK_SUBMISSION_MAX_FILES):
            with self.subTest(count=count):
                task = self.runtime.work_action(lead["id"], {"action": "create", "title": "manifest"})
                files = [f"src/file-{index}.rs" for index in range(count)]
                submitted = self.runtime.work_action(lead["id"], {
                    "action": "submit", "task_id": task["id"], "result": "done",
                    "checks": "fixture", "revision": "fixture", "files": files,
                })
                self.assertEqual(submitted["results"][0]["files"], files)
        task = self.runtime.work_action(lead["id"], {"action": "create", "title": "refuse"})
        for files in (["src/file.rs"] * (TASK_SUBMISSION_MAX_FILES + 1),
                      ["🦀" * (TASK_SUBMISSION_PATH_MAX_CODEPOINTS + 1)]):
            with self.assertRaisesRegex(ValueError, "Submission files exceed"):
                self.runtime.work_action(lead["id"], {
                    "action": "submit", "task_id": task["id"], "result": "done",
                    "checks": "fixture", "revision": "fixture", "files": files,
                })

    def calls(self):
        return [params for method, params in self.runtime.server.calls if method == "turn/start"]

    def test_rust_bundled_current_sender_tools_match_the_python_owner(self):
        resources = json.loads(BUNDLED_RESOURCES.read_text())
        self.assertEqual(stale_delivery_tools(resources), [])
        for tool in resources["tools"]:
            if tool["name"] in DELIVERY_TOOL_NAMES:
                self.assertEqual(tool["inputSchema"]["properties"]["delivery"]["enum"], list(DeliveryMoment))
                self.assertFalse(tool["inputSchema"]["additionalProperties"])
                self.assertNotIn("delivery", tool["inputSchema"]["required"])
        drifted = json.loads(json.dumps(resources))
        for tool in drifted["tools"]:
            if tool["name"] in DELIVERY_TOOL_NAMES:
                tool["inputSchema"]["properties"].pop("delivery")
        self.assertEqual(stale_delivery_tools(drifted), list(DELIVERY_TOOL_NAMES))

    def test_request_boundary_normalizes_legacy_and_tools_advertise_canonical_only(self):
        for name in ("orchestration_message", "orchestration_send"):
            definition = next(tool for tool in TOOLS if tool["name"] == name)
            self.assertEqual(definition["inputSchema"]["properties"]["delivery"]["enum"], list(DeliveryMoment))
        for legacy, expected in (("queue", DeliveryMoment.CURRENT_TURN), ("steer", DeliveryMoment.CURRENT_TURN),
                                 ("after_tool", DeliveryMoment.CURRENT_TURN), ("after_turn", DeliveryMoment.NEXT_TURN)):
            body = ManagedMessageBody(id="00000000-0000-0000-0000-000000000001", room="agent", text="Hello", delivery=legacy)
            self.assertIs(body.delivery, expected)
            self.assertEqual(body.model_dump(mode="json")["delivery"], expected)
        for invalid in (None, 1, False, [], "later"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                ManagedMessageBody(id="00000000-0000-0000-0000-000000000001", room="agent", text="Hello", delivery=invalid)
        queue = QueueItem.model_validate({"id": "input", "text": "Hello", "kind": "user", "status": "pending", "created": 1,
                                         "delivery": "current_turn", "requestedDelivery": "next_turn"})
        self.assertIs(queue.delivery, DeliveryMoment.CURRENT_TURN)
        self.assertIs(queue.requestedDelivery, DeliveryMoment.NEXT_TURN)

    def test_persisted_metadata_defaults_and_invalid_values_match_shared_owner(self):
        event = {"id": "metadata-boundary", "kind": "agent_message", "text": '{"importance":"progress"}'}
        with self.runtime.db() as db:
            for metadata in (None, {}, {"delivery": None}):
                db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)", (event["id"], json.dumps(metadata)))
                self.assertIs(event_delivery(db, event), DeliveryMoment.NEXT_TURN)
            for metadata in ([], 1, "bad", {"delivery": False}, {"delivery": "later"}):
                db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)", (event["id"], json.dumps(metadata)))
                with self.subTest(metadata=metadata), self.assertRaises(ValueError):
                    event_delivery(db, event)

    def test_next_agent_notifications_wait_then_share_one_batch(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        for key in ("next-one", "next-two"):
            self.runtime.chat_message(worker["id"], lead["id"], key, key, delivery=DeliveryMoment.NEXT_TURN)
        before = len(self.calls())
        self.runtime.dispatch()
        self.assertEqual(len(self.calls()), before)
        current = self.runtime.chat_message(worker["id"], lead["id"], "Act now", "current", delivery=DeliveryMoment.CURRENT_TURN)
        self.assertEqual(self.runtime.chat_message(worker["id"], lead["id"], "Act now", "current"), current)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.chat_message(worker["id"], lead["id"], "Act now", "current", delivery=DeliveryMoment.NEXT_TURN)
        self.runtime.dispatch()
        current_id = "chat:current:" + lead["id"]
        f.eventually(lambda: self.runtime.delivery_receipt(current_id)["status"] == "delivered")
        self.assertEqual(self.runtime.agent(lead["id"])["turnId"], lead["turnId"])
        self.assertNotIn("next-one", self.calls()[-1]["input"][0]["text"])
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        f.eventually(lambda: not self.runtime.agent(lead["id"]).get("inFlight"))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt("chat:next-two:" + lead["id"])["status"] == "delivered")
        text = self.calls()[-1]["input"][0]["text"]
        self.assertIn("next-one", text)
        self.assertIn("next-two", text)
        self.assertEqual(len(self.calls()), before + 2)
        with self.runtime.read_db() as db:
            stored = [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_items WHERE agent=?", (lead["id"],))]
        expected = ["chat:" + key + ":" + lead["id"] for key in ("next-one", "next-two")]
        inputs = [entry for item in stored for entry in item.get("inputs", []) if entry.get("id") in expected]
        self.assertEqual([entry["id"] for entry in inputs], expected)
        self.assertTrue(all(entry["kind"] == "agent_message" for entry in inputs))

    def test_next_user_inputs_drain_past_historical_count_limit(self):
        lead = self.start(self.lead())
        for index in range(40):
            self.runtime.send(lead["id"], f"Compatible input {index}", f"batch-{index}", delivery=DeliveryMoment.NEXT_TURN)
        before = len(self.calls())
        self.runtime.dispatch()
        self.assertEqual(len(self.calls()), before)
        self.runtime.send(lead["id"], "Current after older next inputs", "current-after-next", delivery=DeliveryMoment.CURRENT_TURN)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt("current-after-next")["status"] == "delivered")
        self.assertEqual(self.runtime.agent(lead["id"])["turnId"], lead["turnId"])
        self.assertEqual(len(self.calls()), before + 1)
        self.assertTrue(all(self.runtime.delivery_receipt(f"batch-{index}")["status"] == "pending" for index in range(40)))
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        f.eventually(lambda: not self.runtime.agent(lead["id"]).get("inFlight"))
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(f"batch-{index}")["status"] == "delivered" for index in range(40)))
        self.assertEqual(len(self.calls()), before + 2)
        text = self.calls()[-1]["input"][0]["text"]
        self.assertIn("Compatible input 0", text)
        self.assertIn("Compatible input 39", text)
        with self.runtime.read_db() as db:
            stored = [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_items WHERE agent=?", (lead["id"],))]
        inputs = [entry for item in stored for entry in item.get("inputs", []) if entry.get("id", "").startswith("batch-")]
        self.assertEqual([entry["id"] for entry in inputs], [f"batch-{index}" for index in range(40)])
        self.assertTrue(all(entry["kind"] == "user" for entry in inputs))

    def test_idle_next_user_inputs_drain_in_one_batch(self):
        lead = self.lead()
        for index in range(40):
            self.runtime.send(lead["id"], f"Idle input {index}", f"idle-{index}", delivery=DeliveryMoment.NEXT_TURN)
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(f"idle-{index}")["status"] == "delivered" for index in range(40)))
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.calls()[0]["clientUserMessageId"], "idle-0")
        with self.runtime.read_db() as db:
            stored = [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_items WHERE agent=?", (lead["id"],))]
        inputs = [entry for item in stored for entry in item.get("inputs", []) if entry.get("id", "").startswith("idle-")]
        self.assertEqual([entry["id"] for entry in inputs], [f"idle-{index}" for index in range(40)])
        self.assertTrue(all(entry["kind"] == "user" for entry in inputs))

    def test_attachment_budget_matches_rust_owner_and_generated_web_contract(self):
        owner = (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/shared_values.rs").read_text()
        declared = re.search(r"pub const MAX_TURN_ATTACHMENTS: usize = (\d+);", owner)
        self.assertIsNotNone(declared, "runtime-schema shared_values.rs owns attachment count per turn")
        self.assertEqual(MAX_TURN_ATTACHMENTS, int(declared[1]))
        generated = (REPOSITORY_ROOT / "workspaces/client/apps/web/src/generated/sharedValues.ts").read_text()
        self.assertIn(f"export const MAX_TURN_ATTACHMENTS = {MAX_TURN_ATTACHMENTS};", generated)
        for consumer in ("workspaces/runtime/apps/server/src/codex_runtime.py", "workspaces/runtime/apps/server/src/codex_workspace.py",
                         "workspaces/client/apps/web/src/components/Conversation.tsx", "workspaces/client/apps/web/src/components/ComposerAttachments.tsx"):
            with self.subTest(consumer=consumer):
                self.assertIn("MAX_TURN_ATTACHMENTS", (REPOSITORY_ROOT / consumer).read_text())

    def test_single_input_over_attachment_budget_is_rejected_before_queueing(self):
        lead = self.lead()
        assets = [f"asset-{index}" for index in range(MAX_TURN_ATTACHMENTS + 1)]
        with self.assertRaisesRegex(ValueError, "Attach up to"):
            self.runtime.send(lead["id"], "Impossible input", "over-budget", assets=assets)
        self.assertIsNone(self.runtime.delivery_receipt("over-budget"))
        with self.assertRaises(ValueError):
            ManagedMessageBody(id="00000000-0000-0000-0000-000000000001", room=lead["id"], text="Impossible input", assets=assets)

    def test_compatible_batch_still_splits_at_attachment_admission_limit(self):
        lead = self.lead()
        for batch in range(2):
            assets = [self.runtime.upload_asset({"agent": lead["id"], "name": f"document-{batch}-{index}.txt",
                "base64": base64.b64encode(b"Small fixture attachment").decode()})["id"] for index in range(5)]
            self.runtime.send(lead["id"], f"Attachment batch {batch}", f"assets-{batch}", assets=assets, delivery=DeliveryMoment.NEXT_TURN)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt("assets-0")["status"] == "delivered")
        self.assertEqual(self.runtime.delivery_receipt("assets-1")["status"], "pending")
        self.assertEqual(len(self.calls()), 1)
        current = self.runtime.agent(lead["id"])
        self.runtime.server.complete(current["threadId"], current["turnId"])
        f.eventually(lambda: not self.runtime.agent(lead["id"]).get("inFlight"))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt("assets-1")["status"] == "delivered")
        self.assertEqual(len(self.calls()), 2)

    def test_progress_defaults_next_and_explicit_current_is_not_coalesced_away(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        self.runtime.chat_message(worker["id"], lead["id"], "Current progress", "current-progress",
            importance="progress", progress_key="task", progress_version=1, delivery=DeliveryMoment.CURRENT_TURN)
        self.runtime.chat_message(worker["id"], lead["id"], "Later progress", "later-progress",
            importance="progress", progress_key="task", progress_version=2)
        with self.runtime.db() as db:
            rows = pending_batch(self.runtime, db, self.runtime.agent(lead["id"], db))
            self.assertEqual(len([row for row in rows if row["kind"] == "agent_message"]), 2)
            for key, moment in (("current-progress", DeliveryMoment.CURRENT_TURN), ("later-progress", DeliveryMoment.NEXT_TURN)):
                metadata = json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", ("chat:" + key + ":" + lead["id"],)).fetchone()[0])
                self.assertIs(normalize_delivery(metadata["delivery"]), moment)
        with patch.object(self.runtime, "progress_batch_ready", return_value=True):
            self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt("chat:current-progress:" + lead["id"])["status"] == "delivered")
        self.assertEqual(self.runtime.delivery_receipt("chat:later-progress:" + lead["id"])["status"], "pending")

    def test_send_now_keeps_original_request_identity_for_replay(self):
        lead = self.lead()
        original = self.runtime.send(lead["id"], "Later", "stable-input", delivery=DeliveryMoment.NEXT_TURN)
        queue = self.runtime.queue_action(lead["id"])
        self.runtime.queue_action(lead["id"], {"action": "send_now", "message_id": "stable-input",
            "request_id": "send-now", "expected_revision": queue["revision"]})
        row = self.runtime.queue_action(lead["id"])["items"][0]
        self.assertEqual(row["delivery"], DeliveryMoment.CURRENT_TURN)
        self.assertEqual(row["requestedDelivery"], DeliveryMoment.NEXT_TURN)
        with self.runtime.lock, self.runtime.db() as db:
            before = db.execute("SELECT record FROM runtime_event_meta WHERE id='stable-input'").fetchone()[0]
            self.runtime.enqueue(db, self.runtime.agent(lead["id"], db), "user", "Later", "stable-input", delivery=DeliveryMoment.NEXT_TURN)
            self.assertEqual(db.execute("SELECT record FROM runtime_event_meta WHERE id='stable-input'").fetchone()[0], before)
        replay = self.runtime.send(lead["id"], "Later", "stable-input", delivery="after_turn")
        self.assertEqual(replay["id"], original["id"])
        self.assertEqual(replay["status"], "pending")
        self.assertEqual(len(self.runtime.queue_action(lead["id"])["items"]), 1)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.send(lead["id"], "Later", "stable-input", delivery=DeliveryMoment.CURRENT_TURN)

    def test_legacy_promotion_only_backfills_original_timing_when_known(self):
        lead = self.lead()
        for promoted in (False, True):
            key = "legacy-promoted-" + str(promoted)
            self.runtime.send(lead["id"], "Legacy input", key, delivery=DeliveryMoment.NEXT_TURN)
            with self.runtime.db() as db:
                metadata = json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()[0])
                metadata.pop("requestedDelivery")
                metadata["delivery"] = "steer" if promoted else "after_turn"
                if promoted:
                    metadata["sendNow"] = True
                db.execute("UPDATE runtime_event_meta SET record=? WHERE id=?", (json.dumps(metadata), key))
            queue = self.runtime.queue_action(lead["id"])
            self.runtime.queue_action(lead["id"], {"action": "send_now", "message_id": key,
                "request_id": "promote-" + key, "expected_revision": queue["revision"]})
            with self.runtime.db() as db:
                metadata = json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()[0])
            if promoted:
                self.assertNotIn("requestedDelivery", metadata)
            else:
                self.assertEqual(metadata["requestedDelivery"], DeliveryMoment.NEXT_TURN)
            replay = self.runtime.send(lead["id"], "Legacy input", key, delivery=DeliveryMoment.NEXT_TURN)
            self.assertEqual(replay["id"], key)
            self.assertEqual(replay["status"], "pending")


if __name__ == "__main__":
    unittest.main()
