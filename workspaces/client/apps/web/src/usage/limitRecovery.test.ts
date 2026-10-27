import { ProviderKind } from "../generated/api";
import type { AccountLimitsSnapshot } from "./accountUsage";
import { createAccountLimitsCache } from "./accountUsage";
import type { JsonInputOf, Agent } from "../types";
import type { components } from "../generated/api";
import assert from "node:assert/strict";
import { test } from "vitest";
import { limitRecovery, limitRecovered } from "./limitRecovery.ts";

const now = 1_800_000_000;
const agent: Agent = {
  id: "agent",
  accountKey: "work",
  model: "gpt-6",
  provider: ProviderKind.codex,
  error: { codexErrorInfo: "rateLimitExceeded" },
  nativeLimitErrorAt: now - 10,
};
const decision = (
  overrides: Partial<components["schemas"]["UsageEligibility"]> = {},
): components["schemas"]["UsageEligibility"] => ({
  agentId: "agent",
  accountKey: "work",
  model: "gpt-6",
  provider: "codex",
  allowed: false,
  canRun: false,
  fresh: true,
  checkedAt: now,
  validUntil: now + 5,
  resetAt: now + 90,
  buckets: [{ id: "codex", overageAllowed: false }],
  overage: "unavailable",
  ...overrides,
});
type UsageLimitsResponse = JsonInputOf<
  components["schemas"]["UsageLimitsResponse"]
>;
type RecoveryLimits = UsageLimitsResponse &
  Pick<AccountLimitsSnapshot, "stale" | "loading">;
const limits = (
  data: JsonInputOf<components["schemas"]["RateLimitBucket"]> = {},
  overrides: Partial<RecoveryLimits> = {},
) =>
  ({
    accountKey: "work",
    at: now,
    data: { rateLimits: data },
    eligibility: decision(),
    ...overrides,
  }) satisfies RecoveryLimits;

test("recovery callers retain the generated eligibility contract", () => {
  const invalid = () =>
    limitRecovered(
      agent,
      {
        accountKey: "work",
        eligibility: {
          ...decision(),
          // @ts-expect-error A malformed decision cannot enter the recovery caller.
          canRun: "true",
        },
      },
      now,
    );
  assert.equal(typeof invalid, "function");
});

test("restored eligibility retains its identity and expires at use time", () => {
  const records = new Map<string, string>();
  const storage = {
    getItem: (key: string) => records.get(key) ?? null,
    setItem: (key: string, value: string) => {
      records.set(key, value);
    },
  };
  const cache = createAccountLimitsCache(storage);
  cache.configureLimitsCache("server");
  const available = limits(
    {},
    { eligibility: decision({ allowed: true, canRun: true }) },
  );
  cache.cacheAccountLimits(available);
  const restored = createAccountLimitsCache(storage);
  restored.configureLimitsCache("server");
  const snapshot = restored.cachedAccountLimits("work");
  assert.deepEqual(snapshot, available);
  assert.equal(limitRecovered(agent, snapshot, now), true);
  assert.equal(limitRecovered(agent, snapshot, now + 6), false);
  assert.equal(
    limitRecovered({ ...agent, model: "other" }, snapshot, now),
    false,
  );
});

test("future and ambiguous vendor error records cannot enable limit recovery", () => {
  for (const codexErrorInfo of [
    "futureLimit",
    { futureLimit: { opaque: [1, null] } },
    { rateLimitExceeded: {}, futureLimit: { id: 42 } },
  ]) {
    const error = { message: "Provider extension", codexErrorInfo };
    const original = JSON.stringify(error);
    const current = { ...agent, error } satisfies Agent;
    assert.equal(limitRecovered(current, limits(), now), false);
    assert.equal(limitRecovery(current, limits(), now), null);
    assert.equal(JSON.stringify(error), original);
  }
});

test("recovery consumes the Rust decision and validates its identity and validity", () => {
  const available = limits(
    { primary: { usedPercent: 100 } },
    { eligibility: decision({ allowed: true, canRun: true }) },
  );
  assert.equal(limitRecovered(agent, available, now), true);
  for (const patch of [
    { eligibility: undefined },
    { eligibility: decision({ canRun: false }) },
    { eligibility: decision({ canRun: true, fresh: false }) },
    { eligibility: decision({ canRun: true, validUntil: now - 1 }) },
    ...["agentId", "accountKey", "model", "provider"].map((key) => ({
      eligibility: decision({ canRun: true, [key]: "other" }),
    })),
    { stale: true },
    { error: "offline" },
    { at: now - 10 },
    { accountKey: "other" },
  ]) {
    assert.equal(
      limitRecovered(agent, { ...available, ...patch }, now),
      false,
      JSON.stringify(patch),
    );
  }
  const sonnet: Agent = {
    ...agent,
    provider: ProviderKind.claude,
    model: "sonnet",
  };
  const response = {
    ...available,
    eligibility: decision({
      provider: "claude",
      model: "sonnet",
      canRun: true,
      allowed: true,
      overage: "allowed",
    }),
    data: {
      rateLimits: { primary: { usedPercent: 100 } },
      rateLimitsByLimitId: { "claude-opus": { primary: { usedPercent: 100 } } },
    },
  };
  assert.equal(
    limitRecovered(sonnet, response, now),
    true,
    "the UI does not recompute applicability or overage",
  );
  assert.equal(limitRecovery(sonnet, response, now), null);
  assert.equal(
    limitRecovered(
      { ...sonnet, error: { codexErrorInfo: "serverOverloaded" } },
      response,
      now,
    ),
    false,
  );
});

test("billing guidance keeps workspace roles and uses the Rust reset time", () => {
  for (const [reason, title, action, owner] of [
    [
      "workspace_owner_credits_depleted",
      "Workspace credits depleted",
      "Add workspace credits",
      undefined,
    ],
    [
      "workspace_member_credits_depleted",
      "Workspace credits depleted",
      undefined,
      "add credits",
    ],
    [
      "workspace_owner_usage_limit_reached",
      "Workspace usage limit reached",
      "Open workspace usage limits",
      undefined,
    ],
    [
      "workspace_member_usage_limit_reached",
      "Workspace usage limit reached",
      undefined,
      "increase my limit",
    ],
  ]) {
    const recovery = limitRecovery(
      agent,
      limits({
        rateLimitReachedType: reason,
        primary: { usedPercent: 100, resetsAt: now + 30 },
      }),
      now,
    );

    assert.equal(recovery!.title, title);

    assert.equal(recovery!.action?.label, action);

    if (owner) assert.ok(recovery!.ownerRequest!.includes(owner));

    assert.equal(
      recovery!.resetAt,
      now + 90,
      "the Rust policy owns which reset applies",
    );
    const usage = limitRecovery(
      { ...agent, error: { codexErrorInfo: "usageLimitExceeded" } },
      limits({ rateLimitReachedType: reason }),
      now,
    );

    assert.match(usage!.title, /usage limit/);
  }
  for (const [plan, label] of [
    ["pro", "View credits in ChatGPT"],
    ["plus", "View credits in ChatGPT"],
    ["pro_lite", "View credits in ChatGPT"],
    ["free", "View ChatGPT usage"],
    ["go", "View ChatGPT usage"],
  ]) {
    assert.equal(
      limitRecovery(agent, limits({ planType: plan }), now)!.action!.label,
      label,
    );
  }

  assert.equal(
    limitRecovery(
      agent,
      limits({ rateLimitReachedType: "future", planType: "pro" }),
      now,
    )!.action,
    undefined,
  );
  for (const snapshot of [
    null,
    limits({}, { stale: true }),
    limits({}, { error: "offline" }),
    limits({}, { accountKey: "other" }),
    limits({}, { at: now - 301 }),
  ]) {
    assert.match(
      limitRecovery(agent, snapshot, now)!.message,
      /^View account limits/,
    );
  }
});

test("unsupported provider identities retain generic recovery without Codex billing actions", () => {
  for (const provider of [undefined, null, "future-provider"]) {
    const unknown: Agent = {
      ...agent,
      // @ts-expect-error An unknown wire provider must not authorize billing actions.
      provider,
    };
    for (const data of [
      { rateLimitReachedType: "workspace_owner_credits_depleted" },
      { rateLimitReachedType: "workspace_member_credits_depleted" },
      { planType: "pro" },
    ]) {
      const recovery = limitRecovery(unknown, limits(data), now);
      assert.equal(recovery?.action, undefined);
      assert.equal(recovery?.ownerRequest, undefined);
      assert.match(recovery!.message, /^View account limits/);
    }
  }
});

test("Claude notices show model reset and overage without Codex purchase actions", () => {
  const claude: Agent = {
    ...agent,
    provider: ProviderKind.claude,
    model: "sonnet",
  };
  const eligibility = decision({
    provider: "claude",
    model: "sonnet",
    overage: "rejected",
    resetAt: now + 180,
  });
  const recovery = limitRecovery(
    claude,
    limits({ planType: "pro" }, { eligibility }),
    now,
  );

  assert.equal(recovery!.eligibility, eligibility);

  assert.equal(recovery!.resetAt, now + 180);

  assert.equal(recovery!.action, undefined);
});
