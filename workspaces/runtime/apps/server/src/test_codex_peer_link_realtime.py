"""A large peer link response arrives within a wall-clock bound.

This witness measures real time, so it belongs to the heavy tier: a busy machine
can stretch a 3 MiB transfer. The ordinary tier keeps the whole-response contract
in test_a_large_response_arrives_whole, within the exchange's own 10 s deadline.
No controlled-clock version of the throughput bound exists.
"""

from __future__ import annotations

import time
import unittest

from test_codex_peer_link import BIG, PeerLinkFixture


class PeerLinkRealTimeTests(PeerLinkFixture):
    def test_a_large_response_arrives_within_five_seconds(self) -> None:
        started = time.monotonic()
        response = self._send("a", "b", "/api/big", method="GET", raw=b"")
        self.assertEqual(response["raw"], BIG)
        self.assertLess(time.monotonic() - started, 5)


if __name__ == "__main__":
    unittest.main()
