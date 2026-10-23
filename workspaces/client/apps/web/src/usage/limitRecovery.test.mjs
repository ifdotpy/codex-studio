import assert from "node:assert/strict";
import { test } from "vitest";
import { limitRecovery, limitRecovered } from "./limitRecovery.ts";

const now = 1_800_000_000;
const agent = {
  id: "agent",
  accountKey: "work",
  model: "gpt-6",
  provider: "codex",
  error: { codexErrorInfo: "rateLimitExceeded" },
  nativeLimitErrorAt: now - 10,
};
const decision = (overrides = {}) => ({
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
const limits = (data = {}, overrides = {}) => ({
  accountKey: "work",
  at: now,
  data: { rateLimits: data },
  eligibility: decision(),
  ...overrides,
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
  const sonnet = { ...agent, provider: "claude", model: "sonnet" };
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
    assert.equal(recovery.title, title);
    assert.equal(recovery.action?.label, action);
    if (owner) assert.ok(recovery.ownerRequest.includes(owner));
    assert.equal(
      recovery.resetAt,
      now + 90,
      "the Rust policy owns which reset applies",
    );
    const usage = limitRecovery(
      { ...agent, error: { codexErrorInfo: "usageLimitExceeded" } },
      limits({ rateLimitReachedType: reason }),
      now,
    );
    assert.match(usage.title, /usage limit/);
  }
  for (const [plan, label] of [
    ["pro", "View credits in ChatGPT"],
    ["plus", "View credits in ChatGPT"],
    ["pro_lite", "View credits in ChatGPT"],
    ["free", "View ChatGPT usage"],
    ["go", "View ChatGPT usage"],
  ]) {
    assert.equal(
      limitRecovery(agent, limits({ planType: plan }), now).action.label,
      label,
    );
  }
  assert.equal(
    limitRecovery(
      agent,
      limits({ rateLimitReachedType: "future", planType: "pro" }),
      now,
    ).action,
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
      limitRecovery(agent, snapshot, now).message,
      /^View account limits/,
    );
  }
});

test("Claude notices show model reset and overage without Codex purchase actions", () => {
  const claude = { ...agent, provider: "claude", model: "sonnet" };
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
  assert.equal(recovery.eligibility, eligibility);
  assert.equal(recovery.resetAt, now + 180);
  assert.equal(recovery.action, undefined);
});
