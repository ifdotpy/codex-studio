import { expect, test } from "vitest";
import {
  claudeTurnStatus,
  isClaudeResultLifecycle,
} from "./provider-lifecycle.ts";
import { sdkResult } from "./usage-fixtures.ts";

test("result error, missing result and Stop retain the existing precedence", () => {
  expect(claudeTurnStatus(sdkResult, false, undefined)).toBe("completed");
  expect(
    claudeTurnStatus({ ...sdkResult, is_error: true }, false, undefined),
  ).toBe("failed");
  expect(claudeTurnStatus(sdkResult, false, new Error("failure"))).toBe(
    "failed",
  );
  expect(claudeTurnStatus(null, false, undefined)).toBe("failed");
  expect(
    claudeTurnStatus({ ...sdkResult, is_error: true }, true, new Error("stop")),
  ).toBe("interrupted");
});

test("only recognized vendor result lifecycles cross the terminal boundary", () => {
  expect(isClaudeResultLifecycle(sdkResult)).toBe(true);
  for (const subtype of [
    "error_during_execution",
    "error_max_turns",
    "error_max_budget_usd",
    "error_max_structured_output_retries",
  ]) {
    const result = { ...sdkResult, subtype, is_error: false };
    if (!isClaudeResultLifecycle(result))
      throw new Error("Known result must validate");
    expect(claudeTurnStatus(result, false, undefined)).toBe("failed");
  }
  for (const result of [
    { subtype: "success" },
    { subtype: "success", is_error: null },
    { subtype: "success", is_error: 0 },
    { subtype: "future", is_error: false },
  ])
    expect(isClaudeResultLifecycle(result)).toBe(false);
});
