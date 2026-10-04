// Test the production recovery classifier without a backend or model request.
import assert from "node:assert/strict";
import { test } from "vitest";
import { limitRecovery, limitRecovered } from "./limitRecovery.ts";

test("limit recovery handles quota guidance and recovered snapshots", () => {
  const now = 1800000000;
  const agent = {
    accountKey: "work",
    error: { codexErrorInfo: "rateLimitExceeded" },
  };
  const limits = (reached) => ({
    accountKey: "work",
    at: now,
    data: { rateLimits: { rateLimitReachedType: reached } },
  });
  for (const [reached, guidance] of [
    ["workspace_owner_credits_depleted", "Add credits"],
    [
      "workspace_member_credits_depleted",
      "Ask your workspace owner to add credits",
    ],
    [
      "workspace_owner_usage_limit_reached",
      "Increase your workspace usage limit",
    ],
    [
      "workspace_member_usage_limit_reached",
      "Ask your workspace owner to increase your usage limit",
    ],
  ]) {
    assert.ok(
      limitRecovery(agent, limits(reached), now).message.includes(guidance),
      reached,
    );
  }
  for (const role of ["owner", "member"]) {
    const result = limitRecovery(
      {
        ...agent,
        error: JSON.stringify({ codexErrorInfo: "usageLimitExceeded" }),
      },
      limits(`workspace_${role}_credits_depleted`),
      now,
    );
    assert.match(result.title, /usage limit/);
    assert.doesNotMatch(result.message, /add credits/i);
  }
  for (const snapshot of [
    null,
    { ...limits("workspace_owner_credits_depleted"), error: "refresh failed" },
    { ...limits("workspace_owner_credits_depleted"), stale: true },
    { ...limits("workspace_owner_credits_depleted"), accountKey: "personal" },
    { ...limits("workspace_owner_credits_depleted"), at: now - 301 },
    {
      ...limits("workspace_owner_credits_depleted"),
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_owner_credits_depleted",
          primary: { resetsAt: now - 1 },
        },
      },
    },
    limits("future_unknown_type"),
  ]) {
    assert.match(
      limitRecovery(agent, snapshot, now).message,
      /^View account limits/,
    );
  }
  assert.equal(
    limitRecovery(
      { ...agent, error: { codexErrorInfo: "serverOverloaded" } },
      limits("workspace_owner_credits_depleted"),
      now,
    ),
    null,
  );
  assert.equal(
    limitRecovery(
      { ...agent, error: undefined },
      limits("workspace_owner_credits_depleted"),
      now,
    ),
    null,
  );
  for (const [plan, label] of [
    ["pro", "View credits in ChatGPT"],
    ["plus", "View credits in ChatGPT"],
    ["free", "View ChatGPT usage"],
  ]) {
    const snapshot = limits(undefined);
    snapshot.data.rateLimits.planType = plan;
    snapshot.data.rateLimits.primary = { usedPercent: 100, resetsAt: now + 60 };
    snapshot.data.rateLimits.secondary = {
      usedPercent: 100,
      resetsAt: now + 180,
    };
    const recovery = limitRecovery(agent, snapshot, now);
    assert.equal(recovery.action.label, label);
    assert.equal(
      recovery.resetAt,
      now + 180,
      "All exhausted windows must reset",
    );
    assert.equal(
      limitRecovery(agent, { ...snapshot, accountKey: "another" }, now).action,
      undefined,
    );
  }
  assert.match(
    limitRecovery(agent, limits("workspace_owner_credits_depleted"), now).action
      .href,
    /admin\/billing/,
  );
  assert.match(
    limitRecovery(agent, limits("workspace_owner_usage_limit_reached"), now)
      .action.href,
    /usage-limits/,
  );
  assert.match(
    limitRecovery(agent, limits("workspace_member_usage_limit_reached"), now)
      .ownerRequest,
    /increase my limit/,
  );
  assert.equal(
    limitRecovery(agent, limits("workspace_member_credits_depleted"), now)
      .action,
    undefined,
  );

  const recoveredAgent = { ...agent, nativeLimitErrorAt: now - 10 };
  const available = {
    accountKey: "work",
    at: now,
    data: {
      ordinaryUsageAllowed: true,
      rateLimits: {
        rateLimitReachedType: null,
        primary: { usedPercent: 14, resetsAt: now + 3600 },
      },
    },
  };
  assert.equal(limitRecovery(recoveredAgent, available, now), null);
  assert.equal(limitRecovered(recoveredAgent, available, now), true);
  for (const unavailable of [
    { ...available, accountKey: "another" },
    { ...available, stale: true },
    { ...available, error: "Offline" },
    { ...available, at: now - 20 },
    { ...available, at: now - 301 },
    { ...available, data: { ...available.data, ordinaryUsageAllowed: false } },
    {
      ...available,
      data: { ...available.data, ordinaryUsageAllowed: undefined },
    },
    {
      ...available,
      data: {
        ...available.data,
        rateLimits: {
          rateLimitReachedType: "workspace_member_usage_limit_reached",
        },
      },
    },
    {
      ...available,
      data: {
        ...available.data,
        rateLimits: { primary: { usedPercent: 100, resetsAt: now + 3600 } },
      },
    },
  ])
    assert.equal(limitRecovered(recoveredAgent, unavailable, now), false);
});

const now = 1_800_000_000;
const rateLimitAgent = {
  accountKey: "work",
  error: { codexErrorInfo: "rateLimitExceeded" },
  nativeLimitErrorAt: now - 10,
};
const availableLimits = (overrides = {}) => ({
  accountKey: "work",
  at: now,
  data: {
    ordinaryUsageAllowed: true,
    rateLimits: {
      rateLimitReachedType: null,
      primary: { usedPercent: 50, resetsAt: now + 60 },
    },
  },
  ...overrides,
});

test("recovered limit requires a newer, fresh snapshot for the same account", () => {
  const lastEventAgent = {
    ...rateLimitAgent,
    nativeLimitErrorAt: "not a timestamp",
    lastEvent: new Date((now - 10) * 1000).toISOString(),
  };
  assert.equal(limitRecovered(lastEventAgent, availableLimits(), now), true);
  assert.equal(
    limitRecovered(
      {
        ...lastEventAgent,
        lastEvent: new Date((now - 10) * 1000).toISOString(),
      },
      availableLimits({ at: now - 10 }),
      now,
    ),
    false,
    "a snapshot at the error time is not a recovery",
  );
  assert.equal(
    limitRecovered(
      { ...lastEventAgent, lastEvent: "invalid" },
      availableLimits(),
      now,
    ),
    true,
    "an invalid event time does not block a valid newer snapshot",
  );

  for (const limits of [
    null,
    availableLimits({ error: "offline" }),
    availableLimits({ stale: true }),
    availableLimits({ loading: true }),
    availableLimits({ accountKey: "personal" }),
    availableLimits({ at: Number.NaN }),
    availableLimits({ at: now - 301 }),
    availableLimits({ at: now + 6 }),
    availableLimits({ at: now - 10 }),
    availableLimits({ data: { ordinaryUsageAllowed: false, rateLimits: {} } }),
    availableLimits({ data: { ordinaryUsageAllowed: 1, rateLimits: {} } }),
    availableLimits({ data: { ordinaryUsageAllowed: true } }),
    availableLimits({ data: null }),
  ]) {
    assert.equal(
      limitRecovered(rateLimitAgent, limits, now),
      false,
      JSON.stringify(limits),
    );
  }
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, nativeLimitErrorAt: now - 400 },
      availableLimits({ at: now - 300 }),
      now,
    ),
    true,
    "a snapshot exactly 300 seconds old is still fresh",
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, nativeLimitErrorAt: now - 400 },
      availableLimits({ at: now - 301 }),
      now,
    ),
    false,
    "a snapshot older than 300 seconds is stale despite an older error",
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, nativeLimitErrorAt: now - 10 },
      availableLimits({ at: now + 5 }),
      now,
    ),
    true,
    "a snapshot exactly five seconds in the future is accepted",
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, nativeLimitErrorAt: Number.POSITIVE_INFINITY },
      availableLimits(),
      now,
    ),
    true,
    "non-finite native timestamps fall back to lastEvent",
  );
  assert.equal(
    limitRecovered(
      {
        ...rateLimitAgent,
        nativeLimitErrorAt: String(now + 10),
        lastEvent: new Date((now - 10) * 1000).toISOString(),
      },
      availableLimits(),
      now,
    ),
    true,
    "numeric strings are not native numeric timestamps",
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, error: { codexErrorInfo: "serverOverloaded" } },
      availableLimits(),
      now,
    ),
    false,
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, error: "{bad json" },
      availableLimits(),
      now,
    ),
    false,
  );
  assert.equal(
    limitRecovered(
      { error: { codexErrorInfo: "rateLimitExceeded" } },
      availableLimits({ accountKey: "default" }),
      now,
    ),
    true,
    "an omitted account key uses the default account identity",
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, accountKey: undefined },
      availableLimits({ accountKey: "default" }),
      now,
    ),
    true,
  );
  assert.equal(
    limitRecovered(
      { ...rateLimitAgent, accountKey: "default" },
      availableLimits({ accountKey: undefined }),
      now,
    ),
    true,
    "a missing limits account key uses the default identity",
  );
});

test("recovered limit requires every rate-limit bucket and window to allow usage", () => {
  const rejectedSnapshots = [
    { rateLimitReachedType: "workspace_member_usage_limit_reached" },
    { spendControlReached: true },
    { individualLimit: { remainingPercent: 0 } },
    { individualLimit: { remainingPercent: -1 } },
    { primary: { usedPercent: 100 } },
    { primary: { usedPercent: 101, resetsAt: now + 10 } },
    { primary: { usedPercent: -1, resetsAt: now + 10 } },
    { primary: { usedPercent: Number.NaN, resetsAt: now + 10 } },
    { primary: { usedPercent: 50, resetsAt: now } },
    { primary: { usedPercent: 50, resetsAt: now - 1 } },
    { secondary: { usedPercent: 100, resetsAt: now + 10 } },
  ];
  for (const bucket of rejectedSnapshots) {
    const limits = availableLimits({
      data: {
        ordinaryUsageAllowed: true,
        rateLimits: { ...availableLimits().data.rateLimits, ...bucket },
      },
    });
    assert.equal(
      limitRecovered(rateLimitAgent, limits, now),
      false,
      JSON.stringify(bucket),
    );
  }
  assert.equal(
    limitRecovered(
      rateLimitAgent,
      availableLimits({
        data: {
          ordinaryUsageAllowed: true,
          rateLimits: { primary: { usedPercent: 50 } },
          rateLimitsByLimitId: {
            allowed: { primary: { usedPercent: 30 } },
            denied: { spendControlReached: true },
          },
        },
      }),
      now,
    ),
    false,
    "a secondary limit bucket can prevent recovered status",
  );
  assert.equal(
    limitRecovered(
      rateLimitAgent,
      availableLimits({
        data: {
          ordinaryUsageAllowed: true,
          rateLimits: { primary: { usedPercent: 0 } },
        },
      }),
      now,
    ),
    true,
    "a zero usage window is still available",
  );
  assert.equal(
    limitRecovered(
      rateLimitAgent,
      availableLimits({
        data: {
          ordinaryUsageAllowed: true,
          rateLimits: { individualLimit: { remainingPercent: 1 } },
        },
      }),
      now,
    ),
    true,
    "a positive individual allowance remains usable",
  );
  assert.equal(
    limitRecovered(
      rateLimitAgent,
      availableLimits({
        data: {
          ordinaryUsageAllowed: true,
          rateLimits: { primary: { usedPercent: 30 } },
          rateLimitsByLimitId: { allowed: { primary: { usedPercent: 20 } } },
        },
      }),
      now,
    ),
    true,
    "the primary bucket and every additional bucket may be available",
  );
});

test("limit notice exposes the right workspace action and preserves reset time", () => {
  const cases = [
    [
      "workspace_owner_credits_depleted",
      "Workspace credits depleted",
      "Add workspace credits",
      "admin/billing?codex_credit_action=add_credits",
    ],
    [
      "workspace_owner_usage_limit_reached",
      "Workspace usage limit reached",
      "Open workspace usage limits",
      "admin/usage-limits/workspace",
    ],
  ];
  for (const [reached, title, label, href] of cases) {
    const recovery = limitRecovery(
      rateLimitAgent,
      availableLimits({
        data: {
          rateLimits: {
            rateLimitReachedType: reached,
            primary: { usedPercent: 100, resetsAt: now + 30 },
            secondary: { usedPercent: 100, resetsAt: now + 90 },
          },
        },
      }),
      now,
    );
    assert.equal(recovery.title, title);
    assert.equal(recovery.action.label, label);
    assert.ok(recovery.action.href.includes(href));
    assert.equal(recovery.resetAt, now + 90);
    assert.equal(recovery.ownerRequest, undefined);
  }

  const ownerCredits = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_owner_credits_depleted",
        },
      },
    }),
    now,
  );
  assert.equal(
    ownerCredits.message,
    "Your workspace is out of credits. Add credits to continue using Codex.",
  );

  const memberCredits = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_member_credits_depleted",
        },
      },
    }),
    now,
  );
  assert.equal(memberCredits.title, "Workspace credits depleted");
  assert.equal(memberCredits.action, undefined);
  assert.match(memberCredits.ownerRequest, /add credits/);
  const memberUsage = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_member_usage_limit_reached",
        },
      },
    }),
    now,
  );
  assert.equal(memberUsage.action, undefined);
  assert.match(memberUsage.ownerRequest, /increase my limit/);
  assert.equal(
    limitRecovery(
      rateLimitAgent,
      availableLimits({
        data: { rateLimits: { rateLimitReachedType: "unknown_future_reason" } },
      }),
      now,
    ).message,
    "View account limits for reset times and available credits.",
    "unknown reached types must not expose a purchase action",
  );
  assert.equal(
    limitRecovery(
      { ...rateLimitAgent, error: { codexErrorInfo: "usageLimitExceeded" } },
      availableLimits({
        data: { rateLimits: { rateLimitReachedType: "unknown_future_reason" } },
      }),
      now,
    ).message,
    "View account limits for reset times and available credits.",
    "usage-limit normalization leaves unknown reasons generic",
  );

  const defaultAccountNotice = limitRecovery(
    { error: rateLimitAgent.error, accountKey: "default" },
    {
      at: now,
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_owner_credits_depleted",
        },
      },
    },
    now,
  );
  assert.equal(defaultAccountNotice.action.label, "Add workspace credits");
  assert.equal(
    limitRecovery(
      { error: rateLimitAgent.error, accountKey: "default" },
      {
        at: now,
        data: {
          rateLimits: {
            rateLimitReachedType: "workspace_owner_credits_depleted",
          },
        },
      },
      now,
    ).action.label,
    "Add workspace credits",
    "a missing limits account key matches the default account",
  );
  assert.equal(
    limitRecovery(
      { error: rateLimitAgent.error },
      {
        accountKey: "default",
        at: now,
        data: {
          rateLimits: {
            rateLimitReachedType: "workspace_owner_credits_depleted",
          },
        },
      },
      now,
    ).action.label,
    "Add workspace credits",
    "an omitted agent account key uses the default account",
  );
  assert.equal(
    limitRecovery(
      rateLimitAgent,
      availableLimits({
        at: now - 300,
        data: {
          rateLimits: {
            rateLimitReachedType: "workspace_owner_credits_depleted",
          },
        },
      }),
      now,
    ).action.label,
    "Add workspace credits",
    "a limits snapshot exactly 300 seconds old remains eligible for guidance",
  );
  assert.equal(
    limitRecovery(
      rateLimitAgent,
      availableLimits({
        at: now - 301,
        data: {
          rateLimits: {
            rateLimitReachedType: "workspace_owner_credits_depleted",
          },
        },
      }),
      now,
    ).message,
    "View account limits for reset times and available credits.",
    "limit guidance expires after 300 seconds",
  );
});

test("limit notice handles stale snapshots, reset boundaries, and plan guidance", () => {
  for (const limits of [
    undefined,
    availableLimits({ loading: true }),
    availableLimits({ at: now - 301 }),
    availableLimits({ at: Number.POSITIVE_INFINITY }),
    availableLimits({ at: now + 301 }),
    availableLimits({ data: {} }),
    availableLimits({ data: null }),
    availableLimits({
      data: {
        rateLimits: {
          rateLimitReachedType: "workspace_owner_credits_depleted",
          primary: { resetsAt: now },
        },
      },
    }),
  ]) {
    const recovery = limitRecovery(rateLimitAgent, limits, now);
    assert.equal(
      recovery.action,
      undefined,
      "unusable snapshots retain generic guidance",
    );
    assert.equal(
      recovery.message,
      "View account limits for reset times and available credits.",
    );
  }

  for (const [plan, label, href] of [
    ["pro", "View credits in ChatGPT", "credits_modal=true"],
    ["plus", "View credits in ChatGPT", "credits_modal=true"],
    ["pro_lite", "View credits in ChatGPT", "credits_modal=true"],
    ["free", "View ChatGPT usage", "/codex/settings/usage"],
    ["go", "View ChatGPT usage", "/codex/settings/usage"],
  ]) {
    const recovery = limitRecovery(
      rateLimitAgent,
      availableLimits({ data: { rateLimits: { planType: plan } } }),
      now,
    );
    assert.equal(recovery.action.label, label);
    assert.equal(recovery.title, "Usage limit reached");
    assert.ok(recovery.action.href.includes(href));
    assert.equal(recovery.resetAt, undefined);
    assert.equal(
      recovery.message,
      plan === "free" || plan === "go"
        ? "Wait for the limit to reset, or review your plan in ChatGPT."
        : "Wait for the limit to reset, or add credits in ChatGPT.",
    );
  }
  const unknownPlan = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: {
        rateLimits: {
          planType: "enterprise_future",
          primary: { usedPercent: 100, resetsAt: now + 45 },
        },
      },
    }),
    now,
  );
  assert.equal(unknownPlan.action, undefined);
  assert.equal(unknownPlan.resetAt, now + 45);
  assert.equal(unknownPlan.title, "Usage limit reached");
  assert.equal(
    unknownPlan.message,
    "View account limits for reset times and available credits.",
  );
  const unknownReasonWithPlan = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: { rateLimits: { planType: "pro", rateLimitReachedType: "future" } },
    }),
    now,
  );
  assert.equal(unknownReasonWithPlan.action, undefined);
  assert.equal(
    unknownReasonWithPlan.message,
    "View account limits for reset times and available credits.",
  );

  const invalidResetCandidate = limitRecovery(
    rateLimitAgent,
    availableLimits({
      data: {
        rateLimits: {
          planType: "plus",
          primary: { usedPercent: 99, resetsAt: now + 30 },
        },
      },
    }),
    now,
  );
  assert.equal(invalidResetCandidate.resetAt, undefined);
  for (const window of [
    { usedPercent: 99, resetsAt: now + 30 },
    { usedPercent: "100", resetsAt: now + 30 },
    { usedPercent: 100, resetsAt: String(now + 30) },
  ]) {
    const recovery = limitRecovery(
      rateLimitAgent,
      availableLimits({
        data: { rateLimits: { planType: "plus", primary: window } },
      }),
      now,
    );
    assert.equal(recovery.resetAt, undefined, JSON.stringify(window));
  }
});
