import type { RateLimitInfo } from "./features.ts";
import type { SDKAssistantMessage } from "@anthropic-ai/claude-agent-sdk";
import type { KnownCodexErrorKind } from "./provider-errors.generated.ts";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "vitest";
import {
  usageLimits,
  limitEvent as applyLimitEvent,
  parseRateLimitInfo,
  nativeItem,
  InputQueue,
  claudeErrorInfo,
  legacyClaudeErrorInfo,
  overageAllowed,
  limitApplies,
} from "./features.ts";

const limitEvent = (
  current: Parameters<typeof applyLimitEvent>[0],
  raw: unknown,
) => applyLimitEvent(current, parseRateLimitInfo(raw));
const canUseOverage = (raw: unknown) => {
  const evidence = parseRateLimitInfo(raw);
  return evidence.type === "known" && overageAllowed(evidence.info);
};

test("subscription windows use percentages and seconds", () => {
  const raw = {
    rate_limits_available: true,
    subscription_type: "max",
    rate_limits: {
      five_hour: { utilization: 11, resets_at: "2026-09-22T08:00:00Z" },
      seven_day: { utilization: 4, resets_at: null },
      model_scoped: [
        { display_name: "Fable", utilization: 7, resets_at: null },
      ],
      extra_usage: {
        is_enabled: true,
        monthly_limit: null,
        used_credits: null,
        utilization: null,
      },
    },
  };
  const actual = usageLimits(raw);
  assert.equal(actual.rateLimits.primary!.usedPercent, 11);
  assert.equal(
    actual.rateLimits.primary!.resetsAt,
    Date.parse(raw.rate_limits.five_hour.resets_at) / 1000,
  );
  assert.equal(actual.rateLimits.primary!.windowDurationMins, 300);
  assert.equal(actual.rateLimits.secondary!.windowDurationMins, 10080);
  assert.equal(
    actual.rateLimitsByLimitId["claude-fable"]!.secondary!.usedPercent,
    7,
  );
  assert.deepEqual(actual.extraUsage, {
    is_enabled: true,
    monthly_limit: null,
    used_credits: null,
    utilization: null,
  });
  assert.throws(
    () =>
      usageLimits({
        rate_limits_available: false,
        subscription_type: null,
        rate_limits: null,
      }),
    /did not return/,
  );
});
test("invalid or absent utilization never becomes zero usage", () => {
  const actual = usageLimits({
    rate_limits_available: true,
    subscription_type: null,
    rate_limits: { five_hour: { utilization: null, resets_at: "invalid" } },
  });
  assert.equal(actual.rateLimits.primary!.usedPercent, null);
  assert.equal(actual.rateLimits.primary!.resetsAt, null);
  assert.ok(actual.rateLimits.secondary == null);
});
test("native events use fractional utilization without mutating cached windows", () => {
  const initial = usageLimits({
    rate_limits_available: true,
    subscription_type: null,
    rate_limits: {
      five_hour: { utilization: 11, resets_at: null },
      seven_day: { utilization: 4, resets_at: null },
    },
  });
  const updated = limitEvent(initial, {
    rateLimitType: "five_hour",
    status: "allowed",
    utilization: 0.25,
    resetsAt: 123,
  });
  assert.equal(updated!.rateLimits.primary!.usedPercent, 25);
  assert.equal(updated!.rateLimits.primary!.resetsAt, 123);
  assert.equal(initial.rateLimits.primary!.usedPercent, 11);
  assert.equal(updated!.rateLimits.secondary!.usedPercent, 4);
  for (const raw of [
    { rateLimitType: "unrecognized", status: "rejected", utilization: 0.1 },
    { rateLimitType: "five_hour", status: "allowed", utilization: null },
  ]) {
    assert.deepEqual(limitEvent(initial, raw).claudeRateLimit, {
      type: "unsupported",
      raw,
    });
  }
});
test("native tools preserve commands and file edits", () => {
  const command = nativeItem(
    { type: "tool_use", id: "bash", name: "Bash", input: { command: "pwd" } },
    "/work",
  );
  assert.equal(command.type, "commandExecution");
  assert.equal(command.command, "pwd");
  assert.equal(command.cwd, "/work");
  const edit = nativeItem(
    {
      type: "tool_use",
      id: "edit",
      name: "Edit",
      input: {
        file_path: "/work/a",
        old_string: "before",
        new_string: "after",
      },
    },
    "/work",
  );
  assert.equal(edit.type, "fileChange");
  assert.ok(edit.changes?.[0]);
  assert.equal(edit.changes![0].path, "/work/a");
  assert.match(edit.changes![0].diff as string, /before/);
  assert.equal(
    nativeItem(
      {
        type: "tool_use",
        id: "read",
        name: "Read",
        input: { file_path: "/work/a" },
      },
      "/work",
    ).type,
    "mcpToolCall",
  );
});
test("AskUserQuestion starts as the stored question item contract", () => {
  const { item } = JSON.parse(
    readFileSync(
      new URL(
        "../../../client/contracts/agent-question-item.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const started = nativeItem(
    { type: "tool_use", id: item.id, name: item.tool, input: item.arguments },
    "/work",
  );
  const { status, result, ...fields } = item;
  assert.equal(status, "completed");
  assert.ok(result.content);
  assert.deepEqual(started, { ...fields, status: "inProgress" });
});
test("persistent queue preserves order and close releases an idle consumer", async () => {
  const queue = new InputQueue<string>(),
    iterator = queue[Symbol.asyncIterator]();
  const pending = iterator.next();
  queue.push("one");
  queue.push("two");
  assert.deepEqual(await pending, { value: "one", done: false });
  assert.deepEqual(await iterator.next(), { value: "two", done: false });
  const idle = iterator.next();
  queue.close();
  assert.equal((await idle).done, true);
  assert.throws(() => queue.push("late"), /closed/);
});

test("rate limit events retain overage permission without utilization", () => {
  const current = usageLimits({
    rate_limits_available: true,
    subscription_type: null,
    rate_limits: { five_hour: { utilization: 100, resets_at: null } },
  });
  const info = {
    rateLimitType: "five_hour",
    status: "rejected",
    overageStatus: "allowed",
    resetsAt: 1234,
  } satisfies RateLimitInfo;
  const next = limitEvent(current, info);
  assert.deepEqual(next!.rateLimits.claudeRateLimit, info);
  assert.equal(next!.rateLimits.primary!.usedPercent, 100);
  assert.equal(current.rateLimits.claudeRateLimit, undefined);
});

test("overage permission is replaced and unknown limit resets remain visible", () => {
  const allowed = limitEvent(null, {
    rateLimitType: "five_hour",
    status: "rejected",
    overageStatus: "allowed",
    utilization: 1,
    resetsAt: 100,
  });
  assert.equal(canUseOverage(allowed!.claudeRateLimit), true);
  const rejected = limitEvent(allowed, {
    rateLimitType: "five_hour",
    status: "rejected",
    overageStatus: "rejected",
    isUsingOverage: true,
    resetsAt: 200,
  });
  assert.equal(canUseOverage(rejected!.claudeRateLimit), false);
  const unknown = limitEvent(allowed, {
    rateLimitType: "future_scope",
    status: "rejected",
    resetsAt: 900,
  });
  assert.equal(canUseOverage(unknown!.claudeRateLimit), false);
  assert.deepEqual(unknown!.claudeRateLimit, {
    type: "unsupported",
    raw: { rateLimitType: "future_scope", status: "rejected", resetsAt: 900 },
  });
  assert.equal(unknown!.rateLimitsByLimitId["claude-unknown-limit"], undefined);
  const overage = limitEvent(allowed, {
    rateLimitType: "overage",
    status: "rejected",
    overageStatus: "allowed_warning",
  });
  assert.equal(canUseOverage(overage!.claudeRateLimit), true);
});

test("model scope excludes only a known different family", () => {
  const opus = {
    rateLimitType: "seven_day_opus",
    status: "allowed",
  } satisfies RateLimitInfo;
  assert.equal(limitApplies(opus, "sonnet"), false);
  assert.equal(limitApplies(opus, "claude-sonnet-4-6"), false);
  for (const model of ["opus", "default", "custom-sonnet-proxy", null])
    assert.equal(limitApplies(opus, model), true);
  assert.equal(
    parseRateLimitInfo({ rateLimitType: "future_scope", status: "allowed" })
      .type,
    "unsupported",
  );
});

test("API error classification retains unrelated failure types", () => {
  for (const [error, text, expected] of [
    ["rate_limit", "Too Many Requests", "rateLimitExceeded"],
    ["unknown", "API Error: 429 Too Many Requests", "rateLimitExceeded"],
    ["overloaded", "Busy", "serverOverloaded"],
    ["authentication_failed", "Sign in", "unauthorized"],
    ["unknown", "ECONNRESET", "httpConnectionFailed"],
    ["unknown", "Failed to parse row 429", null],
  ] as const satisfies readonly (readonly [
    SDKAssistantMessage["error"],
    string,
    KnownCodexErrorKind | null,
  ])[])
    assert.equal(claudeErrorInfo(error, text), expected);
});

test("untyped terminal errors use the explicit adapter and preserve unknown input", () => {
  const future = {
    error: "future_vendor_error",
    opaque: { nested: [1, null] },
  };
  const original = JSON.stringify(future);
  assert.equal(legacyClaudeErrorInfo(future.error, "API Error: 429"), null);
  assert.equal(legacyClaudeErrorInfo(future, "API Error: 429"), null);
  assert.equal(
    legacyClaudeErrorInfo(undefined, "API Error: 429"),
    "rateLimitExceeded",
  );
  assert.equal(
    legacyClaudeErrorInfo("server_error", "Failure"),
    "internalServerError",
  );
  assert.equal(JSON.stringify(future), original);
});
