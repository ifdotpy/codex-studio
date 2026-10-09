"""Durable field merge and validation, with real entity storage."""
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path
from pydantic import ValidationError
from codex_sync_entities import ensure_tables
from studio_api.models import JsonValue
from studio_api.sync.preferences import PreferenceField, PreferencePushRequest, UiPreferencesDto, merge_preferences, read_preferences, bootstrap_preferences


class PreferencesTests(unittest.TestCase):
    def field(self, attr: str, value: JsonValue, timestamp: int = 1, writer: str = "a") -> dict[str, PreferenceField]:
        return {json.dumps(["codex-studio-preferences-v1", attr], separators=(",", ":")): PreferenceField(value=value, timestamp=timestamp, writer=writer)}

    def test_validation(self) -> None:
        cases: list[tuple[str, JsonValue]] = [("theme", "blue"), ("mainFontSize", True), ("mainFontSize", 25), ("contentWidth", 59), ("unknown", True)]
        for attr, value in cases:
            with self.subTest(attr=attr, value=value), self.assertRaises(ValidationError):
                PreferencePushRequest(scope="user", fields=self.field(attr, value))
        with self.assertRaises(ValidationError):
            PreferencePushRequest(scope="server", fields=self.field("theme", "light"))
        with self.assertRaises(ValidationError):
            UiPreferencesDto.model_validate({"fields": {'["codex.terminal.height"]': {"value": 30, "timestamp": 1, "writer": "a"}}})
        with self.assertRaises(ValidationError):
            PreferencePushRequest(scope="user", fields=self.field("theme", "light", -1))

    def test_merge_restart_and_retry(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "preferences.db"
            with sqlite3.connect(path) as db:
                ensure_tables(db)
                first = PreferencePushRequest(scope="user", fields=self.field("theme", "light", 10))
                merge_preferences(db, first)
                merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("mainFontSize", 18, 11)))
                merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("theme", "dark", 9)))
                before = db.execute("SELECT seq FROM sync_entities WHERE collection='uiPreferences'").fetchone()[0]
                merge_preferences(db, first)
                self.assertEqual(before, db.execute("SELECT seq FROM sync_entities WHERE collection='uiPreferences'").fetchone()[0])
            with sqlite3.connect(path) as db:
                values = read_preferences(db, "user").fields
                self.assertEqual({field.value for field in values.values()}, {"light", 18})
                self.assertIn(b'application/json', bootstrap_preferences(db))
                self.assertEqual(len(json.loads(db.execute("SELECT payload FROM sync_entities").fetchone()[0])["value"]["fields"]), 2)

    def test_ties_and_isolation(self) -> None:
        with sqlite3.connect(":memory:") as db:
            ensure_tables(db)
            for writer, value in [("b", "dark"), ("a", "light")]:
                result = merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("theme", value, 10, writer)))
            self.assertEqual(next(iter(result.fields.values())).value, "dark")
            merge_preferences(db, PreferencePushRequest(scope="server", fields={'["codex-project-tree:/state","/project"]': PreferenceField(value=True, timestamp=10, writer="a")}))
            self.assertEqual(len(read_preferences(db, "user").fields), 1)
            self.assertEqual(len(read_preferences(db, "server").fields), 1)
