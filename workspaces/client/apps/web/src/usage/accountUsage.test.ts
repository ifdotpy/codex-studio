import type { components } from "../generated/api";
import assert from "node:assert/strict";
import {
  limitsReadSucceeded,
  shouldReplaceLimitsSnapshot,
} from "./accountUsage.ts";
import type { AccountLimitsSnapshot } from "./accountUsage.ts";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  accountLimits,
  createAccountLimitsCache,
  cacheAccountLimits,
  cachedAccountLimits,
  configureLimitsCache,
  limitsSnapshotIsFresh,
} from "./accountUsage";
import { fakeUsageCore } from "./usageCore.fixture";
const value = (
  accountKey: string,
  at = 10,
  usedPercent = 42,
): AccountLimitsSnapshot =>
  ({
    accountKey,
    at,
    data: { accountId: accountKey, rateLimits: { primary: { usedPercent } } },
    error: null,
  }) satisfies components["schemas"]["AccountRateLimitsDto"];
const puts = (core: ReturnType<typeof fakeUsageCore>) =>
  core.methods.filter((method) => method === "usage.limits.put");
let scope = 0;
beforeEach(() => {
  configureLimitsCache(`test-${++scope}`);
});
describe("account limits cache", () => {
  it("replaces a cached snapshot with newer data and rejects older data", () => {
    cacheAccountLimits(value("a"));
    cacheAccountLimits(value("a", 20, 75));
    cacheAccountLimits(value("a", 5, 1));
    expect(cachedAccountLimits("a")).toEqual(value("a", 20, 75));
  });
  it("keeps accounts, identities, and server state directories separate", () => {
    cacheAccountLimits(value("a"));
    cacheAccountLimits(value("b", 20, 80));
    expect(cachedAccountLimits("a", "b")).toBeNull();
    expect(cachedAccountLimits("b")).toEqual(value("b", 20, 80));
    configureLimitsCache("new-server");
    expect(cachedAccountLimits("a")).toBeNull();
  });
  it("keeps known values after a refresh error without marking them fresh", () => {
    cacheAccountLimits(value("a"));
    const failed = {
      ...value("a"),
      error: "Provider request failed",
      readError: {
        message: "Provider request failed",
        status: 503,
        location: ["query", "account_key"],
      },
    };
    cacheAccountLimits(failed);
    expect(cachedAccountLimits("a")?.data).toEqual(value("a").data);
    expect(limitsSnapshotIsFresh(cachedAccountLimits("a"), "a", "a", 20)).toBe(
      false,
    );
  });
  it("returns no value when nothing is known and honors a newer empty read", () => {
    expect(cachedAccountLimits("unknown")).toBeNull();
    cacheAccountLimits(value("a"));
    cacheAccountLimits({ accountKey: "a", data: null, at: 30, error: null });
    expect(cachedAccountLimits("a")?.data).toBeNull();
    expect(accountLimits(value("b"), "a")).toBeNull();
  });
});

it("isolates overlapping server state directories and account keys through delayed writes and reloads", async () => {
  const coreA = fakeUsageCore(),
    coreB = fakeUsageCore();
  const a = createAccountLimitsCache(() => coreA.request);
  const b = createAccountLimitsCache(() => coreB.request);
  a.configureLimitsCache("/same/state");
  b.configureLimitsCache("/same/state");
  const changedA = vi.fn();
  const changedB = vi.fn();
  const stopA = a.subscribeLimitsCache(changedA);
  const stopB = b.subscribeLimitsCache(changedB);
  b.cacheAccountLimits(value("default", 40, 90));
  a.cacheAccountLimits(value("default", 30, 10));
  await vi.waitFor(() => expect(puts(coreA)).toHaveLength(1));
  await vi.waitFor(() => expect(puts(coreB)).toHaveLength(1));
  expect(a.cachedAccountLimits("default")).toEqual(value("default", 30, 10));
  expect(b.cachedAccountLimits("default")).toEqual(value("default", 40, 90));
  expect(changedA).toHaveBeenCalledTimes(1);
  expect(changedB).toHaveBeenCalledTimes(1);
  const restoredA = createAccountLimitsCache(() => coreA.request);
  const restoredB = createAccountLimitsCache(() => coreB.request);
  restoredA.configureLimitsCache("/same/state");
  restoredB.configureLimitsCache("/same/state");
  await vi.waitFor(() =>
    expect(restoredA.cachedAccountLimits("default")).toEqual(
      value("default", 30, 10),
    ),
  );
  await vi.waitFor(() =>
    expect(restoredB.cachedAccountLimits("default")).toEqual(
      value("default", 40, 90),
    ),
  );
  stopA();
  stopB();
});

it("loads saved limits without replacing a newer live snapshot", async () => {
  const core = fakeUsageCore();
  const first = createAccountLimitsCache(() => core.request);
  first.configureLimitsCache("persisted");
  first.cacheAccountLimits(value("saved", 10, 20));
  first.cacheAccountLimits(value("live", 10, 30));
  await vi.waitFor(() => expect(puts(core)).toHaveLength(2));
  const restored = createAccountLimitsCache(() => core.request);
  restored.configureLimitsCache("persisted");
  restored.cacheAccountLimits(value("live", 50, 60));
  await vi.waitFor(() =>
    expect(restored.cachedAccountLimits("saved")).toEqual(
      value("saved", 10, 20),
    ),
  );
  expect(restored.cachedLimitsByAccount()).toEqual({
    saved: value("saved", 10, 20),
    live: value("live", 50, 60),
  });
});

it("returns limits only for the selected account and matching native identity", () => {
  const a = { accountKey: "a", data: { accountId: "native-a" } };
  assert.equal(accountLimits(a, "a", "native-a"), a);
  assert.equal(accountLimits(a, "b"), null);
  assert.equal(accountLimits(a, "a", "replacement-login"), null);
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
  assert.equal(accountLimits(null, "default"), null);

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
