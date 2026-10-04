"""Contract tests for the public insights request and response models."""

from __future__ import annotations

import unittest

from pydantic import ValidationError

from studio_api.insights.models import (
    AccountCostResponse,
    AnalyticsResponse,
    AnalyticsQuery,
    AnalyticsScope,
    SessionCostResponse,
    WorktreeDiskQuery,
    WorktreeDiskResponse,
)


class InsightsModelTests(unittest.TestCase):
    def test_analytics_query_preserves_wire_string_values_and_aliases(self) -> None:
        query = AnalyticsQuery.model_validate(
            {"scope": "team", "from": "10.5", "limit": "20", "view": "detail"}
        )

        self.assertEqual(AnalyticsScope(query.scope), AnalyticsScope.TEAM)
        self.assertEqual(
            query.service_options(),
            {"scope": "team", "from": "10.5", "limit": "20", "view": "detail"},
        )

    def test_analytics_query_rejects_unknown_closed_values(self) -> None:
        with self.assertRaisesRegex(ValueError, "Unknown analytics scope"):
            AnalyticsQuery.model_validate({"scope": "workspace"}).service_options()
        self.assertEqual(
            AnalyticsQuery.model_validate({"legacyUnknown": "ignored"}).service_options(),
            {},
        )

    def test_worktree_query_keeps_comma_delimited_ids(self) -> None:
        self.assertEqual(
            WorktreeDiskQuery.model_validate(
                {"workers": "worker-a,worker-b", "legacyUnused": "ignored"}
            ).workers,
            "worker-a,worker-b",
        )

    def test_cost_responses_keep_nulls_and_reject_unknown_fields(self) -> None:
        cost = AccountCostResponse.model_validate(
            {
                "at": None,
                "error": None,
                "data": None,
                "refreshing": False,
                "stale": True,
                "accountKey": "default",
            }
        )
        self.assertIsNone(cost.data)
        with self.assertRaises(ValidationError):
            SessionCostResponse.model_validate(
                {
                    "rootId": "root",
                    "totalUSD": None,
                    "pricedSamples": 0,
                    "breakdown": {"providers": {}, "models": {}},
                    "unknownModels": [],
                    "estimated": True,
                    "pricingState": "ready",
                    "cacheAgeSeconds": 0,
                    "refreshing": False,
                    "unmodeled": "not allowed",
                }
            )

    def test_analytics_summary_response_validates_typed_nested_contract(self) -> None:
        response = AnalyticsResponse.model_validate(
            {
                "version": 1,
                "generatedAt": 5.0,
                "filters": {
                    "agent": "agent-a",
                    "scope": "agent",
                    "from": None,
                    "to": None,
                    "tool": None,
                },
                "coverage": {
                    "trackingSince": 1.0,
                    "captureErrors": {"count": 0, "last": None},
                    "historyErrors": [],
                    "provisionalUsageSamples": 0,
                    "tokenAttribution": "provider_usage_only",
                    "payloadMeasurement": "observed_protocol_payload",
                    "history": "live_and_stored_history",
                    "notes": [],
                },
                "summary": {
                    "agents": 1,
                    "turns": 2,
                    "usageSamples": 3,
                    "provisionalUsageSamples": 0,
                    "exactResponseSamples": 3,
                    "legacyUsageSamples": 0,
                    "modelToolCalls": 1,
                    "protocolToolCalls": 2,
                    "observedToolRows": 3,
                    "toolCalls": 1,
                    "failedToolCalls": 0,
                    "modelFailedToolCalls": 0,
                    "protocolFailedToolCalls": 0,
                    "compactions": 0,
                    "inputBytes": 10,
                    "outputBytes": 20,
                    "modelInputBytes": 10,
                    "modelOutputBytes": 20,
                    "protocolInputBytes": None,
                    "protocolOutputBytes": None,
                    "durationMs": 1,
                    "modelDurationMs": 1,
                    "protocolDurationMs": None,
                    "tokens": {"inputTokens": 10, "cacheWriteInputTokens": None},
                    "tokenObservations": {"inputTokens": 1},
                    "cacheHitRate": None,
                    "cacheHitRateSamples": 0,
                    "cacheHitRateTotalSamples": 3,
                    "peakContextTokens": None,
                    "peakContextPercent": None,
                    "baselineMissingSamples": 0,
                },
                "agents": [{"id": "agent-a", "providerMetadata": {"provider": "test"}}],
                "agentTotals": [{"id": "agent-a", "tokens": {"inputTokens": 10}}],
                "tools": [
                    {
                        "name": "search",
                        "type": "modelToolCall",
                        "payloadBoundary": "model",
                        "calls": 1,
                        "failed": 0,
                        "inputBytes": 10,
                        "outputBytes": 20,
                        "modelInputBytes": 10,
                        "modelOutputBytes": 20,
                        "durationMs": 1,
                        "imageCount": 0,
                        "inputMeasurements": 1,
                        "outputMeasurements": 1,
                        "duration": {
                            "count": 1,
                            "min": 1,
                            "max": 1,
                            "mean": 1,
                            "p50": 1,
                            "p95": 1,
                        },
                    }
                ],
                "operations": {
                    "monitors": [
                        {
                            "id": "monitor-a",
                            "agent": "agent-a",
                            "status": "finished",
                            "created": 1,
                            "finished": None,
                            "bytes": 0,
                            "exitCode": None,
                            "error": None,
                            "timeout_ms": None,
                        }
                    ],
                    "eventCounts": {},
                    "approvalCounts": {},
                },
                "notifications": [
                    {"id": "notice-a", "agent": "agent-a", "method": "test", "hour": 0, "count": 1, "bytes": 4}
                ],
                "modelTotals": [{"model": "model-a", "samples": 3, "tokens": {"inputTokens": 10}}],
                "accountTotals": [{"accountKey": "default", "samples": 3, "tokens": {"inputTokens": 10}}],
                "pagination": {"limit": 50, "offset": 0, "total": 1, "hasMore": False},
                "detailPagination": {
                    "rateLimits": {"limit": 0, "offset": 0, "total": 0, "hasMore": False},
                    "turns": {"limit": 1, "offset": 0, "total": 1, "hasMore": False},
                },
            }
        )

        self.assertIsNotNone(response.summary)
        assert response.summary is not None
        self.assertEqual(response.summary.tokens.inputTokens, 10)

    def test_worktree_response_uses_closed_states_and_measures(self) -> None:
        result = WorktreeDiskResponse.model_validate(
            {
                "workers": {
                    "worker-a": {
                        "state": "ready",
                        "bytes": 42,
                        "scannedAt": 5.0,
                        "measure": "allocated blocks",
                    },
                    "worker-b": {
                        "state": "unavailable",
                        "measure": "allocated blocks",
                        "error": "permission denied",
                    },
                },
                "totalBytes": 42,
                "limitBytes": 100,
                "warning": False,
                "scanning": False,
                "error": None,
                "measure": "allocated blocks",
            }
        )
        self.assertEqual(result.workers["worker-a"].bytes, 42)
        self.assertEqual(result.workers["worker-a"].scannedAt, 5.0)
        self.assertEqual(result.workers["worker-b"].state.value, "unavailable")
        with self.assertRaises(ValidationError):
            WorktreeDiskResponse.model_validate(
                {
                    "workers": {},
                    "totalBytes": 0,
                    "limitBytes": 0,
                    "warning": False,
                    "scanning": False,
                    "error": None,
                    "measure": "unexpected",
                }
            )


if __name__ == "__main__":
    unittest.main()
