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
)
from studio_api.system.models import OrchestrationSettings


class OrchestrationSettingsTests(unittest.TestCase):
    def test_unset_hold_reads_the_default_and_a_write_persists(self) -> None:
        db = sqlite3.connect(":memory:")
        field = codex_orchestration_settings.CHILD_RESULT_COALESCE_FIELD
        self.assertEqual(codex_orchestration_settings.read(db), {field: CHILD_RESULT_COALESCE_DEFAULT_SECONDS})
        for seconds in (CHILD_RESULT_COALESCE_MIN_SECONDS, CHILD_RESULT_COALESCE_MAX_SECONDS):
            self.assertEqual(codex_orchestration_settings.write(db, seconds), {field: seconds})
            self.assertEqual(codex_orchestration_settings.read(db), {field: seconds})

    def test_request_model_enforces_the_shared_bounds(self) -> None:
        for seconds in (CHILD_RESULT_COALESCE_MIN_SECONDS - 1, CHILD_RESULT_COALESCE_MAX_SECONDS + 1):
            with self.assertRaises(ValidationError):
                OrchestrationSettings(childResultCoalesceSeconds=seconds)


if __name__ == "__main__":
    unittest.main()
