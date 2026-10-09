#!/usr/bin/env python3
"""Agent entities omit internal recovery fields and lead-only overviews."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('projection_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_sync_entities import project


class AgentEntityProjection(f.RuntimeContract):
    def test_internal_fields_and_lead_overview_are_not_projected(self):
        lead = self.lead()
        internal = {
            'startOutcomeHold': {'reason': 'internal'},
            'turnRecovery': {'phase': 'internal'},
            'checkpointError': 'internal',
            'connectionRecovery': {'phase': 'internal'},
            'lastContextRepairCheck': {'phase': 'internal'},
            'lastContextRepairWait': {'phase': 'internal'},
        }
        with self.runtime.lock, self.runtime.db() as db:
            record = self.runtime.agent(lead['id'], db)
            record.update(internal)
            self.runtime.put(db, 'agents', record)
            view = self.runtime.agent_entity_view(db, record)
            entity = project('agent', view)
        self.assertIsInstance(entity, dict)
        self.assertNotIn('overview', entity)
        for field in internal:
            self.assertNotIn(field, entity)
        self.assertFalse(any(field.startswith('lastContextRepair') for field in entity))


if __name__ == '__main__':
    suite = unittest.TestSuite(AgentEntityProjection(name) for name in AgentEntityProjection.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
