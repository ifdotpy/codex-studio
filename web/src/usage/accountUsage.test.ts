import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  accountLimits,
  cacheAccountLimits,
  cachedAccountLimits,
  configureLimitsCache,
  limitsSnapshotIsFresh,
} from "./accountUsage";
const value = (accountKey: string, at = 10, usedPercent = 42) => ({
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
    cacheAccountLimits({ ...value("a"), error: "Refresh failed" });
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
