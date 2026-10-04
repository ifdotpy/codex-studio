import assert from "node:assert/strict";
import { accountLimits } from "./accountUsage.ts";

import { it } from "vitest";

it("returns limits only for the selected account and matching native identity", () => {
  const a = { accountKey: "a", data: { accountId: "native-a" } };
  assert.equal(accountLimits(a, "a", "native-a"), a);
  assert.equal(accountLimits(a, "b"), null);
  assert.equal(accountLimits(a, "a", "replacement-login"), null);
  assert.equal(
    accountLimits(
      { accountKey: "a", data: { accountId: 42 } },
      "a",
      "native-a",
    ),
    null,
    "malformed native identity does not match the selected account",
  );
  const legacyWithoutAccountId = { accountKey: "a", data: {} };
  assert.equal(
    accountLimits(legacyWithoutAccountId, "a", "native-a"),
    legacyWithoutAccountId,
  );
  const legacyWithoutData = { accountKey: "a" };
  assert.equal(
    accountLimits(legacyWithoutData, "a", "native-a"),
    legacyWithoutData,
  );
  assert.equal(accountLimits({ data: {} }, "a"), null);
  assert.equal(accountLimits({ data: {} }, "default"), null);
  assert.equal(accountLimits(null, "default"), null);
  assert.equal(accountLimits("not an object", "a"), null);

  const generatedResponse = {
    accountKey: "a",
    at: 120,
    checkedAt: 121,
    data: {
      accountId: "native-a",
      ordinaryUsageAllowed: true,
      rateLimits: { primary: { usedPercent: 25, resetsAt: 200 } },
    },
  };
  assert.equal(
    accountLimits(generatedResponse, "a", "native-a"),
    generatedResponse,
  );
  assert.equal(
    accountLimits(
      {
        ...generatedResponse,
        data: {
          ...generatedResponse.data,
          rateLimits: { primary: { usedPercent: "25" } },
        },
      },
      "a",
      "native-a",
    ),
    null,
  );
  const generatedSnapshot = {
    accountKey: "a",
    at: 122,
    data: {
      accountId: "native-a",
      ordinaryUsageAllowed: true,
      rateLimits: {
        planType: "plus",
        primary: { usedPercent: 25, windowDurationMins: 10080 },
      },
    },
  };
  assert.equal(
    accountLimits(generatedSnapshot, "a", "native-a"),
    generatedSnapshot,
  );
  console.log("Account limits ownership: 11 passed");
});
