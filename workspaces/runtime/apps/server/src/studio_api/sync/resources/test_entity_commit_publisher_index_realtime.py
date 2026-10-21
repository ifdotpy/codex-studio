"""The non-transcript checkpoint scan stays fast with many transcript rows.

This witness measures real time, so it belongs to the heavy tier: a busy machine
can stretch one query past its bound. The ordinary tier owns the same contract
with the query plan (test_non_transcript_checkpoint_scan_uses_partial_seq_index).
"""

from __future__ import annotations

import time
import unittest

from studio_api.sync.resources.test_entity_commit_publisher import (
    CHECKPOINT_TRANSCRIPT_ROWS,
    NON_TRANSCRIPT_MAX_SEQ_QUERY,
    PublisherFixture,
    insert_checkpoint_rows,
)


class EntityCheckpointScanRealTimeTests(PublisherFixture):
    async def test_non_transcript_checkpoint_scan_is_fast(self) -> None:
        insert_checkpoint_rows(self.database)
        started = time.perf_counter()
        maximum = self.database.execute(NON_TRANSCRIPT_MAX_SEQ_QUERY).fetchone()[0]
        elapsed_ms = (time.perf_counter() - started) * 1000
        self.assertEqual(maximum, 20)
        print({"checkpointRows": CHECKPOINT_TRANSCRIPT_ROWS, "elapsedMs": round(elapsed_ms, 3)})
        self.assertLess(elapsed_ms, 250)


if __name__ == "__main__":
    unittest.main()
