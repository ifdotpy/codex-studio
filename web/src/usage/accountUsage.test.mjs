import assert from "node:assert/strict";
import {
  accountLimits,
  limitsReadSucceeded,
  limitsSnapshotIsFresh,
  shouldReplaceLimitsSnapshot,
} from "./accountUsage.ts";

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

it("uses matching, error-free snapshot timestamps for limits freshness", () => {
  const snapshot = {
    accountKey: "a",
    at: 41,
    data: { accountId: "native-a" },
  };
  assert.equal(limitsSnapshotIsFresh(snapshot, "a", "native-a", 100), true);
  assert.equal(
    limitsSnapshotIsFresh({ ...snapshot, at: 40 }, "a", "native-a", 100),
    false,
    "a snapshot at the 60-second boundary is stale",
  );
  assert.equal(
    limitsSnapshotIsFresh({ ...snapshot, at: null }, "a", "native-a", 100),
    false,
  );
  assert.equal(
    limitsSnapshotIsFresh(
      { ...snapshot, error: "provider unavailable" },
      "a",
      "native-a",
      100,
    ),
    false,
  );
  assert.equal(limitsSnapshotIsFresh(snapshot, "b", "native-a", 100), false);
  assert.equal(
    limitsSnapshotIsFresh(snapshot, "a", "replacement-login", 100),
    false,
  );
});

it("accepts an error-free null-data limits response as a successful read", () => {
  const noLimits = {
    accountKey: "a",
    at: 99,
    data: null,
    error: null,
  };
  assert.equal(accountLimits(noLimits, "a"), noLimits);
  assert.equal(limitsReadSucceeded(noLimits), true);
  assert.equal(limitsSnapshotIsFresh(noLimits, "a", undefined, 100), true);
  assert.equal(
    shouldReplaceLimitsSnapshot(
      { accountKey: "a", at: 98, data: { rateLimits: {} } },
      noLimits,
    ),
    true,
    "a fresh no-limits answer replaces older allowance data",
  );
  assert.equal(
    shouldReplaceLimitsSnapshot(
      { accountKey: "a", at: 99, data: { rateLimits: {} } },
      { ...noLimits, at: null },
    ),
    true,
    "a null timestamp does not preserve stale allowance data",
  );
  assert.equal(
    shouldReplaceLimitsSnapshot(
      { accountKey: "a", at: 99, data: { rateLimits: {} } },
      { ...noLimits, at: 99 },
    ),
    true,
    "an equal timestamp does not preserve stale allowance data",
  );
  assert.equal(
    shouldReplaceLimitsSnapshot(
      { accountKey: "a", at: 99, data: { rateLimits: {} } },
      { ...noLimits, at: null, error: "read failed" },
    ),
    false,
    "an error response does not replace a successful snapshot",
  );
});
