#!/usr/bin/env node
// Test the production recovery classifier without a backend or model request.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { nativeErrorKind } from "../web/src/nativeErrors.ts";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const ts = require("typescript");
const source = await readFile(
  new URL("../web/src/limitRecovery.ts", import.meta.url),
  "utf8",
);
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS },
}).outputText;
const module = { exports: {} };
new Function("require", "module", "exports", compiled)(
  (name) => (name === "./nativeErrors" ? { nativeErrorKind } : require(name)),
  module,
  module.exports,
);
const { limitRecovery, limitRecovered } = module.exports;
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
console.log(
  "Native limit recovery: role guidance, usage precedence, stale/account isolation, unknown types passed.",
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
  assert.equal(recovery.resetAt, now + 180, "All exhausted windows must reset");
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
  limitRecovery(agent, limits("workspace_member_credits_depleted"), now).action,
  undefined,
);

const recoveredAgent = { ...agent, nativeLimitErrorAt: now - 10 };
const available = { accountKey: "work", at: now, data: {
  ordinaryUsageAllowed: true,
  rateLimits: { rateLimitReachedType: null, primary: { usedPercent: 14, resetsAt: now + 3600 } },
} };
assert.equal(limitRecovery(recoveredAgent, available, now), null);
assert.equal(limitRecovered(recoveredAgent, available, now), true);
for (const unavailable of [
  { ...available, accountKey: "another" },
  { ...available, stale: true },
  { ...available, error: "Offline" },
  { ...available, at: now - 20 },
  { ...available, at: now - 301 },
  { ...available, data: { ...available.data, ordinaryUsageAllowed: false } },
  { ...available, data: { ...available.data, ordinaryUsageAllowed: undefined } },
  { ...available, data: { ...available.data, rateLimits: { rateLimitReachedType: "workspace_member_usage_limit_reached" } } },
  { ...available, data: { ...available.data, rateLimits: { primary: { usedPercent: 100, resetsAt: now + 3600 } } } },
]) assert.equal(limitRecovered(recoveredAgent, unavailable, now), false);
console.log("PASS: recovered quota clears the current notice; stale, earlier, other-account, exhausted, and unknown data retain it.");
