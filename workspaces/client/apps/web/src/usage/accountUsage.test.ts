import assert from "node:assert/strict";
import {
  limitsReadSucceeded,
  shouldReplaceLimitsSnapshot,
} from "./accountUsage.ts";
import type { AccountLimitsSnapshot } from "./accountUsage.ts";
import { scopedStorage } from "../servers/storage";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  accountLimits,
  createAccountLimitsCache,
  cacheAccountLimits,
  cachedAccountLimits,
  configureLimitsCache,
  limitsSnapshotIsFresh,
} from "./accountUsage";
const value = (
  accountKey: string,
  at = 10,
  usedPercent = 42,
): AccountLimitsSnapshot => ({
  accountKey,
  at,
  data: { accountId: accountKey, rateLimits: { primary: { usedPercent } } },
  error: null,
});
let scope = 0;
beforeEach(() => {
  const records = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => records.get(key) ?? null,
    setItem: (key: string, text: string) => records.set(key, text),
  });
  configureLimitsCache(`test-${++scope}`);
});
describe("account limits cache", () => {
  it("restores values synchronously before a read starts", () => {
    const name = `test-${scope}`;
    cacheAccountLimits(value("a"));
    configureLimitsCache("other");
    configureLimitsCache(name);
    expect(cachedAccountLimits("a", "a")).toEqual(value("a"));
  });
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
    configureLimitsCache("other-test");
    configureLimitsCache(`test-${scope}`);
    expect(cachedAccountLimits("a")).toEqual(failed);
  });
  it("returns no value when nothing is known and honors a newer empty read", () => {
    expect(cachedAccountLimits("unknown")).toBeNull();
    cacheAccountLimits(value("a"));
    cacheAccountLimits({ accountKey: "a", data: null, at: 30, error: null });
    expect(cachedAccountLimits("a")?.data).toBeNull();
    expect(accountLimits(value("b"), "a")).toBeNull();
  });
});

it("isolates overlapping server paths and account keys through delayed writes and reloads", () => {
  const storage = (owner: string) =>
    scopedStorage(() => localStorage, `server:${owner}:`);
  const a = createAccountLimitsCache(storage("a"));
  const b = createAccountLimitsCache(storage("b"));
  a.configureLimitsCache("/same/state");
  b.configureLimitsCache("/same/state");
  const changedA = vi.fn();
  const changedB = vi.fn();
  const stopA = a.subscribeLimitsCache(changedA);
  const stopB = b.subscribeLimitsCache(changedB);
  const completeA = () => a.cacheAccountLimits(value("default", 30, 10));
  b.cacheAccountLimits(value("default", 40, 90));
  completeA();
  expect(a.cachedAccountLimits("default")).toEqual(value("default", 30, 10));
  expect(b.cachedAccountLimits("default")).toEqual(value("default", 40, 90));
  expect(changedA).toHaveBeenCalledTimes(1);
  expect(changedB).toHaveBeenCalledTimes(1);
  const restoredA = createAccountLimitsCache(storage("a"));
  const restoredB = createAccountLimitsCache(storage("b"));
  restoredA.configureLimitsCache("/same/state");
  restoredB.configureLimitsCache("/same/state");
  expect(restoredA.cachedAccountLimits("default")).toEqual(
    a.cachedAccountLimits("default"),
  );
  expect(restoredB.cachedAccountLimits("default")).toEqual(
    b.cachedAccountLimits("default"),
  );
  stopA();
  stopB();
});

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
