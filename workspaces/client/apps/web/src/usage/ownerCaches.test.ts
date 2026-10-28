import type { components } from "../generated/api";
import { afterEach, expect, it, vi } from "vitest";
import { createSessionCostCache } from "./sessionCostCache";
import { fakeUsageCore } from "./usageCore.fixture";

afterEach(() => vi.restoreAllMocks());

const cost = (totalUSD: number) =>
  ({
    rootId: "same",
    cacheAgeSeconds: 0,
    pricedSamples: 0,
    refreshing: false,
    pricingState: "ready",
    estimated: true,
    unknownModels: [],
    breakdown: { models: {}, providers: {} },
    totalUSD,
  }) satisfies components["schemas"]["SessionCostResponse"];

it("isolates in-memory and persisted estimates for matching session IDs", async () => {
  const coreA = fakeUsageCore(),
    coreB = fakeUsageCore();
  const a = createSessionCostCache(() => coreA.request),
    b = createSessionCostCache(() => coreB.request);
  a.storeSessionCost("state", "same", cost(1));
  expect(b.peekSessionCost("state", "same")).toBeNull();
  b.storeSessionCost("state", "same", cost(9));
  expect(a.peekSessionCost("state", "same")?.totalUSD).toBe(1);
  await vi.waitFor(() =>
    expect(coreB.methods).toContain("usage.sessionCosts.put"),
  );
  const restored = createSessionCostCache(() => coreB.request);
  await vi.waitFor(() =>
    expect(restored.peekSessionCost("state", "same")?.totalUSD).toBe(9),
  );
  expect(a.peekSessionCost("state", "same")?.totalUSD).toBe(1);
});

it("restores saved estimates after a restart without touching browser storage", async () => {
  const touched = vi.fn(() => {
    throw new Error("browser storage must not be used");
  });
  vi.stubGlobal("localStorage", new Proxy({}, { get: touched }));
  vi.stubGlobal("sessionStorage", new Proxy({}, { get: touched }));
  const core = fakeUsageCore();
  createSessionCostCache(() => core.request).storeSessionCost("state", "same", {
    ...cost(4),
    cacheAgeSeconds: 30,
  });
  await vi.waitFor(() =>
    expect(core.methods).toContain("usage.sessionCosts.put"),
  );
  const restored = createSessionCostCache(() => core.request);
  await vi.waitFor(() =>
    expect(restored.peekSessionCost("state", "same")?.totalUSD).toBe(4),
  );
  expect(
    restored.peekSessionCost("state", "same")?.cacheAgeSeconds,
  ).toBeGreaterThanOrEqual(30);
  expect(touched).not.toHaveBeenCalled();
});

it("keeps the live estimate when the native core is unavailable", () => {
  const core = fakeUsageCore({ available: false });
  const cache = createSessionCostCache(() => core.request);
  cache.storeSessionCost("state", "same", cost(2));
  expect(cache.peekSessionCost("state", "same")?.totalUSD).toBe(2);
});

it("drops from memory the estimates the core evicts", async () => {
  const core = fakeUsageCore();
  const cache = createSessionCostCache(() => core.request);
  for (let index = 0; index < 17; index++)
    cache.storeSessionCost("state", `root-${index}`, {
      ...cost(index),
      rootId: `root-${index}`,
    });
  await vi.waitFor(() =>
    expect(cache.peekSessionCost("state", "root-0")).toBeNull(),
  );
  expect(cache.peekSessionCost("state", "root-16")?.totalUSD).toBe(16);
});
