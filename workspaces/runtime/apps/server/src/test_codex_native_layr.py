"""Retry boundaries for the native Layr attachment client."""
from pathlib import Path
import json
import tempfile
import unittest
from typing import Any, Iterator
from unittest.mock import patch

from codex_native_layr import Client


class NativeLayrTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.source = self.root / "source"
        self.source.mkdir()
        self.client = Client(self.root / "state", self.root / "store", Path("/bin/echo"))

    def invoke(self, *args: str, **kwargs: Any) -> str:
        if args[:2] == ("workspace", "attach"):
            return json.dumps({"project": args[2], "store": str(self.client.store),
                               "line": "main", "binding_id": "binding"})
        return ""

    def test_retry_does_not_import_again(self) -> None:
        with patch.object(self.client, "command", side_effect=self.invoke) as command:
            first = self.client.ensure_project("project", self.source)
            again = self.client.ensure_project("project", self.source)
        self.assertEqual(first, again)
        self.assertEqual(sum(c.args[0] == "init" for c in command.call_args_list), 1)
        self.assertEqual(first["cwd"], str(self.source))

    def test_unknown_import_does_not_repeat(self) -> None:
        with patch.object(self.client, "command", side_effect=TimeoutError("lost response")) as command:
            with self.assertRaises(TimeoutError):
                self.client.ensure_project("project", self.source)
            with self.assertRaisesRegex(RuntimeError, "outcome is unknown"):
                self.client.ensure_project("project", self.source)
        self.assertEqual(command.call_count, 1)

    def test_changed_source_is_not_adopted(self) -> None:
        with patch.object(self.client, "command", side_effect=self.invoke) as command:
            self.client.ensure_project("project", self.source)
            other = self.root / "other"
            other.mkdir()
            count = command.call_count
            with self.assertRaisesRegex(ValueError, "different native"):
                self.client.ensure_project("project", other)
            self.assertEqual(command.call_count, count)

    def test_marker_never_selects_a_store(self) -> None:
        (self.source / ".layr").mkdir()
        with patch.object(self.client, "command") as command:
            with self.assertRaisesRegex(ValueError, "already has"):
                self.client.ensure_project("project", self.source)
            command.assert_not_called()

    def test_worker_retry_preserves_later_work(self) -> None:
        project = "studio-" + "a" * 64
        def command(*args: str, **kwargs: Any) -> str:
            if "rev-parse" in args:
                return "exact-state"
            if args[:2] == ("workspace", "inspect"):
                return json.dumps({"line_id": "worker-id", "project_id": "project-id",
                                   "state": "exact-state", "path": str(self.client.store / "projects" / project / "lines" / args[-1])})
            if "--path" in args:
                line = args[-2]
                path = self.client.store / "projects" / project / "lines" / line
                path.mkdir(parents=True)
                (path / "work").write_text("later changes")
                return str(path)
            return str(kwargs["cwd"].name)
        with patch.object(self.client, "command", side_effect=command) as invoke:
            first = self.client.ensure_worker(project, "worker", self.source, "exact-state")
            again = self.client.ensure_worker(project, "worker", self.source, "exact-state")
        self.assertEqual(first, again)
        self.assertEqual((Path(first["cwd"]) / "work").read_text(), "later changes")
        self.assertEqual(sum("--path" in c.args for c in invoke.call_args_list), 1)

    def test_unknown_worker_creation_does_not_repeat(self) -> None:
        def command(*args: str, **kwargs: Any) -> str:
            if "rev-parse" in args:
                return "exact-state"
            raise TimeoutError("lost response")
        with patch.object(self.client, "command", side_effect=command) as invoke:
            with self.assertRaises(TimeoutError):
                self.client.ensure_worker("studio-" + "a" * 64, "worker", self.source, "exact-state")
            with self.assertRaisesRegex(RuntimeError, "outcome is unknown"):
                self.client.ensure_worker("studio-" + "a" * 64, "worker", self.source, "exact-state")
        self.assertEqual(sum("--path" in c.args for c in invoke.call_args_list), 1)

    def test_turn_save_reuses_its_exact_result(self) -> None:
        with patch.object(self.client, "command", return_value="main saved-state") as invoke:
            first = self.client.save_turn("studio-" + "a" * 64, "main", self.source, "agent", "turn")
            second = self.client.save_turn("studio-" + "a" * 64, "main", self.source, "agent", "turn")
        self.assertEqual(first, "saved-state")
        self.assertEqual(first, second)
        self.assertEqual(invoke.call_count, 1)

    def test_unknown_turn_save_never_repeats_without_evidence(self) -> None:
        with patch.object(self.client, "command", side_effect=[TimeoutError("lost"), ""]) as invoke:
            with self.assertRaises(TimeoutError):
                self.client.save_turn("studio-" + "a" * 64, "main", self.source, "agent", "turn")
            with self.assertRaisesRegex(RuntimeError, "outcome is unknown"):
                self.client.save_turn("studio-" + "a" * 64, "main", self.source, "agent", "turn")
        self.assertEqual(sum("save" in c.args for c in invoke.call_args_list), 1)

    def test_unknown_merge_does_not_repeat(self) -> None:
        info = json.dumps({"line_id": "line-id", "path": str(self.source)})
        with patch.object(self.client, "command", side_effect=[info, "state", TimeoutError("lost"), info, ""]) as invoke:
            with self.assertRaises(TimeoutError):
                self.client.merge_result("studio-" + "a" * 64, "main", self.source, "state", "result", "line-id")
            with self.assertRaisesRegex(RuntimeError, "outcome is unknown"):
                self.client.merge_result("studio-" + "a" * 64, "main", self.source, "state", "result", "line-id")
        self.assertEqual(sum("merge" in c.args for c in invoke.call_args_list), 1)

    def test_merge_rejects_changed_destination_before_write(self) -> None:
        info = json.dumps({"line_id": "replacement", "path": str(self.source)})
        with patch.object(self.client, "command", return_value=info) as invoke:
            with self.assertRaisesRegex(ValueError, "destination changed"):
                self.client.merge_result("studio-" + "a" * 64, "main", self.source, "state", "result", "line-id")
        self.assertEqual(invoke.call_count, 1)

    def test_merge_conflicts_do_not_become_acceptance(self) -> None:
        import hashlib
        project = "studio-" + "a" * 64
        token = hashlib.sha256(b"result").hexdigest()
        with self.client.project_lock(project) as attachment:
            self.client.write(attachment.parent / ("accept-" + token + ".json"),
                {"project": project, "line": "main", "lineId": "line-id", "revision": "state",
                 "result": "result", "phase": "merging"})
        info = json.dumps({"line_id": "line-id", "path": str(self.source)})
        row = json.dumps({"op": "merge", "actor": {"session": "studio-accept-" + token},
                          "data": {"conflicts": 1}})
        with patch.object(self.client, "command", side_effect=[info, row]) as invoke:
            with self.assertRaisesRegex(RuntimeError, "has conflicts"):
                self.client.merge_result(project, "main", self.source, "state", "result", "line-id")
        self.assertFalse(any("merge" in c.args for c in invoke.call_args_list))


if __name__ == "__main__":
    unittest.main()
