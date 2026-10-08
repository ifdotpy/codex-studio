"""Measure 100 signed ASGI reads with real, read-only Tailscale identity checks."""
from __future__ import annotations

import json
import statistics
import time
from unittest.mock import patch

from codex_multi_server import _owner_login, _peer_login, _tailscale_json
from studio_api.multi_server.test_access import AccessTests


def main() -> None:
    AccessTests.setUpClass()
    fixture = AccessTests()
    fixture.setUp()
    samples: list[float] = []
    try:
        login = _owner_login()
        address = _tailscale_json("status", "--json")["Self"]["TailscaleIPs"][0]
        with patch("codex_multi_server._owner_login", side_effect=_owner_login), \
             patch("codex_multi_server._peer_login", side_effect=_peer_login):
            body = fixture.pair_body()
            raw = json.dumps(body).encode()
            headers = fixture.signed("POST", "/api/multi-server/v1/pair", raw, body["requestId"])
            headers["X-Forwarded-For"] = address
            headers["Tailscale-User-Login"] = login
            paired = fixture.client.post("/api/multi-server/v1/pair", content=raw, headers=headers)
            if paired.status_code != 200:
                raise RuntimeError("The isolated benchmark could not pair its test device")
            requests = [fixture.signed("GET", "/api/probe", request_id=f"bench-{index}") for index in range(100)]
            for headers in requests:
                headers["X-Forwarded-For"] = address
                headers["Tailscale-User-Login"] = login
                begin = time.perf_counter()
                reply = fixture.client.get("/api/probe", headers=headers)
                samples.append((time.perf_counter() - begin) * 1000)
                if reply.status_code != 200:
                    raise RuntimeError("The isolated benchmark request failed")
        print(json.dumps({"N": len(samples), "p50_ms": round(statistics.median(samples), 2),
                          "p95_ms": round(sorted(samples)[94], 2), "total_s": round(sum(samples) / 1000, 2)}))
    finally:
        fixture.doCleanups()


if __name__ == "__main__":
    main()
