"""Durable field merge and validation, with real entity storage."""
import json
import hashlib
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from pydantic import ValidationError
from codex_sync_entities import ensure_tables
from studio_api.models import JsonValue
from studio_api.sync.preferences import PreferenceField, PreferencePushRequest, UiPreferencesDto, merge_preferences, read_preferences, bootstrap_preferences


class PreferencesTests(unittest.TestCase):
    def field(self, attr: str, value: JsonValue, timestamp: int = 1, writer: str = "a") -> dict[str, PreferenceField]:
        return {json.dumps(["codex-studio-preferences-v1", attr], separators=(",", ":")): PreferenceField(value=value, timestamp=timestamp, writer=writer)}

    def test_all_appearance_fields_sync_and_unknown_batch_is_atomic(self) -> None:
        values: dict[str, JsonValue] = {
            "theme": "dark",
            "typography": "custom",
            "contentLayout": "custom",
            "sidebarFontSize": 18,
            "mainFontSize": 18,
            "fontFamily": "inter",
            "contentWidth": 90,
            "sidebarShortcut": "Control+b",
            "showMessageAvatars": True,
            "hideOldChatsThreshold": 4,
            "checkProviderUpdates": False,
        }
        fields = {
            json.dumps(["codex-studio-preferences-v1", name], separators=(",", ":")):
                PreferenceField(value=value, timestamp=10, writer="fixture")
            for name, value in values.items()
        }
        request = PreferencePushRequest(scope="user", fields=fields)
        with sqlite3.connect(":memory:") as db:
            ensure_tables(db)
            accepted = merge_preferences(db, request)
            self.assertEqual(set(accepted.fields), set(fields))
            before = read_preferences(db, "user").fields

            invalid_batch = dict(fields)
            invalid_batch['["codex-studio-preferences-v1","unknownAppearance"]'] = (
                PreferenceField(value=True, timestamp=11, writer="fixture")
            )
            with self.assertRaises(ValidationError):
                PreferencePushRequest(scope="user", fields=invalid_batch)
            self.assertEqual(read_preferences(db, "user").fields, before)

    def test_hide_old_chats_threshold_domain_boundaries(self) -> None:
        for value in (1, 50):
            with self.subTest(value=value):
                PreferencePushRequest(scope="user", fields=self.field("hideOldChatsThreshold", value))
        for invalid_value in (0, 51, 1.0, True):
            with self.subTest(value=invalid_value), self.assertRaises(ValidationError):
                PreferencePushRequest(
                    scope="user",
                    fields=self.field("hideOldChatsThreshold", invalid_value),
                )

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

    def test_growth_cap_preserves_appearance_and_server_keys(self) -> None:
        fields = self.field("theme", "dark", 1)
        fields['["server-alias:workspace"]'] = PreferenceField(value="ABC", timestamp=1, writer="a")
        for index in range(20050):
            name = json.dumps(["studio-logical-project-collapsed", str(index)], separators=(",", ":"))
            fields[name] = PreferenceField(value=True, timestamp=index, writer="a")
        with sqlite3.connect(":memory:") as db:
            ensure_tables(db)
            result = merge_preferences(db, PreferencePushRequest(scope="user", fields=fields))
            self.assertEqual(len(result.fields), 3002)
            self.assertEqual(result.fields['["codex-studio-preferences-v1","theme"]'].value, "dark")
            self.assertEqual(result.fields['["server-alias:workspace"]'].value, "ABC")
            self.assertNotIn('["studio-logical-project-collapsed","0"]', result.fields)
            again = merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("mainFontSize", 18)))
            self.assertEqual(len(again.fields), 3003)
            self.assertEqual(len(read_preferences(db, "user").fields), 3003)

    def test_future_clock_is_clamped_and_normal_edit_can_replace_it(self) -> None:
        with sqlite3.connect(":memory:") as db, patch("studio_api.sync.preferences.time.time", return_value=1000):
            ensure_tables(db)
            result = merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("theme", "dark", 1_300_001)))
            self.assertEqual(next(iter(result.fields.values())).timestamp, 1_000_000)
            result = merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("theme", "light", 1_000_001)))
            self.assertEqual(next(iter(result.fields.values())).value, "light")
            result = merge_preferences(db, PreferencePushRequest(scope="user", fields=self.field("mainFontSize", 18, 1_300_000)))
            self.assertEqual(result.fields['["codex-studio-preferences-v1","mainFontSize"]'].timestamp, 1_300_000)

    def test_legacy_tool_expansions_are_removed_on_next_merge(self) -> None:
        with sqlite3.connect(":memory:") as db:
            ensure_tables(db)
            merge_preferences(db, PreferencePushRequest(scope="server", fields={'["codex-project-tree:/state","project"]': PreferenceField(value=True, timestamp=1, writer="a")}))
            row = db.execute("SELECT payload FROM sync_entities WHERE collection='uiPreferences' AND id='server'").fetchone()
            payload = json.loads(row[0])
            payload["value"]["fields"]['["studio-turns:/state:chat:tools-v3","turn"]'] = {"value": True, "timestamp": 2, "writer": "a"}
            serialized = json.dumps(payload)
            db.execute("UPDATE sync_entities SET payload=?, hash=? WHERE collection='uiPreferences' AND id='server'", (serialized, hashlib.sha256(serialized.encode()).hexdigest()))
            result = merge_preferences(db, PreferencePushRequest(scope="server", fields={}))
            self.assertEqual(len(result.fields), 1)
            self.assertNotIn("studio-turns:", db.execute("SELECT payload FROM sync_entities WHERE collection='uiPreferences' AND id='server'").fetchone()[0])
