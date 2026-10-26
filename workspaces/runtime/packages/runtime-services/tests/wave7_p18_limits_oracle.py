"""Run the real Python limits endpoint on Python-written response records."""

import json
from pathlib import Path
import sys
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))
from studio_api.accounts.models import LimitsQuery, UsageLimitsResponse
from studio_api.accounts.router import create_router

cases = []
context = SimpleNamespace(runtime=None, send=lambda request, value: value)
route = next(route for route in create_router(context).routes if route.path == "/api/limits")
missing = object()
for data in (missing, None, {}, {"rateLimits": {"primary": {"usedPercent": 25.0}}}):
    for error in (missing, None, "", "fixture unavailable"):
        for cached in (False, True):
            raw = {"accountKey": "fixture", "at": 7.0}
            for key, value in (("data", data), ("error", error)):
                if value is not missing:
                    raw[key] = value
            record = UsageLimitsResponse.model_validate(raw).model_dump(exclude_unset=True)
            fresh = {"accountKey": "fixture", "data": {"rateLimits": {}}, "at": 8.0}
            calls = []

            def read(key):
                calls.append(["read", key])
                return fresh

            runtime = SimpleNamespace(
                rate_limits_for=lambda key: record,
                refresh_limits_background=lambda key: calls.append(["refresh", key]),
                limits=read,
            )
            context.runtime = runtime
            result = route.endpoint(None, LimitsQuery(account_key="fixture", cached="1" if cached else None))
            cases.append({"record": record, "fresh": fresh, "cached": cached, "result": result, "calls": calls})
print(json.dumps(cases))
