"""The Python owner of the orchestration settings row matches the shared contract."""
from __future__ import annotations

import sqlite3
import unittest

from pydantic import ValidationError

import codex_orchestration_settings
from codex_shared_values import (
    CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
    CHILD_RESULT_COALESCE_MAX_SECONDS,
    CHILD_RESULT_COALESCE_MIN_SECONDS,
    IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
    IDLE_SESSION_UNLOAD_MAX_MINUTES,
    IDLE_SESSION_UNLOAD_MIN_MINUTES,
)
from studio_api.system.models import OrchestrationSettings

HOLD = codex_orchestration_settings.CHILD_RESULT_COALESCE_FIELD
UNLOAD = codex_orchestration_settings.IDLE_SESSION_UNLOAD_FIELD


class OrchestrationSettingsTests(unittest.TestCase):
    def test_unset_settings_read_the_defaults_and_a_write_persists(self) -> None:
        db = sqlite3.connect(":memory:")
        self.assertEqual(
            codex_orchestration_settings.read(db),
            {HOLD: CHILD_RESULT_COALESCE_DEFAULT_SECONDS, UNLOAD: IDLE_SESSION_UNLOAD_DEFAULT_MINUTES},
        )
        for seconds, minutes in (
            (CHILD_RESULT_COALESCE_MIN_SECONDS, IDLE_SESSION_UNLOAD_MAX_MINUTES),
            (CHILD_RESULT_COALESCE_MAX_SECONDS, IDLE_SESSION_UNLOAD_MIN_MINUTES),
        ):
            expected = {HOLD: seconds, UNLOAD: minutes}
            self.assertEqual(codex_orchestration_settings.write(db, seconds, minutes), expected)
            self.assertEqual(codex_orchestration_settings.read(db), expected)

    def test_request_model_enforces_the_shared_bounds(self) -> None:
        valid = {HOLD: CHILD_RESULT_COALESCE_DEFAULT_SECONDS, UNLOAD: IDLE_SESSION_UNLOAD_DEFAULT_MINUTES}
        OrchestrationSettings(**valid)
        for field, value in (
            (HOLD, CHILD_RESULT_COALESCE_MIN_SECONDS - 1),
            (HOLD, CHILD_RESULT_COALESCE_MAX_SECONDS + 1),
            (UNLOAD, IDLE_SESSION_UNLOAD_MIN_MINUTES - 1),
            (UNLOAD, IDLE_SESSION_UNLOAD_MAX_MINUTES + 1),
        ):
            with self.assertRaises(ValidationError):
                OrchestrationSettings(**{**valid, field: value})


if __name__ == "__main__":
    unittest.main()
