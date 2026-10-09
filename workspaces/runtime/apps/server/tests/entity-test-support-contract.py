#!/usr/bin/env python3
"""Keep shared persisted-value fixtures aligned with their strict DTOs."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from entity_test_support import capacity_retry, context_repair_wait, usage_resume
from studio_api.sync.models import AgentEntityContextRepairWaitDto, CapacityRetryDto, UsageResumeDto


class EntityTestSupportContract(unittest.TestCase):
    def test_context_repair_wait_fixture_matches_persisted_model(self):
        value = context_repair_wait(stage="pending", reason="fixture marker")
        parsed = AgentEntityContextRepairWaitDto.model_validate({
            "error": value["error"], "scope": value["scope"],
        })
        self.assertEqual(parsed.error, value["error"])
        self.assertEqual(parsed.scope, value["scope"])

    def test_capacity_retry_fixture_matches_persisted_model(self):
        parsed = CapacityRetryDto.model_validate(capacity_retry())
        self.assertEqual(parsed.id, "fixture-capacity-retry")

    def test_usage_resume_fixture_matches_persisted_model(self):
        parsed = UsageResumeDto.model_validate(usage_resume())
        self.assertEqual(parsed.id, "fixture-usage-resume")


if __name__ == "__main__":
    unittest.main()
