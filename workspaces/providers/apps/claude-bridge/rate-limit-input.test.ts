import assert from "node:assert/strict";
import { test } from "vitest";
import { sdkDiagnostics, mutateSdkField } from "./testing/sdk-typecheck.ts";
import { parseRateLimitInfo } from "./rate-limit-input.ts";
import { startBridge } from "./testing/bridge-harness.ts";
import { sdkResult } from "./usage-fixtures.ts";

for (const [field, change] of [
  ["rateLimitType", (type: string) => type + " | 'future_scope'"],
  ["status", (type: string) => type + " | 'future_status'"],
  ["overageStatus", (type: string) => type + " | 'future_overage'"],
  ["resetsAt", () => "string"],
] as const) {
  test(`a future SDK rate ${field} fails the actual parser or policy`, () => {
    const errors = sdkDiagnostics("rate-limit-input.ts", (source) =>
      mutateSdkField(source, "SDKRateLimitInfo", [field], change),
    );
    assert.ok(
      errors.some((error) =>
        error.file?.fileName.endsWith("/rate-limit-input.ts"),
      ),
    );
  });
}

test("a removed seven-day SDK reset field fails the actual usage API consumer", () => {
  const errors = sdkDiagnostics("features.ts", (source) =>
    mutateSdkField(
      source,
      "SDKControlGetUsageResponse",
      ["rate_limits", "seven_day", "resets_at"],
      () => null,
    ),
  );
  assert.ok(
    errors.some((error) => error.file?.fileName.endsWith("/features.ts")),
  );
});

for (const raw of [
  { status: "rejected", rateLimitType: "future_scope", resetsAt: 9000 },
  { status: "future", rateLimitType: "five_hour" },
  { status: "rejected", rateLimitType: "five_hour", overageStatus: "future" },
  { status: "rejected", rateLimitType: "five_hour", resetsAt: "soon" },
  {
    status: "rejected",
    rateLimitType: "seven_day_overage_included",
    resetsAt: 9000,
  },
  null,
]) {
  test(`unsupported live rate evidence is retained without a known-limit decision: ${JSON.stringify(raw)}`, async () => {
    assert.deepEqual(parseRateLimitInfo(raw), { type: "unsupported", raw });
    const bridge = await startBridge(
      new URL("./usage-events.fixture.mts", import.meta.url),
      "claude-unsupported-rate-",
      {
        env: {
          STUDIO_FIXTURE_EVENTS: JSON.stringify([
            { type: "rate_limit_event", rate_limit_info: raw },
            sdkResult,
          ]),
        },
      },
    );
    try {
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Run" }],
      });
      const note = await bridge.note("account/rateLimits/updated");
      assert.deepEqual(note.params.claudeRateLimit, {
        type: "unsupported",
        raw,
      });
      const completed = await bridge.note("turn/completed");
      assert.equal(completed.params.turn.error, undefined);
      assert.equal(completed.params.turn.status, "completed");
    } finally {
      await bridge.close();
    }
  });
}
